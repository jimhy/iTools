//! 开发者中心的命令（**只授予管理中心窗口**，见 `capabilities/default.json`）。
//!
//! 除 `dev_log_push`（由插件调试窗口的 bridge 探针上报，只在 `capabilities/plugin-dev.json` 里放行）
//! 之外，这里的每个命令都只在管理中心可调——调试环境的配置权不该落到被调试的插件手里。

use std::sync::Arc;

use tauri::{AppHandle, Manager, State};

use crate::logging::ilog;
use crate::plugin::{ActiveSession, PluginRegistry};
use crate::settings::SettingsStore;

use super::logs::DevLogEntry;
use super::mock::DevMockConfig;
use super::{DevConfig, DevPluginInfo, DevRuntime, DEV_SCHEME, DEV_WINDOW_LABEL};

/// 调试插件窗口的默认尺寸（比正式插件窗略大：要同时看页面和 DevTools）。
const DEV_WINDOW_SIZE: (f64, f64) = (1000.0, 700.0);

// ==================== 列表 / 配置 ====================

/// 列出全部调试插件（含逐字段的清单校验结论）。
#[tauri::command]
pub fn dev_list_plugins(
    dev: State<'_, Arc<DevRuntime>>,
    settings: State<'_, SettingsStore>,
) -> Vec<DevPluginInfo> {
    dev.list(&settings.get().dev_plugin_permissions)
}

/// 重扫全部调试目录并刷新搜索索引，返回加载到的插件数。
#[tauri::command]
pub fn dev_rescan(app: AppHandle, dev: State<'_, Arc<DevRuntime>>, settings: State<'_, SettingsStore>) -> usize {
    let n = dev.rescan(&settings.get().dev_plugin_dirs);
    super::refresh_search(&app);
    ilog!("[iTools] 调试插件已重扫：{n} 个");
    n
}

/// 开发者中心的配置快照（目录列表 / Mock / 测试库与沙盒的真实路径）。
#[tauri::command]
pub fn dev_get_config(
    dev: State<'_, Arc<DevRuntime>>,
    settings: State<'_, SettingsStore>,
) -> DevConfig {
    config_of(&dev, &settings)
}

fn config_of(dev: &DevRuntime, settings: &SettingsStore) -> DevConfig {
    DevConfig {
        search_visible: settings.get().dev_search_visible,
        dirs: dev.dirs(),
        mock: dev.mock(),
        db_path: dev.db_path.to_string_lossy().into_owned(),
        files_root: dev.files_root.to_string_lossy().into_owned(),
    }
}

/// 弹原生目录选择器，返回选中的绝对路径（取消返回 None）。
#[tauri::command]
pub async fn dev_pick_dir() -> Option<String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择要调试的插件目录（可以是插件本身，也可以是装着多个插件的目录）")
            .pick_folder()
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten()
}

/// 添加一个调试目录：**先校验目录真实存在**再落库，避免列表里躺着一个永远扫不出东西的路径。
#[tauri::command]
pub fn dev_add_dir(
    path: String,
    app: AppHandle,
    dev: State<'_, Arc<DevRuntime>>,
    settings: State<'_, SettingsStore>,
) -> Result<DevConfig, String> {
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err("路径为空".to_string());
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("目录不存在：{path}"));
    }
    if same_path(&path, &dev.fixed_root.to_string_lossy()) {
        return Err("这是固定的调试目录，已经在列表里了".to_string());
    }
    let mut s = settings.get();
    if s.dev_plugin_dirs.iter().any(|d| same_path(d, &path)) {
        return Err("该目录已在列表中".to_string());
    }
    s.dev_plugin_dirs.push(path.clone());
    settings.set(s);
    dev.rescan(&settings.get().dev_plugin_dirs);
    super::refresh_search(&app);
    ilog!("[iTools] 新增调试目录：{path}");
    Ok(config_of(&dev, &settings))
}

/// 移除一个调试目录（固定根不可移除）。
#[tauri::command]
pub fn dev_remove_dir(
    path: String,
    app: AppHandle,
    dev: State<'_, Arc<DevRuntime>>,
    settings: State<'_, SettingsStore>,
) -> Result<DevConfig, String> {
    if same_path(&path, &dev.fixed_root.to_string_lossy()) {
        return Err("固定调试目录不可移除".to_string());
    }
    let mut s = settings.get();
    let before = s.dev_plugin_dirs.len();
    s.dev_plugin_dirs.retain(|d| !same_path(d, &path));
    if s.dev_plugin_dirs.len() == before {
        return Err("列表里没有这个目录".to_string());
    }
    settings.set(s);
    dev.rescan(&settings.get().dev_plugin_dirs);
    super::refresh_search(&app);
    Ok(config_of(&dev, &settings))
}

/// 路径等价判断：忽略大小写与首尾分隔符（Windows 路径大小写不敏感）。
fn same_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_end_matches(['/', '\\'])
            .replace('/', "\\")
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// 调试插件是否并入主搜索（默认关闭）。改完**立即**重建索引，开关真实生效。
///
/// ⚠ 这个开关管的**只有**「出不出现在主搜索 / 最近使用 / 已固定」。它管不到、也不打算管
/// 调试插件已注册的**全局热键**：热键按下照样唤起调试插件（理由见
/// `crate::plugin::hotkey::dispatch` 的文档）。UI 与文档描述本开关时不得越过这条边界。
#[tauri::command]
pub fn dev_set_search_visible(enabled: bool, app: AppHandle, settings: State<'_, SettingsStore>) {
    let mut s = settings.get();
    s.dev_search_visible = enabled;
    settings.set(s);
    super::refresh_search(&app);
    ilog!("[iTools] 调试插件进入主搜索：{enabled}");
}

// ==================== 权限模拟 ====================

/// 授予 / 撤销调试插件的高危能力。写的是**独立的** `dev_plugin_permissions`，
/// 与正式插件的授权表毫无关系——调试时随便开，不会影响正式环境的授权状态。
#[tauri::command]
pub fn dev_set_permission(
    id: String,
    perm: String,
    granted: bool,
    settings: State<'_, SettingsStore>,
) {
    let mut s = settings.get();
    let list = s.dev_plugin_permissions.entry(id).or_default();
    list.retain(|p| p != &perm);
    if granted {
        list.push(perm);
    }
    settings.set(s);
}

// ==================== 触发模拟器 / 打开目录 ====================

/// 「触发模拟器」：按指定的 code / query / 触发类型打开调试插件窗口。
///
/// `kind` 传空则由清单自动判定（与主搜索命中时的判定同一套规则），
/// 传 `keyword` / `regex` / `text` 则强制模拟该类型——插件的 `onEnter` 会原样收到，
/// 开发者据此验证「不同触发路径下的分支」。
#[tauri::command]
pub async fn dev_open_plugin(
    id: String,
    code: String,
    query: String,
    kind: String,
    app: AppHandle,
) -> Result<(), String> {
    open_dev_window(app, id, code, query, kind).await
}

/// 打开/复用调试插件窗口的实现（命令与「主搜索里进入 dev: 前缀目标」共用）。
pub async fn open_dev_window(
    app: AppHandle,
    id: String,
    code: String,
    query: String,
    kind: String,
) -> Result<(), String> {
    let registry = app.state::<PluginRegistry>();
    let dev = registry.dev.clone();

    let Some(dir) = dev.dir_of(&id) else {
        return Err(format!("调试插件不存在：{id}（请先在开发者中心重扫）"));
    };
    if !dir.join("index.html").exists() {
        return Err(format!("插件缺少 index.html，无法运行：{}", dir.display()));
    }
    if !dev.runnable(&id) {
        return Err(format!("插件 {id} 存在致命问题，请先修复「问题」列表里的 error 项"));
    }

    let kind = match kind.trim() {
        "" => dev.trigger_kind(&id, &code, &query),
        k => k.to_string(),
    };
    let enter = crate::plugin::EnterInfo {
        code: code.clone(),
        kind: kind.clone(),
        query: query.clone(),
        plugin_id: id.clone(),
    };
    let session = ActiveSession {
        id: id.clone(),
        dev: true,
    };
    // 切换插件前先记住上一个调试插件的窗口尺寸（与正式窗口同一套记忆机制，
    // 只是作用域键带 `dev:` 前缀，不会盖掉同名正式插件的尺寸）
    if let Some(win) = app.get_webview_window(DEV_WINDOW_LABEL) {
        if let Some(prev) = registry.session_for(DEV_WINDOW_LABEL) {
            if prev.id != id {
                let scale = win.scale_factor().unwrap_or(1.0);
                if let Ok(sz) = win.inner_size() {
                    app.state::<SettingsStore>().set_plugin_window(
                        &prev.scope_key(),
                        [sz.width as f64 / scale, sz.height as f64 / scale],
                    );
                }
            }
        }
    }
    registry.set_enter(DEV_WINDOW_LABEL, enter);
    registry.set_session(DEV_WINDOW_LABEL, session.clone());

    let url_str = format!("http://{DEV_SCHEME}.localhost/{id}/index.html");
    let url: tauri::Url = url_str.parse().map_err(|e| format!("URL 解析失败: {e}"))?;
    // 该调试插件上次的窗口尺寸（`plugin_hide` / `plugin_exit` / 切换插件时写入）
    let saved = app.state::<SettingsStore>().get_plugin_window(&session.scope_key());
    let (win_w, win_h) = saved.map(|s| (s[0], s[1])).unwrap_or(DEV_WINDOW_SIZE);

    if let Some(win) = app.get_webview_window(DEV_WINDOW_LABEL) {
        let _ = win.set_title(&dev_title(&id));
        win.navigate(url).map_err(|e| e.to_string())?;
        if saved.is_some() {
            let _ = win.set_size(tauri::LogicalSize::new(win_w, win_h));
        }
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else {
        // 注入的两个全局量：
        // - `__ITOOLS_DEV__`：沿用正式插件窗口的语义（是否 debug 构建）；
        // - `__ITOOLS_DEBUG__`：**当前窗口是插件调试窗**，bridge 的调试探针据此决定是否挂钩。
        //   它必须是常量（不含插件 id）——初始化脚本在每次导航时重放，带 id 会在换插件后失真。
        let init = format!(
            "window.__ITOOLS_DEV__={};window.__ITOOLS_DEBUG__=true;\n{}",
            cfg!(debug_assertions),
            crate::plugin::commands::BRIDGE_JS
        );
        tauri::WebviewWindowBuilder::new(&app, DEV_WINDOW_LABEL, tauri::WebviewUrl::External(url))
            .title(dev_title(&id))
            .inner_size(win_w, win_h)
            .min_inner_size(360.0, 240.0)
            .resizable(true)
            .disable_drag_drop_handler()
            .initialization_script(&init)
            // 只允许在调试协议内导航（与正式插件窗口同样的越站防护）
            .on_navigation(|u| {
                u.scheme() == DEV_SCHEME || u.host_str() == Some(&format!("{DEV_SCHEME}.localhost"))
            })
            .build()
            .map_err(|e| e.to_string())?;
    }

    dev.logs.push(DevLogEntry::backend(
        &id,
        "console",
        "info",
        "session.open",
        &format!("打开调试会话：code={code} kind={kind} query={query}"),
    ));
    Ok(())
}

/// 调试窗口标题：一眼能与正式插件窗区分开。
fn dev_title(id: &str) -> String {
    format!("{id} - iTools 插件调试")
}

/// 在资源管理器里打开该调试插件的目录。
#[tauri::command]
pub fn dev_open_dir(id: String, dev: State<'_, Arc<DevRuntime>>) -> Result<(), String> {
    let dir = dev
        .dir_of(&id)
        .ok_or_else(|| format!("调试插件不存在：{id}"))?;
    crate::launch::open_detached(&dir.to_string_lossy())
}

// ==================== 日志 ====================

/// 增量拉取调试日志（`after_seq` 传上次收到的最大 seq；首次传 0）。
///
/// ⚠ 前端调用时参数名是 **camelCase**：`invoke("dev_log_list", { afterSeq })`。
#[tauri::command]
pub fn dev_log_list(after_seq: u64, dev: State<'_, Arc<DevRuntime>>) -> Vec<DevLogEntry> {
    dev.logs.list_after(after_seq)
}

/// 清空调试日志缓冲（seq 不回退，前端可继续用手里的 afterSeq 续拉）。
#[tauri::command]
pub fn dev_log_clear(dev: State<'_, Arc<DevRuntime>>) {
    dev.logs.clear();
}

// ==================== 同步 Mock ====================

/// 读云同步 Mock 配置。
#[tauri::command]
pub fn dev_get_mock(dev: State<'_, Arc<DevRuntime>>) -> DevMockConfig {
    dev.mock()
}

/// 写云同步 Mock 配置（会被夹紧到可执行范围后持久化，`dev_get_mock` 读回的即实际行为）。
#[tauri::command]
pub fn dev_set_mock(cfg: DevMockConfig, dev: State<'_, Arc<DevRuntime>>) {
    dev.set_mock(cfg);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 路径大小写/尾分隔符/斜杠方向不同也算同一个目录（否则会重复添加）。
    #[test]
    fn path_equality_is_windows_ish() {
        assert!(same_path("C:\\Dev\\Plugins", "c:/dev/plugins/"));
        assert!(same_path(" C:\\a\\b\\ ", "C:\\a\\b"));
        assert!(!same_path("C:\\a\\b", "C:\\a\\c"));
    }
}
