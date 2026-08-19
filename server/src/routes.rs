//! HTTP 路由：实现 iTools 客户端约定的 REST 契约。
//!
//! 契约（与 `src-tauri/src/account.rs` / `sync.rs` 精确对齐）：
//! - `POST /auth/login`      body `{username,password}`                        → `{token,username}`（首登可自动注册）
//! - `POST /auth/logout`     Bearer + body `{allDevices?}`                      → `{ok:true}`
//! - `POST /account/delete`  body `{username,password}`                         → `{ok:true}`
//! - `POST /data/:ns`        Bearer + body `{records:[{key,value,updatedAt}]}`  → `{records:[...]}`
//! - `GET  /data/_usage`     Bearer                                             → `{counts:{ns:n}, bytes}`
//! - `GET  /api/mirrors`     **公开、无需认证**（按 IP 限流）                     → GitHub 镜像源配置（见 mirrors.rs）
//! - `GET  /health`          → 健康检查
//!
//! 插件市场与提审（见 market.rs）：
//! - `POST /api/plugins/submit`         Bearer + body = 插件包 zip 字节           → 提审单
//! - `GET  /api/plugins/submissions`    Bearer                                    → 我的提审记录
//! - `GET  /api/plugins/submissions/{id}` Bearer                                  → 单条详情（含裁决原文）
//! - `GET  /api/market/index`           **公开**（限流 + ETag）                    → 市场索引
//! - `GET  /api/market/package/{name}`  **公开**                                  → 已上线插件包 zip
//! - `POST /api/market/revoke`          Bearer（作者本人或维护者）                 → 下架 / 恢复
//!
//! 鉴权：会话令牌走 `Authorization: Bearer <token>`。数据按 (用户, 命名空间) 隔离，last-write-wins。

use std::collections::HashSet;
use std::net::SocketAddr;
// axum::extract::Path 已占用 `Path` 这个名字，std 的路径类型用别名
use std::path::Path as StdPath;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::market::MarketService;
use crate::mirrors::{matches_etag, MirrorRegistry};
use crate::proxy::client_ip;
use crate::ratelimit::SlidingWindowLimiter;
use crate::store::{MariaDbStore, UserRecord, WireRecord};
use crate::{auth, Clock};

pub struct AppState {
    pub store: Arc<MariaDbStore>,
    pub config: Arc<Config>,
    pub mirrors: Arc<MirrorRegistry>,
    pub mirror_limiter: Arc<SlidingWindowLimiter>,
    /// 插件市场与提审。与镜像端点一样是公开可读的，所以另配一个限流桶。
    pub market: Arc<MarketService>,
    pub market_limiter: Arc<SlidingWindowLimiter>,
    pub clock: Clock,
}

/// 组装路由表。
///
/// 定时探测**不在这里启动**：由 `main.rs` 显式 `MirrorRegistry::start()`，
/// 这样测试里构造 Router 不会偷偷起定时器打外网。
pub fn build_router(state: Arc<AppState>) -> Router {
    let upload_limit = state.config.market.max_upload_bytes as usize;
    let router = Router::new()
        .route("/health", get(health))
        .route("/api/mirrors", get(get_mirrors))
        // 官网读它来显示「最新版 vX.Y.Z」；扫真实目录得出，不写死
        .route("/api/download/latest", get(latest_download))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/account/delete", post(delete_account))
        // GET /data/_usage 与 POST /data/{ns} 分属不同方法与路径，静态段优先匹配，不冲突。
        .route("/data/_usage", get(usage))
        .route("/data/{ns}", post(sync_data))
        // 插件市场与提审
        .route("/api/market/index", get(market_index))
        .route("/api/market/package/{name}", get(market_package))
        .route("/api/market/revoke", post(market_revoke))
        // 上传插件包会超过 axum 默认 2MB 的 body 上限，按配置放开。
        // **只给这一条路由放开**：其它端点收的都是小 JSON，给它们同样的上限等于凭空多一个内存放大面。
        .route(
            "/api/plugins/submit",
            post(plugin_submit).layer(axum::extract::DefaultBodyLimit::max(upload_limit)),
        )
        .route("/api/plugins/submissions", get(my_submissions))
        .route("/api/plugins/submissions/{id}", get(one_submission))
        .with_state(Arc::clone(&state));

    mount_site(router, &state.config.site)
}

/// 把官网静态站与安装包目录挂到 API 路由之后。
///
/// # 顺序与优先级
///
/// 静态站走 `fallback_service`，只接**没被任何 API 路由匹配到**的请求，所以不可能
/// 顶掉 `/health`、`/api/*`、`/auth/*`、`/data/*`。`/download` 用 `nest_service` 挂在
/// 一个专属前缀下，与现有端点也没有交集。
///
/// # 目录不存在就不挂
///
/// 挂一个指向空气的 ServeDir 只会让每个请求 404，而运维看不出「是没配还是配错了」。
/// 这里如实分三种情况写日志：没配 / 配了但目录不存在（**当作错误喊出来**）/ 挂载成功。
///
/// # 安全
///
/// 用 `ServeDir` 而不是自己拼路径读文件：它拒绝 `..` 与绝对路径（路径穿越），
/// 并原生支持 Range 请求——安装包好几 MB，断点续传是刚需。
fn mount_site(router: Router, site: &crate::config::SiteConfig) -> Router {
    let mut router = router;

    match &site.downloads {
        Some(dir) if dir.is_dir() => {
            tracing::info!("安装包下载已挂载：/download → {}", dir.display());
            router = router.nest_service("/download", ServeDir::new(dir));
        }
        Some(dir) => {
            tracing::error!(
                "SYNC_DOWNLOADS_DIR 指向的目录不存在：{}，/download 未挂载（安装包将无法下载）",
                dir.display()
            );
        }
        None => tracing::info!("未配置 SYNC_DOWNLOADS_DIR，不提供安装包下载"),
    }

    match &site.root {
        Some(dir) if dir.is_dir() => {
            tracing::info!("官网静态站已挂载：/ → {}", dir.display());
            // append_index_html_on_directories 默认开启：`/` 会落到 index.html
            router = router.fallback_service(ServeDir::new(dir));
        }
        Some(dir) => {
            tracing::error!(
                "SYNC_SITE_DIR 指向的目录不存在：{}，官网未挂载（/ 将返回 404）",
                dir.display()
            );
        }
        None => tracing::info!("未配置 SYNC_SITE_DIR，不提供官网静态站"),
    }

    router
}

/// TCP 对端地址（限流计数的基准）。
///
/// 取自 `ConnectInfo`，**取不到时为 None 而不是报错**：`axum::serve` 与 TLS 循环都会注入它，
/// 但集成测试里直接调 Router 时没有真实连接——那种场景下限流该照常工作（按 `unknown` 分桶），
/// 不该让整个端点 500。
pub struct ClientAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ClientAddr(
            parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(a)| *a),
        ))
    }
}

// ---------------------------------------------------------------- 小工具

fn error_response(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

/// 存储层出错时的统一出口：细节只进日志，不回给客户端（避免泄露库结构/凭据线索）。
fn store_error(scope: &str, err: sqlx::Error) -> Response {
    tracing::error!("[store] {scope} 失败: {err}");
    error_response(StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误")
}

/// 从 `Authorization` 头取 Bearer 令牌（无则空串）。
fn bearer(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

/// 宽容地把请求体解析成 JSON 对象（非法/空体一律视为空对象，与旧版 `req.body ?? {}` 一致）。
fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn field_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// scrypt 是有意设计成慢的（CPU + 内存密集），必须挪出异步线程池，
/// 否则一次登录就会卡住整条 tokio worker 线程。
async fn verify_password_off_thread(password: String, salt: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || auth::verify_password(&password, &salt, &hash))
        .await
        .unwrap_or(false)
}

async fn hash_password_off_thread(password: String) -> Option<auth::PasswordHash> {
    tokio::task::spawn_blocking(move || auth::hash_password(&password)).await.ok()
}

// ---------------------------------------------------------------- 端点

/// 健康检查（也用于确认端点可达）。**不限流**：健康检查被限流会让负载均衡/监控误判服务已死。
async fn health(State(st): State<Arc<AppState>>) -> Response {
    Json(json!({
        "ok": true,
        "service": "itools-sync",
        "allowRegister": st.config.allow_register,
    }))
    .into_response()
}

/// 官网用：**服务器上实际放着的**最新安装包版本与体积（公开端点）。
///
/// # 为什么要有它
///
/// 官网是纯静态页，自己不可能知道服务器上放的是哪个版本。把版本号写死在 HTML 里，
/// 等于给发版流程加一个「必须记得同步改页面」的步骤——那一步迟早会忘，
/// 而忘了的表现是**页面上写着旧版本号、下载下来却是新包**，比不显示版本号更糟。
///
/// 所以这里扫真实目录、按文件名里的版本号取最大的那个。扫不到就返回 `version: null`，
/// 让前端如实退回「下载最新版」，绝不编一个版本号出来。
async fn latest_download(State(st): State<Arc<AppState>>) -> Response {
    let Some(dir) = st.config.site.downloads.as_ref() else {
        return Json(json!({ "version": Value::Null, "reason": "未配置下载目录" })).into_response();
    };
    match scan_latest_installer(dir) {
        Some((version, size)) => Json(json!({
            "version": version,
            "sizeBytes": size,
            // 官网固定链到这个名字，发版时覆盖它即可，页面不用跟着改
            "file": "iTools-latest-setup.exe",
        }))
        .into_response(),
        None => {
            Json(json!({ "version": Value::Null, "reason": "下载目录里没有可识别的安装包" }))
                .into_response()
        }
    }
}

/// 从 `iTools_<版本>_x64-setup.exe` 这类文件名里挑出版本号最大的一个，返回 (版本, 固定名文件的体积)。
///
/// 体积取的是**固定名那个文件**（`iTools-latest-setup.exe`，官网真正下载的就是它），
/// 而不是带版本号那份——两者理论上一致，但真正决定用户下载多大的是前者，
/// 报体积就该报实际会下载的那个。
fn scan_latest_installer(dir: &StdPath) -> Option<(String, u64)> {
    const PREFIX: &str = "iTools_";
    const SUFFIX: &str = "_x64-setup.exe";

    let mut best: Option<(Vec<u64>, String)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(PREFIX).and_then(|s| s.strip_suffix(SUFFIX)) else {
            continue;
        };
        // 按 `.` 分段做数值比较（与客户端 version_gt 同口径），解析不了的段直接跳过这个文件
        let Some(parts) = rest
            .split('.')
            .map(|s| s.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| parts > *b) {
            best = Some((parts, rest.to_string()));
        }
    }
    let (_, version) = best?;
    // 体积以官网实际会下载的那个固定名文件为准；它不在就退回 0（前端不显示体积）
    let size = std::fs::metadata(dir.join("iTools-latest-setup.exe"))
        .map(|m| m.len())
        .unwrap_or(0);
    Some((version, size))
}

/// GitHub 镜像源配置（公开端点，无需认证）。
///
/// 没登录的用户也要能装插件，这里加鉴权会直接堵死插件安装这条路——**不要**给它加鉴权。
/// 代价是它是全服唯一的免认证读端点，所以单独上一层按 IP 限流（全服其它路由都要 Bearer 令牌）。
/// 支持 ETag / If-None-Match：内容未变返回 304（客户端本地 TTL 30 分钟）。
async fn get_mirrors(
    State(st): State<Arc<AppState>>,
    ClientAddr(peer): ClientAddr,
    headers: HeaderMap,
) -> Response {
    let mut extra: Vec<(&'static str, String)> = Vec::new();

    if st.mirror_limiter.enabled() {
        let ip = client_ip(
            &st.config.trust_proxy,
            peer.map(|a| a.ip()),
            headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
        );
        let verdict = st.mirror_limiter.hit(&ip);
        extra.push(("x-ratelimit-limit", verdict.limit.to_string()));
        extra.push(("x-ratelimit-remaining", verdict.remaining.to_string()));
        if !verdict.allowed {
            // 只在「刚越限」时打一条日志：限流日志自己不能变成新的日志放大源。
            if verdict.first_reject {
                let hint = if st.config.trust_proxy == crate::proxy::TrustProxy::No {
                    "（若本服务置于反向代理之后，请设 SYNC_TRUST_PROXY，否则所有客户端共用一个限流桶）"
                } else {
                    ""
                };
                tracing::warn!(
                    "[mirrors] 限流生效：{ip} 在 {}s 内超过 {} 次请求{hint}",
                    st.config.mirrors.rate_limit_window_sec,
                    verdict.limit
                );
            }
            let mut res = error_response(StatusCode::TOO_MANY_REQUESTS, "请求过于频繁，请稍后再试");
            put_headers(&mut res, &extra);
            insert_header(&mut res, "retry-after", &verdict.retry_after_sec.to_string());
            return res;
        }
    }

    // 按需重载配置文件（mtime 轮询，带节流）；失败只记日志、沿用当前配置，不影响响应。
    st.mirrors.refresh_if_needed().await;
    let body = st.mirrors.body();

    extra.push(("etag", body.etag.clone()));
    extra.push((
        "cache-control",
        format!("public, max-age={}, must-revalidate", st.config.mirrors.cache_max_age_sec),
    ));
    // 「本轮探测时刻」每轮都变，只能走响应头：进响应体会让 ETag 每轮失效、304 永不命中。
    // 200 与 304 都带，运维 `curl -I` 随时能看到数据新鲜度。
    if let Some(at) = st.mirrors.last_probe_at() {
        extra.push(("x-mirror-probe-at", at));
    }

    let if_none_match = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let mut res = if matches_etag(if_none_match, &body.etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut r = body.json.into_response();
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        r
    };
    put_headers(&mut res, &extra);
    res
}

fn put_headers(res: &mut Response, pairs: &[(&'static str, String)]) {
    for (k, v) in pairs {
        insert_header(res, k, v);
    }
}

fn insert_header(res: &mut Response, key: &'static str, value: &str) {
    if let Ok(v) = header::HeaderValue::from_str(value) {
        if let Ok(name) = header::HeaderName::from_bytes(key.as_bytes()) {
            res.headers_mut().insert(name, v);
        }
    }
}

/// 登录（首登可自动注册）。
async fn login(State(st): State<Arc<AppState>>, body: Bytes) -> Response {
    let body = json_body(&body);
    let username = field_str(&body, "username").trim().to_string();
    let password = field_str(&body, "password").to_string();
    if username.is_empty() || password.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "username 与 password 必填");
    }

    let existing = match st.store.get_user(&username).await {
        Ok(u) => u,
        Err(e) => return store_error("查询用户", e),
    };
    match existing {
        None => {
            if !st.config.allow_register {
                return error_response(StatusCode::NOT_FOUND, "账号不存在");
            }
            let Some(hashed) = hash_password_off_thread(password).await else {
                tracing::error!("[auth] 口令哈希任务失败");
                return error_response(StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误");
            };
            let user = UserRecord {
                username: username.clone(),
                password_hash: hashed.hash,
                salt: hashed.salt,
                created_at: (st.clock)(),
            };
            if let Err(e) = st.store.create_user(&user).await {
                return store_error("创建用户", e);
            }
            tracing::info!("[auth] {username} 首登自动注册");
        }
        Some(user) => {
            if !verify_password_off_thread(password, user.salt, user.password_hash).await {
                return error_response(StatusCode::UNAUTHORIZED, "用户名或密码错误");
            }
        }
    }

    let token = auth::generate_token(st.config.token_bytes);
    if let Err(e) = st.store.create_session(&token, &username, (st.clock)()).await {
        return store_error("创建会话", e);
    }
    Json(json!({ "token": token, "username": username })).into_response()
}

/// 退出登录（可全设备）。
async fn logout(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let token = bearer(&headers);
    if token.is_empty() {
        return error_response(StatusCode::UNAUTHORIZED, "缺少会话令牌");
    }
    let username = match st.store.session_user(&token).await {
        Ok(Some(u)) => u,
        Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "会话无效"),
        Err(e) => return store_error("查询会话", e),
    };
    let body = json_body(&body);
    let all_devices = body.get("allDevices").and_then(Value::as_bool).unwrap_or(false);
    let result = if all_devices {
        st.store.delete_user_sessions(&username).await
    } else {
        st.store.delete_session(&token).await
    };
    if let Err(e) = result {
        return store_error("删除会话", e);
    }
    Json(json!({ "ok": true })).into_response()
}

/// 注销账号（真实鉴权 + 删除服务端数据）。
async fn delete_account(State(st): State<Arc<AppState>>, body: Bytes) -> Response {
    let body = json_body(&body);
    let username = field_str(&body, "username").trim().to_string();
    let password = field_str(&body, "password").to_string();
    if username.is_empty() || password.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "username 与 password 必填");
    }
    let user = match st.store.get_user(&username).await {
        Ok(u) => u,
        Err(e) => return store_error("查询用户", e),
    };
    let ok = match user {
        None => false,
        Some(u) => verify_password_off_thread(password, u.salt, u.password_hash).await,
    };
    if !ok {
        return error_response(StatusCode::UNAUTHORIZED, "用户名或密码错误");
    }
    if let Err(e) = st.store.delete_user(&username).await {
        return store_error("注销账号", e);
    }
    Json(json!({ "ok": true })).into_response()
}

/// 用量统计：某用户各命名空间条数与占用字节（供「我的数据」页）。
async fn usage(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    match st.store.usage(&username).await {
        Ok((counts, bytes)) => Json(json!({ "counts": counts, "bytes": bytes })).into_response(),
        Err(e) => store_error("统计用量", e),
    }
}

/// 数据同步：上行合并 + 回拉。
async fn sync_data(
    State(st): State<Arc<AppState>>,
    Path(ns): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    if ns.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "缺少命名空间");
    }

    // 规范化并校验推送记录（丢弃结构不合法的）
    let body = json_body(&body);
    let incoming = body.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut pushed: Vec<WireRecord> = Vec::with_capacity(incoming.len());
    for raw in &incoming {
        let (Some(key), Some(updated_at)) = (
            raw.get("key").and_then(Value::as_str),
            raw.get("updatedAt").and_then(as_millis),
        ) else {
            continue;
        };
        pushed.push(WireRecord {
            key: key.to_string(),
            value: raw.get("value").cloned().unwrap_or(Value::Null),
            updated_at,
        });
    }
    if let Err(e) = st.store.upsert_data(&username, &ns, &pushed).await {
        return store_error("上行合并", e);
    }

    // 回拉：返回该 (用户,命名空间) 的全部记录，排除刚推送的纯回声（同 key 同 updatedAt），
    // 让客户端 pulled 计数只反映「真正从云端/其它设备取到的新数据」。
    let echo: HashSet<(&str, i64)> =
        pushed.iter().map(|r| (r.key.as_str(), r.updated_at)).collect();
    let all = match st.store.get_data(&username, &ns).await {
        Ok(a) => a,
        Err(e) => return store_error("回拉数据", e),
    };
    let records: Vec<Value> = all
        .into_iter()
        .filter(|(key, rec)| !echo.contains(&(key.as_str(), rec.updated_at)))
        .map(|(key, rec)| json!({ "key": key, "value": rec.value, "updatedAt": rec.updated_at }))
        .collect();
    Json(json!({ "records": records })).into_response()
}

// ---------------------------------------------------------------- 插件市场 / 提审

/// 公开端点的按 IP 限流（与 `/api/mirrors` 同一套逻辑，另一个桶）。
///
/// 返回 `Some(响应)` 表示已越限，调用方应直接返回它。
fn market_rate_guard(
    st: &AppState,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    extra: &mut Vec<(&'static str, String)>,
) -> Option<Response> {
    if !st.market_limiter.enabled() {
        return None;
    }
    let ip = client_ip(
        &st.config.trust_proxy,
        peer.map(|a| a.ip()),
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
    );
    let verdict = st.market_limiter.hit(&ip);
    extra.push(("x-ratelimit-limit", verdict.limit.to_string()));
    extra.push(("x-ratelimit-remaining", verdict.remaining.to_string()));
    if verdict.allowed {
        return None;
    }
    if verdict.first_reject {
        let hint = if st.config.trust_proxy == crate::proxy::TrustProxy::No {
            "（若本服务置于反向代理之后，请设 SYNC_TRUST_PROXY，否则所有客户端共用一个限流桶）"
        } else {
            ""
        };
        tracing::warn!(
            "[market] 限流生效：{ip} 在 {}s 内超过 {} 次请求{hint}",
            st.config.market.rate_limit_window_sec,
            verdict.limit
        );
    }
    let mut res = error_response(StatusCode::TOO_MANY_REQUESTS, "请求过于频繁，请稍后再试");
    put_headers(&mut res, extra);
    insert_header(&mut res, "retry-after", &verdict.retry_after_sec.to_string());
    Some(res)
}

/// 市场索引（公开端点，无需认证）。
///
/// 与 `/api/mirrors` 同理：没登录的用户也要能浏览与安装插件，加鉴权会直接堵死这条路。
/// 支持 ETag / If-None-Match；索引只在发布 / 下架时变，所以 304 能稳定命中。
async fn market_index(
    State(st): State<Arc<AppState>>,
    ClientAddr(peer): ClientAddr,
    headers: HeaderMap,
) -> Response {
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    if let Some(res) = market_rate_guard(&st, peer, &headers, &mut extra) {
        return res;
    }

    let body = st.market.index().await;
    extra.push(("etag", body.etag.clone()));
    extra.push((
        "cache-control",
        format!("public, max-age={}, must-revalidate", st.config.mirrors.cache_max_age_sec),
    ));

    let inm = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    let mut res = if crate::market::matches_etag(inm, &body.etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut r = body.json.into_response();
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        r
    };
    put_headers(&mut res, &extra);
    res
}

/// 下载已上线的插件包（公开端点）。
///
/// 客户端下载后会用索引里的 `contentHash` 逐字节校验，所以这里不需要另做签名——
/// 但索引与包必须来自同一台服务器，否则哈希这条信任链就断了。
async fn market_package(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    ClientAddr(peer): ClientAddr,
    headers: HeaderMap,
) -> Response {
    let mut extra: Vec<(&'static str, String)> = Vec::new();
    if let Some(res) = market_rate_guard(&st, peer, &headers, &mut extra) {
        return res;
    }
    // 名字直接参与路径拼接，必须先过白名单（防目录穿越）
    if !crate::pkg::is_valid_plugin_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, "插件名不合法");
    }

    let found = match st.market.package_path(&name).await {
        Ok(f) => f,
        Err(e) => return store_error("查询市场条目", e),
    };
    let Some((path, row)) = found else {
        return error_response(StatusCode::NOT_FOUND, "市场里没有这个插件，或它已被下架");
    };
    let bytes = match tokio::task::spawn_blocking(move || crate::market::read_package(&path)).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            // 条目在库里、包却读不到 —— 这是服务端的数据不一致，如实报 500 并记日志，
            // 不要伪装成 404「没有这个插件」，那会让运维完全查不到问题。
            tracing::error!("[market] {name} 的包文件读取失败：{e}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "插件包暂时不可用，请稍后重试");
        }
        Err(e) => {
            tracing::error!("[market] 读取包任务异常终止：{e}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误");
        }
    };

    let mut res = bytes.into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/zip"),
    );
    put_headers(&mut res, &extra);
    insert_header(
        &mut res,
        "content-disposition",
        &format!("attachment; filename=\"{name}-{}.zip\"", row.version),
    );
    // 包是按版本不可变的，可以放心长缓存
    insert_header(&mut res, "cache-control", "public, max-age=86400");
    res
}

/// 提交插件包审核（body = zip 原始字节）。
async fn plugin_submit(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    if body.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "请求体为空：应上传插件包 zip 的原始字节");
    }

    match st.market.submit(&username, body.to_vec()).await {
        Ok(sub) => (StatusCode::ACCEPTED, Json(submission_json(&sub, false))).into_response(),
        Err(crate::market::SubmitError::Rejected(msg)) => error_response(StatusCode::BAD_REQUEST, &msg),
        Err(crate::market::SubmitError::Forbidden(msg)) => error_response(StatusCode::FORBIDDEN, &msg),
        Err(crate::market::SubmitError::TooSoon(sec)) => {
            let mut res = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                &format!("提交过于频繁，请 {sec} 秒后再试"),
            );
            insert_header(&mut res, "retry-after", &sec.to_string());
            res
        }
        Err(crate::market::SubmitError::Server(msg)) => {
            tracing::error!("[market] 提审失败：{msg}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误")
        }
    }
}

/// 我的提审记录（新→旧）。
async fn my_submissions(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    match st.market.store().list_submissions(&username, 50).await {
        Ok(list) => {
            let items: Vec<Value> = list.iter().map(|s| submission_json(s, false)).collect();
            Json(json!({ "submissions": items })).into_response()
        }
        Err(e) => store_error("查询提审记录", e),
    }
}

/// 单条提审单详情（含模型裁决原文）。只有本人（或维护者）能看。
async fn one_submission(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    match st.market.store().get_submission(&id).await {
        Ok(Some(s)) if s.author == username || st.market.is_admin(&username) => {
            Json(submission_json(&s, true)).into_response()
        }
        // 不区分「不存在」与「不是你的」：区分开就成了一个探测他人提审单是否存在的接口
        Ok(_) => error_response(StatusCode::NOT_FOUND, "没有这条提审记录"),
        Err(e) => store_error("查询提审单", e),
    }
}

/// 下架 / 恢复一个插件（作者本人或维护者）。
///
/// 鉴权在 [`crate::market::MarketService::set_revoked`] 里：作者只能动自己的插件，
/// 且**收不回维护者下的架**。这里只做参数校验与错误码映射。
async fn market_revoke(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let username = match authenticate(&st, &headers).await {
        Ok(u) => u,
        Err(res) => return res,
    };
    let body = json_body(&body);
    let name = field_str(&body, "name").trim().to_string();
    let revoked = body.get("revoked").and_then(Value::as_bool).unwrap_or(true);
    let reason = field_str(&body, "reason").trim().to_string();
    if name.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "缺少 name");
    }
    // 下架原因会原样展示给已安装该插件的用户 —— 用户有权知道为什么他装的东西被下架了
    if revoked && reason.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "下架必须给出原因（会展示给已安装的用户）");
    }
    if reason.chars().count() > 500 {
        return error_response(StatusCode::BAD_REQUEST, "下架原因太长（最多 500 字）");
    }
    match st.market.set_revoked(&username, &name, revoked, &reason).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(crate::market::RevokeError::NotFound) => {
            error_response(StatusCode::NOT_FOUND, "市场里没有这个插件")
        }
        Err(crate::market::RevokeError::Forbidden(msg)) => {
            error_response(StatusCode::FORBIDDEN, &msg)
        }
        Err(crate::market::RevokeError::Server(e)) => {
            tracing::error!("[market] 下架失败：{e}");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "服务端内部错误")
        }
    }
}

/// 提审单 → JSON。`full = true` 时附带模型裁决原文。
fn submission_json(s: &crate::store::Submission, full: bool) -> Value {
    let mut v = json!({
        "id": s.id,
        "name": s.name,
        "version": s.version,
        "status": s.status,
        "message": s.message,
        "contentHash": s.content_hash,
        "fileCount": s.file_count,
        "sizeBytes": s.size_bytes,
        "createdAt": s.created_at,
        "updatedAt": s.updated_at,
    });
    if full {
        // 裁决原样给出（含 findings），不做美化、不吞细节 —— 作者有权看到模型到底说了什么
        let review = serde_json::from_str::<Value>(&s.review).unwrap_or(Value::Null);
        v["review"] = review;
        v["manifest"] = serde_json::from_str::<Value>(&s.manifest).unwrap_or(Value::Null);
    }
    v
}

/// JS 的 number 是浮点：`updatedAt` 允许带小数形态（如 `1000.0`），一律取整成毫秒。
fn as_millis(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// 校验 Bearer 令牌并返回用户名；失败时直接给出 401 响应。
async fn authenticate(st: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    match st.store.session_user(&bearer(headers)).await {
        Ok(Some(u)) => Ok(u),
        Ok(None) => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "未授权（需有效会话令牌）",
        )),
        Err(e) => Err(store_error("查询会话", e)),
    }
}

#[cfg(test)]
mod site_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// 只带一条 API 路由的最小 Router，用来验证静态站有没有顶掉它。
    fn api_only() -> Router {
        Router::new().route("/health", get(|| async { "api-alive" }))
    }

    async fn get_status(app: Router, uri: &str) -> (StatusCode, String) {
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).expect("构造请求"))
            .await
            .expect("路由应当返回响应");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap_or_default();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    fn tmp_site(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("itools-site-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时站点目录");
        dir
    }

    /// 版本必须按**数值**比较：字符串排序会让 1.10.0 排在 1.9.0 前面，
    /// 于是发了 1.10.0 之后官网反而显示 1.9.0。
    #[test]
    fn picks_highest_version_numerically() {
        let dir = tmp_site("latest");
        for n in ["iTools_1.9.0_x64-setup.exe", "iTools_1.10.0_x64-setup.exe", "iTools_1.4.0_x64-setup.exe"] {
            std::fs::write(dir.join(n), b"x").expect("写安装包");
        }
        std::fs::write(dir.join("iTools-latest-setup.exe"), vec![0u8; 1234]).expect("写固定名包");

        let (ver, size) = scan_latest_installer(&dir).expect("应当扫到");
        assert_eq!(ver, "1.10.0", "1.10.0 必须大于 1.9.0（数值比较，不是字典序）");
        assert_eq!(size, 1234, "体积要报固定名那个文件——用户实际下载的就是它");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不认识的文件名一律跳过，绝不瞎猜出一个版本号。
    #[test]
    fn ignores_unrecognized_files() {
        let dir = tmp_site("latest-junk");
        for n in ["README.txt", "iTools_beta_x64-setup.exe", "iTools_1.2.x_x64-setup.exe", "setup.exe"] {
            std::fs::write(dir.join(n), b"x").expect("写文件");
        }
        assert!(scan_latest_installer(&dir).is_none(), "没有可识别的安装包时必须返回 None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 挂了静态站之后，**API 路由必须照常命中**。
    ///
    /// fallback_service 只接没被任何路由匹配到的请求；这条要是挂了，
    /// 整个同步服务会被一个静态站顶掉——比没有官网严重得多。
    #[tokio::test]
    async fn static_site_never_shadows_api_routes() {
        let dir = tmp_site("shadow");
        std::fs::write(dir.join("index.html"), "<h1>官网</h1>").expect("写 index.html");
        let app = mount_site(
            api_only(),
            &crate::config::SiteConfig { root: Some(dir.clone()), downloads: None },
        );

        let (status, body) = get_status(app.clone(), "/health").await;
        assert_eq!(status, StatusCode::OK, "API 路由必须优先于静态站");
        assert_eq!(body, "api-alive", "/health 必须还是那个 API，不能被 index.html 顶掉");

        // 而没被 API 匹配到的路径才落到静态站
        let (status, body) = get_status(app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("官网"), "/ 应当落到 index.html");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录不存在时**不挂载**，而不是挂上去让每个请求 404 —— 两者对运维的含义完全不同。
    #[tokio::test]
    async fn missing_dir_is_not_mounted() {
        let app = mount_site(
            api_only(),
            &crate::config::SiteConfig {
                root: Some(std::path::PathBuf::from("这个目录不存在")),
                downloads: Some(std::path::PathBuf::from("这个也不存在")),
            },
        );
        // API 照常
        assert_eq!(get_status(app.clone(), "/health").await.0, StatusCode::OK);
        // 没挂载 → 404（而不是 500 或别的）
        assert_eq!(get_status(app, "/").await.0, StatusCode::NOT_FOUND);
    }

    /// 安装包目录挂在 /download 下，且**路径穿越必须被拒**。
    #[tokio::test]
    async fn downloads_are_served_and_traversal_is_rejected() {
        let base = tmp_site("dl");
        let dl = base.join("downloads");
        std::fs::create_dir_all(&dl).expect("建下载目录");
        std::fs::write(dl.join("iTools-latest-setup.exe"), b"FAKE-INSTALLER").expect("写假安装包");
        // 放一个「上级目录里的秘密文件」，穿越成功就会读到它
        std::fs::write(base.join("secret.txt"), "绝不能被下载到").expect("写秘密文件");

        let app = mount_site(
            api_only(),
            &crate::config::SiteConfig { root: None, downloads: Some(dl.clone()) },
        );

        let (status, body) = get_status(app.clone(), "/download/iTools-latest-setup.exe").await;
        assert_eq!(status, StatusCode::OK, "安装包应当能下载");
        assert_eq!(body, "FAKE-INSTALLER");

        // 路径穿越：ServeDir 必须拒绝，绝不能读到上级目录
        for evil in ["/download/../secret.txt", "/download/%2e%2e/secret.txt"] {
            let (status, body) = get_status(app.clone(), evil).await;
            assert_ne!(status, StatusCode::OK, "{evil} 不该成功");
            assert!(!body.contains("绝不能被下载到"), "{evil} 读到了上级目录的文件");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
