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

/// 主界面里一条**插件**记录当前该怎么处理（由 `commands::home_data` 按插件注册表 + 开关判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginEntry {
    /// 照常显示。
    Show,
    /// 本次不显示，但**记录保留**：调试插件在「调试插件参与主搜索」关闭时就是这种
    /// ——开关再打开，它应该原样回来，而不是被悄悄删掉。
    Hide,
    /// 指向的插件已经不存在（卸载 / 改名 / 调试目录被移除）：点了只会报错。
    Gone,
}

/// 一条记录的判定：非插件（应用 / 文件）一律照常显示——后端不去猜它们还在不在
/// （误删用户记录比多显示一条糟得多）；插件类交给调用方的 `judge`。
fn verdict(it: &StoredItem, judge: &impl Fn(&str) -> PluginEntry) -> PluginEntry {
    if it.kind != "plugin" {
        return PluginEntry::Show;
    }
    judge(&it.target)
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
    ///
    /// `judge` 由调用方注入，对**插件类**记录逐条判定当前该显示 / 隐藏 / 清掉
    /// （入参是记录的 `target`，形如 `<插件id>#<code>`，调试插件多带一个 `dev:` 前缀）。
    /// 本模块刻意不认识插件注册表与开关——那是 `commands::home_data` 的事，
    /// 这里只负责「按判定结果过滤 + 把 Gone 的记录真正清掉并落盘」。
    ///
    /// 为什么需要它：主界面的「最近使用 / 已固定」此前**完全绕开**了「调试插件参与主搜索」
    /// 开关——用户短暂打开过开关、用过一次调试插件后，即便关掉开关，那条 `dev:` 项仍常驻
    /// 主界面且点了照样开调试窗，等于开关只管住了搜索索引这一半。
    pub fn snapshot(
        &self,
        judge: impl Fn(&str) -> PluginEntry,
    ) -> (Vec<SearchItem>, Vec<SearchItem>) {
        let Ok(mut data) = self.data.lock() else {
            return (Vec::new(), Vec::new());
        };
        // 「最近使用」里指向已不存在插件的记录直接清掉：它是自动积累的 MRU 列表，
        // 残留项点了只会在 console 报错，留着是纯噪音。清完立刻落盘（有变更才写）。
        let before = data.recent.len();
        data.recent
            .retain(|e| verdict(&e.item, &judge) != PluginEntry::Gone);
        if data.recent.len() != before {
            self.save(&data);
        }
        let recent = data
            .recent
            .iter()
            .filter(|e| {
                (e.item.kind == "app" || e.item.kind == "plugin")
                    && verdict(&e.item, &judge) == PluginEntry::Show
            })
            .map(|e| e.item.to_item())
            .collect();
        // 「已固定」是用户的**显式动作**：插件只是暂时加载不出来（清单写坏 / 目录暂时不可达）
        // 时把它删掉，用户修好后还得重新固定一次。所以这边只隐藏、不删——
        // 插件回来了，固定项也原样回来。
        let pinned = data
            .pinned
            .iter()
            .filter(|p| verdict(p, &judge) == PluginEntry::Show)
            .map(|p| p.to_item())
            .collect();
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

    /// 一条插件记录：id 形如 `plugin::<插件id>#<code>`、target 形如 `<插件id>#<code>`
    /// （与 `PluginCommand::to_item` 完全一致，调试插件的插件 id 带 `dev:` 前缀）。
    fn plugin_item(plugin_id: &str) -> SearchItem {
        SearchItem {
            id: format!("plugin::{plugin_id}#main"),
            title: plugin_id.to_string(),
            subtitle: String::new(),
            kind: "plugin".to_string(),
            target: format!("{plugin_id}#main"),
            icon: None,
            action: "plugin".to_string(),
        }
    }

    /// 一切照常显示（多数用例不关心过滤）。
    fn show_all(_: &str) -> PluginEntry {
        PluginEntry::Show
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
        let (recent, pinned) = store2.snapshot(show_all);
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
        let (_, pinned) = store2.snapshot(show_all);
        assert!(pinned.is_empty());
    }

    /// 调试插件在「调试插件参与主搜索」关闭时**不出现在主界面**，但记录保留；
    /// 指向已不存在插件的「最近使用」记录被永久清掉（含落盘）。
    ///
    /// 这条是本轮修的越界：开关此前只管住了搜索索引，主界面的「最近使用」完全绕开它
    /// ——关掉开关后那条 `dev:` 项照样常驻、点了照样开调试窗。
    #[test]
    fn dev_entries_follow_the_switch_and_dead_ones_are_purged() {
        let db = Arc::new(Db::open_memory());
        let store = UsageStore::load(db.clone());
        store.record(&item("app-a", "app"));
        store.record(&plugin_item("demo")); // 正式插件，还在
        store.record(&plugin_item("dev:demo")); // 调试插件（与正式插件同名）
        store.record(&plugin_item("gone")); // 已卸载的插件
        assert!(store.toggle_pin(&plugin_item("dev:demo")));
        assert!(store.toggle_pin(&plugin_item("gone")));

        // 判定：调试插件隐藏（开关关着）、gone 已不存在、其余照常
        let judge = |target: &str| match target.split('#').next().unwrap_or(target) {
            "dev:demo" => PluginEntry::Hide,
            "gone" => PluginEntry::Gone,
            _ => PluginEntry::Show,
        };
        let (recent, pinned) = store.snapshot(judge);
        let ids: Vec<&str> = recent.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"app-a"), "应用不受影响: {ids:?}");
        assert!(ids.contains(&"plugin::demo#main"), "正式插件照常显示: {ids:?}");
        assert!(
            !ids.contains(&"plugin::dev:demo#main"),
            "开关关着时调试插件不该出现在主界面: {ids:?}"
        );
        assert!(!ids.contains(&"plugin::gone#main"), "已不存在的插件必须清掉: {ids:?}");
        assert!(pinned.is_empty(), "两条固定项一条隐藏一条已不存在，都不该显示");

        // 开关再打开：调试插件原样回来（Hide 只是不显示，绝不能顺手删掉记录）
        let (recent, pinned) = store.snapshot(|t| {
            if t.starts_with("gone") {
                PluginEntry::Gone
            } else {
                PluginEntry::Show
            }
        });
        assert!(
            recent.iter().any(|r| r.id == "plugin::dev:demo#main"),
            "隐藏过的调试插件记录必须还在"
        );
        assert!(pinned.iter().any(|p| p.id == "plugin::dev:demo#main"));

        // gone 的**最近使用**记录已从库里真删（换个 store 从同一库重载仍然没有）；
        // 而它的**固定项**只是不显示——用户显式固定的东西不因插件一时加载不出来就被删
        let store2 = UsageStore::load(db.clone());
        let (recent, pinned) = store2.snapshot(show_all);
        assert!(
            !recent.iter().any(|r| r.id == "plugin::gone#main"),
            "残留的最近使用记录应已落盘删除"
        );
        assert!(
            pinned.iter().any(|p| p.id == "plugin::gone#main"),
            "固定项只隐藏不删除"
        );
    }
}
