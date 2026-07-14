//! 主面板数据的持久化存储：最近使用（带次数/时间戳）与已固定项。
//! 落盘位置：统一 SQLite 库的 `app_kv['usage']`（整体 JSON blob），读写全程容错——损坏/缺失按空数据处理。

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::search::SearchItem;

/// 最近使用的容量上限（前端折叠展示，展开后最多看到这么多）
const RECENT_CAP: usize = 50;

/// 持久化的条目（SearchItem 去掉 icon——图标由前端按 target 重新提取）
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredItem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub kind: String,
    pub target: String,
    pub action: String,
    /// 插件 logo（base64 data URL）：插件图标无法按 target 重新提取，故随最近使用/固定一并持久化。
    /// 应用/文件图标仍由前端按 target 重新提取，不在此存（省空间）。旧数据缺此字段时默认 None。
    #[serde(default)]
    pub icon: Option<String>,
}

impl StoredItem {
    fn from_item(item: &SearchItem) -> Self {
        Self {
            id: item.id.clone(),
            title: item.title.clone(),
            subtitle: item.subtitle.clone(),
            kind: item.kind.clone(),
            target: item.target.clone(),
            action: item.action.clone(),
            // 仅插件保留图标（其 logo 无法按 target 重新提取）；应用图标由前端重新提取
            icon: if item.kind == "plugin" {
                item.icon.clone()
            } else {
                None
            },
        }
    }

    fn to_item(&self) -> SearchItem {
        SearchItem {
            id: self.id.clone(),
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            kind: self.kind.clone(),
            target: self.target.clone(),
            icon: self.icon.clone(),
            action: self.action.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct RecentEntry {
    item: StoredItem,
    count: u64,
    last_used: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct UsageData {
    recent: Vec<RecentEntry>,
    pinned: Vec<StoredItem>,
}

/// 线程安全的使用记录存储；每次变更立即落盘（数据量小，无需批量）
pub struct UsageStore {
    db: Arc<Db>,
    data: Mutex<UsageData>,
}

impl UsageStore {
    /// 从统一 SQLite 库加载（不存在/损坏 → 空数据）
    pub fn load(db: Arc<Db>) -> Self {
        let data = db
            .blob_get("usage")
            .and_then(|s| serde_json::from_str::<UsageData>(&s).ok())
            .unwrap_or_default();
        Self {
            db,
            data: Mutex::new(data),
        }
    }

    /// 记录一次执行。「最近使用」收录应用与插件；文件/文件夹/即时命令不进入。
    pub fn record(&self, item: &SearchItem) {
        if item.kind != "app" && item.kind != "plugin" {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let Ok(mut data) = self.data.lock() else {
            return;
        };
        if let Some(entry) = data.recent.iter_mut().find(|e| e.item.id == item.id) {
            entry.count += 1;
            entry.last_used = now;
        } else {
            data.recent.push(RecentEntry {
                item: StoredItem::from_item(item),
                count: 1,
                last_used: now,
            });
        }
        // 最近优先排序并截断
        data.recent.sort_by_key(|e| std::cmp::Reverse(e.last_used));
        data.recent.truncate(RECENT_CAP);
        self.save(&data);
    }

    /// 固定/取消固定，返回操作后是否处于固定状态
    pub fn toggle_pin(&self, item: &SearchItem) -> bool {
        let Ok(mut data) = self.data.lock() else {
            return false;
        };
        if let Some(pos) = data.pinned.iter().position(|p| p.id == item.id) {
            data.pinned.remove(pos);
            self.save(&data);
            false
        } else {
            data.pinned.push(StoredItem::from_item(item));
            self.save(&data);
            true
        }
    }

    /// 快照：（最近使用（按时间倒序，应用与插件）、已固定）。
    /// 读取时也按 kind 过滤，兼容旧数据里可能残留的文件/命令记录。
    pub fn snapshot(&self) -> (Vec<SearchItem>, Vec<SearchItem>) {
        let Ok(data) = self.data.lock() else {
            return (Vec::new(), Vec::new());
        };
        let recent = data
            .recent
            .iter()
            .filter(|e| e.item.kind == "app" || e.item.kind == "plugin")
            .map(|e| e.item.to_item())
            .collect();
        let pinned = data.pinned.iter().map(|p| p.to_item()).collect();
        (recent, pinned)
    }

    fn save(&self, data: &UsageData) {
        if let Ok(json) = serde_json::to_string(data) {
            self.db.blob_set("usage", &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str) -> SearchItem {
        SearchItem {
            id: id.to_string(),
            title: id.to_string(),
            subtitle: String::new(),
            kind: kind.to_string(),
            target: id.to_string(),
            icon: None,
            action: "open".to_string(),
        }
    }

    /// 记录/固定/落盘往返（共享同一内存库，第二个 store 从库里重载验证持久化）
    #[test]
    fn store_roundtrip() {
        let db = Arc::new(Db::open_memory());

        let store = UsageStore::load(db.clone());
        store.record(&item("a", "app"));
        store.record(&item("c", "app"));
        store.record(&item("b", "file")); // 文件不进「最近使用」
        store.record(&item("a", "app")); // a 第二次，应排最前
        store.record(&item("noise", "command")); // command 不记录
        store.record(&item("p", "plugin")); // 插件应进「最近使用」
        assert!(store.toggle_pin(&item("b", "file"))); // 固定不限类型

        // 从同一库重新加载验证持久化
        let store2 = UsageStore::load(db.clone());
        let (recent, pinned) = store2.snapshot();
        assert_eq!(recent.len(), 3, "最近使用应含 2 个应用 + 1 个插件");
        assert!(
            recent.iter().all(|r| r.kind == "app" || r.kind == "plugin"),
            "最近使用只应含应用与插件"
        );
        // 插件确实进入最近使用（本测试内多条记录落在同一秒，稳定排序保持插入序，故不断言具体名次）
        assert!(recent.iter().any(|r| r.id == "p" && r.kind == "plugin"), "插件应进入最近使用");
        assert!(recent.iter().any(|r| r.id == "a" && r.kind == "app"), "应用应在最近使用中");
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].id, "b");

        // 再次 toggle 取消固定
        assert!(!store2.toggle_pin(&item("b", "file")));
        let (_, pinned) = store2.snapshot();
        assert!(pinned.is_empty());
    }
}
