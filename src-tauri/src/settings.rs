//! 应用设置：数据模型 + 持久化（统一 SQLite 库的 `app_kv['settings']`，整体 JSON blob）。
//! 读写全程容错——数据损坏/缺失回退默认值。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// 「从不」自动清除搜索框的哨兵值（与前端 `AUTO_CLEAR_NEVER` 对齐）。
pub const AUTO_CLEAR_NEVER: u32 = u32::MAX;

/// 本地启动的一条目：随 iTools 启动逐个打开的文件/文件夹/程序。
/// 序列化结构与前端 `LaunchItem`（src/types.ts）保持一致。
#[derive(Clone, Serialize, Deserialize)]
pub struct LaunchItem {
    /// 唯一标识 = 绝对路径（去重、删除都按它）
    pub id: String,
    /// 目标绝对路径
    pub path: String,
    /// 显示名（路径末段）
    pub name: String,
    /// 是否为文件夹
    pub is_dir: bool,
}

/// 全部设置项。加 `serde(default)`：老版本配置文件缺字段时用默认值补齐。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Acrylic 毛玻璃底色透明度（0=全透 ~ 255=不透，默认 180）
    pub opacity: u8,
    /// 自定义背景图片绝对路径（None = 不用背景图）
    pub background_image: Option<String>,
    /// 唤起快捷键，如 "alt+space"、"ctrl+alt+k"（小写、`+` 分隔）
    pub hotkey: String,
    /// 手动添加进搜索库的程序路径（exe/lnk）
    pub custom_apps: Vec<String>,
    /// 开机自启
    pub autostart: bool,

    // ---------- 外观 ----------
    /// 主题："system" 跟随系统 / "light" / "dark"（默认 "system"）
    pub theme: String,
    /// 是否启用背景图（关掉则不渲染，但保留已选路径）
    pub background_enabled: bool,
    /// 背景图暗化程度 0-100（0=不暗化，越大越暗）
    pub background_dim: u8,

    // ---------- 使用偏好 ----------
    /// 搜索框占位符（空 = 用默认问候语）
    pub search_placeholder: String,
    /// 分离独立窗口的快捷键（默认 "ctrl+d"）
    pub separate_hotkey: String,
    /// 失焦后自动清除搜索内容的秒数：0=立即，60..=600=1~10 分钟，`AUTO_CLEAR_NEVER`=从不
    pub auto_clear_seconds: u32,

    // ---------- 网络代理（模拟，暂不接真实代理栈） ----------
    /// 是否启用代理
    pub proxy_enabled: bool,
    /// 代理地址（如 "127.0.0.1:7890"）
    pub proxy_address: String,

    // ---------- 本地启动（可搜索的自定义启动项，仅手动/搜索打开，不开机自动打开） ----------
    /// 本地启动清单：加入后可在主搜索栏搜到并打开、或在面板里「立即启动」
    pub local_launch_items: Vec<LaunchItem>,

    // ---------- 插件 ----------
    /// 被禁用的插件名清单：仍加载展示于「插件管理」，但不参与主搜索
    pub disabled_plugins: Vec<String>,
    /// 按插件已授权的高危能力：插件名 → 已授权能力（如 ["runCommand","network"]）
    pub plugin_permissions: HashMap<String, Vec<String>>,
    /// 每个插件上次的窗口尺寸（逻辑像素 [宽, 高]）：下次打开该插件时还原到此尺寸。
    #[serde(default)]
    pub plugin_window_sizes: HashMap<String, [f64; 2]>,

    // ---------- 截图（宿主内置，原生覆盖层，无界面/像 PixPin） ----------
    /// 截图全局快捷键（默认 "ctrl+shift+a"，可改；空 = 不注册）
    pub screenshot_hotkey: String,
    /// 贴图全局快捷键（宿主内置原生贴图：读剪贴板图片贴成置顶浮窗；默认 "f3"，空 = 不注册）
    #[serde(default = "default_pin_hotkey")]
    pub pin_hotkey: String,

    // ---------- 云同步 ----------
    /// 云同步服务器地址（用户在「账号 → 数据同步」里手动填写，如 https://api.jimhy.cn:7010）。
    /// **绝不写死在源码、也不随仓库上传**；release 未填 = 云端未接入（诚实降级为纯本地）。空 = 未接入。
    #[serde(default)]
    pub sync_endpoint: String,
}

fn default_pin_hotkey() -> String {
    "f3".to_string()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            opacity: 180,
            background_image: None,
            hotkey: "alt+space".to_string(),
            custom_apps: Vec::new(),
            autostart: false,
            theme: "system".to_string(),
            background_enabled: true,
            background_dim: 0,
            search_placeholder: String::new(),
            separate_hotkey: "ctrl+d".to_string(),
            auto_clear_seconds: AUTO_CLEAR_NEVER,
            proxy_enabled: false,
            proxy_address: String::new(),
            local_launch_items: Vec::new(),
            disabled_plugins: Vec::new(),
            plugin_permissions: HashMap::new(),
            plugin_window_sizes: HashMap::new(),
            screenshot_hotkey: "ctrl+shift+a".to_string(),
            pin_hotkey: "f3".to_string(),
            sync_endpoint: String::new(),
        }
    }
}

/// 线程安全的设置存储；每次保存立即落盘
pub struct SettingsStore {
    db: Arc<Db>,
    data: Mutex<AppSettings>,
}

impl SettingsStore {
    pub fn load(db: Arc<Db>) -> Self {
        let data = db
            .blob_get("settings")
            .and_then(|s| serde_json::from_str::<AppSettings>(&s).ok())
            .unwrap_or_default();
        Self {
            db,
            data: Mutex::new(data),
        }
    }

    pub fn get(&self) -> AppSettings {
        self.data
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn set(&self, next: AppSettings) {
        if let Ok(mut guard) = self.data.lock() {
            *guard = next.clone();
        }
        if let Ok(json) = serde_json::to_string(&next) {
            self.db.blob_set("settings", &json);
        }
    }

    /// 读某插件上次保存的窗口尺寸（逻辑像素 [宽,高]）。无记录返回 None。
    pub fn get_plugin_window(&self, id: &str) -> Option<[f64; 2]> {
        self.data
            .lock()
            .ok()
            .and_then(|g| g.plugin_window_sizes.get(id).copied())
    }

    /// 保存某插件当前窗口尺寸（下次打开还原）。只在尺寸有效（>0）时写。
    pub fn set_plugin_window(&self, id: &str, size: [f64; 2]) {
        if size[0] < 1.0 || size[1] < 1.0 {
            return;
        }
        let mut s = self.get();
        s.plugin_window_sizes.insert(id.to_string(), size);
        self.set(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设置保存/加载往返 + 缺字段回退默认（共享内存库）
    #[test]
    fn settings_roundtrip() {
        let db = Arc::new(Db::open_memory());

        let store = SettingsStore::load(db.clone());
        assert_eq!(store.get().opacity, 180, "默认透明度 180");
        assert_eq!(store.get().hotkey, "alt+space");
        // 新增字段默认值
        assert_eq!(store.get().theme, "system");
        assert!(store.get().background_enabled);
        assert_eq!(store.get().separate_hotkey, "ctrl+d");
        assert_eq!(store.get().auto_clear_seconds, AUTO_CLEAR_NEVER);
        assert!(store.get().local_launch_items.is_empty());

        let mut s = store.get();
        s.opacity = 120;
        s.hotkey = "ctrl+alt+k".to_string();
        s.custom_apps.push(r"C:\x\a.exe".to_string());
        s.theme = "dark".to_string();
        s.local_launch_items.push(LaunchItem {
            id: r"C:\x\a.exe".to_string(),
            path: r"C:\x\a.exe".to_string(),
            name: "a.exe".to_string(),
            is_dir: false,
        });
        store.set(s);

        let store2 = SettingsStore::load(db.clone());
        let loaded = store2.get();
        assert_eq!(loaded.opacity, 120);
        assert_eq!(loaded.hotkey, "ctrl+alt+k");
        assert_eq!(loaded.custom_apps.len(), 1);
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.local_launch_items.len(), 1);
        assert_eq!(loaded.local_launch_items[0].name, "a.exe");

        // 损坏数据回退默认
        db.blob_set("settings", "not json");
        let store3 = SettingsStore::load(db.clone());
        assert_eq!(store3.get().opacity, 180);
        assert_eq!(store3.get().theme, "system");
    }
}
