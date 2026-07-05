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

/// 判定某插件是否被用户授权了某高危能力（runCommand / network / screen-capture / …）。
pub(crate) fn plugin_granted(settings: &SettingsStore, plugin: &str, perm: &str) -> bool {
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
    let registry = app.state::<PluginRegistry>();
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
            .inner_size(760.0, 560.0)
            .min_inner_size(360.0, 240.0)
            .resizable(true)
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

/// 删除插件沙盒内的文件（相对路径，同 write/read 的沙盒约束）。不存在则视为成功（幂等）。
#[tauri::command]
pub fn plugin_remove_file(path: String, registry: State<'_, PluginRegistry>) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    let sandbox = plugin_files_dir(&id);
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
pub fn plugin_notify(app: AppHandle, body: String) {
    use tauri_plugin_notification::NotificationExt;
    ilog!("[iTools][plugin] notify: {body}");
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

pub(crate) fn current_plugin(registry: &PluginRegistry) -> Result<String, String> {
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
