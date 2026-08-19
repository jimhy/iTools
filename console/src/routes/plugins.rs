//! 插件市场与提审单查询。**全部只读**，原因见 `store::plugins` 的模块文档。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::{not_found, store_err, AppState};
use crate::store::Page;

/// 提审单的合法状态值。前端传别的一律当没传（不筛选），
/// 而不是拿去查库返回空列表让人以为「真的一条都没有」。
const SUBMISSION_STATUSES: [&str; 5] = ["reviewing", "approved", "rejected", "manual", "failed"];

#[derive(Deserialize)]
pub struct MarketQuery {
    #[serde(default)]
    q: Option<String>,
    /// 是否包含已下架条目，默认包含（运营需要看到全貌）
    #[serde(default = "yes")]
    include_revoked: bool,
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "thirty")]
    size: i64,
}

fn yes() -> bool {
    true
}
fn one() -> i64 {
    1
}
fn thirty() -> i64 {
    30
}

pub async fn list_market(State(st): State<Arc<AppState>>, Query(q): Query<MarketQuery>) -> Response {
    let query = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let page = Page::new(q.page, q.size);
    match st.store.list_market(query, q.include_revoked, page).await {
        Ok((rows, total)) => Json(json!({
            "total": total,
            "page": q.page.max(1),
            "size": page.size,
            "capabilities": {
                // 主服务旧版本没有 revoked_by 列时，前端不该把空串显示成「作者下架」
                "revokedBy": st.store.caps().market_revoked_by,
                // 第一版没有任何写操作，前端据此禁用下架/改判按钮并写明原因
                "canRevoke": false,
            },
            "items": rows.iter().map(|p| json!({
                "name": p.name,
                "title": p.title,
                "description": p.description,
                "owner": p.owner,
                "version": p.version,
                "contentHash": p.content_hash,
                "packageFile": p.package_file,
                "revoked": p.revoked,
                "revokedReason": p.revoked_reason,
                "revokedBy": p.revoked_by,
                "publishedAt": p.published_at,
                "updatedAt": p.updated_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("查询市场条目", e),
    }
}

pub async fn market_detail(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let entry = match st.store.get_market_entry(&name).await {
        Ok(Some(v)) => v,
        Ok(None) => return not_found("市场里没有这个插件"),
        Err(e) => return store_err("查询市场条目", e),
    };
    // 这个插件的历次提审记录
    let history = match st
        .store
        .list_submissions(None, Some(&name), Page::new(1, 50))
        .await
    {
        Ok((rows, _)) => rows.into_iter().filter(|s| s.name == name).collect::<Vec<_>>(),
        Err(e) => return store_err("查询提审历史", e),
    };

    Json(json!({
        "name": entry.name,
        "title": entry.title,
        "description": entry.description,
        "owner": entry.owner,
        "version": entry.version,
        "contentHash": entry.content_hash,
        "packageFile": entry.package_file,
        "revoked": entry.revoked,
        "revokedReason": entry.revoked_reason,
        "revokedBy": entry.revoked_by,
        "publishedAt": entry.published_at,
        "updatedAt": entry.updated_at,
        "submissions": history.iter().map(|s| json!({
            "id": s.id,
            "version": s.version,
            "status": s.status,
            "message": s.message,
            "createdAt": s.created_at,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SubmissionQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default = "one")]
    page: i64,
    #[serde(default = "thirty")]
    size: i64,
}

pub async fn list_submissions(
    State(st): State<Arc<AppState>>,
    Query(q): Query<SubmissionQuery>,
) -> Response {
    let status = q
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| SUBMISSION_STATUSES.contains(s));
    let query = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let page = Page::new(q.page, q.size);

    match st.store.list_submissions(status, query, page).await {
        Ok((rows, total)) => Json(json!({
            "total": total,
            "page": q.page.max(1),
            "size": page.size,
            "statuses": SUBMISSION_STATUSES,
            "capabilities": {
                // `manual`（待人工处理）在主服务里目前是死胡同：全仓没有任何端点能改判。
                // 前端必须把这句话显示出来，不能放一个点了没反应的按钮。
                "canReview": false,
                "manualNote": "主服务当前没有人工改判入口，`manual` 状态的提审单只能请作者重新提交。",
            },
            "items": rows.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "version": s.version,
                "author": s.author,
                "status": s.status,
                "fileCount": s.file_count,
                "sizeBytes": s.size_bytes,
                "message": s.message,
                "createdAt": s.created_at,
                "updatedAt": s.updated_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("查询提审单", e),
    }
}

pub async fn submission_detail(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match st.store.get_submission(&id).await {
        Ok(Some(d)) => Json(json!({
            "id": d.row.id,
            "name": d.row.name,
            "version": d.row.version,
            "author": d.row.author,
            "status": d.row.status,
            "contentHash": d.row.content_hash,
            "fileCount": d.row.file_count,
            "sizeBytes": d.row.size_bytes,
            "message": d.row.message,
            "createdAt": d.row.created_at,
            "updatedAt": d.row.updated_at,
            "manifest": d.manifest,
            "review": d.review,
        }))
        .into_response(),
        Ok(None) => not_found("提审单不存在"),
        Err(e) => store_err("查询提审单", e),
    }
}
