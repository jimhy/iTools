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
        "unavailable": unavailable_metrics(st.store.caps()),
        "generatedAt": now,
    }))
    .into_response()
}

/// 仍然**无法提供**的指标。逐条列出原因与接入条件。
///
/// 前端拿这个渲染「未采集」占位块。这个列表是**跟着服务端能力动态缩短**的：
/// 主服务补上指标采集后，请求量/带宽/耗时/下载量会从这里消失、变成真实曲线。
/// 剩下的是那些结构上就拿不到的东西。
fn unavailable_metrics(caps: crate::store::Caps) -> serde_json::Value {
    let mut list = Vec::new();

    if !caps.traffic {
        list.push(json!({
            "key": "http_requests",
            "label": "HTTP 请求量 / QPS",
            "reason": "云同步服务端还没有请求指标表（traffic_hourly 不存在）。",
            "unblockedBy": "把服务端更新到带指标采集的版本（SYNC_METRICS=true）。",
        }));
        list.push(json!({
            "key": "bandwidth",
            "label": "出入带宽",
            "reason": "同上——服务端未统计请求/响应体积。",
            "unblockedBy": "同上。",
        }));
        list.push(json!({
            "key": "latency",
            "label": "响应耗时",
            "reason": "同上——服务端没有任何计时。",
            "unblockedBy": "同上。",
        }));
        list.push(json!({
            "key": "plugin_downloads",
            "label": "插件下载量",
            "reason": "同上——下载端点没有计数。",
            "unblockedBy": "同上。",
        }));
    }

    // 这两条与服务端版本无关，是结构性缺失
    list.push(json!({
        "key": "login_history",
        "label": "历史登录次数",
        "reason": "主服务在用户登出时会删掉会话行，登录事件本身没有留痕。",
        "unblockedBy": "需要主服务把登录记成事件（或保留会话历史）。",
    }));
    list.push(json!({
        "key": "unique_visitors",
        "label": "独立访客 / IP 维度",
        "reason": "生产走 frp 纯 TCP 透传，服务端看到的对端恒为回环地址，拿不到真实客户端 IP；指标本身也刻意不记 IP。",
        "unblockedBy": "需要 frp 改 HTTP 模式或前置反代注入 X-Forwarded-For。",
    }));

    json!(list)
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

// ---------- 流量（数据源：主服务的 traffic_hourly / plugin_downloads_hourly）----------

#[derive(Deserialize)]
pub struct TrafficQuery {
    /// `hour`（默认）或 `day`
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default = "default_traffic_points")]
    points: i64,
}

fn default_traffic_points() -> i64 {
    48
}

/// 流量时间序列。
///
/// # 「没有数据」与「没有流量」必须分开
///
/// 主服务是某次发版之后才开始采集的。在那之前的小时桶里一行都没有，但那**不代表**
/// 那段时间没有请求——它代表没人记录。把这两种情况都画成 0 是最典型的编数据。
///
/// 所以每个点都带一个 `covered`：落在采集范围之外的点 `covered=false` 且不给数值，
/// 前端据此把那一段画成断线而不是贴地的零线。
pub async fn traffic(State(st): State<Arc<AppState>>, Query(q): Query<TrafficQuery>) -> Response {
    let caps = st.store.caps();
    if !caps.traffic {
        return Json(json!({
            "available": false,
            "reason": "云同步服务端还没有请求指标表（traffic_hourly 不存在）。请把服务端更新到带指标采集的版本。",
            "points": [],
        }))
        .into_response();
    }

    let now = st.now();
    let tz = st.config.tz_offset_min;
    let daily = q.bucket.as_deref() == Some("day");
    let bucket_sec = if daily { DAY } else { HOUR };
    let points = q.points.clamp(1, 400);

    let rows = match st.store.traffic_series(bucket_sec, points, now, tz).await {
        Ok(v) => v,
        Err(e) => return store_err("统计流量序列", e),
    };
    let coverage = st.store.traffic_coverage().await.unwrap_or(None);

    let series = fill_traffic(&rows, bucket_sec, points, now / 1000, tz, coverage);
    let covered_points = series.iter().filter(|p| p["covered"] == json!(true)).count();

    Json(json!({
        "available": true,
        "bucket": if daily { "day" } else { "hour" },
        "bucketSec": bucket_sec,
        "tzOffsetMin": tz,
        "coverageFrom": coverage.map(|(lo, _)| lo),
        "coverageTo": coverage.map(|(_, hi)| hi),
        "coveredPoints": covered_points,
        "note": "服务端按小时聚合后落库，最细粒度就是小时。标记为未采集的时段是「服务端那时还没开始记录」，不是「那时没有流量」——所以那一段没有数值，也不会画成 0。",
        "flushNote": "最近一个尚未落库的窗口（默认 1 分钟）不在图里；进程重启会丢掉那个窗口。",
        "points": series,
    }))
    .into_response()
}

/// 把稀疏的流量行补齐成连续序列。
///
/// 采集范围之内缺的桶补 0（那个小时确实没有请求）；采集范围之外的桶标 `covered=false`
/// 且不带任何数值——这是本函数存在的全部意义。
fn fill_traffic(
    rows: &[crate::store::stats::TrafficPoint],
    bucket_sec: i64,
    points: i64,
    now_sec: i64,
    tz_offset_min: i64,
    coverage: Option<(i64, i64)>,
) -> Vec<serde_json::Value> {
    let offset_sec = tz_offset_min * 60;
    let current = ((now_sec + offset_sec) / bucket_sec) * bucket_sec - offset_sec;
    let start = current - (points - 1) * bucket_sec;
    // 采集起点所在的桶。没有任何数据时视为「全都没采集」。
    let covered_from = coverage.map(|(lo, _)| ((lo + offset_sec) / bucket_sec) * bucket_sec - offset_sec);

    let mut out = Vec::with_capacity(points as usize);
    for i in 0..points {
        let b = start + i * bucket_sec;
        let covered = match covered_from {
            Some(from) => b >= from,
            None => false,
        };
        if !covered {
            out.push(json!({ "t": b, "covered": false }));
            continue;
        }
        let found = rows.iter().find(|r| r.bucket == b);
        let (reqs, errs, server_errs, bytes_in, bytes_out, dur_sum, dur_max) = match found {
            Some(r) => (
                r.reqs,
                r.errs,
                r.server_errs,
                r.bytes_in,
                r.bytes_out,
                r.dur_sum_ms,
                r.dur_max_ms,
            ),
            None => (0, 0, 0, 0, 0, 0, 0),
        };
        out.push(json!({
            "t": b,
            "covered": true,
            "reqs": reqs,
            "errs": errs,
            "serverErrs": server_errs,
            "bytesIn": bytes_in,
            "bytesOut": bytes_out,
            // 平均耗时是精确的（总和 / 请求数），不是估算的百分位
            "avgMs": if reqs > 0 { dur_sum as f64 / reqs as f64 } else { 0.0 },
            "maxMs": dur_max,
        }));
    }
    out
}

#[derive(Deserialize)]
pub struct RoutesQuery {
    #[serde(default = "default_hours")]
    hours: i64,
    #[serde(default = "default_route_limit")]
    limit: i64,
}

fn default_hours() -> i64 {
    24
}
fn default_route_limit() -> i64 {
    20
}

/// 按路由汇总 + 全局状态码与延迟分布。
pub async fn traffic_routes(
    State(st): State<Arc<AppState>>,
    Query(q): Query<RoutesQuery>,
) -> Response {
    if !st.store.caps().traffic {
        return Json(json!({ "available": false, "items": [] })).into_response();
    }
    let now = st.now();

    let routes = match st.store.traffic_routes(q.hours, q.limit, now).await {
        Ok(v) => v,
        Err(e) => return store_err("统计路由流量", e),
    };
    let status_mix = st.store.traffic_status_mix(q.hours, now).await.unwrap_or_default();
    let latency = st.store.traffic_latency_mix(q.hours, now).await.unwrap_or([0; 4]);

    Json(json!({
        "available": true,
        "hours": q.hours,
        "note": "路由是**模板**不是具体路径（如 /api/market/package/{name}）——用具体路径会让统计维度随插件数无限增长。/download/* 与官网静态站同理归一。",
        "latencyNote": "延迟给的是四个桶的精确计数与平均值、最大值。刻意不给 P95：从桶里插值出来的百分位是估算，而这几个数是精确的。",
        "statusMix": status_mix.iter().map(|(c, n)| json!({
            "class": c,
            "label": match c { 2 => "2xx 成功", 3 => "3xx 重定向", 4 => "4xx 客户端错误", 5 => "5xx 服务端错误", _ => "其它" },
            "reqs": n,
        })).collect::<Vec<_>>(),
        "latency": {
            "fast": latency[0],
            "mid": latency[1],
            "slow": latency[2],
            "verySlow": latency[3],
            "labels": ["< 50ms", "50ms ~ 200ms", "200ms ~ 1s", "≥ 1s"],
        },
        "items": routes.iter().map(|r| json!({
            "route": r.route,
            "method": r.method,
            "reqs": r.reqs,
            "errs": r.errs,
            "serverErrs": r.server_errs,
            "bytesOut": r.bytes_out,
            "avgMs": if r.reqs > 0 { r.dur_sum_ms as f64 / r.reqs as f64 } else { 0.0 },
            "maxMs": r.dur_max_ms,
            "latency": { "fast": r.b_fast, "mid": r.b_mid, "slow": r.b_slow, "verySlow": r.b_vslow },
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct DownloadsQuery {
    #[serde(default = "default_days")]
    days: i64,
    #[serde(default = "default_route_limit")]
    limit: i64,
}

fn default_days() -> i64 {
    30
}

/// 插件下载量排行。
pub async fn downloads(State(st): State<Arc<AppState>>, Query(q): Query<DownloadsQuery>) -> Response {
    if !st.store.caps().traffic {
        return Json(json!({ "available": false, "items": [] })).into_response();
    }
    match st.store.plugin_downloads(q.days, q.limit, st.now()).await {
        Ok(rows) => Json(json!({
            "available": true,
            "days": q.days,
            "note": "只统计真正把包读出来返回的那些请求——404 与读盘失败不计入。",
            "items": rows.iter().map(|(name, n)| json!({ "name": name, "downloads": n })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => store_err("统计插件下载量", e),
    }
}
