//! iTools 运营控制台（`itools-console`）。
//!
//! 独立进程、独立端口、独立管理员账号体系，与云同步服务端（`itools-sync-server`）
//! 共用同一个 MariaDB 库，但对主服务的表**以只读为主**（见 `store` 模块的读写边界表）。
//!
//! ## 与主服务的关系
//!
//! ```text
//!   浏览器 ──HTTPS:7005──▶ itools-console ──┐
//!                                            ├──▶ MariaDB (itools 库)
//!   iTools 客户端 ─HTTPS:7101─▶ itools-sync ─┘
//! ```
//!
//! 控制台**不代理**主服务的任何流量，主服务挂了控制台照常能用（反之亦然），
//! 两者唯一的耦合点是数据库和一次只读的 `/health` 探测。
//!
//! ## 诚信实现的边界（重要）
//!
//! 有两件事控制台**做不到**，且绝不假装能做到：
//!
//! 1. **禁用账号**：`users` 表没有禁用位，主服务的登录与鉴权也不会去读任何这样的列。
//!    控制台能做的只有「强制下线」（删会话，立即生效）——但那不阻止对方重新登录。
//!    真正的封停需要主服务侧的补丁，在那之前前端的禁用开关是禁用状态并写明原因。
//! 2. **HTTP 流量统计**：主服务零指标采集，库里没有任何请求量/带宽/耗时数据。
//!    控制台只展示能从库里**真算**出来的东西（注册、会话、同步数据、插件），
//!    请求量面板显示「服务端尚未采集」，不画任何估算曲线。

use std::sync::Arc;

pub mod auth;
pub mod config;
pub mod ratelimit;
pub mod routes;
pub mod store;

/// 时钟：返回 Unix **毫秒**。
///
/// 单位与主服务严格一致（`server/src/lib.rs` 的 `system_clock`）——两边共用
/// 同一批时间戳列，单位错一次整个统计就全错。
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// 毫秒时间戳 → RFC3339 字符串（UTC）。越界值回落到 epoch 而不是 panic。
pub fn format_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(|| chrono::DateTime::from_timestamp_millis(0).expect("epoch 恒合法"))
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_returns_milliseconds() {
        let now = system_clock()();
        // 2020-01-01 的毫秒时间戳量级，用来确认单位不是秒
        assert!(now > 1_577_836_800_000, "时钟必须返回毫秒，不是秒");
    }

    #[test]
    fn format_handles_out_of_range_without_panic() {
        assert!(format_ms(0).starts_with("1970-01-01"));
        assert!(!format_ms(i64::MAX).is_empty(), "越界值不能 panic");
        assert!(!format_ms(i64::MIN).is_empty());
    }
}
