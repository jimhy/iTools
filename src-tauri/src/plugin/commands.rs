//! 插件运行时命令：打开插件窗口 + `window.itools` 门面背后的 `plugin_*` 白名单命令。
//!
//! 安全：这些命令只对 label="plugin" 的窗口开放（见 `capabilities/plugin.json`）。
//! writeFile 限定插件沙盒目录；runCommand 受全局开关 [`ALLOW_RUN_COMMAND`] 控制。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::{AppHandle, Manager, State};

use crate::account::AccountStore;
use crate::db::Db;
use crate::launch;
use crate::logging::ilog;
use crate::settings::SettingsStore;
use crate::sync::{DataStore, SyncResult};

use super::{ActiveSession, EnterInfo, PluginRegistry};

/// 注入插件页的桥接脚本（构造 window.itools）。
pub const BRIDGE_JS: &str = include_str!("bridge.js");

/// 正式插件窗口的 label（调试插件窗是 [`crate::dev::DEV_WINDOW_LABEL`]）。
pub const PLUGIN_WINDOW_LABEL: &str = "plugin";

/// 判定某插件**会话**是否被用户授权了某高危能力（runCommand / network / screen-capture / …）。
///
/// **双条件，缺一不可**：
/// 1. 用户授权过（正式会话查 `settings.plugin_permissions`，**调试会话查独立的
///    `settings.dev_plugin_permissions`**——调试时随手开的授权不该影响正式插件）；
/// 2. 该插件**当前**的 `plugin.json` 仍声明了这个 permission。
///
/// 只查条件 1 会留下一个权限提升口子：授权表是按**插件名**存的，同名覆盖安装 / 卸载重装 /
/// 手工换包之后，新代码会直接继承旧插件的授权，甚至能用新清单里根本没声明的能力
/// （详情页也不会展示这条授权，用户根本看不到，更谈不上撤销）。
pub(crate) fn plugin_granted(
    settings: &SettingsStore,
    registry: &PluginRegistry,
    session: &ActiveSession,
    perm: &str,
) -> bool {
    let s = settings.get();
    let (declared, table) = if session.dev {
        (
            registry.dev.declares_permission(&session.id, perm),
            &s.dev_plugin_permissions,
        )
    } else {
        (
            registry.declares_permission(&session.id, perm),
            &s.plugin_permissions,
        )
    };
    if !declared {
        return false;
    }
    table
        .get(&session.id)
        .is_some_and(|v| v.iter().any(|p| p == perm))
}

/// 打开（或复用）插件窗口，加载 `itplugin://` 下的插件页并注入 window.itools。
///
/// 必须是 async 命令：动态 `WebviewWindowBuilder::build()` 若跑在同步命令/主线程回调里会死锁
/// （tauri#13963 / wry#583，见 lib.rs::open_admin 注释）。async 命令在独立任务执行，规避该坑。
#[tauri::command]
pub async fn open_plugin_window(app: AppHandle, target: String, query: String) -> Result<(), String> {
    open_plugin(app, target, query, false).await
}

/// 打开/复用插件窗口的实现（命令与「热键唤起」共用）。内部自取 PluginRegistry。
/// `hidden=true`：建/导航后**不显示**面板——热键触发「不需要面板」的动作（如截图）时用，
/// 杜绝「先弹面板再被藏起来」的闪现。隐藏的 webview 照常加载并跑 onEnter（可自行截图）。
pub async fn open_plugin(
    app: AppHandle,
    target: String,
    query: String,
    hidden: bool,
) -> Result<(), String> {
    let (raw_id, code) = target
        .split_once('#')
        .ok_or_else(|| "非法插件目标（缺 #code）".to_string())?;
    // 主搜索里的调试插件带 `dev:` 前缀（见 dev::DEV_ID_PREFIX）：路由到**调试窗口**，
    // 绝不能落到正式窗口——否则调试插件会拿到正式插件的存储与授权。
    if let Some(dev_id) = raw_id.strip_prefix(crate::dev::DEV_ID_PREFIX) {
        return crate::dev::commands::open_dev_window(
            app,
            dev_id.to_string(),
            code.to_string(),
            query,
            String::new(),
        )
        .await;
    }
    let (plugin_id, code) = (raw_id, code);
    let registry = app.state::<PluginRegistry>();

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

    let enter = EnterInfo {
        code: code.to_string(),
        kind,
        query: query.clone(),
        plugin_id: plugin_id.to_string(),
    };
    // 存入本窗口的待取进入信息（同时留存一份供热更新重放 onEnter）
    registry.set_enter(PLUGIN_WINDOW_LABEL, enter);
    // 切换插件前：把「上一个插件」的当前窗口尺寸记住，供其下次打开时还原。
    if let Some(win) = app.get_webview_window(PLUGIN_WINDOW_LABEL) {
        if let Some(prev) = registry.session_for(PLUGIN_WINDOW_LABEL) {
            if prev.id != plugin_id {
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

    registry.set_session(
        PLUGIN_WINDOW_LABEL,
        ActiveSession {
            id: plugin_id.to_string(),
            dev: false,
        },
    );

    let url_str = format!("http://itplugin.localhost/{plugin_id}/index.html");
    let url: tauri::Url = url_str.parse().map_err(|e| format!("URL 解析失败: {e}"))?;

    // 该插件的目标窗口尺寸：优先用它上次保存的，否则用默认 960×660。
    let saved = app.state::<SettingsStore>().get_plugin_window(plugin_id);
    let (win_w, win_h) = saved.map(|s| (s[0], s[1])).unwrap_or((960.0, 660.0));

    if let Some(win) = app.get_webview_window("plugin") {
        win.navigate(url).map_err(|e| e.to_string())?;
        // 还原该插件上次的窗口尺寸（有记录才调整；无记录保持当前，避免跳变）
        if saved.is_some() {
            let _ = win.set_size(tauri::LogicalSize::new(win_w, win_h));
        }
        // hidden 时不显示——若面板本就隐藏则保持隐藏（热键截图不闪）；本就显示的则不强抢焦点
        if !hidden {
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        }
    } else {
        let init = format!(
            "window.__ITOOLS_DEV__={};\n{}",
            cfg!(debug_assertions),
            BRIDGE_JS
        );
        tauri::WebviewWindowBuilder::new(&app, "plugin", tauri::WebviewUrl::External(url))
            .title(format!("{plugin_id} - iTools 插件"))
            .inner_size(win_w, win_h)
            .min_inner_size(360.0, 240.0)
            .resizable(true)
            // 禁用 Tauri 的 OS 级拖放拦截：否则它会吞掉插件页内的 HTML5 拖拽（draggable），
            // 导致笔记拖入文件夹、待办拖动排序等失效。插件不需要「拖文件进窗口」的能力。
            .disable_drag_drop_handler()
            // 热键截图：隐藏建窗，页面照常加载并 onEnter 触发截图，面板全程不露脸
            .visible(!hidden)
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
///
/// **按调用窗口取**：正式插件窗与调试插件窗可能同时在加载，共用一个槽会互相取走对方的上下文
/// （表现为「打开 A 却收到 B 的 code/query」）。
#[tauri::command]
pub fn plugin_take_enter(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Option<EnterInfo> {
    registry.take_enter(webview.label())
}

/// 热重载：重扫 plugins/ 目录、刷新搜索索引（过滤禁用），返回加载出的可搜索命令数。
/// 供托盘「重新加载插件」与管理中心触发——改/生成插件后无需重启 iTools。
#[tauri::command]
pub fn rescan_plugins(
    app: AppHandle,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, crate::settings::SettingsStore>,
) -> usize {
    let cmds = registry.reload(&settings.get().disabled_plugins);
    let n = cmds.len();
    // 经 dev::apply_plugin_search 写索引：直接 set 会把「调试插件进主搜索」的那部分冲掉
    crate::dev::apply_plugin_search(&app, cmds);
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
    app: AppHandle,
    settings: State<'_, crate::settings::SettingsStore>,
    registry: State<'_, PluginRegistry>,
) {
    let mut s = settings.get();
    s.disabled_plugins.retain(|n| n != &name);
    if !enabled {
        s.disabled_plugins.push(name);
    }
    settings.set(s.clone());
    crate::dev::apply_plugin_search(&app, registry.commands(&s.disabled_plugins));
}

/// 删除一个插件：删其目录（校验在 plugins 根内）+ **删寄存副本** + 清理禁用清单 + **清授权**
/// + **清安装记录** + 重扫刷新。
///
/// 必须一起清干净的三处残留（都会在「删掉再装同名插件」或「下次启动」时变成真实缺陷）：
/// - `plugin_permissions[name]`：留着的话，下次装同名插件（哪怕是完全不同的作者）
///   会直接继承 runCommand / network 等高危授权（权限提升）；
/// - `.installed.json` 里的来源记录：留着的话，用户之后手写一个同名插件放进 plugins/，
///   列表会把它标成来自那个仓库、显示「查看仓库 / 有新版」，一点更新就把手写代码整目录换掉；
/// - `<root>/.recover-<name>`：更新时若 `atomic_place` 末尾那句「删除旧目录」失败
///   （Windows 上杀软 / 索引器在 rename 后短暂持有句柄是现实场景），寄存副本会一直留着。
///   此时 `<root>/<name>` 是新版本，启动时的 `recover_orphans` 见目标已存在会正确跳过，
///   表面看不出异常；**但用户在这里删掉插件之后**，目标不存在了，下次启动就会把那份旧版本整个搬回原位
///   ——用户明确删掉的插件自己复活了，而且此时授权与安装记录都已被抹掉，
///   它会以「本地安装、无来源」的面目出现，用户根本不知道它从哪来。
///   **显式删除 = 放弃寄存副本**，所以这里一并删掉。
///
/// 收尾走与安装 / 更新同一个 [`super::install::refresh_after_change`]：除了重扫 + 刷新搜索索引，
/// 还要 `emit("plugins-changed")`。少了这一发，管理中心那边**只有发起删除的那个页面**
/// 会自己更新，别处（以及本就监听该事件的插件页）要等用户手动刷新才看得到——
/// 而 `install.rs` 的注释一直声称这三种变动都会广播。取 `AppHandle` 就是为了这一发。
#[tauri::command]
pub fn delete_plugin(
    name: String,
    app: AppHandle,
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
    // 显式删除即放弃寄存副本：不删它，下次启动 recover_orphans 会把旧版本搬回来「复活」这个插件
    let recover = super::install::recover_dir(&registry.root, &name);
    if recover.is_dir() {
        match std::fs::remove_dir_all(&recover) {
            Ok(()) => ilog!("[iTools] 已一并清理插件 {name} 的寄存副本 {}", recover.display()),
            // 删不掉只是「下次启动它会被搬回来」，如实记日志，不因此让删除整体失败
            Err(e) => ilog!(
                "[iTools] 插件 {name} 的寄存副本 {} 清理失败（下次启动可能被恢复，请手动删除）: {e}",
                recover.display()
            ),
        }
    }
    // 目录已删 → 抹掉安装来源记录（用未 canonicalize 的原始根，与写入时同一路径形态）
    super::install::forget(&registry.root, &name);
    let mut s = settings.get();
    s.disabled_plugins.retain(|n| n != &name);
    s.plugin_permissions.remove(&name);
    settings.set(s);
    // 重扫 + 刷新搜索索引 + 广播 plugins-changed（与安装 / 更新同一条收尾路径）
    super::install::refresh_after_change(&app, &registry, &settings, &index);
    ilog!("[iTools] 已删除插件 {name}（含其授权与安装来源记录）");
    Ok(())
}

// ---------- 窗口 ----------

/// 快照**调用方所在窗口**的尺寸并存入 settings（供下次打开该插件时还原）。
/// 用户 Esc 隐藏 / 关闭面板时调用，配合 open_plugin 的「切换前保存」覆盖所有离开时机。
///
/// 按窗口而非全局 current：调试窗与正式窗并存时，否则会把调试窗的尺寸记到正式插件头上。
fn save_window_size(app: &AppHandle, win: &tauri::Window) {
    let registry = app.state::<PluginRegistry>();
    let Some(session) = registry.session_for(win.label()) else {
        return;
    };
    let scale = win.scale_factor().unwrap_or(1.0);
    if let Ok(sz) = win.inner_size() {
        app.state::<SettingsStore>().set_plugin_window(
            &session.scope_key(),
            [sz.width as f64 / scale, sz.height as f64 / scale],
        );
    }
}

/// 隐藏**发起调用的**插件窗口（正式窗 / 调试窗各自生效）。
#[tauri::command]
pub fn plugin_hide(app: AppHandle, webview: tauri::Webview) {
    let win = webview.window();
    save_window_size(&app, &win);
    let _ = win.hide();
}

/// 关闭**发起调用的**插件窗口。
#[tauri::command]
pub fn plugin_exit(app: AppHandle, webview: tauri::Webview) {
    let win = webview.window();
    save_window_size(&app, &win);
    let _ = win.close();
}

/// 调整**发起调用的**插件窗口高度（插件按内容自适应用）。
#[tauri::command]
pub fn plugin_set_height(webview: tauri::Webview, height: f64) -> Result<(), String> {
    let win = webview.window();
    let scale = win.scale_factor().unwrap_or(1.0);
    let cur = win.inner_size().map_err(|e| e.to_string())?;
    let w = cur.width as f64 / scale;
    let h = height.clamp(120.0, 2000.0);
    win.set_size(tauri::LogicalSize::new(w, h))
        .map_err(|e| e.to_string())
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

/// 读剪贴板里的图片 → base64 PNG（桥接层解回 ArrayBuffer 给插件）。剪贴板无图片则 Err。
/// 与 read_text 同为未门控能力（读剪贴板本就无需授权）。
///
/// 为何 base64 而非原始 Response 字节：插件页严格 CSP（connect-src 'self'）拦掉了 Tauri 的
/// IPC 自定义协议，IPC 退化为 postMessage，此路径下 `Vec<u8>` 会被序列化成「数字数组」（体积 4×、极慢）。
/// base64 字符串走 JSON 字符串路径无退化，比数字数组小得多，且不动作者刻意收紧的 CSP。
#[tauri::command]
pub fn plugin_read_image() -> Result<String, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = cb.get_image().map_err(|e| format!("剪贴板没有图片: {e}"))?;
    let png = rgba_to_png(img.width, img.height, &img.bytes)?;
    Ok(png_to_b64(&png))
}

/// 把图片（PNG/JPEG/…任意 image 能解码的格式，base64）写入剪贴板为**真实图片**
/// （非文本）。取代插件用 base64-过-剪贴板-文本 + 外部转换的老套路。
#[tauri::command]
pub fn plugin_write_image(b64: String) -> Result<(), String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let rgba = image::load_from_memory(&bytes)
        .map_err(|e| format!("图片解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    })
    .map_err(|e| format!("写剪贴板图片失败: {e}"))
}

/// PNG 字节 → base64（IPC 回传给插件的载体，桥接层解回 ArrayBuffer）。
pub(crate) fn png_to_b64(png: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// RGBA8 → **BMP 字节**（无压缩，编码近乎瞬时；浏览器解码也快）。用于截图冻结图这类转瞬即用、
/// 只求最快出图的场景——PNG 压缩（尤其带 filter）对 4K 整屏要 1 秒级，BMP 只是内存拷贝。
pub(crate) fn rgba_to_bmp(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    use image::codecs::bmp::BmpEncoder;
    use image::ImageEncoder;
    let mut out = Vec::with_capacity(54 + rgba.len());
    BmpEncoder::new(&mut out)
        .write_image(rgba, width as u32, height as u32, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("BMP 编码失败: {e}"))?;
    Ok(out)
}

/// RGBA8 像素缓冲 → PNG 字节。供 read_image 与截图类命令共用。
pub(crate) fn rgba_to_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let buf = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
        .ok_or_else(|| "图片尺寸与像素数据不符".to_string())?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("PNG 编码失败: {e}"))?;
    Ok(out.into_inner())
}

// ---------- 文件 ----------

/// 读文件：限定在当前插件的沙盒目录 `<localAppData>/itools/plugin-data/<id>/files/` 内，
/// path 为相对路径（与 writeFile 对称，禁绝对路径与 `..`，杜绝任意文件读取+外泄）。
/// **调试会话读的是调试沙盒**（`<localAppData>/itools/dev/plugin-data/<id>/files/`）。
#[tauri::command]
pub fn plugin_read_file(
    path: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<String, String> {
    let sandbox = session_files_dir(&caller_session(&webview, &registry)?, &registry);
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
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let sandbox = session_files_dir(&caller_session(&webview, &registry)?, &registry);
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

/// 删除插件沙盒内的文件（相对路径，同 write/read 的沙盒约束）。不存在则视为成功（幂等）。
#[tauri::command]
pub fn plugin_remove_file(
    path: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let sandbox = session_files_dir(&caller_session(&webview, &registry)?, &registry);
    let rel = sandbox_relative(&path)?;
    let target = sandbox.join(rel);
    match std::fs::remove_file(&target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除文件失败: {e}")),
    }
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

/// 保存图片（base64 PNG）到用户选择的位置：弹原生「另存为」对话框（用户显式选路径即授权，故不额外门控）。
/// 默认目录为「图片」，默认文件名可传入。返回保存的绝对路径；用户取消返回 Ok(None)。
#[tauri::command]
pub async fn plugin_save_image(
    b64: String,
    default_name: Option<String>,
) -> Result<Option<String>, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let name = default_name.unwrap_or_else(|| "iTools截图.png".to_string());
    let mut dlg = rfd::AsyncFileDialog::new()
        .set_file_name(&name)
        .add_filter("PNG 图片", &["png"]);
    if let Some(dir) = dirs::picture_dir() {
        dlg = dlg.set_directory(dir);
    }
    match dlg.save_file().await {
        Some(handle) => {
            let path = handle.path().to_path_buf();
            std::fs::write(&path, &bytes).map_err(|e| format!("写文件失败: {e}"))?;
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        None => Ok(None),
    }
}

/// 读取本地图片文件 → base64（供「文件路径粘贴 / 资源管理器拖入路径」把外部图片**本地化前**取字节）。
/// 只读、且按图片扩展名白名单放行（黑名单不可靠，同 open_path 思路），拒 UNC/远程与超大文件；
/// 不落任何执行/写入面。与 read_image（剪贴板）同属未门控的低危读能力，但仅限图片类型。
#[tauri::command]
pub fn plugin_read_local_image(path: String) -> Result<String, String> {
    use base64::Engine as _;
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("路径为空".to_string());
    }
    if trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
        return Err("不支持 UNC/远程路径".to_string());
    }
    let p = Path::new(trimmed);
    const ALLOWED: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "tif", "tiff"];
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !ALLOWED.contains(&ext.as_str()) {
        return Err("只支持读取图片文件（png/jpg/jpeg/gif/bmp/webp/svg/ico/tiff）".to_string());
    }
    if !p.is_file() {
        return Err("文件不存在".to_string());
    }
    let bytes = std::fs::read(p).map_err(|e| format!("读文件失败: {e}"))?;
    // 防御：单张上限 ~30MB，避免超大文件撑爆 IPC / 内存。
    if bytes.len() > 30 * 1024 * 1024 {
        return Err("图片过大（>30MB）".to_string());
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
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

/// 弹一条系统通知。正文由插件经 `window.itools.notify` 传入。
///
/// `webview` / `registry` 只用来查「是哪个插件在弹」，不参与通知本身——所以查不到会话也照弹，
/// 不给这条命令加新的失败路径。
#[tauri::command]
pub fn plugin_notify(
    app: AppHandle,
    body: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) {
    use tauri_plugin_notification::NotificationExt;
    // 日志里**只记「谁弹的 + 多长」，绝不记正文**：body 是插件传进来的任意字符串，翻译 /
    // 剪贴板 / 待办 / 密码类插件的通知正文本身就是用户的剪贴板内容、查询词、账号名。
    // release 现在会把日志长期写在 %LOCALAPPDATA%\itools\itools.log 里，用户报障时还会
    // 整份发给我们——原样落盘等于把这些内容永久留在磁盘上（见 `crate::logging` 的「隐私」段）。
    // 插件 id + 字数已足够定位「哪个插件在弹通知 / 到底弹没弹 / 是不是弹了空正文」。
    let who = registry
        .session_for(webview.label())
        .map(|s| if s.dev { format!("dev:{}", s.id) } else { s.id })
        // 查不到会话就退化成窗口 label：至少还能分清是插件窗还是调试窗
        .unwrap_or_else(|| webview.label().to_string());
    ilog!(
        "[iTools][plugin] notify: 来自 {who}，正文 {} 字（正文含用户内容，不入日志）",
        body.chars().count()
    );
    // 真·系统通知（失败不影响插件，已落日志兜底）
    let _ = app
        .notification()
        .builder()
        .title("iTools")
        .body(&body)
        .show();
}

/// 执行程序：显式 program + args，**不经 cmd.exe**（元字符 `&`/`|`/`>` 不会被解释，无 shell 注入面）。
/// 需当前插件已被用户授权 runCommand（在「插件管理」里授权），否则拒绝。
#[tauri::command]
pub fn plugin_run_command(
    program: String,
    args: Vec<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "runCommand") {
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
/// 这里按【调用窗口当前的插件会话】判定，即便被别的插件框入也按顶层插件的授权决定，杜绝借道。
#[tauri::command]
pub async fn plugin_fetch(
    url: String,
    method: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    body: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<FetchResponse, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "network") {
        return Err("插件未获授权联网（请在「插件管理」里授权 network）".to_string());
    }
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("fetch 只支持 http/https".to_string());
    }
    let method = method.unwrap_or_else(|| "GET".to_string()).to_uppercase();
    tauri::async_runtime::spawn_blocking(move || -> Result<FetchResponse, String> {
        // 走统一出站出口（代理才可能真正生效）；原来的 20 秒整体超时改用**按请求**设置，
        // 与 AgentBuilder::timeout 语义相同（都是整次请求-响应的截止时间），行为不变。
        // 重定向次数等其余行为沿用 ureq 默认，与改造前一致。
        let mut req = crate::http::request(&method, &url)
            .timeout(std::time::Duration::from_secs(20));
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

/// 本次调用该用哪个库：**调试会话一律落独立测试库**（另一个 SQLite 文件），
/// 正式会话落正式库。这是「调试数据永不进正式库、永不上云」的物理保证点。
fn session_db<'a>(
    session: &ActiveSession,
    registry: &'a PluginRegistry,
    db: &'a Arc<Db>,
) -> &'a Arc<Db> {
    if session.dev {
        &registry.dev.db
    } else {
        db
    }
}

#[tauri::command]
pub fn plugin_db_get(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<Option<String>, String> {
    let s = caller_session(&webview, &registry)?;
    Ok(session_db(&s, &registry, &db).pkv_get(&s.id, &key))
}

#[tauri::command]
pub fn plugin_db_set(
    key: String,
    value: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<(), String> {
    let s = caller_session(&webview, &registry)?;
    session_db(&s, &registry, &db).pkv_set(&s.id, &key, &value)
}

#[tauri::command]
pub fn plugin_db_remove(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<(), String> {
    let s = caller_session(&webview, &registry)?;
    session_db(&s, &registry, &db).pkv_remove(&s.id, &key)
}

#[tauri::command]
pub fn plugin_db_keys(
    prefix: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<Vec<String>, String> {
    let s = caller_session(&webview, &registry)?;
    Ok(session_db(&s, &registry, &db).pkv_keys(&s.id, &prefix.unwrap_or_default()))
}

// ---------- 账号态 & 本地优先数据（带云同步） ----------

/// 给插件的精简账号态：只暴露「是否登录 / 云端是否已配置 / 是否开启同步」，
/// **不含用户名 / token**（第三方插件不应拿到 PII 与凭据）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAccountState {
    /// 是否已登录云账号（插件据此决定是否走云同步 / 展示登录引导）。
    pub logged_in: bool,
    /// 云端服务是否已配置（false = 云端未接入，只能本地）。
    pub cloud_configured: bool,
    /// 是否开启「登录后自动同步」。
    pub sync_enabled: bool,
}

/// 插件查询账号态（如「是否已登录」以决定要不要云同步）。
///
/// **调试会话返回的是 Mock 的账号态**（由「云同步 Mock」的 mode 推导）——否则开发者把 Mock
/// 设成 success，插件却因为真实账号未登录而根本不走同步分支，模拟器就形同虚设。
/// 调试环境全程不读真实登录态、不联网。
#[tauri::command]
pub fn plugin_account_state(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    account: State<'_, AccountStore>,
) -> PluginAccountState {
    if let Some(s) = registry.session_for(webview.label()) {
        if s.dev {
            let logged_in = registry.dev.mock().mode != "notLoggedIn";
            return PluginAccountState {
                logged_in,
                cloud_configured: true,
                sync_enabled: logged_in,
            };
        }
    }
    PluginAccountState {
        logged_in: account.is_logged_in(),
        cloud_configured: crate::account::cloud_configured(),
        sync_enabled: account.sync_enabled(),
    }
}

/// 当前插件的数据命名空间（按插件 id 隔离，互不可见）。
///
/// `pub(crate)`：调试环境的存储查看器必须用**同一个**函数算命名空间，
/// 否则两处各写一遍 `format!("plugin:{id}")`，将来改了一处就会出现
/// 「插件写进去了、查看器却看不到」的灵异现象。
pub(crate) fn plugin_ns(id: &str) -> String {
    format!("plugin:{id}")
}

/// 读一条同步型数据（本地优先）。返回 JSON 文本（桥接层再 JSON.parse），无则 None。
#[tauri::command]
pub fn plugin_data_get(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    data: State<'_, DataStore>,
) -> Result<Option<String>, String> {
    let s = caller_session(&webview, &registry)?;
    let ns = plugin_ns(&s.id);
    if s.dev {
        return Ok(registry.dev.db.pd_get(&ns, &key));
    }
    Ok(data.get(&ns, &key).map(|v| v.to_string()))
}

/// 写一条同步型数据：**先落本地**（标记待上行）。`value` 为 JSON 文本（桥接层已 stringify）。
///
/// 调试会话写进**测试库**，且**不调度自动上行**——自动同步会把数据真的推到用户的云端，
/// 调试数据绝不能走这条路（这也是隔离必须落在库文件层面的原因：换个 ns 是挡不住 `sync_now` 的）。
#[tauri::command]
pub fn plugin_data_set(
    key: String,
    value: String,
    app: AppHandle,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    data: State<'_, DataStore>,
) -> Result<(), String> {
    let s = caller_session(&webview, &registry)?;
    let ns = plugin_ns(&s.id);
    // value 应为合法 JSON；解析失败则按纯字符串存，保证不丢数据。
    let parsed = serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
    if s.dev {
        return registry
            .dev
            .db
            .pd_set(&ns, &key, &parsed.to_string(), now_secs(), true);
    }
    data.set(&ns, &key, parsed)?;
    // 开启「登录后自动同步」且已登录时：数据变更后防抖自动上行（未开 / 未登录则静默跳过）。
    crate::sync::schedule_auto_sync(&app, &ns);
    Ok(())
}

/// 删一条同步型数据（本地删除）。
#[tauri::command]
pub fn plugin_data_remove(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    data: State<'_, DataStore>,
) -> Result<(), String> {
    let s = caller_session(&webview, &registry)?;
    let ns = plugin_ns(&s.id);
    if s.dev {
        return registry.dev.db.pd_remove(&ns, &key);
    }
    data.remove(&ns, &key)
}

/// 列本插件同步型数据的 key（可前缀过滤）。
#[tauri::command]
pub fn plugin_data_keys(
    prefix: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    data: State<'_, DataStore>,
) -> Result<Vec<String>, String> {
    let s = caller_session(&webview, &registry)?;
    let ns = plugin_ns(&s.id);
    let prefix = prefix.unwrap_or_default();
    if s.dev {
        return Ok(registry.dev.db.pd_keys(&ns, &prefix));
    }
    Ok(data.keys(&ns, &prefix))
}

/// 把本插件的数据同步到云端。诚实降级：未配置 / 未登录返回 `{ synced:false, reason }`（数据仍在本地）。
///
/// **调试会话走本地 Mock，绝不碰真实服务端**（见 [`crate::dev::mock::sync`]）；返回结构与真实
/// 同步完全一致，插件代码无需为调试环境写分支。
///
/// 之所以是 async：Mock 支持人为延迟（测 loading 态），同步命令会阻塞主线程把整个 app 卡住。
#[tauri::command]
pub async fn plugin_data_sync(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    data: State<'_, DataStore>,
    account: State<'_, AccountStore>,
) -> Result<SyncResult, String> {
    let s = caller_session(&webview, &registry)?;
    if s.dev {
        let dev = registry.dev.clone();
        // 阻塞式 sleep 放到 blocking 线程池，不占 async 执行器
        return tauri::async_runtime::spawn_blocking(move || crate::dev::mock::sync(&dev, &s.id))
            .await
            .map_err(|e| format!("模拟同步失败: {e}"));
    }
    Ok(data.sync_gated(&plugin_ns(&s.id), &account))
}

// ---------- 内部辅助 ----------

/// 判定**发起本次 IPC 的窗口**属于哪个插件会话。
///
/// 为什么按窗口而不是全局「当前插件」：正式插件窗（`plugin`）与调试插件窗（`plugin-dev`）
/// 可以同时开着，全局槽会被后打开的那个覆盖——那时调试窗的写入就会落进正式库、
/// 正式插件也可能反过来读到测试库。存储 / 文件 / 授权 / 同步这四类命令必须按窗口判定。
pub(crate) fn caller_session(
    webview: &tauri::Webview,
    registry: &PluginRegistry,
) -> Result<ActiveSession, String> {
    registry
        .session_for(webview.label())
        .ok_or_else(|| "没有正在运行的插件".to_string())
}

/// 正式插件的数据目录：`<数据根>\plugin-data\<id>`（数据根见 [`crate::paths::data_root`]）。
fn plugin_data_dir(id: &str) -> PathBuf {
    crate::paths::data_root().join("plugin-data").join(id)
}

/// 某会话的沙盒文件根：正式会话在 `plugin-data/<id>/files`，**调试会话在 `dev/plugin-data/<id>/files`**。
pub(crate) fn session_files_dir(session: &ActiveSession, registry: &PluginRegistry) -> PathBuf {
    if session.dev {
        crate::dev::storage::sandbox_root(&registry.dev, &session.id)
    } else {
        plugin_data_dir(&session.id).join("files")
    }
}

/// 当前 Unix 秒（调试库写入用；正式路径走 DataStore 内部的时间戳）。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::DevRuntime;
    use crate::plugin::{LoadedPlugin, PluginManifest};

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "itools-session-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    fn manifest(json: &str) -> PluginManifest {
        serde_json::from_str(json).expect("测试清单必须能解析")
    }

    /// 建一套「正式注册表 + 调试运行时」：正式插件 demo 声明 network，调试插件 demo 也声明 network。
    fn fixtures(tag: &str) -> (PluginRegistry, Arc<DevRuntime>, PathBuf) {
        let home = tmp(tag);
        let dev_root = home.join("dev-plugins");
        let plugin_dir = dev_root.join("demo");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"demo","version":"1.0.0","description":"d","permissions":["network"],
                "features":[{"code":"main","cmds":["demo"]}]}"#,
        )
        .unwrap();
        std::fs::write(plugin_dir.join("index.html"), "<html></html>").unwrap();

        let dev = Arc::new(DevRuntime::new(home.join("data"), dev_root));
        assert_eq!(dev.rescan(&[]), 1);
        let registry = PluginRegistry::new(
            home.join("plugins"),
            vec![LoadedPlugin {
                manifest: manifest(
                    r#"{"name":"demo","version":"1.0.0","description":"d","permissions":["network"],
                        "features":[{"code":"main","cmds":["demo"]}]}"#,
                ),
                dir: home.join("plugins").join("demo"),
            }],
            dev.clone(),
        );
        (registry, dev, home)
    }

    fn dev_session() -> ActiveSession {
        ActiveSession {
            id: "demo".to_string(),
            dev: true,
        }
    }
    fn prod_session() -> ActiveSession {
        ActiveSession {
            id: "demo".to_string(),
            dev: false,
        }
    }

    /// **本轮最关键的隔离**：调试会话的存储写入必须落测试库，正式库一行都不能多。
    #[test]
    fn dev_session_writes_only_to_the_test_db() {
        let (registry, dev, home) = fixtures("db");
        let prod = Arc::new(Db::open_memory());

        session_db(&dev_session(), &registry, &prod)
            .pkv_set("demo", "k", "\"from-dev\"")
            .unwrap();
        assert_eq!(
            dev.db.pkv_get("demo", "k").as_deref(),
            Some("\"from-dev\""),
            "调试写入必须落测试库"
        );
        assert_eq!(prod.pkv_get("demo", "k"), None, "正式库绝不能被调试会话写到");

        // 反向：正式会话写正式库，测试库不受影响
        session_db(&prod_session(), &registry, &prod)
            .pkv_set("demo", "k2", "\"from-prod\"")
            .unwrap();
        assert_eq!(prod.pkv_get("demo", "k2").as_deref(), Some("\"from-prod\""));
        assert_eq!(dev.db.pkv_get("demo", "k2"), None);

        // 同步型数据同理（这条才是会被 sync_now 全量上推的那张表）
        let ns = plugin_ns("demo");
        dev.db.pd_set(&ns, "note", "1", 1, true).unwrap();
        assert!(
            prod.pd_namespaces().is_empty(),
            "正式库连命名空间都不该出现——「调试数据永不上云」是物理保证"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 文件沙盒同样分流：调试会话的沙盒根在 dev 目录下，且**不在**正式沙盒里面。
    #[test]
    fn dev_session_uses_a_separate_file_sandbox() {
        let (registry, _dev, home) = fixtures("files");
        let dev_dir = session_files_dir(&dev_session(), &registry);
        let prod_dir = session_files_dir(&prod_session(), &registry);
        assert_ne!(dev_dir, prod_dir);
        assert!(!dev_dir.starts_with(&prod_dir));
        assert!(dev_dir.starts_with(home.join("data")));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 授权表分流：调试授权只对调试会话生效，绝不外溢到正式插件（反之亦然）。
    #[test]
    fn permission_tables_are_separate() {
        let (registry, _dev, home) = fixtures("perm");
        let db = Arc::new(Db::open_memory());
        let settings = SettingsStore::load(db);

        // 只授权「调试环境」的 network
        let mut s = settings.get();
        s.dev_plugin_permissions
            .insert("demo".to_string(), vec!["network".to_string()]);
        settings.set(s);
        assert!(plugin_granted(&settings, &registry, &dev_session(), "network"));
        assert!(
            !plugin_granted(&settings, &registry, &prod_session(), "network"),
            "调试环境的授权不得让正式插件也拿到能力"
        );

        // 只授权「正式」的 network
        let mut s = settings.get();
        s.dev_plugin_permissions.clear();
        s.plugin_permissions
            .insert("demo".to_string(), vec!["network".to_string()]);
        settings.set(s);
        assert!(plugin_granted(&settings, &registry, &prod_session(), "network"));
        assert!(
            !plugin_granted(&settings, &registry, &dev_session(), "network"),
            "正式授权不得自动带到调试会话"
        );

        // 未在清单里声明的能力，两边都不给（双条件的第二条）
        let mut s = settings.get();
        s.dev_plugin_permissions
            .insert("demo".to_string(), vec!["runCommand".to_string()]);
        settings.set(s);
        assert!(!plugin_granted(&settings, &registry, &dev_session(), "runCommand"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 两个窗口各记各的会话：全局单槽会被后打开者覆盖，那正是串库的根因。
    #[test]
    fn sessions_are_tracked_per_window() {
        let (registry, _dev, home) = fixtures("window");
        registry.set_session(PLUGIN_WINDOW_LABEL, prod_session());
        registry.set_session(crate::dev::DEV_WINDOW_LABEL, dev_session());
        // 后登记的调试会话不能改写正式窗口的会话
        assert_eq!(registry.session_for(PLUGIN_WINDOW_LABEL), Some(prod_session()));
        assert_eq!(
            registry.session_for(crate::dev::DEV_WINDOW_LABEL),
            Some(dev_session())
        );
        assert_eq!(registry.session_for("main"), None, "非插件窗口没有会话");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 进入信息也按窗口隔离：否则两个窗口同时加载时会互相取走对方的 onEnter 上下文。
    #[test]
    fn pending_enter_is_per_window() {
        let (registry, _dev, home) = fixtures("enter");
        let info = |code: &str| EnterInfo {
            code: code.to_string(),
            kind: "keyword".to_string(),
            query: String::new(),
            plugin_id: "demo".to_string(),
        };
        registry.set_enter(PLUGIN_WINDOW_LABEL, info("prod"));
        registry.set_enter(crate::dev::DEV_WINDOW_LABEL, info("dev"));
        assert_eq!(
            registry.take_enter(PLUGIN_WINDOW_LABEL).map(|e| e.code),
            Some("prod".to_string())
        );
        assert_eq!(
            registry.take_enter(crate::dev::DEV_WINDOW_LABEL).map(|e| e.code),
            Some("dev".to_string())
        );
        assert!(registry.take_enter(PLUGIN_WINDOW_LABEL).is_none(), "取走即清");
        // 热更新重放只补回本窗口的
        registry.reseed_enter(PLUGIN_WINDOW_LABEL);
        assert_eq!(
            registry.take_enter(PLUGIN_WINDOW_LABEL).map(|e| e.code),
            Some("prod".to_string())
        );
        assert!(registry.take_enter(crate::dev::DEV_WINDOW_LABEL).is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}
