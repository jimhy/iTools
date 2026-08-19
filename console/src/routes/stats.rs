//! 统计接口。
//!
//! 每条曲线都带一段 `note`，把「这个数字到底是什么」写清楚。这不是啰嗦：
//! `sessions` 这类指标看着像「登录量」，实际是「当前还存在的会话」，
//! 不标注就是拿一个近似值冒充另一个指标。
//!
//! 服务端尚未采集的指标（HTTP 请求量、带宽、响应耗时、插件下载次数）
//! 在 `unavailable` 里逐条列出，前端照着显示「未采集」，不画任何估算曲线。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::{store_err, AppState};
use crate::store::stats::{fill_buckets, Bucket};

const DAY: i64 = 86_400;
const HOUR: i64 = 3_600;

pub async fn overview(State(st): State<Arc<AppState>>) -> Response {
    let now = st.now();
    let o = match st.store.overview(now).await {
        Ok(v) => v,
        Err(e) => return store_err("统计概览", e),
    };
    let submissions = match st.store.submission_status_counts().await {
        Ok(v) => v,
        Err(e) => return store_err("统计提审单", e),
    };
    let console_sessions = st
        .store
        .count_active_console_sessions(now)
        .await
        .unwrap_or(-1);

    Json(json!({
        "users": {
            "total": o.users_total,
            "new1d": o.users_new_1d,
            "new7d": o.users_new_7d,
            "new30d": o.users_new_30d,
        },
        "sessions": {
            "total": o.sessions_total,
            "new1d": o.sessions_new_1d,
            "note": "主服务的会话永不过期、登出即删行。这是「当前存在的会话数」，不是在线人数。",
        },
        "data": {
            "records": o.records_total,
            "bytes": o.bytes_total,
            "updated1d": o.records_updated_1d,
            "note": "data_records 主键是 (username, ns, k)，重复写入只更新时间戳，所以「近一天更新」是记录数不是写入次数。",
        },
        "market": {
            "total": o.market_total,
            "revoked": o.market_revoked,
            "online": o.market_total - o.market_revoked,
        },
        "submissions": {
            "total": o.submissions_total,
            "byStatus": submissions.iter().map(|s| json!({
                "status": s.status,
                "count": s.count,
            })).collect::<Vec<_>>(),
        },
        "storage": {
            // -1 表示 information_schema 读不到（权限问题），前端据此显示「读不到」
            // 而不是显示 0 让人以为库是空的
            "dbBytes": o.db_bytes,
            "note": if o.db_bytes < 0 {
                "读不到 information_schema，无法给出库占用（通常是数据库账号权限不足）。"
            } else {
                "来自 information_schema 的估算值，InnoDB 下本身就是近似数。"
            },
        },
        "console": {
            "activeSessions": console_sessions,
        },
        "unavailable": unavailable_metrics(),
        "generatedAt": now,
    }))
    .into_response()
}

/// 服务端尚未采集、因而**无法提供**的指标。逐条列出去向与原因。
///
/// 前端拿这个渲染「未采集」占位块。诚实标注比留白更重要——留白会被理解成
/// 「这段时间没有流量」，而事实是「这个东西从来就没被记录过」。
fn unavailable_metrics() -> serde_json::Value {
    json!([
        {
            "key": "http_requests",
            "label": "HTTP 请求量 / QPS",
            "reason": "云同步服务端没有请求计数中间件，数据库里也没有任何请求记录。",
            "unblockedBy": "需要给主服务加请求指标采集并按小时落库。",
        },
        {
            "key": "bandwidth",
            "label": "出入带宽",
            "reason": "服务端未统计请求/响应体积；插件包与安装包走静态文件服务，字节数无处可取。",
            "unblockedBy": "同上，需要主服务在中间件里累计字节数。",
        },
        {
            "key": "latency",
            "label": "响应耗时 / P95",
            "reason": "服务端没有任何计时。",
            "unblockedBy": "同上。",
        },
        {
            "key": "plugin_downloads",
            "label": "插件下载量",
            "reason": "下载端点直接读盘返回，既没有计数列也没有下载日志。",
            "unblockedBy": "需要主服务在下载路径上累计计数。",
        },
        {
            "key": "login_history",
            "label": "历史登录次数",
            "reason": "主服务在用户登出时会删掉会话行，登录事件本身没有留痕。",
            "unblockedBy": "需要主服务记录登录事件（或保留会话历史）。",
        }
    ])
}

#[derive(Deserialize)]
pub struct SeriesQuery {
    /// `users` | `sessions` | `records` | `market` | `submissions`
    #[serde(default)]
    metric: String,
    /// `day`（默认）或 `hour`
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default = "default_points")]
    points: i64,
}

fn default_points() -> i64 {
    30
}

/// 时间序列。一次只返回一条曲线，前端按需并发拉取。
pub async fn series(State(st): State<Arc<AppState>>, Query(q): Query<SeriesQuery>) -> Response {
    let now = st.now();
    let tz = st.config.tz_offset_min;
    let hourly = q.bucket.as_deref() == Some("hour");
    let bucket_sec = if hourly { HOUR } else { DAY };
    let points = q.points.clamp(1, 400);

    let (rows, note): (Result<Vec<Bucket>, sqlx::Error>, &str) = match q.metric.as_str() {
        "users" => (
            st.store.users_by_day(points, now, tz).await,
            "按注册时间统计的新增账号数。",
        ),
        "sessions" => {
            let r = if hourly {
                st.store.sessions_by_hour(points, now, tz).await
            } else {
                st.store.sessions_by_day(points, now, tz).await
            };
            (
                r,
                "当前仍然存在的会话是什么时候创建的。用户登出会删掉会话行，所以这不等于历史登录次数。",
            )
        }
        "records" => (
            st.store.records_by_day(points, now, tz).await,
            "同步记录按最后更新时间分布。同一条记录被反复覆盖只更新时间戳，所以这不等于写入次数。",
        ),
        "market" => (
            st.store.market_by_day(points, now, tz).await,
            "插件首次上线时间分布。同名插件发新版本会覆盖同一行，只保留最新的上线时间。",
        ),
        "submissions" => (
            st.store.submissions_by_day(points, now, tz).await,
            "提审量。这张表一次提审一行、永不覆盖，是本库里唯一真正意义上的事件流，趋势最可信。",
        ),
        other => {
            return super::bad_request(&format!(
                "未知指标 `{other}`，支持：users / sessions / records / market / submissions"
            ))
        }
    };

    let rows = match rows {
        Ok(v) => v,
        Err(e) => return store_err("统计时间序列", e),
    };
    // 补齐成连续序列。缺的桶填 0——那一天真的没有新增，不是数据缺失。
    let filled = fill_buckets(&rows, bucket_sec, points, now / 1000, tz);

    Json(json!({
        "metric": q.metric,
        "bucket": if hourly { "hour" } else { "day" },
        "bucketSec": bucket_sec,
        "tzOffsetMin": tz,
        "note": note,
        "points": filled.iter().map(|b| json!({
            "t": b.bucket,
            "v": b.count,
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct StorageQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    20
}

pub async fn storage(State(st): State<Arc<AppState>>, Query(q): Query<StorageQuery>) -> Response {
    match st.store.storage_ranking(q.limit).await {
        Ok(rows) => Json(json!({
            "note": "按同步数据字节数排行。只统计 LENGTH(v)，不读取任何内容。",
            "items": rows.iter().map(|r| json!({
                "username": r.username,
                "count": r.count,
                "bytes": r.bytes,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("统计存储排行", e),
    }
}

pub async fn namespaces(State(st): State<Arc<AppState>>) -> Response {
    match st.store.ns_distribution().await {
        Ok(rows) => Json(json!({
            "note": "命名空间（ns）是客户端各功能模块的数据分区，这里给出全局分布。",
            "items": rows.iter().map(|r| json!({
                "ns": r.ns,
                "users": r.users,
                "count": r.count,
                "bytes": r.bytes,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("统计命名空间分布", e),
    }
}
