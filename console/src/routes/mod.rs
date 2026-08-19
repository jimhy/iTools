//! HTTP 层：路由组装、统一鉴权守卫、错误格式、客户端 IP 判定、安全响应头。
//!
//! ## 鉴权靠中间件，不靠每个 handler 自觉
//!
//! 主服务是每个 handler 手写三行 `authenticate(...)`（`server/src/routes.rs:782`），
//! 漏写一个就是完全公开的端点。控制台是运营入口、权限高得多，这里改成
//! **受保护路由整组套一层 `middleware::from_fn`**：新增端点时只要挂在
//! `protected_routes()` 里就自动受保护，想漏都漏不掉。
//!
//! 守卫做三件事，缺一不可：
//! 1. 校验令牌（含过期、账号被停用、角色未知）
//! 2. `must_change_password` 为真时，除改密/登出/whoami 外一律拒绝
//! 3. 把会话塞进 request extensions 供 handler 取用

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde_json::json;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;
use crate::ratelimit::RateLimiter;
use crate::store::admins::ConsoleSession;
use crate::store::{role, Store};
use crate::Clock;

mod admins;
mod audit;
mod plugins;
mod session;
mod stats;
mod system;
mod users;

pub struct AppState {
    pub store: Store,
    pub config: Config,
    pub clock: Clock,
    pub login_limiter: RateLimiter,
    /// 探测主服务 `/health` 用。`None` 表示构造失败（TLS 后端异常），
    /// 系统页会如实显示「探测器不可用」，而不是显示成主服务挂了。
    pub http: Option<reqwest::Client>,
}

impl AppState {
    pub fn now(&self) -> i64 {
        (self.clock)()
    }
}

/// 统一错误响应。
///
/// 比主服务多一个 `code`：前端必须能区分「没登录」「权限不够」「端点不存在」
/// 「服务端炸了」，否则只能拿中文文案做字符串匹配，或者一律显示成「网络异常」——
/// 后者正是项目审查报告点名的「二次误导」。
pub fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "code": code }))).into_response()
}

pub fn bad_request(message: &str) -> Response {
    err(StatusCode::BAD_REQUEST, "bad_request", message)
}

pub fn not_found(message: &str) -> Response {
    err(StatusCode::NOT_FOUND, "not_found", message)
}

pub fn forbidden(message: &str) -> Response {
    err(StatusCode::FORBIDDEN, "forbidden", message)
}

/// 存储层错误的统一出口：细节只进日志，浏览器只拿到一句话。
///
/// 数据库错误信息里可能带表结构甚至数据片段，回显给前端等于白送情报。
pub fn store_err(what: &str, e: sqlx::Error) -> Response {
    tracing::error!("[db] {what} 失败：{e}");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "服务端内部错误",
    )
}

/// 已判定的客户端 IP。由 [`resolve_client_ip`] 中间件放进 request extensions，
/// handler 用 `Extension(ClientIp(ip))` 取。
///
/// 做成中间件而不是让每个 handler 自己取 `ConnectInfo`，是为了让「信任不信任
/// X-Forwarded-For」这条判断**只有一个地方**——散在十几个 handler 里迟早会写歪一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIp(pub String);

/// 算出客户端 IP 并塞进 extensions。挂在最外层，所有路由都会经过。
async fn resolve_client_ip(State(st): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let ip = client_ip(&st.config, req.headers(), peer);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// 判定客户端 IP。
///
/// 只有显式 `CONSOLE_TRUST_PROXY=true` 时才认 `X-Forwarded-For`——
/// 不然任何人加个请求头就能伪造 IP、绕开登录限流。
pub fn client_ip(cfg: &Config, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if cfg.trust_proxy {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            // XFF 是逗号分隔链，最左边是原始客户端
            if let Some(first) = xff.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    peer.map(|p| p.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 从 `Authorization: Bearer <token>` 取令牌。
///
/// 前缀比较**大小写不敏感**（RFC 7235 规定 scheme 不区分大小写），
/// 且容忍多个空格——主服务那边是严格 `strip_prefix("Bearer ")`，
/// 一个大小写差异就会变成「明明登录了却一直 401」这种极难排查的故障。
pub fn bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    let rest = raw.strip_prefix("Bearer ").or_else(|| {
        let lower = raw.to_ascii_lowercase();
        lower.starts_with("bearer ").then(|| &raw[7..])
    })?;
    let token = rest.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// `must_change_password` 为真时仍然放行的端点。
///
/// 必须包含登出——否则一个被强制改密的账号连退出都做不到，只能干等会话过期。
fn allowed_while_password_expired(path: &str) -> bool {
    matches!(path, "/api/password" | "/api/whoami" | "/api/logout")
}

/// 受保护路由的统一守卫。
async fn guard(State(st): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "请先登录（缺少 Authorization: Bearer 令牌）",
        );
    };

    let now = st.now();
    let session = match st.store.verify_console_session(&token, now).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return err(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "登录已失效，请重新登录",
            )
        }
        Err(e) => return store_err("校验控制台会话", e),
    };

    if session.must_change_password && !allowed_while_password_expired(req.uri().path()) {
        return err(
            StatusCode::FORBIDDEN,
            "must_change_password",
            "首次登录必须先修改口令",
        );
    }

    req.extensions_mut().insert(session);
    next.run(req).await
}

/// 要求当前会话具备写权限（只读角色会被挡在这里）。
///
/// `Err` 里装的是完整的 `Response`。clippy 会嫌它大（128 字节），但换成 `Box`
/// 只会让每个调用点多一次解包——守卫每请求最多调一次，这点栈开销无关紧要，
/// 而 `if let Err(r) = require_write(&s) { return r; }` 这个写法的可读性更值钱。
#[allow(clippy::result_large_err)]
pub fn require_write(session: &ConsoleSession) -> Result<(), Response> {
    if role::can_write(&session.role) {
        Ok(())
    } else {
        Err(forbidden("当前账号是只读角色，不能执行此操作"))
    }
}

/// 要求当前会话能管理其他管理员账号。
#[allow(clippy::result_large_err)]
pub fn require_super(session: &ConsoleSession) -> Result<(), Response> {
    if role::can_manage_admins(&session.role) {
        Ok(())
    } else {
        Err(forbidden("只有超级管理员能管理控制台账号"))
    }
}

/// 需要登录才能访问的 API。**新增端点一律加在这里**，自动继承守卫。
fn protected_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/whoami", get(session::whoami))
        .route("/api/logout", post(session::logout))
        .route("/api/password", post(session::change_password))
        // 概览与统计
        .route("/api/overview", get(stats::overview))
        .route("/api/stats/series", get(stats::series))
        .route("/api/stats/storage", get(stats::storage))
        .route("/api/stats/namespaces", get(stats::namespaces))
        // 流量（数据源是主服务的 traffic_hourly / plugin_downloads_hourly）
        .route("/api/stats/traffic", get(stats::traffic))
        .route("/api/stats/traffic/routes", get(stats::traffic_routes))
        .route("/api/stats/downloads", get(stats::downloads))
        // 终端用户
        .route("/api/users", get(users::list))
        .route("/api/users/{username}", get(users::detail))
        .route("/api/users/{username}", delete(users::remove))
        .route("/api/users/{username}/kick", post(users::kick))
        .route("/api/users/{username}/disable", post(users::disable))
        .route("/api/users/{username}/enable", post(users::enable))
        // 插件（只读）
        .route("/api/plugins", get(plugins::list_market))
        .route("/api/plugins/{name}", get(plugins::market_detail))
        .route("/api/submissions", get(plugins::list_submissions))
        .route("/api/submissions/{id}", get(plugins::submission_detail))
        // 控制台账号
        .route("/api/admins", get(admins::list))
        .route("/api/admins", post(admins::create))
        .route("/api/admins/{username}", delete(admins::remove))
        .route("/api/admins/{username}/role", post(admins::set_role))
        .route("/api/admins/{username}/status", post(admins::set_status))
        .route("/api/admins/{username}/password", post(admins::reset_password))
        // 审计与系统
        .route("/api/audit", get(audit::list))
        .route("/api/system", get(system::info))
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let protected = protected_routes().layer(middleware::from_fn_with_state(
        state.clone(),
        guard,
    ));

    // 未登录也要能访问的两个端点：
    // - /api/login  显然
    // - /api/meta   前端在登录页就要知道「服务端支持哪些能力」，才能如实标注
    //               哪些功能未就绪。它只返回布尔开关，不含任何数据。
    let public = Router::new()
        .route("/api/login", post(session::login))
        .route("/api/meta", get(system::meta))
        .route("/healthz", get(|| async { "console-alive" }));

    let mut app = Router::new().merge(public).merge(protected);

    // 静态前端。挂在最后做 fallback，不会顶掉上面任何一条 API 路由。
    if let Some(dir) = &state.config.web_dir {
        app = app.fallback_service(ServeDir::new(dir));
        tracing::info!("[web] 控制台前端目录：{dir}");
    } else {
        tracing::warn!("[web] 未配置 CONSOLE_WEB_DIR，只提供 API、不托管页面");
    }

    app.layer(SetResponseHeaderLayer::overriding(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             img-src 'self' data:; \
             style-src 'self' 'unsafe-inline'; \
             script-src 'self'; \
             connect-src 'self'; \
             font-src 'self'; \
             object-src 'none'; \
             frame-ancestors 'none'; \
             base-uri 'none'; \
             form-action 'self'",
        ),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        axum::http::header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    ))
    .layer(SetResponseHeaderLayer::overriding(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    ))
    // 后台页面全程带凭据，缓存必须彻底关掉：共用电脑上按后退键不该还能看到上一个人的数据
    .layer(SetResponseHeaderLayer::overriding(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    ))
    // 最后挂 = 最外层 = 最先执行：后面所有层与 handler 都能拿到 ClientIp
    .layer(middleware::from_fn_with_state(state.clone(), resolve_client_ip))
    .with_state(state)
}

/// 当前会话。守卫保证受保护路由上一定存在。
pub type Session = Extension<ConsoleSession>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::{IpAddr, Ipv4Addr};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn cfg(trust_proxy: bool) -> Config {
        let mut c = Config::from_map(&Default::default()).unwrap();
        c.trust_proxy = trust_proxy;
        c
    }

    #[test]
    fn bearer_is_case_insensitive_and_trims() {
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer abc123")])).as_deref(),
            Some("abc123")
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "bearer abc123")])).as_deref(),
            Some("abc123"),
            "scheme 按 RFC 7235 不区分大小写"
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "BEARER  abc123  ")])).as_deref(),
            Some("abc123"),
            "多余空格要容忍"
        );
        assert_eq!(bearer(&headers(&[("authorization", "Bearer ")])), None, "空令牌不算");
        assert_eq!(bearer(&headers(&[("authorization", "Basic abc")])), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn xff_is_ignored_unless_trusted() {
        let peer = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 1234));
        let h = headers(&[("x-forwarded-for", "1.2.3.4, 5.6.7.8")]);

        assert_eq!(
            client_ip(&cfg(false), &h, peer),
            "10.0.0.1",
            "不信任代理时必须无视 XFF，否则谁都能伪造 IP 绕开限流"
        );
        assert_eq!(
            client_ip(&cfg(true), &h, peer),
            "1.2.3.4",
            "信任代理时取链最左端的原始客户端"
        );
        assert_eq!(
            client_ip(&cfg(true), &HeaderMap::new(), peer),
            "10.0.0.1",
            "信任代理但没有 XFF 时回落到对端地址"
        );
        assert_eq!(client_ip(&cfg(false), &HeaderMap::new(), None), "unknown");
    }

    #[test]
    fn empty_xff_falls_back_to_peer() {
        let peer = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 1));
        let h = headers(&[("x-forwarded-for", "   ")]);
        assert_eq!(client_ip(&cfg(true), &h, peer), "10.0.0.1");
    }

    #[test]
    fn password_expired_allowlist_includes_logout() {
        assert!(allowed_while_password_expired("/api/password"));
        assert!(allowed_while_password_expired("/api/whoami"));
        assert!(
            allowed_while_password_expired("/api/logout"),
            "不放行登出的话，被强制改密的账号连退出都做不到"
        );
        assert!(!allowed_while_password_expired("/api/users"));
        assert!(!allowed_while_password_expired("/api/overview"));
    }

    #[test]
    fn role_guards_reject_insufficient_roles() {
        let viewer = ConsoleSession {
            username: "v".into(),
            role: role::VIEWER.into(),
            must_change_password: false,
            expires_at: 0,
        };
        let admin = ConsoleSession {
            role: role::ADMIN.into(),
            ..viewer.clone()
        };
        let sup = ConsoleSession {
            role: role::SUPER.into(),
            ..viewer.clone()
        };
        assert!(require_write(&viewer).is_err());
        assert!(require_write(&admin).is_ok());
        assert!(require_super(&admin).is_err(), "普通管理员不能管账号");
        assert!(require_super(&sup).is_ok());
    }
}
