//! iTools 云同步服务端（Rust 实现）。
//!
//! 模块划分与旧 Node 版一一对应，便于对照审查：
//! - [`config`]    环境变量配置（含 MariaDB 连接、镜像探测参数）
//! - [`auth`]      口令哈希（scrypt）/ 会话令牌
//! - [`store`]     MariaDB 存储（sqlx 连接池）
//! - [`mirrors`]   镜像源配置装载（热更新）+ 健康探测 + 快照/ETag
//! - [`ratelimit`] 滑动窗口按 IP 限流（供免认证的 `/api/mirrors`）
//! - [`proxy`]     `X-Forwarded-For` 取真实客户端 IP（限流按谁计数）
//! - [`routes`]    axum 路由（全部 REST 契约）
//!
//! 库形态导出是为了让 `tests/` 下的集成测试能直接构造 Router 打端点
//! （等价于旧版的 `fastify.inject()`），不必真的监听端口。

pub mod auth;
pub mod config;
pub mod mirrors;
pub mod proxy;
pub mod ratelimit;
pub mod routes;
pub mod store;

use std::sync::Arc;

/// 可注入的时钟：返回 Unix 毫秒时间戳。测试里换成固定/可推进的假时钟。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 默认时钟：系统当前时间（毫秒）。
pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// 把 Unix 毫秒格式化为 ISO-8601（毫秒精度、UTC、带 `Z`），与 JS `new Date(ms).toISOString()` 一致。
pub fn iso_millis(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).expect("epoch 恒合法"))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
