//! 审计日志查询。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::{store_err, AppState};
use crate::store::Page;

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "fifty")]
    size: i64,
}

fn one() -> i64 {
    1
}
fn fifty() -> i64 {
    50
}

pub async fn list(State(st): State<Arc<AppState>>, Query(q): Query<AuditQuery>) -> Response {
    let actor = q.actor.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let action = q.action.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let page = Page::new(q.page, q.size);

    match st.store.list_audit(actor, action, page).await {
        Ok((rows, total)) => Json(json!({
            "total": total,
            "page": q.page.max(1),
            "size": page.size,
            "note": "控制台的每一个写操作都会留痕，包括失败的尝试（ok=false）。这里只记录控制台自身的操作——主服务侧目前没有审计日志。",
            // IP 列很容易被当成真实来访地址。在 frp 纯 TCP 透传这类部署下它其实是回环地址，
            // 不说清楚就是拿一个假的东西冒充溯源信息。
            "ipNote": if st.config.trust_proxy {
                "来源 IP 取自 X-Forwarded-For（已配置信任反向代理）。"
            } else {
                "来源 IP 是 TCP 对端地址。若控制台位于 frp 纯 TCP 透传或反向代理之后，这里会恒为回环/代理地址，不是访客的真实 IP——要真实 IP 需要前置层注入 X-Forwarded-For 并开启 CONSOLE_TRUST_PROXY。"
            },
            "items": rows.iter().map(|a| json!({
                "id": a.id,
                "at": a.at,
                "actor": a.actor,
                "action": a.action,
                "target": a.target,
                "detail": a.detail,
                "ip": a.ip,
                "ok": a.ok,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("查询审计日志", e),
    }
}
