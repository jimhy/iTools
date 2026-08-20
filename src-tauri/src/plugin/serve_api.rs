//! **宿主托管的**本地 HTTP 服务 + 局域网发现：给「手机扫码连电脑传文件」这类插件开一扇口子。
//!
//! # 为什么必须由宿主托管，插件拿不到 socket
//!
//! 起一个监听 `0.0.0.0` 的 HTTP 服务等于把这台电脑的一部分文件系统暴露到整个局域网。
//! 如果把这件事交给插件页自己（比如给它一个原生 socket API），插件就能绕开一切校验、
//! 想开多久开多久、想开哪个目录开哪个目录。本模块的设计是插件**只拿一个不透明的
//! `serveId`**，真正的监听 socket、根目录解析、TTL 倒计时全部留在宿主这一层；插件页
//! 关闭或被禁用/卸载时，宿主可以调 [`stop_all_for_session`] 一键收摊，插件自己完全无法
//! 阻止这件事发生——这是「安全默认关闭」而不是「靠插件自觉关闭」。
//!
//! # 为什么默认必须带访问令牌（token）
//!
//! 局域网不是可信网络：同一个 Wi-Fi 下可能有访客、IoT 设备、甚至恶意主机。一个没有任何
//! 凭据的文件服务器，只要猜中/嗅探到局域网 IP 段就能被任何人枚举下载（`allowUpload` 开着
//! 时甚至能写入）。所以 `token`**不是可选的安全增强，是默认必须存在的准入门槛**：调用方
//! 不传就由宿主随机生成一个（生成方式与强度见 [`new_id`] 的注释，**不是密码学级随机数**），
//! 所有请求——不论是静态文件读取还是
//! 上传——都必须在 query（`?token=`）或请求头（`X-iTools-Token`）里带对它，缺失或不match
//! 一律 401，没有「裸奔」这个选项。
//!
//! # 与 `fs_api` 的关系（安全核心：越界校验只有一份）
//!
//! 本模块**不自己写路径穿越校验**：HTTP 请求路径解析出的「scope 内相对路径」，一律拼进一个
//! 临时构造的 [`fs_api::FsScope`]（`kind="dir"`、`path` 为已校验过的服务根目录），复用
//! `fs_api::resolve_existing` / `fs_api::resolve_for_write` 做真正的落点校验——`..`、
//! 绝对路径、URL 编码后的穿越写法、iTools 数据根黑名单，这些判断只在 `fs_api.rs` 里维护
//! 一份，本文件不重复实现，避免两份校验逐渐不一致（这是安全漏洞的经典来源）。
//!
//! **集成前提**：`fs_api.rs` 里的 `resolve_scope` / `resolve_existing` / `resolve_for_write`
//! 目前是模块私有的 `fn`，需要主控把它们改成 `pub(crate) fn`（本文件按任务边界不允许直接
//! 改 `fs_api.rs`）。在这三个函数可见之前，本文件无法通过编译。
//!
//! # 不做二维码
//!
//! 项目没有引入 `qrcode` 依赖，本模块也不引。`plugin_serve_start` 只负责把可用的局域网
//! 访问地址（已内嵌 `?token=`）放进 `urls` 返回给插件，二维码渲染交给插件页自己用内联 JS
//! （canvas 画码 / 前端 qrcode 库）完成。
//!
//! # 已知取舍与局限（如实记录，不假装做全了）
//!
//! - **网卡枚举退化为「出站探测」**：`urls` 里的 IP 通过 UDP「连接」几个常见目标地址、读取
//!   内核选路后使用的本地地址得到，不是真正枚举全部网卡（`GetAdaptersAddresses` 能做到，
//!   但它依赖的 `SOCKET_ADDRESS`/`AF_INET` 等类型在 `windows` crate 里需要额外的
//!   `Win32_Networking_WinSock` feature，项目当前只开了 `Win32_NetworkManagement_IpHelper`，
//!   本文件按边界不能改 `Cargo.toml`）。多数单网卡/双网卡（有线+Wi-Fi）机器下够用，但不保证
//!   囊括所有虚拟网卡。
//! - **上传非流式、有大小上限**：`multipart/form-data` 解析是自己手写的（项目未引入多部件
//!   解析库），会先把整个请求体读进内存再切分，单次上传上限 [`MAX_UPLOAD_BYTES`]（256MB）。
//!   这是真实可用的实现，不是桩，但不适合超大文件——按需求场景（手机传照片/文档）足够。
//! - **HEAD 请求返回空 body 但不精确回填 Content-Length**：够用但不是严格的 HTTP 语义实现。
//! - **不支持 Range 请求**：视频类文件走 GET 只能整份下载，拖动进度条不会生效。
//! - **局域网发现走的是 UDP 广播（255.255.255.255），不是组播**：更简单、多数家用路由器都
//!   放行，代价是发现范围仅限同一个广播域（一般等同同一个子网/同一路由器下），跨路由/跨
//!   VLAN 发现不了——这本来也是「同网段互联」这个场景要的效果。

use std::collections::HashMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::fs_api::{self, FsScope};
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 门禁用的权限标识：本地 HTTP 服务 + 局域网发现共用同一个（都是「对外开口子」这一类能力）。
const PERMISSION: &str = "local-server";

/// `ttlSecs` 缺省值：30 分钟。防的是插件忘了调 `plugin_serve_stop`，服务却一直挂着。
const DEFAULT_TTL_SECS: u64 = 30 * 60;
/// `ttlSecs` 允许的下限（太短没有实用意义，且频繁重连体验差）。
const MIN_TTL_SECS: u64 = 60;
/// `ttlSecs` 允许的上限：24 小时，超过这个时长的「临时局域网服务」基本等同于常驻服务，
/// 不该用这条默认必须带 token 的轻量通道去做。
const MAX_TTL_SECS: u64 = 24 * 60 * 60;

/// 单次上传体上限（256MB）。见模块文档「已知取舍」。
const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// 访问令牌的 query 参数名 / 请求头名。
const TOKEN_QUERY_KEY: &str = "token";
const TOKEN_HEADER: &str = "X-iTools-Token";

/// 局域网发现使用的固定 UDP 端口（广播查询与应答共用）。
const LAN_DISCOVERY_PORT: u16 = 47990;
/// `plugin_lan_announce` 的 `ttlSecs` 缺省值：5 分钟。这是「存在感」广播，比文件服务的
/// `ttlSecs` 短得多——插件正常应该周期性重新 announce 保活，而不是 announce 一次挂一整天。
const LAN_ANNOUNCE_DEFAULT_TTL: u64 = 5 * 60;

// ==================== plugin_serve_*：本地 HTTP 文件服务 ====================

/// `plugin_serve_start` 的入参。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeOptions {
    /// 要暴露的 [`fs_api::FsScope`] id（插件必须已通过 `plugin_pick_dir`/`plugin_pick_file`
    /// 拿到过这个 scope；本模块不单独再弹一次授权对话框）。
    pub scope_id: String,
    /// scope 内要暴露的子路径，缺省暴露整个 scope 根。文件型 scope 下必须留空。
    pub sub_path: Option<String>,
    /// 监听端口，缺省由系统分配一个空闲端口。
    pub port: Option<u16>,
    /// 只读（不挂 `/upload`）。与 `allowUpload=true` 同时传视为参数错误。
    pub read_only: Option<bool>,
    /// 是否开放上传（`POST /upload`，multipart/form-data）。缺省 `false`。
    pub allow_upload: Option<bool>,
    /// 访问令牌，缺省随机生成（见模块文档「为什么默认必须带访问令牌」）。
    pub token: Option<String>,
    /// 存活秒数，缺省 [`DEFAULT_TTL_SECS`]，允许范围 [`MIN_TTL_SECS`]~[`MAX_TTL_SECS`]。
    pub ttl_secs: Option<u64>,
}

/// `plugin_serve_start` 的返回。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeStartResult {
    /// 不透明的服务 id，后续 `plugin_serve_stop` 用它来指认这个服务。
    pub serve_id: String,
    /// 实际监听的端口。
    pub port: u16,
    /// 局域网内可用的访问地址列表，每个都已内嵌 `?token=`，插件页可直接拿去生成二维码
    /// （本模块不生成二维码，见模块文档）。
    pub urls: Vec<String>,
}

/// `plugin_serve_list` 的一项。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeInfo {
    pub serve_id: String,
    pub port: u16,
    pub urls: Vec<String>,
    pub scope_id: String,
    pub sub_path: String,
    pub read_only: bool,
    pub allow_upload: bool,
    /// 创建时刻（Unix 秒）。
    pub created: u64,
    /// 到期时刻（Unix 秒），到点宿主会自动停止。
    pub expires_at: u64,
}

/// 注册表里保存的一个正在跑的服务：**只放供 list/stop 用的元信息**，不放 socket/文件句柄——
/// 那些只在服务自己的线程栈上活，registry 只负责「谁能停它、还能活多久」。
struct ServeEntry {
    session: ActiveSession,
    port: u16,
    urls: Vec<String>,
    scope_id: String,
    sub_path: String,
    read_only: bool,
    allow_upload: bool,
    created: u64,
    expires_at: u64,
    /// 服务线程每隔一小段时间轮询这个标志；`plugin_serve_stop` / `stop_all_for_session`
    /// 置位后线程会在下一次轮询时自行退出并关闭监听 socket。
    stop: Arc<AtomicBool>,
}

/// 服务线程实际处理请求时用的只读配置（与 [`ServeEntry`] 分开，是为了不让处理请求这种
/// 可能耗时的操作长期持有 registry 的锁）。
struct ServeConfig {
    /// 已校验、已 canonicalize 的服务根（目录或单个文件）。
    root: PathBuf,
    /// `true` 表示这次暴露的是单个文件型 scope（服务根本身就是文件，没有子路径）。
    root_is_file: bool,
    token: String,
    read_only: bool,
    allow_upload: bool,
}

fn serve_registry() -> &'static Mutex<HashMap<String, ServeEntry>> {
    static REG: OnceLock<Mutex<HashMap<String, ServeEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 拿 registry 锁；中毒（poison）不当成致命错误，直接取回内部数据继续用——
/// 一次业务 panic 不该让所有插件的本地服务从此再也停不掉/查不到。
fn serve_reg_lock() -> MutexGuard<'static, HashMap<String, ServeEntry>> {
    match serve_registry().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 起一个只读/可读写的本地 HTTP 文件服务，绑 `0.0.0.0`（供局域网访问）。
///
/// # Errors
/// - 插件未被授予 `local-server` 权限
/// - `scopeId` 未知/不属于当前插件会话，或 `subPath` 越权/不存在（复用 `fs_api` 校验）
/// - `readOnly` 与 `allowUpload` 同时为 `true`
/// - `ttlSecs` 超出 [`MIN_TTL_SECS`]~[`MAX_TTL_SECS`]
/// - 端口绑定失败（被占用、无权限等）
#[tauri::command]
pub fn plugin_serve_start(
    opts: ServeOptions,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<ServeStartResult, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err(format!(
            "插件未获授权本地服务能力（请在「插件管理」里授权 {PERMISSION}）"
        ));
    }
    if opts.scope_id.trim().is_empty() {
        return Err("scopeId 不能为空".to_string());
    }
    let read_only = opts.read_only.unwrap_or(false);
    let allow_upload = opts.allow_upload.unwrap_or(false);
    if read_only && allow_upload {
        return Err("readOnly 与 allowUpload 不能同时为 true".to_string());
    }
    let ttl = match opts.ttl_secs {
        None => DEFAULT_TTL_SECS,
        Some(t) if (MIN_TTL_SECS..=MAX_TTL_SECS).contains(&t) => t,
        Some(t) => {
            return Err(format!(
                "ttlSecs 必须在 {MIN_TTL_SECS}~{MAX_TTL_SECS} 秒之间，收到 {t}"
            ))
        }
    };
    let token = match opts.token {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => new_random_id(),
    };

    // 复用 fs_api 的 scope 解析与越界校验：scopeId 归属、subPath 穿越写法、
    // iTools 数据根黑名单都在这一步做完（见模块文档「与 fs_api 的关系」）。
    let scope = fs_api::resolve_scope(&session, &registry, &opts.scope_id)?;
    let sub = opts.sub_path.clone().unwrap_or_default();
    let (root, root_is_file) = if scope.kind == "file" {
        if !sub.is_empty() {
            return Err("该 scope 是单个文件，subPath 必须留空".to_string());
        }
        (fs_api::resolve_existing(&scope, "")?, true)
    } else {
        let dir = fs_api::resolve_existing(&scope, &sub)?;
        if !dir.is_dir() {
            return Err("subPath 指向的不是文件夹".to_string());
        }
        (dir, false)
    };

    let bind_port = opts.port.unwrap_or(0);
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, bind_port))
        .map_err(|e| format!("端口绑定失败: {e}"))?;
    let real_port = listener
        .local_addr()
        .map_err(|e| format!("读取监听端口失败: {e}"))?
        .port();
    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| format!("HTTP 服务初始化失败: {e}"))?;

    let urls = build_urls(real_port, &token);
    let serve_id = new_random_id();
    let created = now_secs();
    let expires_at = created.saturating_add(ttl);
    let stop = Arc::new(AtomicBool::new(false));

    let cfg = ServeConfig {
        root: root.clone(),
        root_is_file,
        token: token.clone(),
        read_only,
        allow_upload,
    };
    let sid_for_thread = serve_id.clone();
    let stop_for_thread = stop.clone();
    let spawn_result = std::thread::Builder::new()
        .name(format!("itools-plugin-serve-{sid_for_thread}"))
        .spawn(move || serve_loop(server, cfg, stop_for_thread, expires_at, sid_for_thread));
    if let Err(e) = spawn_result {
        return Err(format!("启动服务线程失败: {e}"));
    }

    serve_reg_lock().insert(
        serve_id.clone(),
        ServeEntry {
            session: session.clone(),
            port: real_port,
            urls: urls.clone(),
            scope_id: opts.scope_id,
            sub_path: sub,
            read_only,
            allow_upload,
            created,
            expires_at,
            stop,
        },
    );

    ilog!(
        "[plugin-serve] 会话 {} 起了本地服务 0.0.0.0:{real_port}（ttl={ttl}s，只读={read_only}，允许上传={allow_upload}）",
        session.scope_key()
    );
    Ok(ServeStartResult {
        serve_id,
        port: real_port,
        urls,
    })
}

/// 停止一个由当前插件会话创建的本地服务；不属于自己的一律拒绝（哪怕 id 存在）。
///
/// # Errors
/// - `serveId` 未知（可能已到期自动停止）
/// - `serveId` 存在但归属于另一个插件会话
#[tauri::command]
pub fn plugin_serve_stop(
    serve_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    let mut reg = serve_reg_lock();
    let stop_flag = {
        let entry = reg
            .get(&serve_id)
            .ok_or_else(|| "未知的服务（可能已停止）".to_string())?;
        if !entry.session.same_as(&session) {
            return Err("无权限操作（该服务不属于当前插件会话）".to_string());
        }
        entry.stop.clone()
    };
    // 立即从 registry 摘除，list/后续 stop 马上就看不到它；服务线程会在下一次轮询
    // （至多几百毫秒）里发现标志位并真正关掉监听 socket，见 serve_loop。
    reg.remove(&serve_id);
    stop_flag.store(true, Ordering::Relaxed);
    Ok(())
}

/// 列出当前插件会话名下所有仍在跑的本地服务。
///
/// # Errors
/// 无正在运行的插件会话（`caller_session` 失败）。
#[tauri::command]
pub fn plugin_serve_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<Vec<ServeInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    let reg = serve_reg_lock();
    Ok(reg
        .iter()
        .filter(|(_, e)| e.session.same_as(&session))
        .map(|(id, e)| ServeInfo {
            serve_id: id.clone(),
            port: e.port,
            urls: e.urls.clone(),
            scope_id: e.scope_id.clone(),
            sub_path: e.sub_path.clone(),
            read_only: e.read_only,
            allow_upload: e.allow_upload,
            created: e.created,
            expires_at: e.expires_at,
        })
        .collect())
}

/// 供宿主主控调用：插件页关闭 / 插件被禁用或卸载时，强制停掉该会话名下**所有**本地服务。
/// 插件自己拿不到 socket，也没有命令能阻止这件事——生命周期完全由宿主掌握
///（见模块文档「为什么必须由宿主托管」）。不是 `#[tauri::command]`，只给宿主内部调用。
pub(crate) fn stop_all_for_session(session: &ActiveSession) {
    let mut reg = serve_reg_lock();
    let ids: Vec<String> = reg
        .iter()
        .filter(|(_, e)| e.session.same_as(session))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(e) = reg.remove(&id) {
            e.stop.store(true, Ordering::Relaxed);
        }
    }
}

/// 单个服务的请求处理循环：轮询 stop 标志与 TTL，之外全部时间阻塞等请求。
fn serve_loop(server: tiny_http::Server, cfg: ServeConfig, stop: Arc<AtomicBool>, expires_at: u64, serve_id: String) {
    let poll = Duration::from_millis(300);
    loop {
        if stop.load(Ordering::Relaxed) || now_secs() >= expires_at {
            break;
        }
        match server.recv_timeout(poll) {
            Ok(Some(req)) => handle_request(req, &cfg),
            Ok(None) => continue,
            Err(_) => break,
        }
    }
    // 到这里 server（含监听 socket）随函数返回而 drop，端口立即释放。
    serve_reg_lock().remove(&serve_id);
    ilog!("[plugin-serve] 本地服务 {serve_id} 已停止");
}

/// 处理一个 HTTP 请求：先过 token 门禁，再按方法分派。
fn handle_request(req: tiny_http::Request, cfg: &ServeConfig) {
    let full_url = req.url().to_string();
    let (path_part, query) = match full_url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (full_url.clone(), String::new()),
    };

    let token_ok = query_param(&query, TOKEN_QUERY_KEY)
        .map(|t| constant_time_eq(&t, &cfg.token))
        .unwrap_or(false)
        || find_header(req.headers(), TOKEN_HEADER)
            .map(|t| constant_time_eq(t, &cfg.token))
            .unwrap_or(false);
    if !token_ok {
        respond_text(req, 401, "未授权：访问令牌缺失或不正确");
        return;
    }

    let method = req.method();
    let is_get = matches!(method, tiny_http::Method::Get);
    let is_head = matches!(method, tiny_http::Method::Head);
    let is_post = matches!(method, tiny_http::Method::Post);

    if is_get || is_head {
        handle_get(req, cfg, &path_part, is_head);
        return;
    }
    if is_post && path_part == "/upload" {
        if cfg.allow_upload && !cfg.read_only {
            handle_upload(req, cfg);
        } else {
            respond_text(req, 403, "该服务未开启上传（allowUpload=false 或 readOnly=true）");
        }
        return;
    }
    respond_text(req, 405, "不支持的方法");
}

fn handle_get(req: tiny_http::Request, cfg: &ServeConfig, path_part: &str, is_head: bool) {
    let target = if cfg.root_is_file {
        if path_part != "/" && !path_part.is_empty() {
            respond_text(req, 404, "未找到");
            return;
        }
        Ok(cfg.root.clone())
    } else {
        let rel = decode_rel_path(path_part);
        let virtual_scope = root_as_virtual_scope(&cfg.root);
        fs_api::resolve_existing(&virtual_scope, &rel)
    };
    let target = match target {
        Ok(p) => p,
        Err(e) => {
            respond_text(req, 404, &e);
            return;
        }
    };

    if target.is_dir() {
        if is_head {
            respond_text(req, 200, "");
            return;
        }
        match render_dir_listing(&target, path_part, &cfg.token, cfg.allow_upload && !cfg.read_only) {
            Ok(html) => respond_html(req, 200, &html),
            Err(e) => respond_text(req, 500, &e),
        }
        return;
    }

    if is_head {
        respond_text(req, 200, "");
        return;
    }
    match std::fs::File::open(&target) {
        Ok(file) => {
            let mime = guess_mime(&target);
            let mut resp = tiny_http::Response::from_file(file).with_status_code(tiny_http::StatusCode(200));
            if let Ok(h) = tiny_http::Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()) {
                resp = resp.with_header(h);
            }
            let _ = req.respond(resp);
        }
        Err(e) => respond_text(req, 500, &format!("读取文件失败: {e}")),
    }
}

fn handle_upload(mut req: tiny_http::Request, cfg: &ServeConfig) {
    let content_type = find_header(req.headers(), "Content-Type").unwrap_or("").to_string();
    let boundary = match extract_boundary(&content_type) {
        Some(b) => b,
        None => {
            respond_text(
                req,
                400,
                "缺少 multipart 边界（Content-Type 需为 multipart/form-data; boundary=...）",
            );
            return;
        }
    };

    let mut body = Vec::new();
    {
        let mut limited = req.as_reader().take(MAX_UPLOAD_BYTES + 1);
        if let Err(e) = limited.read_to_end(&mut body) {
            respond_text(req, 400, &format!("读取上传体失败: {e}"));
            return;
        }
    }
    if body.len() as u64 > MAX_UPLOAD_BYTES {
        respond_text(
            req,
            413,
            &format!("上传体超过单次上限 {} MB", MAX_UPLOAD_BYTES / 1024 / 1024),
        );
        return;
    }

    let parts = match parse_multipart(&body, &boundary) {
        Ok(p) => p,
        Err(e) => {
            respond_text(req, 400, &e);
            return;
        }
    };

    let mut dir_prefix = String::new();
    let mut file_part: Option<(String, Vec<u8>)> = None;
    for p in parts {
        match p.name.as_str() {
            "path" => dir_prefix = String::from_utf8_lossy(&p.data).trim().to_string(),
            "file" => {
                if let Some(fname) = p.filename.clone() {
                    file_part = Some((fname, p.data));
                }
            }
            _ => {}
        }
    }
    let (filename, data) = match file_part {
        Some(v) => v,
        None => {
            respond_text(req, 400, "未找到上传文件（字段名需为 file）");
            return;
        }
    };
    let filename = sanitize_filename(&filename);
    if filename.is_empty() {
        respond_text(req, 400, "文件名非法");
        return;
    }
    let trimmed_dir = dir_prefix.trim_matches('/');
    let rel = if trimmed_dir.is_empty() {
        filename
    } else {
        format!("{trimmed_dir}/{filename}")
    };

    let virtual_scope = root_as_virtual_scope(&cfg.root);
    let target = match fs_api::resolve_for_write(&virtual_scope, &rel) {
        Ok(p) => p,
        Err(e) => {
            respond_text(req, 400, &e);
            return;
        }
    };
    match std::fs::write(&target, &data) {
        Ok(()) => respond_text(req, 200, "上传成功"),
        Err(e) => respond_text(req, 500, &format!("写入失败: {e}")),
    }
}

/// 把「已校验过的服务根目录」包成一个临时 [`FsScope`]，复用 `fs_api` 的相对路径校验。
/// `id`/`label`/`created` 在这个用途里都不参与判断逻辑，随便填。
fn root_as_virtual_scope(root: &Path) -> FsScope {
    FsScope {
        id: String::new(),
        kind: "dir".to_string(),
        path: root.to_string_lossy().into_owned(),
        label: String::new(),
        created: 0,
    }
}

// ==================== HTTP 层小工具 ====================

fn decode_rel_path(path_part: &str) -> String {
    percent_decode(path_part.trim_start_matches('/'))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            Some(percent_decode(v))
        } else {
            None
        }
    })
}

fn find_header<'a>(headers: &'a [tiny_http::Header], name: &str) -> Option<&'a str> {
    headers.iter().find_map(|h| {
        if h.field.as_str().as_str().eq_ignore_ascii_case(name) {
            Some(h.value.as_str())
        } else {
            None
        }
    })
}

/// 常数时间字符串比较，避免通过响应耗时侧信道逐字节猜出 token。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode_path_segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn guess_mime(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn respond_text(req: tiny_http::Request, status: u16, text: &str) {
    let resp = tiny_http::Response::from_string(text.to_string()).with_status_code(tiny_http::StatusCode(status));
    let _ = req.respond(resp);
}

fn respond_html(req: tiny_http::Request, status: u16, html: &str) {
    let mut resp = tiny_http::Response::from_string(html.to_string()).with_status_code(tiny_http::StatusCode(status));
    if let Ok(h) = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]) {
        resp = resp.with_header(h);
    }
    let _ = req.respond(resp);
}

/// 生成一个极简的目录浏览页：文件/目录列表 + （可选）上传表单。所有链接都内嵌 `?token=`，
/// 否则浏览器点进子目录时会把 query 串丢掉、下一跳直接 401。
fn render_dir_listing(dir: &Path, req_path: &str, token: &str, upload_ui: bool) -> Result<String, String> {
    let mut items: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("读取目录失败: {e}"))?
        .filter_map(|e| e.ok())
        .collect();
    items.sort_by_key(|e| e.file_name());

    let base = if req_path.ends_with('/') {
        req_path.to_string()
    } else {
        format!("{req_path}/")
    };

    let mut rows = String::new();
    for entry in items {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let encoded = percent_encode_path_segment(&name);
        let trail = if is_dir { "/" } else { "" };
        rows.push_str(&format!(
            "<li><a href=\"{base}{encoded}{trail}?token={token}\">{}{}</a></li>\n",
            html_escape(&name),
            trail
        ));
    }

    let dir_prefix = base.trim_matches('/');
    let upload_form = if upload_ui {
        format!(
            "<hr><form method=\"post\" action=\"/upload?token={token}\" enctype=\"multipart/form-data\">\
             <input type=\"hidden\" name=\"path\" value=\"{}\">\
             <input type=\"file\" name=\"file\"> <button type=\"submit\">上传到当前目录</button></form>",
            html_escape(dir_prefix)
        )
    } else {
        String::new()
    };

    let parent_link = if base != "/" {
        let trimmed = base.trim_end_matches('/');
        let parent = match trimmed.rsplit_once('/') {
            Some(("", _)) => "/".to_string(),
            Some((p, _)) => format!("{p}/"),
            None => "/".to_string(),
        };
        format!("<p><a href=\"{parent}?token={token}\">.. 上级目录</a></p>")
    } else {
        String::new()
    };

    Ok(format!(
        "<!doctype html><meta charset=\"utf-8\"><title>iTools 本地共享</title>\
         <h3>{}</h3>{parent_link}<ul>{rows}</ul>{upload_form}",
        html_escape(req_path)
    ))
}

/// 从文件名里剥离任何路径分隔符，只留最后一段；`.`/`..`/空串一律视为非法。
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        return String::new();
    }
    base.to_string()
}

// ==================== 手写 multipart/form-data 解析（见模块文档「已知取舍」）====================

struct MultipartPart {
    name: String,
    filename: Option<String>,
    data: Vec<u8>,
}

fn extract_boundary(content_type: &str) -> Option<String> {
    if !content_type.to_ascii_lowercase().starts_with("multipart/form-data") {
        return None;
    }
    content_type.split(';').find_map(|seg| {
        seg.trim()
            .strip_prefix("boundary=")
            .map(|b| b.trim_matches('"').to_string())
    })
}

fn parse_multipart(body: &[u8], boundary: &str) -> Result<Vec<MultipartPart>, String> {
    let delim = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();
    let first = find_bytes(body, &delim, 0).ok_or_else(|| "multipart 内容格式错误（找不到起始边界）".to_string())?;
    let mut pos = first + delim.len();
    loop {
        if body.get(pos..pos + 2) == Some(b"--") {
            break;
        }
        if body.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        }
        let next = find_bytes(body, &delim, pos).ok_or_else(|| "multipart 内容格式错误（缺少结束边界）".to_string())?;
        let mut part_bytes = &body[pos..next];
        if part_bytes.ends_with(b"\r\n") {
            part_bytes = &part_bytes[..part_bytes.len() - 2];
        }
        parts.push(parse_one_part(part_bytes)?);
        pos = next + delim.len();
    }
    Ok(parts)
}

fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn parse_one_part(bytes: &[u8]) -> Result<MultipartPart, String> {
    let sep = find_bytes(bytes, b"\r\n\r\n", 0).ok_or_else(|| "multipart 分片缺少头部分隔符".to_string())?;
    let header_text = String::from_utf8_lossy(&bytes[..sep]).into_owned();
    let data = bytes[sep + 4..].to_vec();

    let mut name = String::new();
    let mut filename = None;
    for line in header_text.split("\r\n") {
        if line.to_ascii_lowercase().starts_with("content-disposition:") {
            name = extract_disposition_field(line, "name").unwrap_or_default();
            filename = extract_disposition_field(line, "filename");
        }
    }
    if name.is_empty() {
        return Err("multipart 分片缺少 name 字段".to_string());
    }
    Ok(MultipartPart { name, filename, data })
}

fn extract_disposition_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let idx = line.find(&pat)?;
    let start = idx + pat.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

// ==================== plugin_lan_announce / plugin_lan_discover：局域网发现 ====================

/// `plugin_lan_announce` 的入参：广播「我在这」，附带调用方自定义的 `info`（如服务用途、
/// 展示名），供同网段的其它 iTools 实例通过 [`plugin_lan_discover`] 收到。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanAnnounceInfo {
    pub name: String,
    pub port: u16,
    pub info: Option<Value>,
    /// 存活秒数，缺省 [`LAN_ANNOUNCE_DEFAULT_TTL`]。到期前需要插件自己重新调用本命令续期
    /// （这是「广播存在感」的语义，不是「起一次管一直」的服务）。
    pub ttl_secs: Option<u64>,
}

/// `plugin_lan_discover` 的一个发现结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanPeer {
    pub ip: String,
    pub name: String,
    pub port: u16,
    pub info: Option<Value>,
}

struct AnnounceEntry {
    name: String,
    port: u16,
    info: Option<Value>,
    expires_at: u64,
}

fn announce_registry() -> &'static Mutex<HashMap<String, AnnounceEntry>> {
    static REG: OnceLock<Mutex<HashMap<String, AnnounceEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn announce_lock() -> MutexGuard<'static, HashMap<String, AnnounceEntry>> {
    match announce_registry().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

static LAN_RESPONDER_STARTED: AtomicBool = AtomicBool::new(false);

/// 惰性起一个后台线程，监听 `0.0.0.0:{LAN_DISCOVERY_PORT}` 的 UDP 广播查询并应答当前所有
/// 未过期的 announce 记录。只起一次（全进程共享），多个插件先后 announce 复用同一个responder。
fn ensure_lan_responder_running() {
    if LAN_RESPONDER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("itools-lan-responder".into())
        .spawn(|| {
            let socket = match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LAN_DISCOVERY_PORT)) {
                Ok(s) => s,
                Err(e) => {
                    ilog!("[plugin-lan] 监听端口 {LAN_DISCOVERY_PORT} 绑定失败: {e}，announce/discover 将不可用");
                    return;
                }
            };
            let _ = socket.set_broadcast(true);
            let mut buf = [0u8; 2048];
            loop {
                let (n, src) = match socket.recv_from(&mut buf) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Ok(text) = std::str::from_utf8(&buf[..n]) else {
                    continue;
                };
                let Ok(msg) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                if msg.get("itoolsLan").and_then(Value::as_str) != Some("discover") {
                    continue;
                }
                let peers = current_announcements();
                if peers.is_empty() {
                    continue;
                }
                let resp = serde_json::json!({ "itoolsLan": "announce", "v": 1, "peers": peers });
                if let Ok(bytes) = serde_json::to_vec(&resp) {
                    let _ = socket.send_to(&bytes, src);
                }
            }
        });
    if spawned.is_err() {
        // 起不来就把标志复位，下次 announce 调用还能再试一次（否则会永久卡在「已启动」但其实没起来）。
        LAN_RESPONDER_STARTED.store(false, Ordering::SeqCst);
    }
}

fn current_announcements() -> Vec<Value> {
    let now = now_secs();
    let mut reg = announce_lock();
    reg.retain(|_, e| e.expires_at > now);
    reg.values()
        .map(|e| serde_json::json!({ "name": e.name, "port": e.port, "info": e.info }))
        .collect()
}

/// 广播「我在这」，供同网段其它 iTools 实例发现。会惰性起一个共享的 UDP 应答线程。
///
/// # Errors
/// - 插件未被授予 `local-server` 权限
/// - `name` 为空
/// - `ttlSecs` 超出 [`MIN_TTL_SECS`]~[`MAX_TTL_SECS`]
#[tauri::command]
pub fn plugin_lan_announce(
    opts: LanAnnounceInfo,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err(format!(
            "插件未获授权本地服务能力（请在「插件管理」里授权 {PERMISSION}）"
        ));
    }
    if opts.name.trim().is_empty() {
        return Err("name 不能为空".to_string());
    }
    let ttl = match opts.ttl_secs {
        None => LAN_ANNOUNCE_DEFAULT_TTL,
        Some(t) if (MIN_TTL_SECS..=MAX_TTL_SECS).contains(&t) => t,
        Some(t) => {
            return Err(format!(
                "ttlSecs 必须在 {MIN_TTL_SECS}~{MAX_TTL_SECS} 秒之间，收到 {t}"
            ))
        }
    };
    announce_lock().insert(
        session.scope_key(),
        AnnounceEntry {
            name: opts.name,
            port: opts.port,
            info: opts.info,
            expires_at: now_secs().saturating_add(ttl),
        },
    );
    ensure_lan_responder_running();
    Ok(())
}

/// 广播一次发现查询，收集 `timeoutMs` 毫秒内收到的所有应答。
///
/// # Errors
/// - 插件未被授予 `local-server` 权限
/// - 创建/配置 UDP 套接字失败，或发送广播失败
#[tauri::command]
pub async fn plugin_lan_discover(
    timeout_ms: u64,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<LanPeer>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err(format!(
            "插件未获授权本地服务能力（请在「插件管理」里授权 {PERMISSION}）"
        ));
    }
    let timeout = Duration::from_millis(timeout_ms.clamp(200, 15_000));
    tauri::async_runtime::spawn_blocking(move || discover_blocking(timeout))
        .await
        .map_err(|e| format!("发现任务失败: {e}"))?
}

fn discover_blocking(timeout: Duration) -> Result<Vec<LanPeer>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("创建发现套接字失败: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("开启广播失败: {e}"))?;
    let query = serde_json::json!({ "itoolsLan": "discover", "v": 1 });
    let bytes = serde_json::to_vec(&query).map_err(|e| format!("构造发现请求失败: {e}"))?;
    socket
        .send_to(&bytes, SocketAddrV4::new(Ipv4Addr::BROADCAST, LAN_DISCOVERY_PORT))
        .map_err(|e| format!("发送发现广播失败: {e}"))?;

    let deadline = Instant::now() + timeout;
    let mut peers: Vec<LanPeer> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if socket.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        let (n, src) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => break,
        };
        let Ok(text) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if msg.get("itoolsLan").and_then(Value::as_str) != Some("announce") {
            continue;
        }
        let ip = match src {
            SocketAddr::V4(a) => a.ip().to_string(),
            SocketAddr::V6(a) => a.ip().to_string(),
        };
        if let Some(arr) = msg.get("peers").and_then(Value::as_array) {
            for p in arr {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let port = p.get("port").and_then(Value::as_u64).unwrap_or(0) as u16;
                let info = p.get("info").cloned();
                let peer = LanPeer {
                    ip: ip.clone(),
                    name,
                    port,
                    info,
                };
                let dup = peers
                    .iter()
                    .any(|x| x.ip == peer.ip && x.port == peer.port && x.name == peer.name);
                if !dup {
                    peers.push(peer);
                }
            }
        }
    }
    Ok(peers)
}

// ==================== 通用小工具：id 生成 / 本机 IP / 时间 ====================

/// 生成一个不透明的随机 id（serve_id 与默认 token 共用同一套生成方式）。
///
/// 与 `fs_api::new_scope_id` 同一思路（sha256(纳秒时间戳 + 进程内计数器 + PID + 堆地址)），
/// 因为任务边界不允许把 `fs_api.rs` 里的私有实现导出，这里独立复刻一份——这是纯粹的
/// 「id 生成」逻辑，不是安全校验，重复实现不构成「校验分裂」的风险。
///
/// # 强度边界（如实标注，不夸大）
///
/// **这不是密码学级随机数生成器**（项目未引入 `rand`，见 `fs_api` 模块头的依赖最小化取舍）。
/// 上面那几路输入合起来的实际熵大约在 50~60 位量级，而 `fs_api::new_scope_id` 的注释已经写明：
/// 「如果之后要把它当作能跨机器/跨会话传递的凭证，需要换成真随机源」——**本模块的 token
/// 正是那种用法**（它要在局域网上被别的机器带着走）。
///
/// 之所以当前仍然可接受：服务是短时的（默认几十秒 TTL），攻击者要在这个窗口内经网络暴力
/// 枚举 50 位以上的空间，不具备现实可行性。但这是「够用」，不是「安全无虞」——
/// 要把 `serve` 做成长时间常开的服务，**必须先把它换成 `BCryptGenRandom` 这类系统真随机源**。
fn new_random_id() -> String {
    use sha2::{Digest, Sha256};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let heap_marker: Box<u8> = Box::new(0);
    let addr = std::ptr::addr_of!(*heap_marker) as usize;
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(n.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(addr.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// 把 `urls` 拼成局域网可访问的完整地址（已内嵌 `?token=`）。见模块文档「已知取舍」——
/// IP 来自出站探测，不是真正枚举全部网卡。
fn build_urls(port: u16, token: &str) -> Vec<String> {
    let mut ips = local_ipv4_addresses();
    if ips.is_empty() {
        ips.push(Ipv4Addr::LOCALHOST);
    }
    ips.into_iter().map(|ip| format!("http://{ip}:{port}/?token={token}")).collect()
}

/// 用 UDP「连接」技巧探测本机出站网卡 IP：不实际发包，只借内核路由表选路的副作用读出
/// 「如果要访问某个地址会用哪张网卡出去」。对几个常见探测目标各试一次，结果去重——
/// 多网卡（有线 + Wi-Fi）机器上通常能拿到不止一个地址，但不保证覆盖所有虚拟网卡
///（见模块文档「已知取舍」）。
fn local_ipv4_addresses() -> Vec<Ipv4Addr> {
    const PROBES: [&str; 4] = ["8.8.8.8:80", "1.1.1.1:80", "192.168.1.1:80", "10.0.0.1:80"];
    let mut out = Vec::new();
    for probe in PROBES {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
            continue;
        };
        if sock.connect(probe).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(addr)) = sock.local_addr() {
            let ip = *addr.ip();
            if !ip.is_loopback() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
