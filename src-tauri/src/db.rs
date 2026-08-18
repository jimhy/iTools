//! 统一本地存储：单个 SQLite 库 `<数据根>\itools.db`
//! （数据根 = release `%LOCALAPPDATA%\itools` / debug `%LOCALAPPDATA%\itools-dev`，见 [`crate::paths`]）。
//!
//! 取代原先散落的 JSON：
//! - `usage.json` / `settings.json` / `account.json` / `profile.json` → `app_kv` 表（单行 JSON blob，整体读写）
//! - `plugin-data/<id>/kv.json`（`itools.db.*`）→ `plugin_kv` 表（按 key 结构化）
//! - `data/<ns>.json`（`itools.data.*`，带 updated_at/dirty，参与云同步）→ `plugin_data` 表
//!
//! **插件沙盒文件**（`itools.writeFile`）仍在文件系统 `plugin-data/<id>/files/`，不入库——
//! 那是插件写任意格式文件（含二进制），不该塞进 KV 数据库。
//!
//! 并发：单连接 + `Mutex` 串行化所有读写（原各 Store 本就各自 `Mutex`+立即落盘，此处合一）。
//! 首启惰性迁移：库初次打开、对应表为空且旧 JSON 存在时，一次性导入（不删旧文件，可回退）。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};

/// 统一本地 SQLite 存储。各 Store 共享一个 `Arc<Db>`。
pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// 默认位置：`<数据根>\itools.db`，并从**同目录**旧 JSON 惰性迁移。
    ///
    /// 数据根由 [`crate::paths::data_root`] 统一给出（release `itools` / debug `itools-dev`），
    /// 这也正是 debug 与 release 能同时跑而不并发写同一个 SQLite 的地方。
    /// 迁移只看同目录的旧 JSON，所以 debug 那份空目录里什么都不会被迁进来（预期行为）。
    pub fn open_default() -> Self {
        let root = crate::paths::data_root();
        let _ = std::fs::create_dir_all(&root);
        let db = Self::open(&root.join("itools.db"));
        db.migrate_legacy(&root);
        db
    }

    /// 打开指定路径的库并建表（幂等）。极端失败（磁盘只读等）回退内存库，保证 app 不崩。
    pub fn open(path: &Path) -> Self {
        let conn = Connection::open(path)
            .or_else(|_| Connection::open_in_memory())
            .expect("打开 SQLite 失败且内存库也失败");
        Self::init(&conn);
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// 纯内存库（测试用）。
    #[cfg(test)]
    pub fn open_memory() -> Self {
        let conn = Connection::open_in_memory().expect("打开内存 SQLite 失败");
        Self::init(&conn);
        Self {
            conn: Mutex::new(conn),
        }
    }

    fn init(conn: &Connection) {
        // WAL：更好的并发读写；NORMAL：崩溃安全与性能的平衡（配 WAL 不丢已提交事务）。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS app_kv (
                k  TEXT PRIMARY KEY,
                v  TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS plugin_kv (
                plugin_id TEXT NOT NULL,
                k         TEXT NOT NULL,
                v         TEXT NOT NULL,
                PRIMARY KEY (plugin_id, k)
             );
             CREATE TABLE IF NOT EXISTS plugin_data (
                ns         TEXT NOT NULL,
                k          TEXT NOT NULL,
                v          TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                dirty      INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (ns, k)
             );",
        )
        .expect("建表失败");
    }

    // ================= app_kv：整体读写的小对象（usage/settings/account/profile） =================

    /// 读一个 blob（不存在返回 None）。
    pub fn blob_get(&self, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row("SELECT v FROM app_kv WHERE k=?1", [key], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
    }

    /// 写一个 blob（覆盖）。容错：失败静默（与原 JSON 落盘的容错风格一致）。
    pub fn blob_set(&self, key: &str, val: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO app_kv(k,v) VALUES(?1,?2)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                (key, val),
            );
        }
    }

    // ================= plugin_kv：itools.db.*（纯本地 KV，value 为字符串） =================

    pub fn pkv_get(&self, plugin: &str, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT v FROM plugin_kv WHERE plugin_id=?1 AND k=?2",
            (plugin, key),
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn pkv_set(&self, plugin: &str, key: &str, val: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute(
            "INSERT INTO plugin_kv(plugin_id,k,v) VALUES(?1,?2,?3)
             ON CONFLICT(plugin_id,k) DO UPDATE SET v=excluded.v",
            (plugin, key, val),
        )
        .map(|_| ())
        .map_err(|e| format!("写插件存储失败: {e}"))
    }

    pub fn pkv_remove(&self, plugin: &str, key: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute(
            "DELETE FROM plugin_kv WHERE plugin_id=?1 AND k=?2",
            (plugin, key),
        )
        .map(|_| ())
        .map_err(|e| format!("删除失败: {e}"))
    }

    pub fn pkv_keys(&self, plugin: &str, prefix: &str) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT k FROM plugin_kv WHERE plugin_id=?1 ORDER BY k")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([plugin], |r| r.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.flatten()
            .filter(|k| prefix.is_empty() || k.starts_with(prefix))
            .collect()
    }

    /// 列某插件的全部 (key, value)（升序）。供调试环境的存储查看器一次取全，
    /// 避免「先 keys 再逐个 get」的 N+1 次加锁查询。
    pub fn pkv_entries(&self, plugin: &str) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT k,v FROM plugin_kv WHERE plugin_id=?1 ORDER BY k")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([plugin], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 清空某插件的全部 KV（调试环境「清空测试数据」用；正式环境的删插件走 delete_plugin）。
    pub fn pkv_clear(&self, plugin: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute("DELETE FROM plugin_kv WHERE plugin_id=?1", [plugin])
            .map(|_| ())
            .map_err(|e| format!("清空插件存储失败: {e}"))
    }

    // ================= plugin_data：itools.data.*（value 为 JSON 文本，带 updated_at/dirty） =================

    /// 读一条记录的 value（JSON 文本），不存在返回 None。
    pub fn pd_get(&self, ns: &str, key: &str) -> Option<String> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT v FROM plugin_data WHERE ns=?1 AND k=?2",
            (ns, key),
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// 写一条记录（value 为 JSON 文本）。
    pub fn pd_set(
        &self,
        ns: &str,
        key: &str,
        value_json: &str,
        updated_at: u64,
        dirty: bool,
    ) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute(
            "INSERT INTO plugin_data(ns,k,v,updated_at,dirty) VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(ns,k) DO UPDATE SET v=excluded.v, updated_at=excluded.updated_at, dirty=excluded.dirty",
            (ns, key, value_json, updated_at as i64, dirty as i64),
        )
        .map(|_| ())
        .map_err(|e| format!("写本地数据失败: {e}"))
    }

    /// 删除一条记录（不存在视为成功，幂等）。
    pub fn pd_remove(&self, ns: &str, key: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute("DELETE FROM plugin_data WHERE ns=?1 AND k=?2", (ns, key))
            .map(|_| ())
            .map_err(|e| format!("删除失败: {e}"))
    }

    /// 列某 ns 下按前缀过滤的 key（升序）。
    pub fn pd_keys(&self, ns: &str, prefix: &str) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT k FROM plugin_data WHERE ns=?1 ORDER BY k") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([ns], |r| r.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.flatten()
            .filter(|k| prefix.is_empty() || k.starts_with(prefix))
            .collect()
    }

    /// 列某 ns 下全部 (key, value_json, updated_at)（升序）。供调试环境的存储查看器展示
    /// 值与更新时间——`pd_keys` 只给 key，逐条再查一次既慢又拿不到时间戳。
    pub fn pd_entries(&self, ns: &str) -> Vec<(String, String, u64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT k,v,updated_at FROM plugin_data WHERE ns=?1 ORDER BY k")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([ns], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?.max(0) as u64,
            ))
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 清空某命名空间的全部记录（调试环境「清空测试数据」用）。
    pub fn pd_clear_ns(&self, ns: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        conn.execute("DELETE FROM plugin_data WHERE ns=?1", [ns])
            .map(|_| ())
            .map_err(|e| format!("清空本地数据失败: {e}"))
    }

    /// 待上行（dirty）记录：返回 (key, value_json, updated_at)。
    pub fn pd_dirty(&self, ns: &str) -> Vec<(String, String, u64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT k,v,updated_at FROM plugin_data WHERE ns=?1 AND dirty=1")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([ns], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 清除【本次已推送且此后未被覆盖】记录的 dirty：仅当 (k, updated_at) 与推送快照一致才清 0。
    /// **关键**：不能无条件清整个 ns——HTTP 上行在途期间落地的新写入有更新的 `updated_at`，与快照
    /// 不匹配则保留 dirty 留待下轮上行；否则它们会被误标「已同步」而永不上行（静默丢失更新）。
    pub fn pd_clear_dirty(&self, ns: &str, pushed: &[(String, u64, String)]) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|_| "存储锁获取失败".to_string())?;
        for (k, updated_at, value) in pushed {
            conn.execute(
                "UPDATE plugin_data SET dirty=0 WHERE ns=?1 AND k=?2 AND updated_at=?3 AND v=?4 AND dirty=1",
                (ns, k.as_str(), *updated_at as i64, value.as_str()),
            )
            .map_err(|e| format!("清除 dirty 失败: {e}"))?;
        }
        Ok(())
    }

    /// 统计每个插件在 `plugin_kv` 里的**纯本地**记录条数（ns, count）。
    ///
    /// `plugin_kv` 是 `itools.db.*` 的落点，**不参与云同步**。它必须和 [`Self::pd_counts`]
    /// 分开统计、在界面上分开呈现：早先「我的数据」页只数 `plugin_data`，
    /// 于是插件用 `itools.db.*` 存的东西一条都不显示——用户看着「本地 2 条」，
    /// 实际上库里躺着十来条他自己的笔记和密码，数字对不上就等于界面在说假话。
    ///
    /// 返回的 ns 与 `pd_counts` 同构（`plugin:<id>`），这样前端两张表能按同一个键对齐。
    pub fn pkv_counts(&self) -> Vec<(String, u64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) =
            conn.prepare("SELECT plugin_id, COUNT(*) FROM plugin_kv GROUP BY plugin_id")
        else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                crate::plugin::commands::plugin_ns(&r.get::<_, String>(0)?),
                r.get::<_, i64>(1)?.max(0) as u64,
            ))
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 统计每个命名空间的**本地**记录条数（ns, count）。供「我的数据」页展示本地已用。
    /// 空表返回空 Vec；仅统计真实存在的行，不臆造。
    pub fn pd_counts(&self) -> Vec<(String, u64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT ns, COUNT(*) FROM plugin_data GROUP BY ns") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        }) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 列出 `plugin_data` 表中出现过的所有命名空间（去重）。供「立即同步」全量遍历真实数据集。
    pub fn pd_namespaces(&self) -> Vec<String> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let Ok(mut stmt) = conn.prepare("SELECT DISTINCT ns FROM plugin_data") else {
            return Vec::new();
        };
        let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
            return Vec::new();
        };
        rows.flatten().collect()
    }

    /// 回拉合并一条远端记录：远端 `updated_at >= 本地`（或本地无此 key）才采纳，采纳后 dirty=0。
    /// 返回是否采纳（供调用方计 `pulled`）。
    pub fn pd_merge_remote(&self, ns: &str, key: &str, value_json: &str, updated_at: u64) -> bool {
        let Ok(conn) = self.conn.lock() else {
            return false;
        };
        let local: Option<i64> = conn
            .query_row(
                "SELECT updated_at FROM plugin_data WHERE ns=?1 AND k=?2",
                (ns, key),
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        let take = match local {
            Some(l) => updated_at as i64 >= l,
            None => true,
        };
        if take {
            let _ = conn.execute(
                "INSERT INTO plugin_data(ns,k,v,updated_at,dirty) VALUES(?1,?2,?3,?4,0)
                 ON CONFLICT(ns,k) DO UPDATE SET v=excluded.v, updated_at=excluded.updated_at, dirty=0",
                (ns, key, value_json, updated_at as i64),
            );
        }
        take
    }

    // ================= 首启惰性迁移：旧 JSON → SQLite =================

    /// 若各表为空且旧 JSON 存在，一次性导入（不删旧文件，保留可回退）。
    fn migrate_legacy(&self, root: &Path) {
        // 1) app_kv blobs：settings/account/usage/profile（文件名无歧义）
        for (key, file) in [
            ("settings", "settings.json"),
            ("account", "account.json"),
            ("usage", "usage.json"),
            ("profile", "profile.json"),
        ] {
            if self.blob_get(key).is_some() {
                continue; // 已迁过
            }
            if let Ok(s) = std::fs::read_to_string(root.join(file)) {
                // 仅在是合法 JSON 时导入（避免把损坏文件塞进库）
                if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                    self.blob_set(key, &s);
                }
            }
        }

        // 2) plugin_kv：plugin-data/<id>/kv.json
        if let Ok(entries) = std::fs::read_dir(root.join("plugin-data")) {
            for e in entries.flatten() {
                let dir = e.path();
                if !dir.is_dir() {
                    continue;
                }
                let id = e.file_name().to_string_lossy().to_string();
                if !self.pkv_keys(&id, "").is_empty() {
                    continue; // 该插件已迁
                }
                let Ok(s) = std::fs::read_to_string(dir.join("kv.json")) else {
                    continue;
                };
                if let Ok(map) =
                    serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&s)
                {
                    for (k, v) in map {
                        // 旧格式 value 多为 JSON string（bridge 层 stringify 后存入）；
                        // 是字符串则取其内容，否则存其 JSON 文本——保持 get 返回给桥接层再 JSON.parse 的语义。
                        let val = v
                            .as_str()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| v.to_string());
                        let _ = self.pkv_set(&id, &k, &val);
                    }
                }
            }
        }

        // 3) plugin_data：data/<ns>.json（文件名经消毒，尽力恢复 ns：app / plugin:<id>）
        #[derive(serde::Deserialize)]
        struct LegacyRec {
            value: serde_json::Value,
            #[serde(default)]
            updated_at: u64,
            #[serde(default)]
            dirty: bool,
        }
        if let Ok(entries) = std::fs::read_dir(root.join("data")) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let stem = path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
                let ns = if stem == "app" {
                    "app".to_string()
                } else if let Some(rest) = stem.strip_prefix("plugin_") {
                    format!("plugin:{rest}")
                } else {
                    stem.to_string()
                };
                if !self.pd_keys(&ns, "").is_empty() {
                    continue; // 该 ns 已迁
                }
                let Ok(s) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if let Ok(map) =
                    serde_json::from_str::<std::collections::BTreeMap<String, LegacyRec>>(&s)
                {
                    for (k, rec) in map {
                        let _ =
                            self.pd_set(&ns, &k, &rec.value.to_string(), rec.updated_at, rec.dirty);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_kv_roundtrip() {
        let db = Db::open_memory();
        assert_eq!(db.blob_get("settings"), None);
        db.blob_set("settings", r#"{"opacity":120}"#);
        assert_eq!(db.blob_get("settings").as_deref(), Some(r#"{"opacity":120}"#));
        // 覆盖
        db.blob_set("settings", r#"{"opacity":200}"#);
        assert_eq!(db.blob_get("settings").as_deref(), Some(r#"{"opacity":200}"#));
    }

    #[test]
    fn plugin_kv_isolated_by_plugin() {
        let db = Db::open_memory();
        db.pkv_set("a", "k1", "\"v1\"").unwrap();
        db.pkv_set("a", "k2", "\"v2\"").unwrap();
        db.pkv_set("b", "k1", "\"other\"").unwrap();
        assert_eq!(db.pkv_get("a", "k1").as_deref(), Some("\"v1\""));
        assert_eq!(db.pkv_get("b", "k1").as_deref(), Some("\"other\""));
        let mut ka = db.pkv_keys("a", "");
        ka.sort();
        assert_eq!(ka, vec!["k1".to_string(), "k2".to_string()]);
        assert_eq!(db.pkv_keys("a", "k1"), vec!["k1".to_string()]);
        // entries 一次取全（按 key 升序）
        assert_eq!(
            db.pkv_entries("a"),
            vec![
                ("k1".to_string(), "\"v1\"".to_string()),
                ("k2".to_string(), "\"v2\"".to_string())
            ]
        );
        db.pkv_remove("a", "k1").unwrap();
        assert_eq!(db.pkv_get("a", "k1"), None);
        // b 不受影响
        assert_eq!(db.pkv_get("b", "k1").as_deref(), Some("\"other\""));
        // 清空只清一个插件
        db.pkv_clear("a").unwrap();
        assert!(db.pkv_entries("a").is_empty());
        assert_eq!(db.pkv_entries("b").len(), 1, "清空 a 不得动到 b");
    }

    #[test]
    fn plugin_data_dirty_and_merge() {
        let db = Db::open_memory();
        db.pd_set("plugin:x", "a", "1", 100, true).unwrap();
        db.pd_set("plugin:x", "b", "2", 100, true).unwrap();
        assert_eq!(db.pd_get("plugin:x", "a").as_deref(), Some("1"));
        // dirty 两条
        assert_eq!(db.pd_dirty("plugin:x").len(), 2);
        // 清 dirty：只清推送快照 (key, updated_at) 匹配的行
        db.pd_clear_dirty("plugin:x", &[("a".to_string(), 100, "1".to_string()), ("b".to_string(), 100, "2".to_string())])
            .unwrap();
        assert!(db.pd_dirty("plugin:x").is_empty());
        // 竞态修复：上行在途期间被重写的行（updated_at 变新）不能被旧快照清掉（独立 key z，勿污染 a/b）
        db.pd_set("plugin:x", "z", "old", 100, true).unwrap();
        db.pd_set("plugin:x", "z", "new", 100, true).unwrap(); // 模拟在途重写：ua 变新
        db.pd_clear_dirty("plugin:x", &[("z".to_string(), 100, "old".to_string())]).unwrap(); // 旧快照 → 不匹配
        assert!(
            db.pd_dirty("plugin:x").iter().any(|(k, _, _)| k == "z"),
            "在途重写的行不应被旧快照清掉"
        );
        db.pd_clear_dirty("plugin:x", &[("z".to_string(), 100, "new".to_string())]).unwrap(); // 当前快照 → 匹配
        assert!(db.pd_dirty("plugin:x").is_empty());
        db.pd_remove("plugin:x", "z").unwrap(); // 清理，勿影响后续 a/b/c 断言
        // 回拉：更新的远端胜出
        assert!(db.pd_merge_remote("plugin:x", "a", "999", 200));
        assert_eq!(db.pd_get("plugin:x", "a").as_deref(), Some("999"));
        // 回拉：更旧的远端不采纳
        assert!(!db.pd_merge_remote("plugin:x", "a", "111", 150));
        assert_eq!(db.pd_get("plugin:x", "a").as_deref(), Some("999"));
        // 回拉：本地没有的 key 直接采纳
        assert!(db.pd_merge_remote("plugin:x", "c", "3", 50));
        assert_eq!(db.pd_get("plugin:x", "c").as_deref(), Some("3"));
        // keys 前缀
        db.pd_remove("plugin:x", "b").unwrap();
        let mut keys = db.pd_keys("plugin:x", "");
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "c".to_string()]);
        // pd_namespaces 去重列出当前命名空间
        assert_eq!(db.pd_namespaces(), vec!["plugin:x".to_string()]);
        // pd_counts：当前 plugin:x 下有 a、c 两条
        assert_eq!(db.pd_counts(), vec![("plugin:x".to_string(), 2u64)]);
        // pd_entries：一次取全（含 updated_at），供调试存储查看器
        let entries = db.pd_entries("plugin:x");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "a");
        assert_eq!(entries[0].1, "999");
        assert_eq!(entries[0].2, 200);
        // pd_clear_ns：只清该命名空间
        db.pd_set("plugin:y", "keep", "1", 1, false).unwrap();
        db.pd_clear_ns("plugin:x").unwrap();
        assert!(db.pd_entries("plugin:x").is_empty());
        assert_eq!(db.pd_entries("plugin:y").len(), 1, "清空 x 不得动到 y");
    }
}
