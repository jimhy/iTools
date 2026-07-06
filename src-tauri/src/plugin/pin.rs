//! 贴图（Pin）：把一张图片钉成无边框、置顶、透明的浮窗，可拖动/滚轮缩放/双击关闭/调透明度。
//! 取代插件用 PowerShell 拉 WinForms 窗的老路。图片经 `itpin://` 协议服务，窗口是 Tauri WebviewWindow。
//!
//! 拖动用 HTML `data-tauri-drag-region`（Tauri 原生识别，无需 IPC）；缩放/关闭由 pin 页调 pin_resize/pin_close
//! （作用于调用它的窗口自身）。多个贴图 = 多个 `pin-<id>` 窗口，图片按 id 存 [`Pins`]。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager, State, WebviewWindow};

/// 贴图图片仓：pinId → PNG 字节。供 itpin 协议按 id 取图。
#[derive(Default)]
pub struct Pins {
    imgs: Mutex<HashMap<String, Vec<u8>>>,
    counter: AtomicU64,
}

impl Pins {
    pub fn img(&self, id: &str) -> Option<Vec<u8>> {
        self.imgs.lock().ok()?.get(id).cloned()
    }
}

/// 创建一个贴图浮窗，显示给定图片（base64，任意 image 可解码格式）。返回 pinId。
/// opacity: 0.1~1.0（默认 1.0）。窗口尺寸自适应图片，超过主屏 80% 则等比缩小。
#[tauri::command]
pub async fn plugin_create_pin(
    b64: String,
    opacity: Option<f64>,
    app: AppHandle,
    pins: State<'_, Pins>,
) -> Result<String, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    create_pin_bytes(&app, &pins, bytes, opacity.unwrap_or(1.0))
}

/// 从图片字节直接创建贴图窗（宿主原生截图「贴图」动作与插件命令共用）。
pub fn create_pin_bytes(
    app: &AppHandle,
    pins: &Pins,
    bytes: Vec<u8>,
    opacity: f64,
) -> Result<String, String> {
    let img = image::load_from_memory(&bytes).map_err(|e| format!("图片解码失败: {e}"))?;
    let (w, h) = (img.width() as f64, img.height() as f64);
    drop(img);

    let pin_id = pins.counter.fetch_add(1, Ordering::Relaxed).to_string();
    pins.imgs.lock().unwrap().insert(pin_id.clone(), bytes);

    // 初始尺寸：不超过主屏工作区 80%（逻辑像素）
    let (mut vw, mut vh) = (w, h);
    if let Some(mon) = app.primary_monitor().ok().flatten() {
        let s = mon.scale_factor();
        let sz = mon.size();
        let maxw = sz.width as f64 / s * 0.8;
        let maxh = sz.height as f64 / s * 0.8;
        let k = (maxw / vw).min(maxh / vh).min(1.0);
        if k > 0.0 && k < 1.0 {
            vw *= k;
            vh *= k;
        }
    }

    let op = opacity.clamp(0.1, 1.0);
    let url: tauri::Url = format!("itpin://localhost/view/{pin_id}?op={op}")
        .parse()
        .map_err(|e| format!("URL 解析失败: {e}"))?;
    let label = format!("pin-{pin_id}");
    let built = tauri::WebviewWindowBuilder::new(app, &label, tauri::WebviewUrl::External(url))
        .title("贴图")
        .inner_size(vw.max(24.0), vh.max(24.0))
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .maximizable(false)
        .build();
    if built.is_err() {
        // build 失败：撤掉刚插入的图片，避免孤儿泄漏（Destroyed 清理不会触发）
        pins.imgs.lock().unwrap().remove(&pin_id);
    }
    built.map_err(|e| format!("创建贴图窗失败: {e}"))?;
    // 窗口被任何方式销毁都清掉其图片缓存（不止 pin_close 一条路），避免内存泄漏
    if let Some(win) = app.get_webview_window(&label) {
        let app2 = app.clone();
        let pid = pin_id.clone();
        win.on_window_event(move |ev| {
            if matches!(ev, tauri::WindowEvent::Destroyed) {
                if let Some(pins) = app2.try_state::<Pins>() {
                    if let Ok(mut m) = pins.imgs.lock() {
                        m.remove(&pid);
                    }
                }
            }
        });
    }
    Ok(pin_id)
}

/// 缩放调用它的贴图窗（逻辑像素）。pin 页滚轮缩放/缩略图切换时调用。
#[tauri::command]
pub fn pin_resize(window: WebviewWindow, width: f64, height: f64) -> Result<(), String> {
    window
        .set_size(tauri::LogicalSize::new(width.max(16.0), height.max(16.0)))
        .map_err(|e| e.to_string())
}

/// 按物理像素增量移动调用它的贴图窗（pin 页拖动时调用；每次读当前位置再加增量，无累积漂移）。
#[tauri::command]
pub fn pin_move(window: WebviewWindow, dx: i32, dy: i32) -> Result<(), String> {
    let pos = window.outer_position().map_err(|e| e.to_string())?;
    window
        .set_position(tauri::PhysicalPosition::new(pos.x + dx, pos.y + dy))
        .map_err(|e| e.to_string())
}

/// 关闭调用它的贴图窗，并清理其图片缓存。
#[tauri::command]
pub fn pin_close(window: WebviewWindow, pins: State<'_, Pins>) {
    if let Some(id) = window.label().strip_prefix("pin-") {
        if let Ok(mut m) = pins.imgs.lock() {
            m.remove(id);
        }
    }
    let _ = window.close();
}

// ============ 宿主贴图全局热键（headless：读剪贴板图片贴成置顶浮窗） ============

/// 宿主贴图热键状态（供全局 handler 辨认 + 换绑时撤旧）。
#[derive(Default)]
pub struct PinHotkeyState {
    pub shortcut: Mutex<Option<tauri_plugin_global_shortcut::Shortcut>>,
}

/// 该 shortcut id 是否是宿主贴图热键。
pub fn is_pin_hotkey(app: &AppHandle, id: u32) -> bool {
    app.try_state::<PinHotkeyState>()
        .and_then(|s| s.shortcut.lock().ok().and_then(|g| g.map(|sc| sc.id() == id)))
        .unwrap_or(false)
}

/// 注册/换绑宿主贴图全局热键（默认 f3，可在 iTools 设置里改）。
pub fn register_pin_hotkey(app: &AppHandle, accel: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let sc = crate::hotkey::parse_hotkey(accel).ok_or_else(|| format!("无效快捷键：{accel}"))?;
    let st = app.state::<PinHotkeyState>();
    // 撤旧键（换绑）
    if let Some(old) = st.shortcut.lock().unwrap().take() {
        if old.id() != sc.id() {
            let _ = app.global_shortcut().unregister(old);
        }
    }
    app.global_shortcut()
        .register(sc)
        .map_err(|e| format!("注册贴图热键失败（可能被占用）：{e}"))?;
    *st.shortcut.lock().unwrap() = Some(sc);
    Ok(())
}

/// 把贴图热键重同步到 accel：先撤旧键+清状态，accel 非空则注册新键（空 = 只清）。
/// 供 save_settings 在主唤起热键改动（unregister_all）后统一重建，避免僵尸状态。
pub fn resync_pin_hotkey(app: &AppHandle, accel: &str) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    if let Some(st) = app.try_state::<PinHotkeyState>() {
        if let Some(old) = st.shortcut.lock().unwrap().take() {
            let _ = app.global_shortcut().unregister(old);
        }
    }
    if !accel.trim().is_empty() {
        let _ = register_pin_hotkey(app, accel.trim());
    }
}

/// 后台异步从剪贴板贴图（热键 handler 调用，不阻塞）。
pub fn trigger_pin(app: &AppHandle) {
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = pin_from_clipboard(&app2) {
            crate::logging::ilog!("[pin] 贴图失败：{e}");
        }
    });
}

/// 读剪贴板图片贴成置顶浮窗（无图片则忽略）。透明度用默认 1.0。
fn pin_from_clipboard(app: &AppHandle) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = cb.get_image().map_err(|_| "剪贴板没有图片".to_string())?;
    let png = crate::plugin::commands::rgba_to_png(img.width, img.height, &img.bytes)?;
    let pins = app.state::<Pins>();
    create_pin_bytes(app, &pins, png, 1.0)?;
    Ok(())
}
