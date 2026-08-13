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
//! 鉴权：会话令牌走 `Authorization: Bearer <token>`。数据按 (用户, 命名空间) 隔离，last-write-wins。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::config::Config;
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
    pub clock: Clock,
}

/// 组装路由表。
///
/// 定时探测**不在这里启动**：由 `main.rs` 显式 `MirrorRegistry::start()`，
/// 这样测试里构造 Router 不会偷偷起定时器打外网。
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/mirrors", get(get_mirrors))
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/account/delete", post(delete_account))
        // GET /data/_usage 与 POST /data/{ns} 分属不同方法与路径，静态段优先匹配，不冲突。
        .route("/data/_usage", get(usage))
        .route("/data/{ns}", post(sync_data))
        .with_state(state)
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
