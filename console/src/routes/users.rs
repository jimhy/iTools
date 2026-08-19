//! 终端用户（iTools 云账号）管理。
//!
//! 「禁用账号」**不在这里**——主服务的 `users` 表没有禁用位、登录与鉴权也不读任何
//! 这样的列，控制台单方面加一列改一改是彻底无效的。前端对应的开关是禁用状态并写明
//! 原因，`/api/whoami` 的 `capabilities.usersStatusColumn` 就是那个开关的依据。
//! 等主服务补上禁用位与拦截逻辑，这里再加端点。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, ClientIp, not_found, require_write, store_err};
use crate::store::admins::ConsoleSession;
use crate::store::audit::action;
use crate::store::users::UserSort;
use crate::store::Page;

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_size")]
    size: i64,
    #[serde(default)]
    sort: Option<String>,
    /// `desc`（默认）或 `asc`
    #[serde(default)]
    order: Option<String>,
}

fn default_page() -> i64 {
    1
}
fn default_size() -> i64 {
    30
}

pub async fn list(State(st): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    let query = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let sort = UserSort::parse(q.sort.as_deref().unwrap_or_default());
    // 默认倒序：运营最关心最新注册的
    let desc = !matches!(q.order.as_deref(), Some("asc"));
    let page = Page::new(q.page, q.size);

    match st.store.list_users(query, sort, desc, page).await {
        Ok((rows, total)) => Json(json!({
            "total": total,
            "page": q.page.max(1),
            "size": page.size,
            "items": rows.iter().map(|u| json!({
                "username": u.username,
                "createdAt": u.created_at,
                "sessionCount": u.session_count,
                "lastSessionAt": u.last_session_at,
                "recordCount": u.record_count,
                "bytes": u.bytes,
                "pluginCount": u.plugin_count,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("查询用户列表", e),
    }
}

pub async fn detail(State(st): State<Arc<AppState>>, Path(username): Path<String>) -> Response {
    let row = match st.store.get_user_row(&username).await {
        Ok(Some(r)) => r,
        Ok(None) => return not_found("用户不存在"),
        Err(e) => return store_err("查询用户", e),
    };
    let ns = match st.store.user_ns_usage(&username).await {
        Ok(v) => v,
        Err(e) => return store_err("查询用户用量", e),
    };
    let sessions = match st.store.user_sessions(&username).await {
        Ok(v) => v,
        Err(e) => return store_err("查询用户会话", e),
    };
    // 该用户名下的插件（含已下架）
    let plugins = match st
        .store
        .list_market(Some(&username), true, Page::new(1, 200))
        .await
    {
        Ok((rows, _)) => rows
            .into_iter()
            .filter(|p| p.owner == username)
            .collect::<Vec<_>>(),
        Err(e) => return store_err("查询用户插件", e),
    };

    Json(json!({
        "username": row.username,
        "createdAt": row.created_at,
        "sessionCount": row.session_count,
        "lastSessionAt": row.last_session_at,
        "recordCount": row.record_count,
        "bytes": row.bytes,
        "namespaces": ns.iter().map(|n| json!({
            "ns": n.ns,
            "count": n.count,
            "bytes": n.bytes,
            "lastUpdatedAt": n.last_updated_at,
        })).collect::<Vec<_>>(),
        "sessions": sessions.iter().map(|s| json!({ "createdAt": s.created_at })).collect::<Vec<_>>(),
        "plugins": plugins.iter().map(|p| json!({
            "name": p.name,
            "version": p.version,
            "revoked": p.revoked,
            "publishedAt": p.published_at,
        })).collect::<Vec<_>>(),
        // 这两句话会原样显示在详情页上，避免运营把数字理解成别的意思
        "notes": {
            "sessions": "主服务的会话永不过期、登出即删行，所以这里是「当前存在的会话」，既不等于在线设备数，也不等于历史登录次数。",
            "storage": "只统计条数与字节数；用户的同步内容本身控制台不读取、不展示。",
        }
    }))
    .into_response()
}

/// 强制下线：删掉该用户在主服务的全部会话。
///
/// 这是**真实生效**的——主服务每个受保护端点都实时查 `sessions` 表，
/// 行删掉后对方下一个请求就是 401。但它**不阻止重新登录**，
/// 返回体里的 `note` 会把这一点如实告诉运营。
pub async fn kick(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
) -> Response {
    if let Err(r) = require_write(&session) {
        return r;
    }
    let now = st.now();

    match st.store.user_exists(&username).await {
        Ok(false) => return not_found("用户不存在"),
        Err(e) => return store_err("查询用户", e),
        Ok(true) => {}
    }

    match st.store.kick_user(&username).await {
        Ok(n) => {
            st.store
                .audit(
                    &session.username,
                    action::USER_KICK,
                    &username,
                    &format!("删除 {n} 条会话"),
                    &ip,
                    true,
                    now,
                )
                .await;
            Json(json!({
                "ok": true,
                "removed": n,
                "note": "已使该账号的全部登录态立即失效。注意：这不阻止对方用原口令重新登录——真正的封停需要服务端的禁用位支持。"
            }))
            .into_response()
        }
        Err(e) => {
            st.store
                .audit(
                    &session.username,
                    action::USER_KICK,
                    &username,
                    "数据库错误",
                    &ip,
                    false,
                    now,
                )
                .await;
            store_err("强制下线", e)
        }
    }
}

#[derive(Deserialize)]
pub struct DeleteQuery {
    /// 必须原样回填用户名才执行，防手滑。
    #[serde(default)]
    confirm: Option<String>,
}

/// 删除终端用户（同步数据 + 会话 + 账号，单事务）。
///
/// 删除范围与主服务的 `/account/delete` 完全一致：**不动**该用户已上线的插件，
/// 那是已经分发给其他人的公共资产。
pub async fn remove(
    State(st): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(session): Extension<ConsoleSession>,
    Path(username): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    if let Err(r) = require_write(&session) {
        return r;
    }
    let now = st.now();

    if q.confirm.as_deref() != Some(username.as_str()) {
        return super::bad_request("请在 confirm 参数里原样回填用户名以确认删除");
    }

    match st.store.delete_user(&username).await {
        Ok(true) => {
            st.store
                .audit(&session.username, action::USER_DELETE, &username, "", &ip, true, now)
                .await;
            Json(json!({
                "ok": true,
                "note": "账号、会话与同步数据已删除。该用户已上线的插件保持原状（与主服务销号行为一致），如需处置请单独下架。"
            }))
            .into_response()
        }
        Ok(false) => not_found("用户不存在"),
        Err(e) => {
            st.store
                .audit(
                    &session.username,
                    action::USER_DELETE,
                    &username,
                    "数据库错误",
                    &ip,
                    false,
                    now,
                )
                .await;
            store_err("删除用户", e)
        }
    }
}
