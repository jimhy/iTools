//! 插件运行时命令：打开插件窗口 + `window.itools` 门面背后的 `plugin_*` 白名单命令。
//!
//! 安全：这些命令只对 label="plugin" 的窗口开放（见 `capabilities/plugin.json`）。
//! writeFile 限定插件沙盒目录；runCommand 受全局开关 [`ALLOW_RUN_COMMAND`] 控制。

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager, State};

use crate::launch;
use crate::logging::ilog;
use crate::settings::SettingsStore;

use super::{EnterInfo, PluginRegistry};

/// 注入插件页的桥接脚本（构造 window.itools）。
pub const BRIDGE_JS: &str = include_str!("bridge.js");

/// 判定某插件是否被用户授权了某高危能力（runCommand / network）。
fn plugin_granted(settings: &SettingsStore, plugin: &str, perm: &str) -> bool {
    settings
        .get()
        .plugin_permissions
        .get(plugin)
        .is_some_and(|v| v.iter().any(|p| p == perm))
}

/// 打开（或复用）插件窗口，加载 `itplugin://` 下的插件页并注入 window.itools。
///
/// 必须是 async 命令：动态 `WebviewWindowBuilder::build()` 若跑在同步命令/主线程回调里会死锁
/// （tauri#13963 / wry#583，见 lib.rs::open_admin 注释）。async 命令在独立任务执行，规避该坑。
#[tauri::command]
pub async fn open_plugin_window(
    app: AppHandle,
    target: String,
    query: String,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let (plugin_id, code) = target
        .split_once('#')
        .ok_or_else(|| "非法插件目标（缺 #code）".to_string())?;

    // 取所需信息后尽快释放对 registry 的借用
    let exists = registry
        .plugin_dir(plugin_id)
        .map(|d| d.join("index.html").exists())
        .unwrap_or(false);
    if !exists {
        return Err(format!("插件不存在或缺 index.html: {plugin_id}"));
    }
    // 判定本次是被关键字还是 regex 命中，回传真实触发类型与 query
    let kind = registry.trigger_kind(plugin_id, code, &query);

    if let Ok(mut g) = registry.pending_enter.lock() {
        *g = Some(EnterInfo {
            code: code.to_string(),
            kind,
            query: query.clone(),
        });
    }
    if let Ok(mut g) = registry.current.lock() {
        *g = Some(plugin_id.to_string());
    }

    let url_str = format!("http://itplugin.localhost/{plugin_id}/index.html");
    let url: tauri::Url = url_str.parse().map_err(|e| format!("URL 解析失败: {e}"))?;

    if let Some(win) = app.get_webview_window("plugin") {
        win.navigate(url).map_err(|e| e.to_string())?;
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else {
        let init = format!(
            "window.__ITOOLS_DEV__={};\n{}",
            cfg!(debug_assertions),
            BRIDGE_JS
        );
        tauri::WebviewWindowBuilder::new(&app, "plugin", tauri::WebviewUrl::External(url))
            .title(format!("{plugin_id} - iTools 插件"))
            .inner_size(760.0, 560.0)
            .min_inner_size(360.0, 240.0)
            .resizable(true)
            .initialization_script(&init)
            // 只允许在插件自身源内导航；外链应走 itools.openExternal（默认浏览器打开），
            // 拦住 window.location/表单把本地数据顶层导航外泄。
            .on_navigation(|u| u.scheme() == "itplugin" || u.host_str() == Some("itplugin.localhost"))
            .build()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 拉取本次进入信息（桥接脚本加载后调用一次，取走即清空）。规避 emit/监听的时序竞态。
#[tauri::command]
pub fn plugin_take_enter(registry: State<'_, PluginRegistry>) -> Option<EnterInfo> {
    registry.pending_enter.lock().ok().and_then(|mut g| g.take())
}

/// 热重载：重扫 plugins/ 目录、刷新搜索索引（过滤禁用），返回加载出的可搜索命令数。
/// 供托盘「重新加载插件」与管理中心触发——改/生成插件后无需重启 iTools。
#[tauri::command]
pub fn rescan_plugins(
    registry: State<'_, PluginRegistry>,
    index: State<'_, crate::search::SearchIndex>,
    settings: State<'_, crate::settings::SettingsStore>,
) -> usize {
    let cmds = registry.reload(&settings.get().disabled_plugins);
    let n = cmds.len();
    index.set_plugin_commands(cmds);
    ilog!("[iTools] 插件已重新加载：{n} 条可搜索命令");
    n
}

/// 列出已装插件（供「插件管理」页）。
#[tauri::command]
pub fn list_plugins(
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Vec<super::PluginInfo> {
    let s = settings.get();
    registry.list_infos(&s.disabled_plugins, &s.plugin_permissions)
}

/// 授予/撤销某插件的某高危能力（runCommand / network）。network 在下次打开插件页时经 CSP 生效。
#[tauri::command]
pub fn set_plugin_permission(
    name: String,
    perm: String,
    granted: bool,
    settings: State<'_, SettingsStore>,
) {
    let mut s = settings.get();
    let list = s.plugin_permissions.entry(name).or_default();
    list.retain(|p| p != &perm);
    if granted {
        list.push(perm);
    }
    settings.set(s);
}

/// 启用/禁用一个插件：更新禁用清单并即时刷新搜索索引（禁用的不参与搜索，仍在管理页展示）。
#[tauri::command]
pub fn set_plugin_enabled(
    name: String,
    enabled: bool,
    settings: State<'_, crate::settings::SettingsStore>,
    registry: State<'_, PluginRegistry>,
    index: State<'_, crate::search::SearchIndex>,
) {
    let mut s = settings.get();
    s.disabled_plugins.retain(|n| n != &name);
    if !enabled {
        s.disabled_plugins.push(name);
    }
    settings.set(s.clone());
    index.set_plugin_commands(registry.commands(&s.disabled_plugins));
}

/// 删除一个插件：删其目录（校验在 plugins 根内）+ 清理禁用清单 + 重扫刷新。
#[tauri::command]
pub fn delete_plugin(
    name: String,
    settings: State<'_, crate::settings::SettingsStore>,
    registry: State<'_, PluginRegistry>,
    index: State<'_, crate::search::SearchIndex>,
) -> Result<(), String> {
    let dir = registry
        .plugin_dir(&name)
        .ok_or_else(|| "插件不存在".to_string())?;
    // 安全校验：目标必须在 plugins 根目录内、且不是根本身
    let root = registry.root.canonicalize().map_err(|e| e.to_string())?;
    let cdir = dir.canonicalize().map_err(|e| e.to_string())?;
    if cdir == root || !cdir.starts_with(&root) {
        return Err("非法插件目录".to_string());
    }
    std::fs::remove_dir_all(&cdir).map_err(|e| format!("删除失败: {e}"))?;
    let mut s = settings.get();
    s.disabled_plugins.retain(|n| n != &name);
    settings.set(s.clone());
    index.set_plugin_commands(registry.reload(&s.disabled_plugins));
    ilog!("[iTools] 已删除插件 {name}");
    Ok(())
}

// ---------- 窗口 ----------

#[tauri::command]
pub fn plugin_hide(app: AppHandle) {
    if let Some(win) = app.get_webview_window("plugin") {
        let _ = win.hide();
    }
}

#[tauri::command]
pub fn plugin_exit(app: AppHandle) {
    if let Some(win) = app.get_webview_window("plugin") {
        let _ = win.close();
    }
}

#[tauri::command]
pub fn plugin_set_height(app: AppHandle, height: f64) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("plugin") {
        let scale = win.scale_factor().unwrap_or(1.0);
        let cur = win.inner_size().map_err(|e| e.to_string())?;
        let w = cur.width as f64 / scale;
        let h = height.clamp(120.0, 2000.0);
        win.set_size(tauri::LogicalSize::new(w, h))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------- 剪贴板 ----------

#[tauri::command]
pub fn plugin_copy_text(text: String) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn plugin_read_text() -> Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.get_text().map_err(|e| e.to_string())
}

// ---------- 文件 ----------

/// 读文件：限定在当前插件的沙盒目录 `<localAppData>/itools/plugin-data/<id>/files/` 内，
/// path 为相对路径（与 writeFile 对称，禁绝对路径与 `..`，杜绝任意文件读取+外泄）。
#[tauri::command]
pub fn plugin_read_file(path: String, registry: State<'_, PluginRegistry>) -> Result<String, String> {
    let id = current_plugin(&registry)?;
    let sandbox = plugin_files_dir(&id);
    let rel = sandbox_relative(&path)?;
    std::fs::read_to_string(sandbox.join(rel)).map_err(|e| format!("读文件失败: {e}"))
}

/// 写文件：限定在当前插件的沙盒目录 `<localAppData>/itools/plugin-data/<id>/files/` 内，
/// path 为相对路径，拒绝驱动器前缀/根/`..`（Windows 上 is_absolute 不认 `/foo`、`C:foo`，故按组件白名单校验），
/// 落盘前再 canonicalize 复核父目录仍在沙盒内（防符号链接穿越）。
#[tauri::command]
pub fn plugin_write_file(
    path: String,
    content: String,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    let sandbox = plugin_files_dir(&id);
    let rel = sandbox_relative(&path)?;
    let dest = sandbox.join(rel);
    let parent = dest.parent().ok_or_else(|| "非法路径".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    // 纵深防御：canonicalize 后父目录必须仍在沙盒内
    if let (Ok(cs), Ok(cp)) = (sandbox.canonicalize(), parent.canonicalize()) {
        if !cp.starts_with(&cs) {
            return Err("路径越出插件沙盒".to_string());
        }
    }
    std::fs::write(&dest, content).map_err(|e| format!("写文件失败: {e}"))
}

/// 校验并返回一个沙盒内相对路径：拒绝空、驱动器前缀(C:)、根(/ 或 \\)、上级(..)——
/// 只允许 Normal / CurDir 组件。修 Windows 下 is_absolute 不认根相对/盘符相对路径的绕过。
fn sandbox_relative(path: &str) -> Result<&Path, String> {
    let rel = Path::new(path);
    let ok = !path.is_empty()
        && rel.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        });
    if !ok {
        return Err("只能访问插件沙盒内的相对路径（禁绝对路径/盘符/根/..）".to_string());
    }
    Ok(rel)
}

// ---------- 系统 ----------

/// 打开外部链接：只放行 http/https/mailto，拒绝 cmd:/file: 等（防经 open_detached 的 cmd: 分支执行命令）。
#[tauri::command]
pub fn plugin_open_external(url: String) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")) {
        return Err("openExternal 只支持 http/https/mailto".to_string());
    }
    launch::open_detached(&url)
}

/// 打开本地路径：文件夹放行；文件走**扩展名白名单**（只放行文档/图片/媒体类）。
/// 黑名单不可靠（尾部点号 `calc.exe.` 使 extension 变空串绕过、LOLBin 类型层出不穷），故用白名单。
/// 归一化剥尾部点/空格（Windows 会剥），拒绝 cmd: 前缀与 UNC/远程路径。
#[tauri::command]
pub fn plugin_open_path(path: String) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.to_ascii_lowercase().starts_with("cmd:") {
        return Err("openPath 不支持 cmd: 前缀".to_string());
    }
    if trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
        return Err("openPath 不支持 UNC/远程路径".to_string());
    }
    // 归一化：剥尾部点与空格（否则 "calc.exe." 归一化后仍是可执行）
    let normalized = trimmed.trim_end_matches(['.', ' ']);
    if normalized.is_empty() {
        return Err("路径为空".to_string());
    }
    let p = Path::new(normalized);
    // 文件夹放行（文件夹名可能带点，不能按扩展名判）；其余按白名单
    if !p.is_dir() {
        const ALLOWED: &[&str] = &[
            "txt", "md", "log", "csv", "json", "xml", "yaml", "yml", "ini", "conf",
            "pdf", "rtf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp",
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff",
            "mp3", "wav", "flac", "aac", "ogg", "m4a", "mp4", "mkv", "avi", "mov", "webm",
        ];
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        if ext.is_empty() || !ALLOWED.contains(&ext.as_str()) {
            return Err("openPath 只允许打开文件夹或文档/图片/媒体类文件（白名单）".to_string());
        }
    }
    launch::open_detached(normalized)
}

#[tauri::command]
pub fn plugin_notify(body: String) {
    // 首期：记录到日志（OS 原生通知后续接 notification 插件再补）。
    ilog!("[iTools][plugin] notify: {body}");
}

/// 执行程序：显式 program + args，**不经 cmd.exe**（元字符 `&`/`|`/`>` 不会被解释，无 shell 注入面）。
/// 需当前插件已被用户授权 runCommand（在「插件管理」里授权），否则拒绝。
#[tauri::command]
pub fn plugin_run_command(
    program: String,
    args: Vec<String>,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    if !plugin_granted(&settings, &id, "runCommand") {
        return Err("插件未获授权执行程序（请在「插件管理」里授权 runCommand）".to_string());
    }
    if program.trim().is_empty() {
        return Err("program 为空".to_string());
    }
    launch::spawn_program(&program, &args)
}

/// itools.fetch 的返回。
#[derive(serde::Serialize)]
pub struct FetchResponse {
    pub status: u16,
    pub ok: bool,
    pub body: String,
}

/// 受权限校验的联网代理：需【当前活动插件】已授权 network。只支持 http/https，返回文本。
/// 联网授权在原生层门禁（不靠 CSP）——所有插件同源，CSP 会被同源 iframe 借道绕过；
/// 这里按 current_plugin 判定，即便被别的插件框入也按顶层插件的授权决定，杜绝借道。
#[tauri::command]
pub async fn plugin_fetch(
    url: String,
    method: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    body: Option<String>,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<FetchResponse, String> {
    let id = current_plugin(&registry)?;
    if !plugin_granted(&settings, &id, "network") {
        return Err("插件未获授权联网（请在「插件管理」里授权 network）".to_string());
    }
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("fetch 只支持 http/https".to_string());
    }
    let method = method.unwrap_or_else(|| "GET".to_string()).to_uppercase();
    tauri::async_runtime::spawn_blocking(move || -> Result<FetchResponse, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .build();
        let mut req = agent.request(&method, &url);
        if let Some(h) = &headers {
            for (k, v) in h {
                req = req.set(k, v);
            }
        }
        let result = match body {
            Some(b) => req.send_string(&b),
            None => req.call(),
        };
        match result {
            Ok(r) => {
                let status = r.status();
                let text = r.into_string().map_err(|e| e.to_string())?;
                Ok(FetchResponse {
                    status,
                    ok: (200..300).contains(&status),
                    body: text,
                })
            }
            // 4xx/5xx：ureq 归为 Error::Status，但对调用方是正常响应
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                Ok(FetchResponse {
                    status: code,
                    ok: false,
                    body: text,
                })
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- 存储（KV，按插件隔离） ----------

#[tauri::command]
pub fn plugin_db_get(key: String, registry: State<'_, PluginRegistry>) -> Result<Option<String>, String> {
    let map = load_db(&current_plugin(&registry)?)?;
    Ok(map.get(&key).and_then(|v| v.as_str()).map(|s| s.to_string()))
}

#[tauri::command]
pub fn plugin_db_set(
    key: String,
    value: String,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    let mut map = load_db(&id)?;
    map.insert(key, serde_json::Value::String(value));
    save_db(&id, &map)
}

#[tauri::command]
pub fn plugin_db_remove(key: String, registry: State<'_, PluginRegistry>) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    let mut map = load_db(&id)?;
    map.remove(&key);
    save_db(&id, &map)
}

#[tauri::command]
pub fn plugin_db_keys(
    prefix: Option<String>,
    registry: State<'_, PluginRegistry>,
) -> Result<Vec<String>, String> {
    let map = load_db(&current_plugin(&registry)?)?;
    let pre = prefix.unwrap_or_default();
    Ok(map
        .keys()
        .filter(|k| pre.is_empty() || k.starts_with(&pre))
        .cloned()
        .collect())
}

// ---------- 内部辅助 ----------

fn current_plugin(registry: &PluginRegistry) -> Result<String, String> {
    registry
        .current
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .ok_or_else(|| "没有正在运行的插件".to_string())
}

fn plugin_data_dir(id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("itools")
        .join("plugin-data")
        .join(id)
}

fn plugin_files_dir(id: &str) -> PathBuf {
    plugin_data_dir(id).join("files")
}

fn db_path(id: &str) -> PathBuf {
    plugin_data_dir(id).join("kv.json")
}

fn load_db(id: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = db_path(id);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("插件存储损坏: {e}")),
        Err(_) => Ok(serde_json::Map::new()),
    }
}

fn save_db(id: &str, map: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let path = db_path(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(&serde_json::Value::Object(map.clone())).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("写插件存储失败: {e}"))
}
