//! 登录、登出、当前身份、改口令。

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, ClientIp, bad_request, bearer, err, store_err};
use crate::auth;
use crate::store::admins::ConsoleSession;
use crate::store::audit::action;

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// 登录。
///
/// 三条纪律：
/// 1. **用户不存在与口令错误返回同一句话**——不然这个接口就成了管理员账号枚举器。
/// 2. 无论成败都过限流，成功后清零。
/// 3. 失败也写审计（`ok=0`）：连续的失败尝试本身就是需要被看见的信号。
pub async fn login(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    let username = body.username.trim().to_string();
    let password = body.password;
    let now = st.now();

    if username.is_empty() || password.is_empty() {
        return bad_request("用户名与口令必填");
    }

    let decision = st.login_limiter.check(&ip, now);
    if !decision.allowed {
        st.store
            .audit(&username, action::LOGIN_FAILED, "", "触发登录限流", &ip, false, now)
            .await;
        tracing::warn!("[auth] 登录限流触发（IP 已省略）");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", decision.retry_after_sec.to_string())],
            Json(json!({
                "error": format!("登录尝试过于频繁，请 {} 秒后再试", decision.retry_after_sec),
                "code": "rate_limited"
            })),
        )
            .into_response();
    }

    let admin = match st.store.get_admin(&username).await {
        Ok(v) => v,
        Err(e) => return store_err("查询控制台管理员", e),
    };

    // 用户不存在时也要走一次口令派生，让「不存在」与「口令错」的耗时接近，
    // 不给时序侧信道留探测账号是否存在的空间。
    let ok = match &admin {
        Some(a) => {
            a.status == "active" && auth::verify_password(&password, &a.salt, &a.password_hash)
        }
        None => {
            let dummy = auth::hash_password("dummy-for-constant-time");
            let _ = auth::verify_password(&password, &dummy.salt, &dummy.hash);
            false
        }
    };

    if !ok {
        // 账号被停用与口令错误对外是同一句话，但审计里如实区分，运维才排查得动
        let detail = match &admin {
            Some(a) if a.status != "active" => "账号已停用",
            Some(_) => "口令错误",
            None => "账号不存在",
        };
        st.store
            .audit(&username, action::LOGIN_FAILED, "", detail, &ip, false, now)
            .await;
        return err(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "用户名或口令错误",
        );
    }

    // 逻辑上 ok 为真蕴含 admin 是 Some。这里不用 expect：万一将来上面的判定被改坏，
    // panic 掉一个请求只会表现成「连接被重置」，远不如一条可读的 500 加日志好排查。
    let Some(admin) = admin else {
        tracing::error!("[auth] 登录判定异常：口令校验通过但账号记录为空");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "服务端内部错误",
        );
    };
    let token = auth::generate_token();
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let expires_at = match st
        .store
        .create_console_session(
            &token,
            &admin.username,
            now,
            st.config.session_ttl_sec as i64 * 1000,
            &ip,
            ua,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return store_err("创建控制台会话", e),
    };

    if let Err(e) = st.store.touch_admin_login(&admin.username, now, &ip).await {
        // 记录登录时间失败不该让人登不进来，但要在日志里留痕
        tracing::warn!("[auth] 更新最后登录时间失败：{e}");
    }
    st.login_limiter.reset(&ip);
    st.store
        .audit(&admin.username, action::LOGIN, "", "", &ip, true, now)
        .await;

    Json(json!({
        "token": token,
        "username": admin.username,
        "role": admin.role,
        "mustChangePassword": admin.must_change_password,
        "expiresAt": expires_at,
    }))
    .into_response()
}

pub async fn logout(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = bearer(&headers) {
        if let Err(e) = st.store.delete_console_session(&token).await {
            return store_err("删除控制台会话", e);
        }
    }
    st.store
        .audit(&session.username, action::LOGOUT, "", "", &ip, true, st.now())
        .await;
    Json(json!({ "ok": true })).into_response()
}

pub async fn whoami(State(st): State<Arc<AppState>>, Extension(session): Extension<ConsoleSession>) -> Response {
    let caps = st.store.caps();
    Json(json!({
        "username": session.username,
        "role": session.role,
        "mustChangePassword": session.must_change_password,
        "expiresAt": session.expires_at,
        "capabilities": {
            "canWrite": crate::store::role::can_write(&session.role),
            "canManageAdmins": crate::store::role::can_manage_admins(&session.role),
            "usersStatusColumn": caps.users_status,
            "marketRevokedBy": caps.market_revoked_by,
        }
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct ChangePasswordBody {
    #[serde(default)]
    old_password: String,
    #[serde(default)]
    new_password: String,
}

/// 改自己的口令。改完踢掉自己**其它**会话——口令换了，旧设备就不该还留着入口。
pub async fn change_password(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordBody>,
) -> Response {
    let now = st.now();

    if let Err(msg) = auth::check_password_strength(&body.new_password) {
        return bad_request(&msg);
    }
    if body.old_password == body.new_password {
        return bad_request("新口令不能与旧口令相同");
    }

    let admin = match st.store.get_admin(&session.username).await {
        Ok(Some(a)) => a,
        Ok(None) => return err(StatusCode::UNAUTHORIZED, "unauthorized", "账号已不存在"),
        Err(e) => return store_err("查询控制台管理员", e),
    };
    if !auth::verify_password(&body.old_password, &admin.salt, &admin.password_hash) {
        st.store
            .audit(
                &session.username,
                action::CHANGE_PASSWORD,
                &session.username,
                "旧口令错误",
                &ip,
                false,
                now,
            )
            .await;
        return bad_request("旧口令不正确");
    }

    if let Err(e) = st.store.set_admin_password(&session.username, &body.new_password).await {
        return store_err("更新口令", e);
    }
    // 先删光全部会话，再让当前这条重新签发：确保其它设备上的旧令牌立刻失效
    if let Err(e) = st.store.delete_console_sessions_of(&session.username).await {
        return store_err("清理会话", e);
    }
    let token = auth::generate_token();
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let expires_at = match st
        .store
        .create_console_session(
            &token,
            &session.username,
            now,
            st.config.session_ttl_sec as i64 * 1000,
            &ip,
            ua,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return store_err("创建控制台会话", e),
    };

    st.store
        .audit(
            &session.username,
            action::CHANGE_PASSWORD,
            &session.username,
            "",
            &ip,
            true,
            now,
        )
        .await;

    Json(json!({ "ok": true, "token": token, "expiresAt": expires_at })).into_response()
}
