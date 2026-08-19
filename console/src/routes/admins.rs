//! 控制台账号管理。**只有超级管理员能进这一组**。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, ClientIp, bad_request, not_found, require_super, store_err};
use crate::auth;
use crate::store::admins::ConsoleSession;
use crate::store::audit::action;
use crate::store::role;

pub async fn list(State(st): State<Arc<AppState>>, Extension(session): Extension<ConsoleSession>) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    match st.store.list_admins().await {
        // 口令哈希与盐**绝不出这一层**——即使前端不显示，也不该让它到达浏览器
        Ok(rows) => Json(json!({
            "items": rows.iter().map(|a| json!({
                "username": a.username,
                "role": a.role,
                "status": a.status,
                "mustChangePassword": a.must_change_password,
                "createdAt": a.created_at,
                "lastLoginAt": a.last_login_at,
                "lastLoginIp": a.last_login_ip,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("查询控制台账号", e),
    }
}

#[derive(Deserialize)]
pub struct CreateBody {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    role: String,
}

/// 用户名规则：ASCII 字母数字加 `.`/`-`/`_`，1~64 字符。
///
/// 收紧到 ASCII 是刻意的：库的 collation 大小写不敏感，而各处比较是精确匹配，
/// 放开非 ASCII 会引入 Unicode 归一化的坑（`ﬁ` 与 `fi`、全角与半角）。
fn valid_username(s: &str) -> bool {
    let n = s.chars().count();
    (1..=64).contains(&n)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub async fn create(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Json(body): Json<CreateBody>,
) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    let now = st.now();

    let username = body.username.trim().to_string();
    if !valid_username(&username) {
        return bad_request("用户名只能用字母、数字与 . - _，长度 1~64");
    }
    if !role::is_known(&body.role) {
        return bad_request("role 必须是 super / admin / viewer 之一");
    }
    if let Err(msg) = auth::check_password_strength(&body.password) {
        return bad_request(&msg);
    }

    // 新账号一律要求首次登录改密：建号的人知道初始口令，这不该是长期状态
    match st
        .store
        .create_admin(&username, &body.password, &body.role, true, now)
        .await
    {
        Ok(true) => {
            st.store
                .audit(
                    &session.username,
                    action::ADMIN_CREATE,
                    &username,
                    &format!("role={}", body.role),
                    &ip,
                    true,
                    now,
                )
                .await;
            Json(json!({ "ok": true, "mustChangePassword": true })).into_response()
        }
        Ok(false) => bad_request("这个用户名已存在"),
        Err(e) => store_err("创建控制台账号", e),
    }
}

pub async fn remove(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    let now = st.now();

    if username == session.username {
        return bad_request("不能删除自己");
    }
    // 删掉最后一个超管 = 谁也进不来了。这种自锁必须在这里挡住。
    if let Err(r) = ensure_not_last_super(&st, &username).await {
        return r;
    }

    match st.store.delete_admin(&username).await {
        Ok(true) => {
            st.store
                .audit(&session.username, action::ADMIN_DELETE, &username, "", &ip, true, now)
                .await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(false) => not_found("账号不存在"),
        Err(e) => store_err("删除控制台账号", e),
    }
}

#[derive(Deserialize)]
pub struct RoleBody {
    #[serde(default)]
    role: String,
}

pub async fn set_role(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
    Json(body): Json<RoleBody>,
) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    let now = st.now();

    if !role::is_known(&body.role) {
        return bad_request("role 必须是 super / admin / viewer 之一");
    }
    if username == session.username && body.role != role::SUPER {
        return bad_request("不能把自己降级——请让另一个超管来操作");
    }
    if body.role != role::SUPER {
        if let Err(r) = ensure_not_last_super(&st, &username).await {
            return r;
        }
    }

    match st.store.set_admin_role(&username, &body.role).await {
        Ok(true) => {
            st.store
                .audit(
                    &session.username,
                    action::ADMIN_SET_ROLE,
                    &username,
                    &format!("→ {}", body.role),
                    &ip,
                    true,
                    now,
                )
                .await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(false) => not_found("账号不存在"),
        Err(e) => store_err("修改角色", e),
    }
}

#[derive(Deserialize)]
pub struct StatusBody {
    /// `active` 或 `disabled`
    #[serde(default)]
    status: String,
}

/// 停用/启用控制台账号。
///
/// 注意这是**控制台自己的账号**——它是真实生效的（`verify_console_session`
/// 每次都联表查 `status`）。与之相对，终端用户的禁用需要主服务支持，
/// 那个开关在前端是禁用状态。两者别混为一谈。
pub async fn set_status(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
    Json(body): Json<StatusBody>,
) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    let now = st.now();

    if !matches!(body.status.as_str(), "active" | "disabled") {
        return bad_request("status 必须是 active 或 disabled");
    }
    if username == session.username && body.status == "disabled" {
        return bad_request("不能停用自己");
    }
    if body.status == "disabled" {
        if let Err(r) = ensure_not_last_super(&st, &username).await {
            return r;
        }
    }

    match st.store.set_admin_status(&username, &body.status).await {
        Ok(true) => {
            // 停用后立刻踢掉他的全部会话，否则要等会话自然过期才真正生效
            if body.status == "disabled" {
                if let Err(e) = st.store.delete_console_sessions_of(&username).await {
                    tracing::warn!("[admin] 停用后清理会话失败：{e}");
                }
            }
            st.store
                .audit(
                    &session.username,
                    action::ADMIN_SET_STATUS,
                    &username,
                    &format!("→ {}", body.status),
                    &ip,
                    true,
                    now,
                )
                .await;
            Json(json!({ "ok": true })).into_response()
        }
        Ok(false) => not_found("账号不存在"),
        Err(e) => store_err("修改账号状态", e),
    }
}

#[derive(Deserialize)]
pub struct ResetPasswordBody {
    #[serde(default)]
    password: String,
}

/// 超管重置别人的口令。重置后强制对方下次登录改密，并踢掉其全部会话。
pub async fn reset_password(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
    Json(body): Json<ResetPasswordBody>,
) -> Response {
    if let Err(r) = require_super(&session) {
        return r;
    }
    let now = st.now();

    if let Err(msg) = auth::check_password_strength(&body.password) {
        return bad_request(&msg);
    }
    match st.store.get_admin(&username).await {
        Ok(None) => return not_found("账号不存在"),
        Err(e) => return store_err("查询控制台账号", e),
        Ok(Some(_)) => {}
    }

    if let Err(e) = st.store.set_admin_password(&username, &body.password).await {
        return store_err("重置口令", e);
    }
    // set_admin_password 会清掉 must_change_password，这里再置回去：
    // 别人给你设的口令，你自己必须改一次
    if let Err(e) = sqlx::query("UPDATE console_admins SET must_change_password = 1 WHERE username = ?")
        .bind(&username)
        .execute(st.store.pool())
        .await
    {
        return store_err("标记强制改密", e);
    }
    if let Err(e) = st.store.delete_console_sessions_of(&username).await {
        tracing::warn!("[admin] 重置口令后清理会话失败：{e}");
    }

    st.store
        .audit(
            &session.username,
            action::ADMIN_RESET_PASSWORD,
            &username,
            "",
            &ip,
            true,
            now,
        )
        .await;
    Json(json!({ "ok": true, "mustChangePassword": true })).into_response()
}

/// 拦住「把最后一个超管删掉/降级/停用」这类自锁操作。
async fn ensure_not_last_super(st: &Arc<AppState>, target: &str) -> Result<(), Response> {
    let admins = match st.store.list_admins().await {
        Ok(v) => v,
        Err(e) => return Err(store_err("查询控制台账号", e)),
    };
    let target_is_super = admins
        .iter()
        .any(|a| a.username == target && a.role == role::SUPER);
    if !target_is_super {
        return Ok(());
    }
    let active_supers = admins
        .iter()
        .filter(|a| a.role == role::SUPER && a.status == "active")
        .count();
    if active_supers <= 1 {
        return Err(bad_request(
            "这是最后一个可用的超级管理员，删除/降级/停用后将没有人能再进入控制台",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_rules() {
        assert!(valid_username("ops"));
        assert!(valid_username("ops-01"));
        assert!(valid_username("ops.admin_2"));
        assert!(!valid_username(""), "空不行");
        assert!(!valid_username("有中文"), "非 ASCII 不行——库的大小写规则会带来歧义");
        assert!(!valid_username("with space"));
        assert!(!valid_username("drop;table"));
        assert!(!valid_username(&"a".repeat(65)), "超长不行");
        assert!(valid_username(&"a".repeat(64)));
    }
}
