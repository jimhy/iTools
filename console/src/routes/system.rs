//! 系统状态：控制台自身信息、主服务健康探测、数据库连通性。
//!
//! 这一页的每个绿灯都必须来自一次**真实探测**。探不到就显示「未知」，
//! 绝不默认成健康——一个假绿灯比没有这一页更糟。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use super::AppState;

/// 公开的能力清单。登录页就要用，所以不需要令牌。
///
/// 只返回布尔开关与版本号，**不含任何业务数据**，也不泄露配置细节
/// （数据库地址、证书路径、上游地址一个都不给）。
pub async fn meta(State(st): State<Arc<AppState>>) -> Response {
    let caps = st.store.caps();
    Json(json!({
        "name": "iTools 运营控制台",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {
            // 终端用户禁用位：主服务补丁落地前恒为 false，前端据此禁用开关并写明原因
            "userDisable": caps.users_status,
            "marketRevokedBy": caps.market_revoked_by,
            // 第一版对插件与提审单只读
            "pluginWrite": false,
            "submissionReview": false,
            // 主服务尚无采集，流量面板显示「未采集」
            "httpMetrics": false,
        },
    }))
    .into_response()
}

pub async fn info(State(st): State<Arc<AppState>>) -> Response {
    let now = st.now();

    // 数据库：真发一条查询，不看连接池状态就下结论
    let db_started = Instant::now();
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(st.store.pool())
        .await
        .is_ok();
    let db_ms = db_started.elapsed().as_millis() as i64;

    let missing_tables = st.store.verify_upstream_tables().await.unwrap_or_default();

    let upstream = probe_upstream(&st).await;

    Json(json!({
        "console": {
            "version": env!("CARGO_PKG_VERSION"),
            "port": st.config.port,
            "tls": st.config.tls.is_some(),
            "sessionTtlSec": st.config.session_ttl_sec,
            "tzOffsetMin": st.config.tz_offset_min,
            "trustProxy": st.config.trust_proxy,
            "loginRateMax": st.config.login_rate_max,
            "loginRateWindowSec": st.config.login_rate_window_sec,
            "webDir": st.config.web_dir.is_some(),
            "serverTime": now,
        },
        "database": {
            "ok": db_ok,
            "latencyMs": db_ms,
            // 只给脱敏形态，口令永远不出这一层
            "target": st.config.db.redacted(),
            "missingUpstreamTables": missing_tables,
            "caps": {
                "usersStatus": st.store.caps().users_status,
                "marketRevokedBy": st.store.caps().market_revoked_by,
            },
        },
        "upstream": upstream,
        "limits": {
            "loginBuckets": st.login_limiter.size(),
            "loginLimiterEnabled": st.login_limiter.enabled(),
        },
    }))
    .into_response()
}

/// 探测云同步主服务的 `/health`。
///
/// 三种结果严格区分，因为运维要据此判断该查哪儿：
/// - `configured: false` —— 压根没配探测地址，不是「主服务挂了」
/// - `ok: false` + `error` —— 真探了，失败了
/// - `ok: true` —— 真探通了
async fn probe_upstream(st: &Arc<AppState>) -> serde_json::Value {
    let Some(url) = &st.config.upstream_health_url else {
        return json!({
            "configured": false,
            "note": "未配置 CONSOLE_UPSTREAM_HEALTH_URL，无法判断云同步服务端状态（这不代表它有问题）。",
        });
    };
    let Some(client) = &st.http else {
        return json!({
            "configured": true,
            "ok": false,
            "error": "HTTP 探测器初始化失败，无法探测",
        });
    };

    let started = Instant::now();
    let res = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    let ms = started.elapsed().as_millis() as i64;

    match res {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            // 主服务 /health 返回 {ok, service, allowRegister}
            let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
            json!({
                "configured": true,
                "ok": (200..300).contains(&status),
                "status": status,
                "latencyMs": ms,
                "service": parsed.as_ref().and_then(|v| v.get("service").cloned()),
                "allowRegister": parsed.as_ref().and_then(|v| v.get("allowRegister").cloned()),
            })
        }
        Err(e) => json!({
            "configured": true,
            "ok": false,
            "latencyMs": ms,
            // reqwest 的错误串里会带完整 URL；地址属于运维拓扑，不回显到浏览器
            "error": classify_probe_error(&e),
        }),
    }
}

/// 把探测错误归类成一句不含地址的说明。
fn classify_probe_error(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "探测超时（5 秒内没有响应）"
    } else if e.is_connect() {
        "连接失败（服务未监听、端口不通或 TLS 握手失败）"
    } else if e.is_request() {
        "请求构造失败（探测地址可能写错了）"
    } else {
        "探测失败"
    }
}
