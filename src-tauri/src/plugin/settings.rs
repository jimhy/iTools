//! 插件详情页后端：README 读取 + schema 驱动的声明式设置系统。
//!
//! 设计（详见 `skills/itools-plugin-dev/references/plugin-settings-spec.md`）：
//! - **schema**：插件目录放 `settings.json`（声明配置项，只读，随插件走）。
//! - **用户值**：存 `%LOCALAPPDATA%\itools\plugin-data\<id>\settings.json`（与业务 `kv.json` 分开、
//!   保留原始 JSON 类型）。首版纯本地；此存储层可后续切到可同步的 `DataStore` 而不改契约。
//! - **管理中心**（main/admin 窗口）按 `name` 显式读写：`plugin_settings_schema/values/set/reset`。
//! - **插件运行时**（plugin 窗口）经 `current_plugin` 隔离、**只读**合并值：`plugin_get_settings/plugin_get_setting`。
//! - 管理中心写入后 emit `plugin-settings-changed`（payload=插件 id），运行中的插件页据此实时刷新
//!   （`itools.settings.onChange`）。
//!
//! 诚信闭环：管理中心写的值与插件运行时读到的值**是同一份**（同一 `settings.json` 值文件 + 同一套
//! default 合并逻辑），杜绝「UI 能改、插件读不到」的假控件。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::{AppHandle, Manager, State};

use super::commands::current_plugin;
use super::PluginRegistry;

// ==================== schema 结构 ====================

/// 一个下拉选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsOption {
    pub value: Value,
    #[serde(default)]
    pub label: String,
}

/// 一个设置项声明。`key` 是存储键；`kind`（JSON `type`）决定前端渲染的控件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsItem {
    /// 存储键（插件内唯一）。
    pub key: String,
    /// 控件类型：text|textarea|number|boolean|select|path|color|hotkey，缺省 text。
    #[serde(rename = "type", default = "default_item_type")]
    pub kind: String,
    /// 展示标题。
    #[serde(default)]
    pub label: String,
    /// 辅助说明（控件下方小字）。
    #[serde(default)]
    pub description: String,
    /// 默认值（用户未设定时生效；也是运行时合并的兜底）。
    #[serde(default)]
    pub default: Value,
    /// select 的选项。
    #[serde(default)]
    pub options: Vec<SettingsOption>,
    /// number 的范围/步进。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// path 的选择模式：file|folder，缺省 folder。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// 输入框占位提示（text/textarea/number/path）。
    #[serde(default)]
    pub placeholder: String,
}

fn default_item_type() -> String {
    "text".to_string()
}

/// 一组设置项（前端按组分节渲染）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsGroup {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub items: Vec<SettingsItem>,
}

/// 给前端的规范化 schema（顶层永远是 groups）。
#[derive(Debug, Clone, Serialize)]
pub struct SettingsSchema {
    pub version: u32,
    pub groups: Vec<SettingsGroup>,
}

/// settings.json 原始形态：允许 `groups`（分组）或顶层 `items`（无分组简写），二选一或并存。
#[derive(Debug, Deserialize)]
struct RawSchema {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    groups: Vec<SettingsGroup>,
    #[serde(default)]
    items: Vec<SettingsItem>,
}

fn default_version() -> u32 {
    1
}

impl RawSchema {
    /// 归一：顶层 `items` 简写包成一个匿名分组，接在显式 groups 之后。
    fn normalized_groups(self) -> Vec<SettingsGroup> {
        let mut groups = self.groups;
        if !self.items.is_empty() {
            groups.push(SettingsGroup {
                title: String::new(),
                description: String::new(),
                items: self.items,
            });
        }
        groups
    }

    fn into_schema(self) -> SettingsSchema {
        let version = self.version;
        SettingsSchema {
            version,
            groups: self.normalized_groups(),
        }
    }
}

/// 解析某插件目录的 settings.json；不存在返回 None，解析失败返回 Err。
fn read_schema(dir: &std::path::Path) -> Result<Option<RawSchema>, String> {
    match std::fs::read_to_string(dir.join("settings.json")) {
        Ok(text) => serde_json::from_str::<RawSchema>(&text)
            .map(Some)
            .map_err(|e| format!("settings.json 解析失败: {e}")),
        Err(_) => Ok(None),
    }
}

// ==================== 用户值存储 ====================

/// 用户设定值文件：`plugin-data/<id>/settings.json`（与业务 kv.json 同目录、分开存）。
fn values_path(id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("itools")
        .join("plugin-data")
        .join(id)
        .join("settings.json")
}

/// 读用户值（仅含用户显式设定过的项）。文件缺失视作空。
fn load_values(id: &str) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(values_path(id)) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("插件设置存储损坏: {e}")),
        Err(_) => Ok(Map::new()),
    }
}

/// 写用户值（保留原始 JSON 类型，pretty 便于人工排查）。
fn save_values(id: &str, map: &Map<String, Value>) -> Result<(), String> {
    let path = values_path(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写插件设置失败: {e}"))
}

/// 合并「schema 默认值 + 用户值」：先铺 schema 里每项的 default（非 null），再用用户值覆盖。
/// 这就是插件运行时 `itools.settings.get/all` 读到的最终值——与管理中心写入的是同一份。
fn merged_values(registry: &PluginRegistry, id: &str) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(dir) = registry.plugin_dir(id) {
        if let Ok(Some(raw)) = read_schema(&dir) {
            for group in raw.normalized_groups() {
                for item in group.items {
                    if !item.default.is_null() {
                        out.insert(item.key, item.default);
                    }
                }
            }
        }
    }
    if let Ok(user) = load_values(id) {
        for (k, v) in user {
            // null 视为「未设定」→ 保留 schema 默认（number 输入框清空会存 null，不应盖掉默认值）
            if v.is_null() {
                continue;
            }
            out.insert(k, v);
        }
    }
    out
}

/// 通知运行中的插件页：某插件的设置变了。走与 onHotkey 相同的事件总线
/// （`webview.eval` → 插件页 `window.__itoolsEmit`），payload=插件 id，页面自行比对是否是自己。
fn emit_changed(app: &AppHandle, id: &str) {
    if let Some(win) = app.get_webview_window("plugin") {
        let payload = serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string());
        let js = format!("window.__itoolsEmit && window.__itoolsEmit('settings-changed', {payload})");
        let _ = win.eval(&js);
    }
}

// ==================== 命令：管理中心（按 name 显式） ====================

/// 读某插件的 README.md（详情页「说明」tab）。不存在返回 None（前端诚实显示占位）。
#[tauri::command]
pub fn plugin_readme(
    name: String,
    registry: State<'_, PluginRegistry>,
) -> Result<Option<String>, String> {
    let Some(dir) = registry.plugin_dir(&name) else {
        return Ok(None);
    };
    match std::fs::read_to_string(dir.join("README.md")) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

/// 读某插件的设置 schema（详情页「设置」tab 据此渲染表单）。无 settings.json 返回 None。
#[tauri::command]
pub fn plugin_settings_schema(
    name: String,
    registry: State<'_, PluginRegistry>,
) -> Result<Option<SettingsSchema>, String> {
    let Some(dir) = registry.plugin_dir(&name) else {
        return Ok(None);
    };
    Ok(read_schema(&dir)?.map(RawSchema::into_schema))
}

/// 读某插件当前生效的全部设置值（schema 默认 + 用户覆盖），供管理中心表单回填。
#[tauri::command]
pub fn plugin_settings_values(
    name: String,
    registry: State<'_, PluginRegistry>,
) -> Result<Map<String, Value>, String> {
    Ok(merged_values(&registry, &name))
}

/// 管理中心即时保存某插件的一个设置值；写后广播变更事件。
#[tauri::command]
pub fn plugin_settings_set(
    app: AppHandle,
    name: String,
    key: String,
    value: Value,
) -> Result<(), String> {
    let mut map = load_values(&name)?;
    map.insert(key, value);
    save_values(&name, &map)?;
    emit_changed(&app, &name);
    Ok(())
}

/// 重置某插件的全部用户设置值（清空覆盖 → 回到 schema 默认）；写后广播变更事件。
#[tauri::command]
pub fn plugin_settings_reset(app: AppHandle, name: String) -> Result<(), String> {
    save_values(&name, &Map::new())?;
    emit_changed(&app, &name);
    Ok(())
}

// ==================== 命令：插件运行时（current_plugin 隔离，只读） ====================

/// 插件读取自己的全部设置（合并 default + 用户值）。供 `itools.settings.all()`。
#[tauri::command]
pub fn plugin_get_settings(
    registry: State<'_, PluginRegistry>,
) -> Result<Map<String, Value>, String> {
    let id = current_plugin(&registry)?;
    Ok(merged_values(&registry, &id))
}

/// 插件读取自己的某个设置项（不存在返回 JSON null）。供 `itools.settings.get(key)`。
#[tauri::command]
pub fn plugin_get_setting(
    key: String,
    registry: State<'_, PluginRegistry>,
) -> Result<Value, String> {
    let id = current_plugin(&registry)?;
    Ok(merged_values(&registry, &id)
        .get(&key)
        .cloned()
        .unwrap_or(Value::Null))
}
