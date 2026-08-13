//! 云同步 **Mock**：调试会话里的 `itools.data.sync()` 一律走这里，**绝不发出任何网络请求**。
//!
//! 为什么必须 Mock 而不是「让它照常同步」：`plugin_data_sync` 真实实现会把命名空间下的
//! 待上行记录 POST 到用户配置的服务端——调试阶段的垃圾数据会真的进云端数据库。
//! 这里是物理阻断：调试会话根本不进入 `DataStore::sync_gated`。
//!
//! 返回值**形态与真实 [`SyncResult`] 完全一致**（同一个结构体，不是另造一个）——
//! 否则插件在调试环境里调通了，上线就崩。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::sync::SyncResult;

use super::DevRuntime;

/// Mock 配置在测试库 `app_kv` 里的键。
const MOCK_KEY: &str = "dev_mock";

/// 模拟延迟上限（毫秒）。真实同步的 HTTP 超时是 30s，但调试里没必要卡这么久；
/// 超出的值在 [`DevMockConfig::normalized`] 里被夹紧，`dev_get_mock` 返回的是**夹紧后**的真实值，
/// 不会出现「设了 60s、界面显示 60s、实际只等 10s」的假控件。
pub const MAX_DELAY_MS: u64 = 10_000;

/// 单次模拟返回的条数上限（防止界面上填个天文数字）。
const MAX_COUNT: u32 = 100_000;

/// 云同步 Mock 配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevMockConfig {
    /// `"success"` / `"fail"` / `"offline"` / `"notLoggedIn"`。
    pub mode: String,
    /// 人为延迟（毫秒，0~[`MAX_DELAY_MS`]）：用来测插件的 loading 态。
    #[serde(rename = "delayMs")]
    pub delay_ms: u64,
    /// `mode="fail"` 时返回的 `reason`（留空则用 `error`）。
    pub reason: String,
    /// `mode="success"` 时**模拟服务端声称**的上行条数。
    pub pushed: u32,
    /// `mode="success"` 时**模拟服务端声称**的下行条数。
    pub pulled: u32,
}

impl Default for DevMockConfig {
    fn default() -> Self {
        Self {
            mode: "success".to_string(),
            delay_ms: 0,
            reason: String::new(),
            pushed: 0,
            pulled: 0,
        }
    }
}

impl DevMockConfig {
    /// 夹紧到**真正能执行**的范围：未知 mode 归为 success，延迟与条数封顶。
    /// 存的就是夹紧后的值，所以 `dev_get_mock` 读回来的永远是实际会发生的行为。
    pub fn normalized(mut self) -> Self {
        self.mode = match self.mode.trim() {
            "fail" => "fail",
            "offline" => "offline",
            "notLoggedIn" => "notLoggedIn",
            _ => "success",
        }
        .to_string();
        self.delay_ms = self.delay_ms.min(MAX_DELAY_MS);
        self.reason = self.reason.trim().to_string();
        self.pushed = self.pushed.min(MAX_COUNT);
        self.pulled = self.pulled.min(MAX_COUNT);
        self
    }
}

/// 从测试库读 Mock 配置（无 / 损坏则用默认值）。
pub fn load(db: &Arc<Db>) -> DevMockConfig {
    db.blob_get(MOCK_KEY)
        .and_then(|s| serde_json::from_str::<DevMockConfig>(&s).ok())
        .unwrap_or_default()
        .normalized()
}

/// 把 Mock 配置写进测试库（跨重启保留）。
pub fn save(db: &Arc<Db>, cfg: &DevMockConfig) {
    if let Ok(json) = serde_json::to_string(cfg) {
        db.blob_set(MOCK_KEY, &json);
    }
}

/// 执行一次模拟同步。**只读写测试库，永不联网。**
///
/// `mode="success"` 时会真的把该命名空间的「待上行」标记清掉（与真实同步成功后的本地效果一致），
/// 因此重复点同步不会一直显示有待上行数据；`pushed`/`pulled` 则是配置里**模拟服务端声称**的条数，
/// `message` 里会同时写出本地实际清理了多少条，两者一目了然。
pub fn sync(dev: &DevRuntime, plugin_id: &str) -> SyncResult {
    let cfg = dev.mock();
    if cfg.delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(cfg.delay_ms));
    }
    let ns = crate::dev::storage::dev_ns(plugin_id);
    let result = match cfg.mode.as_str() {
        "fail" => {
            let reason = if cfg.reason.is_empty() {
                "error".to_string()
            } else {
                cfg.reason.clone()
            };
            SyncResult {
                synced: false,
                reason: Some(reason.clone()),
                pushed: 0,
                pulled: 0,
                message: Some(format!("[Mock] 模拟同步失败（reason={reason}），未联网")),
            }
        }
        "offline" => SyncResult {
            synced: false,
            reason: Some("offline".to_string()),
            pushed: 0,
            pulled: 0,
            message: Some("[Mock] 模拟离线，未联网".to_string()),
        },
        "notLoggedIn" => SyncResult {
            synced: false,
            reason: Some("not_logged_in".to_string()),
            pushed: 0,
            pulled: 0,
            message: Some("[Mock] 模拟未登录，未联网".to_string()),
        },
        // success（含被 normalized 归一化过的未知值）
        _ => {
            // 真实的本地效果：清掉本次「待上行」标记（与真同步成功后一致）
            let dirty = dev.db.pd_dirty(&ns);
            let snapshot: Vec<(String, u64, String)> = dirty
                .iter()
                .map(|(k, v, ua)| (k.clone(), *ua, v.clone()))
                .collect();
            let cleared = snapshot.len();
            let _ = dev.db.pd_clear_dirty(&ns, &snapshot);
            SyncResult {
                synced: true,
                reason: None,
                pushed: cfg.pushed as usize,
                pulled: cfg.pulled as usize,
                message: Some(format!(
                    "[Mock] 模拟同步成功（未联网）：按配置返回 上行 {} / 下行 {}；本地实际清理 {cleared} 条待上行标记",
                    cfg.pushed, cfg.pulled
                )),
            }
        }
    };
    dev.logs.push(super::logs::DevLogEntry::backend(
        plugin_id,
        "api",
        if result.synced { "info" } else { "warn" },
        "sync.mock",
        &format!(
            "mode={} delayMs={} → synced={} reason={:?}（本次未发出任何网络请求）",
            cfg.mode, cfg.delay_ms, result.synced, result.reason
        ),
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(tag: &str) -> DevRuntime {
        let base = std::env::temp_dir().join(format!(
            "itools-mock-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        DevRuntime::new(base.clone(), base.join("dev-plugins"))
    }

    fn cleanup(rt: &DevRuntime) {
        if let Some(home) = rt.db_path.parent() {
            let _ = std::fs::remove_dir_all(home);
        }
    }

    /// 四种模式都返回真实 SyncResult 的形态，且成功模式真的清掉本地 dirty。
    #[test]
    fn mock_covers_all_modes_without_network() {
        let rt = runtime("modes");
        let ns = crate::dev::storage::dev_ns("demo");
        rt.db.pd_set(&ns, "k", "1", 100, true).unwrap();

        rt.set_mock(DevMockConfig {
            mode: "success".into(),
            delay_ms: 0,
            reason: String::new(),
            pushed: 3,
            pulled: 2,
        });
        let r = sync(&rt, "demo");
        assert!(r.synced);
        assert_eq!((r.pushed, r.pulled), (3, 2));
        assert!(rt.db.pd_dirty(&ns).is_empty(), "成功模式必须真的清掉待上行标记");

        rt.set_mock(DevMockConfig {
            mode: "fail".into(),
            reason: "quota_exceeded".into(),
            ..Default::default()
        });
        let r = sync(&rt, "demo");
        assert!(!r.synced);
        assert_eq!(r.reason.as_deref(), Some("quota_exceeded"));

        rt.set_mock(DevMockConfig {
            mode: "offline".into(),
            ..Default::default()
        });
        assert_eq!(sync(&rt, "demo").reason.as_deref(), Some("offline"));

        rt.set_mock(DevMockConfig {
            mode: "notLoggedIn".into(),
            ..Default::default()
        });
        assert_eq!(sync(&rt, "demo").reason.as_deref(), Some("not_logged_in"));
        cleanup(&rt);
    }

    /// 夹紧规则真实生效：读回来的就是实际会发生的行为，没有「设了不生效」的控件。
    #[test]
    fn config_is_clamped_and_persisted() {
        let rt = runtime("clamp");
        rt.set_mock(DevMockConfig {
            mode: "whatever".into(),
            delay_ms: 999_999,
            reason: "  x  ".into(),
            pushed: u32::MAX,
            pulled: 1,
        });
        let got = rt.mock();
        assert_eq!(got.mode, "success", "未知模式归一化为 success");
        assert_eq!(got.delay_ms, MAX_DELAY_MS);
        assert_eq!(got.reason, "x");
        assert_eq!(got.pushed, MAX_COUNT);
        // 持久化：从同一个库重新载入
        assert_eq!(load(&rt.db), got);
        cleanup(&rt);
    }

    /// 序列化后的字段名必须与前端契约一致（delayMs 是 camelCase）。
    #[test]
    fn serializes_as_camel_case() {
        let json = serde_json::to_string(&DevMockConfig::default()).unwrap();
        assert!(json.contains("\"delayMs\""), "实际: {json}");
        assert!(json.contains("\"mode\""));
        assert!(json.contains("\"pushed\""));
    }
}
