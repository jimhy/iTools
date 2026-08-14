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

    // ---------- 网络代理（**真实生效**：全部出站请求经 `crate::http` 走它） ----------
    /// 是否启用出站代理。开启后**所有**出站 HTTP 都经代理：插件下载 / 更新检查 / 下载 msi /
    /// 云同步 / 登录登出注销 / 插件的 `itools.fetch`。
    ///
    /// **本机与内网地址永远直连**（`crate::http::is_bypass_host`：`localhost`、`127.0.0.0/8`、
    /// `::1`、`10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`、`*.local`）——用户的代理常是
    /// `127.0.0.1:7897`，而本地服务端是 `127.0.0.1:8787`，不绕过就会把本机联调整条掐断。
    pub proxy_enabled: bool,
    /// 代理地址，形如 `127.0.0.1:7897`（无 scheme 视为 http）。
    ///
    /// 支持 `http://` / `https://`（后者按 HTTP CONNECT 处理）与
    /// `socks5://` / `socks5h://` / `socks://` / `socks4://` / `socks4a://`，
    /// 均可带 `user:pass@`——但 **SOCKS4 / 4A 例外**：协议没有鉴权字段，带凭据会被拒绝。
    /// 各形态到 ureq 的确切行为见 `crate::http::normalize_proxy` 的「SOCKS 支持形态」表。
    ///
    /// 由 `save_settings` 用 `crate::http::normalize_proxy` **校验后才允许保存**：
    /// 端口非法 / 缺失一律拒绝，绝不静默回落成直连（那等于「填了不生效」）。
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
    /// 插件下载源偏好："auto"（官方+镜像竞速，默认）/ "official"（只走官方直连）/ "mirror"（优先镜像）。
    /// 由 `plugin_mirror_set_mode` 独占写入；运行期镜像模块读它决定候选源（见 `plugin::mirror`）。
    #[serde(default = "default_mirror_mode")]
    pub plugin_mirror_mode: String,

    // ---------- 截图（宿主内置，原生覆盖层，无界面/像 PixPin） ----------
    /// 截图全局快捷键（默认 "ctrl+shift+a"，可改；空 = 不注册）
    pub screenshot_hotkey: String,
    /// 贴图全局快捷键（宿主内置原生贴图：读剪贴板图片贴成置顶浮窗；默认 "f3"，空 = 不注册）
    #[serde(default = "default_pin_hotkey")]
    pub pin_hotkey: String,

    // ---------- 插件调试环境（开发者中心，后端独占写入） ----------
    /// 用户手动添加的调试插件目录（固定的 `dev-plugins/` 不在此列，它不可移除）。
    /// 由 `dev_add_dir` / `dev_remove_dir` 独占写入。
    #[serde(default)]
    pub dev_plugin_dirs: Vec<String>,
    /// **调试环境独立的**授权表：调试插件名 → 已授权能力。
    /// 与 `plugin_permissions` 完全分开——调试时随手开的授权不该让正式插件也拿到能力。
    /// 由 `dev_set_permission` 独占写入。
    #[serde(default)]
    pub dev_plugin_permissions: HashMap<String, Vec<String>>,
    /// 调试插件是否并入主搜索（默认 false：调试插件不该污染日常搜索）。
    /// 由 `dev_set_search_visible` 独占写入。
    #[serde(default)]
    pub dev_search_visible: bool,

    // ---------- 云同步 ----------
    /// 云同步服务器地址（用户在「设置 → 网络」里手动填写，如 https://cloud.example.com:7101）。
    /// **绝不写死在源码、也不随仓库上传**；release 未填 = 云端未接入（诚实降级为纯本地）。空 = 未接入。
    #[serde(default)]
    pub sync_endpoint: String,
}

fn default_pin_hotkey() -> String {
    "f3".to_string()
}

fn default_mirror_mode() -> String {
    "auto".to_string()
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
            plugin_mirror_mode: default_mirror_mode(),
            screenshot_hotkey: "ctrl+shift+a".to_string(),
            pin_hotkey: "f3".to_string(),
            dev_plugin_dirs: Vec::new(),
            dev_plugin_permissions: HashMap::new(),
            dev_search_visible: false,
            sync_endpoint: String::new(),
        }
    }
}

/// **后端独占写入的字段**：整包保存（`commands::save_settings`）必须原样保留 store 现值。
///
/// 这些字段各自由专用命令 / 后端事件独占维护，而前端设置页保存的是一份**整包快照**：
/// 快照里这些字段要么是页面加载时的旧值、要么（TS 声明里压根没有该字段时）是缺省空值，
/// 一旦写回就是静默丢数据。已经踩过的两个坑：
/// - `plugin_mirror_mode`：任意一次「保存设置」把用户刚选的 official / mirror 重置回 auto；
/// - `plugin_window_sizes`：前端 `AppSettings` 里没有这个字段，保存一次就把所有插件
///   调好的窗口尺寸清空（下次打开插件回到默认 960×660）。
///
/// **新增后端独占字段时必须在这里补一行**，否则下一次整包保存就会吃掉它。
pub fn preserve_backend_owned(next: &mut AppSettings, old: &AppSettings) {
    // 本地启动清单：add/remove_launch_items 独占
    next.local_launch_items = old.local_launch_items.clone();
    // 插件禁用清单：set_plugin_enabled / delete_plugin 独占
    next.disabled_plugins = old.disabled_plugins.clone();
    // 插件授权表：set_plugin_permission / delete_plugin / 换源安装 独占
    next.plugin_permissions = old.plugin_permissions.clone();
    // 插件下载源偏好：plugin_mirror_set_mode 独占
    next.plugin_mirror_mode = old.plugin_mirror_mode.clone();
    // 插件窗口尺寸：插件窗口 resize / 关闭时由 SettingsStore::set_plugin_window 独占写入
    next.plugin_window_sizes = old.plugin_window_sizes.clone();
    // 调试插件目录：dev_add_dir / dev_remove_dir 独占
    next.dev_plugin_dirs = old.dev_plugin_dirs.clone();
    // 调试授权表：dev_set_permission 独占
    next.dev_plugin_permissions = old.dev_plugin_permissions.clone();
    // 「调试插件进主搜索」开关：dev_set_search_visible 独占
    next.dev_search_visible = old.dev_search_visible;
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

    /// 整包保存不许吃掉后端独占字段：前端传来的**空快照**必须被 store 现值覆盖回去，
    /// 而普通设置项（前端拥有）必须照常生效。
    #[test]
    fn backend_owned_fields_survive_full_save() {
        // store 现值：后端独占字段都有内容
        let mut old = AppSettings::default();
        old.local_launch_items.push(LaunchItem {
            id: r"C:\x\a.exe".to_string(),
            path: r"C:\x\a.exe".to_string(),
            name: "a.exe".to_string(),
            is_dir: false,
        });
        old.disabled_plugins.push("demo".to_string());
        old.plugin_permissions
            .insert("demo".to_string(), vec!["network".to_string()]);
        old.plugin_mirror_mode = "official".to_string();
        old.plugin_window_sizes
            .insert("demo".to_string(), [1200.0, 800.0]);
        old.dev_plugin_dirs.push(r"C:\work\my-plugin".to_string());
        old.dev_plugin_permissions
            .insert("demo".to_string(), vec!["runCommand".to_string()]);
        old.dev_search_visible = true;

        // 前端整包快照：后端独占字段全空（TS 声明里没有 plugin_window_sizes 时就是这形态），
        // 只改了一个真正属于前端的设置项
        let mut incoming = AppSettings {
            opacity: 90,
            ..AppSettings::default()
        };
        preserve_backend_owned(&mut incoming, &old);

        assert_eq!(incoming.opacity, 90, "前端拥有的设置项必须照常生效");
        assert_eq!(incoming.local_launch_items.len(), 1);
        assert_eq!(incoming.disabled_plugins, vec!["demo".to_string()]);
        assert!(incoming.plugin_permissions.contains_key("demo"));
        assert_eq!(incoming.plugin_mirror_mode, "official");
        assert_eq!(
            incoming.plugin_window_sizes.get("demo").copied(),
            Some([1200.0, 800.0]),
            "插件窗口尺寸由后端独占写入，整包保存不得清空"
        );
        assert_eq!(incoming.dev_plugin_dirs.len(), 1, "调试目录不得被整包保存清空");
        assert!(
            incoming.dev_plugin_permissions.contains_key("demo"),
            "调试授权表不得被整包保存清空"
        );
        assert!(incoming.dev_search_visible, "「调试插件进主搜索」开关不得被整包保存重置");
    }
}
