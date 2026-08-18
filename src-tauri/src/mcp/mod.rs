//! **插件开发 MCP 服务器**：让 AI 编程助手（Claude Code / Cursor / …）直接驱动开发者中心。
//!
//! # 它解决什么
//!
//! 在此之前，用 AI 写 iTools 插件是「人肉转述」：开发者要自己把插件开发规范贴给 AI、
//! 自己去开发者中心点运行、自己把报错复制回来、自己点提交审核。AI 看不到调试日志，
//! 也不知道清单校验报了什么，只能靠猜。
//!
//! 接上 MCP 之后这些都是 AI 能直接调的工具：读规范 → 看清单问题 → 跑触发模拟器 →
//! 读调试日志 → 发布前自检 → 提交审核 → 查审核结论。一台纯净电脑上装好 iTools、
//! 在 AI 客户端里填一行配置就能开始写插件。
//!
//! # 传输与网络形态
//!
//! MCP 的 **Streamable HTTP** 传输：`POST /mcp` 收 JSON-RPC 2.0 请求、返回 JSON 响应。
//! 不实现服务端主动推送（SSE）——本服务的工具全是「请求-响应」式的，没有需要推流的场景，
//! 少一条链路就少一处会坏的地方。
//!
//! **只绑 `127.0.0.1`**：MCP 客户端与 iTools 在同一台机器上，没有任何理由对外监听。
//! 按产品决定**不做鉴权**（本地开发场景，与多数 MCP server 的形态一致）——
//! 代价是本机任意进程都能调这些工具，其中包括「提交审核」这类会改变线上状态的操作。
//! 这一点在文档里如实写明，不靠「大概没人会」蒙混。
//!
//! **端口会自动避让**：默认 7345，被占用（最常见的情形是 debug 版与 release 版同时开着，
//! 这是允许的）就顺序往上找一个空的，见 [`PORT_SCAN`]。无论落在哪个端口，
//! `mcp_status` / 开发者中心面板显示的都是**实际**绑到的那一个。
//! 用户用 `ITOOLS_MCP_PORT` 显式指定端口时**不**避让——绑不上就如实报错。
//!
//! # 诚信约束（doc/开发准则.md）
//!
//! - 每个工具都真实调用开发者中心既有的实现体，**没有一个是桩**；
//! - 工具的返回原样带上后端的错误文本（清单问题、自检阻断项、模型驳回原文），
//!   不做「成功了但其实什么都没干」的返回；
//! - 开发者中心面板上显示的连接地址与运行状态，来自服务器**实际的监听结果**，
//!   起不来就如实报错，不显示一个假的「运行中」。

pub mod tools;

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::logging::ilog;

/// 默认监听端口。选一个不常见的高位端口，降低与其它开发服务撞车的概率。
/// 可用环境变量 `ITOOLS_MCP_PORT` 覆盖（0 = 让系统分配空闲端口）。
const DEFAULT_PORT: u16 = 7345;
const PORT_ENV: &str = "ITOOLS_MCP_PORT";

/// 用默认端口时，被占用就从 [`DEFAULT_PORT`] 起**顺序往上**再试这么多个（7345..=7354）。
///
/// # 为什么要有这个回退
///
/// 单实例机制只保证「同一构建不重复启动」，debug 版与 release 版是**允许并存**的
/// （开发时的常态）。两边都想绑 7345，后起的那个必然拿到 `os error 10048`，
/// 它的「AI 助手接入 / 开发者中心 MCP」整条链路就废了——而这跟 iTools 本身没关系，
/// 纯粹是端口撞车，没道理让用户去手动设环境变量。
///
/// # 为什么是顺序小范围，而不是直接绑 0 让系统分配
///
/// 端口 0 拿到的是**每次启动都不同**的临时端口。而 `ai_clients.rs` 会把这个地址写进
/// AI 客户端（Claude Code / Codex / Cursor）的配置文件里——地址天天变，等于每次开机
/// 都得去「重新安装」一遍。顺序试则是可预测的：常态下只有一个实例，永远落在 7345；
/// 并存时后起的落在 7346，换个启动顺序也就在这两个数之间摆动。
const PORT_SCAN: u16 = 10;
/// 关掉 MCP 服务器的环境变量（`ITOOLS_MCP=off`）。默认开启。
const ENABLE_ENV: &str = "ITOOLS_MCP";

/// 请求体上限：MCP 请求都是小 JSON，1MB 足够；防止一次畸形请求吃光内存。
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 本进程实际监听的端口（0 = 没在跑）。供 UI 显示真实连接地址。
static ACTIVE_PORT: AtomicU16 = AtomicU16::new(0);

/// MCP 服务器的运行状态快照（发给前端，camelCase）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatus {
    /// 是否正在监听。**来自实际的监听结果**，不是配置项的回显。
    pub running: bool,
    /// 实际监听的端口（未运行为 0）。
    pub port: u16,
    /// 给 AI 客户端填的完整地址（未运行为空串）。
    pub url: String,
    /// 未能启动时的真实原因（正常运行为 None）。
    pub error: Option<String>,
}

/// 启动失败的原因（只在启动那一刻写一次，供状态查询回显）。
static START_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 当前状态（`mcp_status` 命令与开发者中心面板用）。
pub fn status() -> McpStatus {
    let port = ACTIVE_PORT.load(Ordering::Relaxed);
    McpStatus {
        running: port != 0,
        port,
        url: if port == 0 {
            String::new()
        } else {
            format!("http://127.0.0.1:{port}/mcp")
        },
        error: START_ERROR.lock().ok().and_then(|g| g.clone()),
    }
}

fn set_start_error(msg: Option<String>) {
    if let Ok(mut g) = START_ERROR.lock() {
        *g = msg;
    }
}

/// 端口从哪来——决定「绑不上时能不能换一个」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortChoice {
    /// 用户用 `ITOOLS_MCP_PORT` **显式指定**。他指定了就按他说的来：绑不上如实报错，
    /// **绝不擅自换一个**——他多半是照着这个端口配好了防火墙/客户端，
    /// 悄悄换到别处比起不来更难查。
    Fixed(u16),
    /// 没指定，用默认端口；被占用时按 [`PORT_SCAN`] 顺序往上找一个空的。
    Default,
}

/// 解析 `ITOOLS_MCP_PORT` 的**原始值**（提成参数是为了能单测，不必去动进程环境变量）。
///
/// 没设、空串、解析不出端口 → [`PortChoice::Default`]。注意 `Fixed(0)` 是有意义的：
/// 用户显式要求「由系统分配」，此时同样不做回退（本来也不会失败）。
fn parse_port_env(raw: Option<&str>) -> PortChoice {
    match raw.map(str::trim) {
        Some(s) if !s.is_empty() => match s.parse::<u16>() {
            Ok(p) => PortChoice::Fixed(p),
            Err(e) => {
                // 不静默吞掉：用户以为自己指定了端口，实际用的是默认端口，得让他在日志里看得见
                ilog!("[iTools] {PORT_ENV}=「{s}」不是合法端口（{e}），按默认端口处理");
                PortChoice::Default
            }
        },
        _ => PortChoice::Default,
    }
}

/// 按 [`PortChoice`] 绑定监听端口，失败时返回**给用户看的**原因文案。
///
/// 只绑 `127.0.0.1`：MCP 客户端与 iTools 在同一台机器上，没有任何理由对外监听。
fn bind(choice: PortChoice) -> Result<TcpListener, String> {
    let candidates: Vec<u16> = match choice {
        PortChoice::Fixed(p) => vec![p],
        // checked_add 只是防呆：DEFAULT_PORT 若被改到接近 65535，溢出会在 debug 下直接 panic
        PortChoice::Default => (0..PORT_SCAN)
            .filter_map(|i| DEFAULT_PORT.checked_add(i))
            .collect(),
    };

    let mut first_err: Option<std::io::Error> = None;
    for &p in &candidates {
        match TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, p)) {
            Ok(l) => {
                if p != candidates[0] {
                    // 换了端口必须留痕：面板上显示的是真实地址（见 status()），
                    // 但已经写进 AI 客户端配置里的旧地址会失效，
                    // `ai_clients.rs` 会把它标成 mcpStale 并提示「重新安装」。
                    ilog!(
                        "[iTools] MCP 默认端口 {DEFAULT_PORT} 被占用（多半是另一个构建的 iTools 在跑），\
                         已改用 {p}；AI 客户端里配的旧地址需要在「AI 助手接入」里重新安装一次"
                    );
                }
                return Ok(l);
            }
            // 只留第一个错误：后面那些「7346 也被占了」对定位问题没有价值
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    // 到这儿说明全试完了。错误文案里给的是**第一个候选**（用户最关心的那个）的真实原因。
    let e = first_err
        .map(|e| e.to_string())
        .unwrap_or_else(|| "没有可用的候选端口".to_string());
    Err(match choice {
        PortChoice::Fixed(p) => format!(
            "MCP 服务器启动失败（127.0.0.1:{p}）：{e}。\
             该端口是你用环境变量 {PORT_ENV} 指定的，故不会自动改用别的端口——\
             请腾出这个端口，或把 {PORT_ENV} 改成别的（设 0 由系统分配）。"
        ),
        PortChoice::Default => format!(
            "MCP 服务器启动失败：127.0.0.1 上 {DEFAULT_PORT}~{} 这 {PORT_SCAN} 个端口都绑不上\
             （{DEFAULT_PORT} 的原因是：{e}）。可用环境变量 {PORT_ENV} 指定一个空闲端口\
             （设 0 由系统分配）。",
            DEFAULT_PORT.saturating_add(PORT_SCAN - 1)
        ),
    })
}

/// 在后台线程启动 MCP 服务器。**默认开启**，`ITOOLS_MCP=off` 可关掉。
///
/// 端口被占用等启动失败**不影响 iTools 本身**：如实记录原因、面板上照实显示，
/// 但绝不因此让应用起不来——MCP 是给 AI 用的旁路能力，不是主功能。
pub fn start(app: AppHandle) {
    if std::env::var(ENABLE_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
        .unwrap_or(false)
    {
        ilog!("[iTools] MCP 服务器已由 {ENABLE_ENV} 关闭");
        set_start_error(Some(format!("已被环境变量 {ENABLE_ENV} 关闭")));
        return;
    }

    let choice = parse_port_env(std::env::var(PORT_ENV).ok().as_deref());
    let listener = match bind(choice) {
        Ok(l) => l,
        Err(msg) => {
            ilog!("[iTools] {msg}");
            set_start_error(Some(msg));
            return;
        }
    };
    // 端口设 0 时系统会分配一个，且回退路径下绑到的也未必是默认端口：
    // **必须回读**真实端口，否则面板上显示的地址是假的（诚信红线）。
    let real_port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            // 连自己监听在哪都问不出来，就不能对外宣称一个端口——宁可如实说没起来
            let msg = format!("MCP 服务器已监听但读不到实际端口：{e}，无法给出连接地址");
            ilog!("[iTools] {msg}");
            set_start_error(Some(msg));
            return;
        }
    };
    let server = match tiny_http::Server::from_listener(listener, None) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("MCP 服务器初始化失败：{e}");
            ilog!("[iTools] {msg}");
            set_start_error(Some(msg));
            return;
        }
    };

    ACTIVE_PORT.store(real_port, Ordering::Relaxed);
    set_start_error(None);
    ilog!("[iTools] MCP 服务器已启动：http://127.0.0.1:{real_port}/mcp");

    std::thread::Builder::new()
        .name("itools-mcp".into())
        .spawn(move || serve_loop(Arc::new(server), app))
        .ok();
}

fn serve_loop(server: Arc<tiny_http::Server>, app: AppHandle) {
    for mut req in server.incoming_requests() {
        // 单线程顺序处理：MCP 客户端是串行调用的，工具本身也多为轻量操作。
        // 真正耗时的（提交审核要上传几十 MB）会阻塞下一个请求，但那种场景下
        // AI 本来也在等这一个结果，没有并发价值。
        let res = handle(&mut req, &app);
        let body = serde_json::to_vec(&res).unwrap_or_else(|_| b"{}".to_vec());
        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
            .expect("固定的头，构造不会失败");
        let _ = req.respond(tiny_http::Response::from_data(body).with_header(header));
    }
}

/// 处理一个 HTTP 请求，返回要写回的 JSON（JSON-RPC 响应体）。
fn handle(req: &mut tiny_http::Request, app: &AppHandle) -> Value {
    if req.method() != &tiny_http::Method::Post {
        return rpc_error(Value::Null, -32600, "本端点只接受 POST（MCP Streamable HTTP）");
    }
    let mut body = String::new();
    if let Err(e) = req.as_reader().take(MAX_BODY_BYTES as u64).read_to_string(&mut body) {
        return rpc_error(Value::Null, -32700, &format!("读取请求体失败: {e}"));
    }
    let parsed: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, &format!("请求不是合法 JSON: {e}")),
    };

    // 批量请求（数组）：逐条处理。多数客户端不用，但协议允许，收到了不能直接崩。
    if let Some(arr) = parsed.as_array() {
        let out: Vec<Value> = arr.iter().map(|one| dispatch(one, app)).collect();
        return Value::Array(out);
    }
    dispatch(&parsed, app)
}

/// 分发一条 JSON-RPC 请求。
fn dispatch(req: &Value, app: &AppHandle) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => rpc_ok(id, initialize_result()),
        // 通知类（没有 id）不需要响应体，但 HTTP 形态下总要回点什么，回一个空 result 最省事
        "notifications/initialized" | "notifications/cancelled" => rpc_ok(id, json!({})),
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => rpc_ok(id, json!({ "tools": tools::list() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools::call(app, name, &args) {
                Ok(text) => rpc_ok(id, tool_text(&text, false)),
                // 工具失败走 `isError`，不走 JSON-RPC 错误：协议规定工具的业务失败要让模型看到内容，
                // 用 JSON-RPC error 会被客户端当成传输层故障、模型读不到原因。
                Err(text) => rpc_ok(id, tool_text(&text, true)),
            }
        }
        "resources/list" => rpc_ok(id, json!({ "resources": tools::resources() })),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            match tools::read_resource(uri) {
                Ok(text) => rpc_ok(
                    id,
                    json!({ "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": text }] }),
                ),
                Err(e) => rpc_error(id, -32602, &e),
            }
        }
        "prompts/list" => rpc_ok(id, json!({ "prompts": [] })),
        "" => rpc_error(id, -32600, "缺少 method"),
        other => rpc_error(id, -32601, &format!("不支持的方法: {other}")),
    }
}

fn initialize_result() -> Value {
    json!({
        // 与主流客户端对齐的协议版本；客户端会按自己支持的版本协商
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {}, "resources": {} },
        "serverInfo": { "name": "itools-plugin-dev", "version": env!("CARGO_PKG_VERSION") },
        "instructions": tools::SERVER_INSTRUCTIONS,
    })
}

/// 工具调用的返回体。`is_error=true` 时模型仍能读到文本内容（这正是要的）。
fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

// ==================== 命令 ====================

/// 命令：MCP 服务器的运行状态（开发者中心面板用）。
#[tauri::command]
pub fn mcp_status() -> McpStatus {
    status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_post_is_rejected_with_rpc_error() {
        let v = rpc_error(Value::Null, -32600, "只接受 POST");
        assert_eq!(v["error"]["code"], json!(-32600));
        assert_eq!(v["jsonrpc"], json!("2.0"));
    }

    #[test]
    fn tool_failure_uses_is_error_not_rpc_error() {
        // 这条是协议要点：工具的业务失败必须让模型读到原因，
        // 走 JSON-RPC error 会被客户端当成传输故障、内容丢失。
        let v = tool_text("插件不存在", true);
        assert_eq!(v["isError"], json!(true));
        assert_eq!(v["content"][0]["text"], json!("插件不存在"));
    }

    #[test]
    fn status_is_not_running_before_start() {
        // 没启动时必须如实说没运行，不能显示一个假的地址
        ACTIVE_PORT.store(0, Ordering::Relaxed);
        let s = status();
        assert!(!s.running);
        assert_eq!(s.port, 0);
        assert!(s.url.is_empty(), "未运行时不该给出连接地址");
    }

    #[test]
    fn status_reports_real_port() {
        ACTIVE_PORT.store(7345, Ordering::Relaxed);
        let s = status();
        assert!(s.running);
        assert_eq!(s.url, "http://127.0.0.1:7345/mcp");
        ACTIVE_PORT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn port_env_decides_whether_fallback_is_allowed() {
        // 没设 / 空串 / 垃圾值 → 走默认端口（可回退）
        assert_eq!(parse_port_env(None), PortChoice::Default);
        assert_eq!(parse_port_env(Some("   ")), PortChoice::Default);
        assert_eq!(parse_port_env(Some("abc")), PortChoice::Default);
        assert_eq!(parse_port_env(Some("70000")), PortChoice::Default, "越界值不是端口");
        // 显式指定 → 固定，绑不上也不换（含 0 = 用户要求由系统分配）
        assert_eq!(parse_port_env(Some(" 8080 ")), PortChoice::Fixed(8080));
        assert_eq!(parse_port_env(Some("0")), PortChoice::Fixed(0));
    }

    /// 默认端口被占用时必须**自动换一个**，而不是让 MCP 整条链路废掉。
    /// 这正是 debug 版与 release 版并存时后起的那个所处的处境。
    #[test]
    fn default_port_falls_back_when_occupied() {
        // 自己占住默认端口，模拟「另一个构建已经在跑」。
        // 若本机真有 iTools 在跑，这一步会失败——不影响结论，下面按实际情况断言。
        let occupied = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, DEFAULT_PORT));
        let l = bind(PortChoice::Default).expect("默认端口被占用时必须回退到别的端口");
        let port = l.local_addr().expect("回读实际端口").port();
        println!("默认端口被占用后落到 {port}（自占默认端口成功={}）", occupied.is_ok());
        if occupied.is_ok() {
            assert_ne!(port, DEFAULT_PORT, "默认端口已被占住，不可能又绑到它");
        }
        assert!(
            (DEFAULT_PORT..DEFAULT_PORT + PORT_SCAN).contains(&port),
            "回退端口应落在 {DEFAULT_PORT}~{} 内，实得 {port}",
            DEFAULT_PORT + PORT_SCAN - 1
        );
        // occupied / l 在此之前不能被回收，否则上面的「被占用」前提就不成立
        drop(occupied);
        drop(l);
    }

    /// 用户显式指定的端口**不许**擅自换：绑不上就如实失败，并说清是他指定的。
    #[test]
    fn explicit_port_never_falls_back() {
        // 借一个系统分配的空闲端口再占住它，避免写死端口号与别的测试/软件撞车
        let occupied =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).expect("借一个空闲端口");
        let p = occupied.local_addr().expect("回读实际端口").port();
        let err = bind(PortChoice::Fixed(p)).expect_err("显式指定的端口被占用时必须失败");
        println!("显式端口失败文案：{err}");
        assert!(err.contains(&p.to_string()), "错误里要有真实端口号，实得 {err}");
        assert!(err.contains(PORT_ENV), "要点明是 {PORT_ENV} 指定的，实得 {err}");
        drop(occupied);
    }

    #[test]
    fn initialize_advertises_tools_and_resources() {
        let r = initialize_result();
        assert!(r["capabilities"]["tools"].is_object());
        assert!(r["capabilities"]["resources"].is_object());
        assert_eq!(r["serverInfo"]["name"], json!("itools-plugin-dev"));
    }
}
