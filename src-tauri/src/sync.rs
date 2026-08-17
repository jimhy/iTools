//! 本地优先数据层 + 云同步引擎（配套 `account.rs`）。
//!
//! 契约（遵 `doc/开发准则.md` 第 7 条：不假装联网）：
//! - **写入永远先落本地**：统一 SQLite 库的 `plugin_data` 表（按命名空间隔离），离线始终可用、始终是真相。
//! - 每条记录带 `updated_at`（Unix 秒）与 `dirty`（待上行）标记。
//! - **同步是登录 + 已配置云端才发生的可选动作**：
//!   - 云端未配置 → 返回 `{ synced:false, reason:"cloud_not_configured" }`，数据留在本地。
//!   - 未登录 → `{ synced:false, reason:"not_logged_in" }`。
//!   - 已登录且已配置 → 真实 `POST {endpoint}/data/{ns}` 上行 dirty 记录并回拉合并（updated_at 大者胜），
//!     网络失败 → `{ synced:false, reason:"offline" }`。任何情况都不谎报 synced。
//! - **自动同步（「登录后自动同步」开关）**：数据经 [`DataStore::set`] 变更后，若已登录且开关开启，
//!   由 [`schedule_auto_sync`] 防抖触发一次后台上行；开关关 / 未登录则不发生——开关真实生效，非摆设。
//!
//! 命名空间：核心 App 用 `app`；第三方插件用 `plugin:<id>`（经桥接 `itools.data.*` 访问，按插件隔离）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::account::AccountStore;
use crate::db::Db;
use crate::logging::ilog;

/// 云端数据同步请求超时（秒）。
const SYNC_TIMEOUT_SECS: u64 = 30;

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    /// 未同步原因：`cloud_not_configured` / `not_logged_in` / `offline` / `session_expired` / `error`。
    /// （`session_expired`：服务端判定会话无效 401/403，已自愈清本地登录态，需重新登录。）
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

/// 单个命名空间的记录条数（`ns` 形如 `app` / `plugin:<id>`）。供「我的数据」页展示。
#[derive(Serialize, Clone)]
pub struct NsCount {
    /// 命名空间：`app`（主程序）或 `plugin:<插件名>`。
    pub ns: String,
    /// 该命名空间的**存储项**数（真实统计，非估算）。
    ///
    /// 注意这是「键」的数量，不是用户眼里的条目数：整份待办清单只占 1 个键，
    /// 而每篇笔记的正文各占 1 个。给用户看的数字请用 [`Self::items`]。
    pub count: u64,
    /// **用户视角**的业务条目明细（如「笔记 2 · 待办 2 · 密码 1」）。
    ///
    /// 由插件在 `plugin.json` 的 `dataSets` 里声明，宿主据此解析真实值统计得出。
    /// 插件没声明就是空——此时界面只能退回按存储项显示，并说明原因，不许编数字。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ItemCount>,
}

/// 一类业务条目的计数（插件声明的口径）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ItemCount {
    /// 给用户看的名字，如「笔记」。
    pub label: String,
    /// 条数（按声明的 `countBy` 解析真实存储值得出）。
    pub count: u64,
}

/// 命名空间 → 该插件声明的数据集口径。由插件注册表在调用处组装。
pub type DataSetSpecs = HashMap<String, Vec<DataSetSpec>>;

/// 一条 `dataSets` 声明的运行期形态（与 `plugin.json` 里的字段一一对应）。
#[derive(Debug, Clone)]
pub struct DataSetSpec {
    /// 存储键；结尾 `*` 表示前缀匹配。
    pub key: String,
    pub label: String,
    /// `length`（数组取长度 / 对象取键数）或 `one`（整键算 1 条）。
    pub count_by: String,
}

impl DataSetSpec {
    /// 该声明是否匹配这个存储键。
    pub fn matches(&self, key: &str) -> bool {
        match self.key.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == self.key,
        }
    }

    /// 按声明的口径数这一个键里有多少条业务记录。
    ///
    /// 值解析不了（脏数据 / 非 JSON）就算 1 条：它**确实占了一条存储**，
    /// 报 0 会让用户以为数据丢了。
    pub fn count_of(&self, value_json: &str) -> u64 {
        if self.count_by == "one" {
            return 1;
        }
        match serde_json::from_str::<serde_json::Value>(value_json) {
            Ok(serde_json::Value::Array(a)) => a.len() as u64,
            Ok(serde_json::Value::Object(o)) => o.len() as u64,
            _ => 1,
        }
    }
}

/// 云端用量快照（真实来自服务端 `GET /data/_usage`）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CloudUsage {
    /// 各命名空间在云端的条数。
    pub counts: Vec<NsCount>,
    /// 云端总条数。
    pub total: u64,
    /// 云端占用的原始字节数（服务端各记录 value 文本长度之和；真实值，不硬编码）。
    pub bytes: u64,
}

/// 「我的数据」页所需的用量汇总：本地始终真实；云端仅在已登录 + 已配置且请求成功时给出，
/// 否则 `cloud=None` 且 `cloud_reason` 如实说明原因（绝不用本地数字冒充云端）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataUsage {
    /// 本地各命名空间条数（**可参与云同步的那部分**，即 `itools.data.*` / `plugin_data`）。
    pub local: Vec<NsCount>,
    /// 上者的总条数。
    pub local_total: u64,
    /// 各命名空间的**纯本地**条数（`itools.db.*` / `plugin_kv`，设计上不参与云同步）。
    ///
    /// 单独一档而不是并进 `local`：这两类数据的去向完全不同，合成一个数字会让用户
    /// 以为「本地 N 条」都会上云，而实际只有 `local` 那部分会。界面必须分开呈现。
    pub local_only: Vec<NsCount>,
    /// 上者的总条数。
    pub local_only_total: u64,
    /// 云端用量（不可用时为 None）。
    pub cloud: Option<CloudUsage>,
    /// 云端不可用原因：`cloud_not_configured` / `not_logged_in` / `offline` / `session_expired` / `error`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_reason: Option<String>,
}

/// 服务端 `GET /data/_usage` 的响应体（camelCase 与服务端对齐）。
#[derive(Deserialize)]
struct UsageResp {
    /// 命名空间 → 条数。
    #[serde(default)]
    counts: HashMap<String, u64>,
    /// 云端占用字节数。
    #[serde(default)]
    bytes: u64,
}

/// 本地优先数据存储：所有读写走统一 SQLite 库的 `plugin_data` 表（并发串行化在 [`Db`] 内）。
pub struct DataStore {
    db: Arc<Db>,
    /// 云端用量的短缓存：(取得时刻, 结果)。见 [`DataStore::cloud_usage_cached`]。
    /// 只缓存成功结果，失败不入缓存。
    cloud_cache: std::sync::Mutex<Option<(std::time::Instant, CloudUsage)>>,
}

impl DataStore {
    /// 绑定统一 SQLite 库。
    pub fn load(db: Arc<Db>) -> Self {
        Self { db, cloud_cache: std::sync::Mutex::new(None) }
    }

    /// 写入一条记录（先落本地、标记 dirty 待上行）。
    pub fn set(&self, ns: &str, key: &str, value: serde_json::Value) -> Result<(), String> {
        self.db.pd_set(ns, key, &value.to_string(), now_secs(), true)
    }

    /// 读取一条记录的值（不存在返回 None）。纯本地、瞬时。
    pub fn get(&self, ns: &str, key: &str) -> Option<serde_json::Value> {
        self.db
            .pd_get(ns, key)
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// 删除一条记录（本地删除；已同步的删除传播需服务端支持，首期仅本地删）。
    pub fn remove(&self, ns: &str, key: &str) -> Result<(), String> {
        self.db.pd_remove(ns, key)
    }

    /// 列出某命名空间下按前缀过滤的所有 key。
    pub fn keys(&self, ns: &str, prefix: &str) -> Vec<String> {
        self.db.pd_keys(ns, prefix)
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
        self.sync(ns, &endpoint, &token, account)
    }

    /// 「立即同步」：把**所有有数据的命名空间**（各插件 `plugin:<id>` 等）逐个同步并汇总结果。
    /// 诚实降级：云端未配置 / 未登录直接返回对应 reason；无数据则返回 0/0（不谎报）。
    /// 修复「立即同步只同步空的 `app` 命名空间」——真实用户数据在 `plugin:<id>`，须全量覆盖。
    pub fn sync_all_gated(&self, account: &AccountStore) -> SyncResult {
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
        let namespaces = self.db.pd_namespaces();
        if namespaces.is_empty() {
            return SyncResult {
                synced: true,
                reason: None,
                pushed: 0,
                pulled: 0,
                message: Some("暂无待同步数据".to_string()),
            };
        }
        let (mut pushed, mut pulled, mut ok, mut fail) = (0usize, 0usize, 0usize, 0usize);
        let mut last_fail: Option<SyncResult> = None;
        for ns in &namespaces {
            let r = self.sync(ns, &endpoint, &token, account);
            if r.synced {
                ok += 1;
                pushed += r.pushed;
                pulled += r.pulled;
            } else if r.reason.as_deref() == Some("session_expired") {
                return r; // 会话失效已自愈清登录态，硬停（后续 ns 也会失败）
            } else {
                fail += 1;
                last_fail = Some(r);
            }
        }
        if fail > 0 {
            if ok == 0 {
                // 全部失败：透传最后一个失败原因（如 offline），不谎报 synced
                return last_fail.unwrap_or_else(|| SyncResult::not_synced("error", "同步失败"));
            }
            // 部分成功：**如实汇报**，绝不把失败的数据集算作已同步（诚信红线：不谎报成功）。
            return SyncResult {
                synced: false,
                reason: Some("partial".to_string()),
                pushed,
                pulled,
                message: Some(format!(
                    "部分同步完成：{ok} 个数据集成功、{fail} 个失败（失败的已留本地待下次重试）"
                )),
            };
        }
        SyncResult {
            synced: true,
            reason: None,
            pushed,
            pulled,
            message: Some(format!("已同步 {ok} 个数据集：上行 {pushed} 条，下行 {pulled} 条")),
        }
    }

    /// 「我的数据」用量汇总：本地条数**始终真实统计**；云端仅在已配置 + 已登录且请求成功时给出，
    /// 否则 `cloud=None` + `cloud_reason` 如实说明（`cloud_not_configured` / `not_logged_in` /
    /// `offline` / `session_expired` / `error`）——绝不用本地数字冒充云端（诚信红线）。
    ///
    /// `account` 用于取 token；服务端判定会话无效（401/403）时**自愈清本地登录态**。
    /// 用量统计。
    ///
    /// `include_cloud=false` 时**完全不碰网络**，只返回本地统计（毫秒级）。
    /// 「我的数据」页据此两步加载：先秒开本地，再异步补云端——
    /// 否则每次进页面都要卡在一次云端往返上（实测约 1 秒）。
    pub fn usage(
        &self,
        account: &AccountStore,
        data_sets: &DataSetSpecs,
        include_cloud: bool,
    ) -> DataUsage {
        // 本地：真实统计（离线始终可用）。两张表分开数——
        // plugin_data 会上云，plugin_kv 只在本机，合成一个数字就是对用户说假话。
        let local_pairs = self.db.pd_counts();
        let local_total: u64 = local_pairs.iter().map(|(_, c)| *c).sum();
        let local: Vec<NsCount> = local_pairs
            .into_iter()
            .map(|(ns, count)| {
                let items = self.count_items(&ns, data_sets);
                NsCount { ns, count, items }
            })
            .collect();
        let local_only_pairs = self.db.pkv_counts();
        let local_only_total: u64 = local_only_pairs.iter().map(|(_, c)| *c).sum();
        let local_only: Vec<NsCount> = local_only_pairs
            .into_iter()
            .map(|(ns, count)| NsCount { ns, count, items: Vec::new() })
            .collect();

        // 云端：诚实门禁——未配置 / 未登录直接给原因，不发请求。
        // include_cloud=false 时给 "pending"，界面显示「查询中…」而不是谎称查不到。
        let (cloud, cloud_reason) = if include_cloud {
            match self.cloud_usage_cached(account) {
                Ok(c) => (Some(c), None),
                Err(reason) => (None, Some(reason)),
            }
        } else {
            (None, Some("pending".to_string()))
        };

        DataUsage {
            local,
            local_total,
            local_only,
            local_only_total,
            cloud,
            cloud_reason,
        }
    }

    /// 按插件声明的 `dataSets` 统计**用户视角**的业务条目数。
    ///
    /// 没有声明就返回空 Vec —— 界面据此退回「N 个存储项」的说法。宁可说得不够漂亮，
    /// 也不能自作聪明去猜（同样一个数组，在 A 插件是 2 条待办，在 B 插件可能是 2 个分组）。
    fn count_items(&self, ns: &str, specs: &DataSetSpecs) -> Vec<ItemCount> {
        let Some(specs) = specs.get(ns) else {
            return Vec::new();
        };
        // 两张表都要看：业务条目是「用户有多少东西」，与它落在可同步表还是纯本地表无关
        // （pixshot 的截图历史就存在 plugin_kv 里，只算 plugin_data 会显示成「暂无数据」）。
        let mut entries: Vec<(String, String)> = self
            .db
            .pd_entries(ns)
            .into_iter()
            .map(|(k, v, _)| (k, v))
            .collect();
        if let Some(id) = ns.strip_prefix("plugin:") {
            entries.extend(self.db.pkv_entries(id));
        }
        specs
            .iter()
            .map(|spec| {
                let mut count = 0u64;
                for (key, value) in &entries {
                    if !spec.matches(key) {
                        continue;
                    }
                    count += spec.count_of(value);
                }
                ItemCount { label: spec.label.clone(), count }
            })
            // 一条都没有的类别不必占位（用户没建密码就别显示「密码 0」）
            .filter(|c| c.count > 0)
            .collect()
    }

    /// 真实请求服务端 `GET /data/_usage`（带 Bearer）。返回 Err(reason) 表示不可用（诚实降级）。
    /// 带缓存的云端用量。用量是个「大致了解」的数字，不需要每次进页面都实时打服务端；
    /// 缓存期内直接复用，既省往返也省服务端配额。失败结果**不缓存**——
    /// 那多半是临时的（掉线 / 会话过期），缓存它会让恢复后仍显示错误。
    fn cloud_usage_cached(&self, account: &AccountStore) -> Result<CloudUsage, String> {
        const TTL: Duration = Duration::from_secs(60);
        if let Ok(g) = self.cloud_cache.lock() {
            if let Some((at, ref c)) = *g {
                if at.elapsed() < TTL {
                    return Ok(c.clone());
                }
            }
        }
        let fresh = self.fetch_cloud_usage(account)?;
        if let Ok(mut g) = self.cloud_cache.lock() {
            *g = Some((std::time::Instant::now(), fresh.clone()));
        }
        Ok(fresh)
    }

    fn fetch_cloud_usage(&self, account: &AccountStore) -> Result<CloudUsage, String> {
        let endpoint = crate::account::cloud_endpoint().ok_or("cloud_not_configured".to_string())?;
        let token = account.token().ok_or("not_logged_in".to_string())?;
        let url = format!("{endpoint}/data/_usage");
        let resp = crate::http::get(&url)
            .timeout(Duration::from_secs(SYNC_TIMEOUT_SECS))
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        let parsed: UsageResp = match resp {
            Ok(r) => r.into_json().map_err(|_| "error".to_string())?,
            Err(ureq::Error::Status(code, _)) => {
                if code == 401 || code == 403 {
                    account.invalidate_session();
                    return Err("session_expired".to_string());
                }
                return Err("error".to_string());
            }
            Err(ureq::Error::Transport(_)) => return Err("offline".to_string()),
        };
        // 云端只回「每个命名空间有多少个键」，值不回传，因此这里给不出业务条目明细
        // （items 留空）——要按业务口径数就得把云端数据全拉回来，代价与收益不成比例。
        // 界面据此把云端一栏呈现为存储项 + 同步状态，不与本地的业务条目数混为一谈。
        let mut counts: Vec<NsCount> = parsed
            .counts
            .into_iter()
            .map(|(ns, count)| NsCount { ns, count, items: Vec::new() })
            .collect();
        // 稳定顺序：按 ns 升序，便于前端与本地行对齐、渲染稳定。
        counts.sort_by(|a, b| a.ns.cmp(&b.ns));
        let total: u64 = counts.iter().map(|c| c.count).sum();
        Ok(CloudUsage {
            counts,
            total,
            bytes: parsed.bytes,
        })
    }

    /// 真实同步一个命名空间：上行 dirty 记录 + 回拉合并（updated_at 大者胜）。
    /// 仅在 [`Self::sync_gated`] 判定「已登录 + 已配置」后调用。
    ///
    /// **HTTP 不持有存储锁**：先短锁读出 dirty 记录，网络请求在锁外进行，回来再短锁清 dirty +
    /// 合并远端。避免一次 30s 网络往返把整库读写全部阻塞（共享单连接下尤其要紧）。
    ///
    /// `account` 仅用于在服务端判定会话无效（401/403）时**自愈清本地登录态**。
    fn sync(&self, ns: &str, endpoint: &str, token: &str, account: &AccountStore) -> SyncResult {
        // 1) 收集待上行（dirty）记录（短锁）
        let dirty = self.db.pd_dirty(ns);
        // 推送快照 (key, updated_at)：上行成功后**只清这些且此后未被覆盖的行**，
        // 避免清掉 HTTP 在途期间落地的新写入（否则它们会被误标已同步而永不上行）。
        let pushed_snapshot: Vec<(String, u64, String)> =
            dirty.iter().map(|(k, v, ua)| (k.clone(), *ua, v.clone())).collect();
        let push: Vec<WireRecord> = dirty
            .iter()
            .map(|(k, v, ua)| WireRecord {
                key: k.clone(),
                value: serde_json::from_str(v).unwrap_or(serde_json::Value::Null),
                updated_at: *ua,
            })
            .collect();
        let pushed = push.len();

        // 2) 真实 HTTP：上行 + 取回远端记录（锁外）
        let url = format!("{endpoint}/data/{ns}");
        let resp = crate::http::post(&url)
            .timeout(Duration::from_secs(SYNC_TIMEOUT_SECS))
            .set("Authorization", &format!("Bearer {token}"))
            .send_json(PushBody { records: push });
        let remote: PullBody = match resp {
            Ok(r) => match r.into_json() {
                Ok(p) => p,
                Err(e) => return SyncResult::not_synced("error", &format!("同步响应解析失败: {e}")),
            },
            Err(ureq::Error::Status(code, _)) => {
                if code == 401 || code == 403 {
                    // 服务端判定会话无效：自愈清本地登录态，避免停留在「僵尸已登录」，让用户重新登录。
                    account.invalidate_session();
                    return SyncResult::not_synced("session_expired", "云端会话已失效，请重新登录");
                }
                return SyncResult::not_synced("error", &format!("云端同步失败（HTTP {code}）"));
            }
            Err(ureq::Error::Transport(t)) => {
                return SyncResult::not_synced("offline", &format!("无法连接云端: {t}"))
            }
        };

        // 3) 上行成功：只清「本次推送且此后未被覆盖」的 dirty（防在途写入被误清）
        let _ = self.db.pd_clear_dirty(ns, &pushed_snapshot);

        // 4) 回拉合并：远端更新（updated_at 更大或本地无此 key）胜出
        let mut pulled = 0usize;
        for rr in remote.records {
            if self
                .db
                .pd_merge_remote(ns, &rr.key, &rr.value.to_string(), rr.updated_at)
            {
                pulled += 1;
            }
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

// ==================== 「登录后自动同步」调度器 ====================

/// 自动同步去抖窗口：合并连续写入，避免每次 `set` 都打一次网络。
const AUTO_SYNC_DEBOUNCE: Duration = Duration::from_millis(1500);

/// 「登录后自动同步」调度器（managed state）。
/// `pending` 集合保证**每个 ns 同时最多一个防抖线程**：批量写入只在首次调度时起线程，其余被合并——
/// 线程醒来时一次性同步该 ns 当前全部 dirty（避免每次 `set` 都 spawn 线程、堆积休眠线程）。
#[derive(Default)]
pub struct AutoSync {
    pending: Mutex<HashSet<String>>,
}

/// 数据变更后调度一次自动上行：**仅当已登录 + 已配置云端 + 开启自动同步**时才安排
/// （否则静默跳过——保证「开关关 / 未登录就真的不同步」，这正是让开关名副其实的关键）。
///
/// 每个 ns 至多一个防抖线程：首次调度起线程，`AUTO_SYNC_DEBOUNCE` 后解除 pending 并同步当前全部
/// dirty；期间同 ns 的写入被合并（不再起线程）。用独立线程承载阻塞式 HTTP（不占 async 执行器）；
/// 同步前再门禁一次（防抖期间可能已登出 / 关开关 / 清端点）。
pub fn schedule_auto_sync(app: &AppHandle, ns: &str) {
    let account = app.state::<AccountStore>();
    // token() 已内含「已登录 + 云端已配置」门禁；再加「开关开启」。
    if !account.sync_enabled() || account.token().is_none() {
        return;
    }
    let first = match app.state::<AutoSync>().pending.lock() {
        Ok(mut s) => s.insert(ns.to_string()), // true = 该 ns 此前无防抖线程
        Err(_) => return,
    };
    if !first {
        return; // 已有防抖线程在跑，本次写入被它合并
    }
    let app = app.clone();
    let ns = ns.to_string();
    let _ = std::thread::Builder::new()
        .name("auto-sync".into())
        .spawn(move || {
            std::thread::sleep(AUTO_SYNC_DEBOUNCE);
            // 先解除 pending（允许后续写入再起线程），再同步当前全部 dirty。
            if let Ok(mut s) = app.state::<AutoSync>().pending.lock() {
                s.remove(&ns);
            }
            // 防抖期间可能已登出 / 关开关 / 清端点——同步前再门禁一次。
            let account = app.state::<AccountStore>();
            if !account.sync_enabled() || account.token().is_none() {
                return;
            }
            let r = app.state::<DataStore>().sync_gated(&ns, &account);
            ilog!(
                "[iTools] 自动同步 {}：synced={} pushed={} pulled={} reason={:?}",
                ns,
                r.synced,
                r.pushed,
                r.pulled,
                r.reason
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> DataStore {
        DataStore::load(Arc::new(Db::open_memory()))
    }

    #[test]
    fn local_first_roundtrip() {
        let store = store();
        store
            .set("app", "nickname", serde_json::json!("海风哥"))
            .unwrap();
        store.set("app", "count", serde_json::json!(3)).unwrap();
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
    }

    #[test]
    fn namespaces_isolated() {
        let store = store();
        store.set("app", "k", serde_json::json!(1)).unwrap();
        store
            .set("plugin:deskbox", "k", serde_json::json!(2))
            .unwrap();
        // 同名 key 在不同命名空间互不可见
        assert_eq!(store.get("app", "k"), Some(serde_json::json!(1)));
        assert_eq!(store.get("plugin:deskbox", "k"), Some(serde_json::json!(2)));
        assert_eq!(store.keys("app", ""), vec!["k".to_string()]);
        assert_eq!(store.keys("plugin:deskbox", ""), vec!["k".to_string()]);
    }
}
