//! 本地优先数据层 + 云同步引擎（配套 `account.rs`）。
//!
//! 契约（遵 `doc/开发准则.md` 第 7 条：不假装联网）：
//! - **写入永远先落本地**：`%LOCALAPPDATA%\itools\data\<namespace>.json`，离线始终可用、始终是真相。
//! - 每条记录带 `updated_at`（Unix 秒）与 `dirty`（待上行）标记。
//! - **同步是登录 + 已配置云端才发生的可选动作**：
//!   - 云端未配置 → 返回 `{ synced:false, reason:"cloud_not_configured" }`，数据留在本地。
//!   - 未登录 → `{ synced:false, reason:"not_logged_in" }`。
//!   - 已登录且已配置 → 真实 `POST {endpoint}/data/{ns}` 上行 dirty 记录并回拉合并（updated_at 大者胜），
//!     网络失败 → `{ synced:false, reason:"offline" }`。任何情况都不谎报 synced。
//!
//! 命名空间：核心 App 用 `app`；第三方插件用 `plugin:<id>`（经桥接 `itools.data.*` 访问，按插件隔离）。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::account::AccountStore;

/// 云端数据同步请求超时（秒）。
const SYNC_TIMEOUT_SECS: u64 = 30;

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 一条数据记录：值 + 最后更新时间 + 是否待上行。
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    value: serde_json::Value,
    #[serde(default)]
    updated_at: u64,
    /// true = 本地已改、尚未成功同步到云端。
    #[serde(default)]
    dirty: bool,
}

/// 一个命名空间的全部记录（key → Record）。
type NsMap = BTreeMap<String, Record>;

/// 云端交换用的记录（camelCase，不含 dirty——dirty 是本地概念）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRecord {
    key: String,
    value: serde_json::Value,
    updated_at: u64,
}

/// `POST {endpoint}/data/{ns}` 的请求体：本次上行的 dirty 记录。
#[derive(Serialize)]
struct PushBody {
    records: Vec<WireRecord>,
}

/// 服务端响应：权威 / 有更新的远端记录（用于回拉合并）。
#[derive(Deserialize)]
struct PullBody {
    #[serde(default)]
    records: Vec<WireRecord>,
}

/// 同步结果（返回前端 / 插件，camelCase）。`synced=false` 时 `reason` 说明原因，绝不谎报。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    /// 是否真正与云端完成了一次同步。
    pub synced: bool,
    /// 未同步原因：`cloud_not_configured` / `not_logged_in` / `offline` / `error`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// 本次上行（推送到云端）的记录数。
    pub pushed: usize,
    /// 本次下行（从云端合并到本地）的记录数。
    pub pulled: usize,
    /// 人类可读的补充信息（成功或失败）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl SyncResult {
    fn not_synced(reason: &str, message: &str) -> Self {
        Self {
            synced: false,
            reason: Some(reason.to_string()),
            pushed: 0,
            pulled: 0,
            message: Some(message.to_string()),
        }
    }
}

/// 线程安全的本地优先数据存储。`lock` 串行化「读—改—写」，避免并发写互相覆盖。
pub struct DataStore {
    root: PathBuf,
    lock: Mutex<()>,
}

impl DataStore {
    /// 默认位置：`%LOCALAPPDATA%\itools\data\`。
    pub fn load() -> Self {
        let root = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("itools")
            .join("data");
        Self {
            root,
            lock: Mutex::new(()),
        }
    }

    /// 命名空间 → 文件路径。把 `:`/路径分隔符等替换为 `_`，避免越出 data 目录。
    fn ns_path(&self, ns: &str) -> PathBuf {
        let safe: String = ns
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
            .collect();
        self.root.join(format!("{safe}.json"))
    }

    fn load_ns(&self, ns: &str) -> NsMap {
        std::fs::read_to_string(self.ns_path(ns))
            .ok()
            .and_then(|s| serde_json::from_str::<NsMap>(&s).ok())
            .unwrap_or_default()
    }

    fn save_ns(&self, ns: &str, map: &NsMap) -> Result<(), String> {
        let path = self.ns_path(ns);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("写本地数据失败: {e}"))
    }

    /// 写入一条记录（先落本地、标记 dirty 待上行）。
    pub fn set(&self, ns: &str, key: &str, value: serde_json::Value) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|_| "数据锁获取失败".to_string())?;
        let mut map = self.load_ns(ns);
        map.insert(
            key.to_string(),
            Record {
                value,
                updated_at: now_secs(),
                dirty: true,
            },
        );
        self.save_ns(ns, &map)
    }

    /// 读取一条记录的值（不存在返回 None）。纯本地、瞬时。
    pub fn get(&self, ns: &str, key: &str) -> Option<serde_json::Value> {
        let _g = self.lock.lock().ok()?;
        self.load_ns(ns).get(key).map(|r| r.value.clone())
    }

    /// 删除一条记录（本地删除；已同步的删除传播需服务端支持，首期仅本地删）。
    pub fn remove(&self, ns: &str, key: &str) -> Result<(), String> {
        let _g = self.lock.lock().map_err(|_| "数据锁获取失败".to_string())?;
        let mut map = self.load_ns(ns);
        if map.remove(key).is_some() {
            self.save_ns(ns, &map)?;
        }
        Ok(())
    }

    /// 列出某命名空间下按前缀过滤的所有 key。
    pub fn keys(&self, ns: &str, prefix: &str) -> Vec<String> {
        let Ok(_g) = self.lock.lock() else {
            return Vec::new();
        };
        self.load_ns(ns)
            .keys()
            .filter(|k| prefix.is_empty() || k.starts_with(prefix))
            .cloned()
            .collect()
    }

    /// 带门禁的同步：云端未配置 / 未登录时诚实返回，不发请求。
    pub fn sync_gated(&self, ns: &str, account: &AccountStore) -> SyncResult {
        let endpoint = match crate::account::cloud_endpoint() {
            Some(e) => e,
            None => {
                return SyncResult::not_synced(
                    "cloud_not_configured",
                    "云端服务未接入（未配置 ITOOLS_SYNC_ENDPOINT），数据已保存在本地",
                )
            }
        };
        let token = match account.token() {
            Some(t) => t,
            None => {
                return SyncResult::not_synced("not_logged_in", "未登录云账号，数据已保存在本地")
            }
        };
        self.sync(ns, &endpoint, &token)
    }

    /// 真实同步一个命名空间：上行 dirty 记录 + 回拉合并（updated_at 大者胜）。
    /// 仅在 [`sync_gated`] 判定「已登录 + 已配置」后调用。
    fn sync(&self, ns: &str, endpoint: &str, token: &str) -> SyncResult {
        let _g = match self.lock.lock() {
            Ok(g) => g,
            Err(_) => return SyncResult::not_synced("error", "数据锁获取失败"),
        };
        let mut map = self.load_ns(ns);

        // 1) 收集待上行（dirty）记录
        let push: Vec<WireRecord> = map
            .iter()
            .filter(|(_, r)| r.dirty)
            .map(|(k, r)| WireRecord {
                key: k.clone(),
                value: r.value.clone(),
                updated_at: r.updated_at,
            })
            .collect();
        let pushed = push.len();

        // 2) 真实 HTTP：上行 + 取回远端记录
        let url = format!("{endpoint}/data/{ns}");
        let resp = ureq::post(&url)
            .timeout(Duration::from_secs(SYNC_TIMEOUT_SECS))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(PushBody { records: push });
        let remote: PullBody = match resp {
            Ok(r) => match r.into_json() {
                Ok(p) => p,
                Err(e) => return SyncResult::not_synced("error", &format!("同步响应解析失败: {e}")),
            },
            Err(ureq::Error::Status(code, _)) => {
                let msg = if code == 401 || code == 403 {
                    "云端会话已失效，请重新登录".to_string()
                } else {
                    format!("云端同步失败（HTTP {code}）")
                };
                return SyncResult::not_synced("error", &msg);
            }
            Err(ureq::Error::Transport(t)) => {
                return SyncResult::not_synced("offline", &format!("无法连接云端: {t}"))
            }
        };

        // 3) 上行成功：清 dirty（随后可能被更新的远端覆盖）
        for wr in map.values_mut() {
            wr.dirty = false;
        }

        // 4) 回拉合并：远端更新（updated_at 更大）胜出
        let mut pulled = 0usize;
        for rr in remote.records {
            let take = match map.get(&rr.key) {
                Some(local) => rr.updated_at >= local.updated_at,
                None => true,
            };
            if take {
                map.insert(
                    rr.key,
                    Record {
                        value: rr.value,
                        updated_at: rr.updated_at,
                        dirty: false,
                    },
                );
                pulled += 1;
            }
        }

        if let Err(e) = self.save_ns(ns, &map) {
            return SyncResult::not_synced("error", &e);
        }
        SyncResult {
            synced: true,
            reason: None,
            pushed,
            pulled,
            message: Some(format!("已同步：上行 {pushed} 条，下行 {pulled} 条")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (DataStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("itools-test-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        (
            DataStore {
                root: root.clone(),
                lock: Mutex::new(()),
            },
            root,
        )
    }

    #[test]
    fn local_first_roundtrip() {
        let (store, root) = temp_store();
        store
            .set("app", "nickname", serde_json::json!("海风哥"))
            .unwrap();
        store
            .set("app", "count", serde_json::json!(3))
            .unwrap();
        assert_eq!(store.get("app", "nickname"), Some(serde_json::json!("海风哥")));
        assert_eq!(store.get("app", "missing"), None);
        let mut keys = store.keys("app", "");
        keys.sort();
        assert_eq!(keys, vec!["count".to_string(), "nickname".to_string()]);
        // 前缀过滤
        assert_eq!(store.keys("app", "nick"), vec!["nickname".to_string()]);
        // 删除
        store.remove("app", "count").unwrap();
        assert_eq!(store.get("app", "count"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ns_path_sanitized() {
        let (store, root) = temp_store();
        // plugin:foo/bar 命名空间不应越出 data 目录
        let p = store.ns_path("plugin:foo/bar");
        assert!(p.starts_with(&root));
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "plugin_foo_bar.json");
        let _ = std::fs::remove_dir_all(&root);
    }
}
