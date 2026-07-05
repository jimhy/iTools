//! 原生屏幕捕获（xcap，底层 Windows.Graphics.Capture）。
//!
//! 取代插件用「隐藏 + base64 编码的 PowerShell 抓屏」的老路——那套是杀软木马指纹的元凶。
//! 本模块全部经 **screen-capture** 授权门控（用户在「插件管理」按插件开启）。
//! 图片以原始 PNG 字节经 `tauri::ipc::Response` 回传（前端拿到 ArrayBuffer，零 base64）。
//!
//! 框选覆盖层（region）见 [`overlay`] 子模块：冻结整屏 → 透明置顶窗拖选 → 裁剪。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use tokio::sync::oneshot;

use super::commands::{current_plugin, plugin_granted, png_to_b64, rgba_to_bmp, rgba_to_png};
use super::PluginRegistry;
use crate::settings::SettingsStore;

/// 校验当前活动插件已获 screen-capture 授权。
pub(crate) fn require_capture(
    registry: &PluginRegistry,
    settings: &SettingsStore,
) -> Result<(), String> {
    let id = current_plugin(registry)?;
    if !plugin_granted(settings, &id, "screen-capture") {
        return Err("插件未获授权截屏（请在「插件管理」里授权 screen-capture）".to_string());
    }
    Ok(())
}

/// 截取主屏为 RGBA（xcap::Monitor 不是 Send，务必在此函数内用完即弃，勿跨 .await 持有）。
fn capture_primary() -> Result<image::RgbaImage, String> {
    let mon = primary_monitor()?;
    mon.capture_image().map_err(|e| format!("截屏失败: {e}"))
}

/// 截主屏并按宽度上限等比缩小（录屏逐帧用）。同样在函数内用完即弃 Monitor。
pub(crate) fn capture_primary_downscaled(max_w: u32) -> Result<image::RgbaImage, String> {
    let img = capture_primary()?;
    if img.width() <= max_w {
        return Ok(img);
    }
    let h = (img.height() as u64 * max_w as u64 / img.width() as u64) as u32;
    Ok(image::imageops::resize(
        &img,
        max_w,
        h.max(1),
        image::imageops::FilterType::Triangle,
    ))
}

/// 挑选主屏（无主屏则第一个）。
fn primary_monitor() -> Result<xcap::Monitor, String> {
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let mut chosen: Option<xcap::Monitor> = None;
    for m in monitors {
        if m.is_primary().unwrap_or(false) {
            return Ok(m);
        }
        if chosen.is_none() {
            chosen = Some(m);
        }
    }
    chosen.ok_or_else(|| "未找到任何显示器".to_string())
}

/// 全局光标位置（物理像素）。用于区域截图定位到「光标所在的显示器」而非固定主屏。
fn cursor_pos() -> (i32, i32) {
    #[cfg(windows)]
    unsafe {
        let mut pt = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        let _ = windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt);
        (pt.x, pt.y)
    }
    #[cfg(not(windows))]
    {
        (0, 0)
    }
}

/// 截取「光标所在显示器」为 RGBA（xcap::Monitor 不 Send，函数内用完即弃）。
fn capture_at(cx: i32, cy: i32) -> Result<image::RgbaImage, String> {
    let mon = xcap::Monitor::from_point(cx, cy)
        .map_err(|e| e.to_string())
        .or_else(|_| primary_monitor())?;
    mon.capture_image().map_err(|e| format!("截屏失败: {e}"))
}

/// 冻结「光标所在」显示器为 BMP 字节 + 返回该屏物理矩形(px,py,pw,ph，用于覆盖窗定位)。
/// 优先 GDI BitBlt（快、返回 BGRA 直封 BMP）；非 Windows 或 GDI 失败退回 xcap。
fn freeze_target(
    app: &tauri::AppHandle,
    cx: i32,
    cy: i32,
) -> Result<(Vec<u8>, i32, i32, u32, u32), String> {
    if let Some(m) = tauri_monitor_at(app, cx, cy) {
        let p = m.position();
        let z = m.size();
        let (px, py, pw, ph) = (p.x, p.y, z.width, z.height);
        #[cfg(windows)]
        {
            if let Ok(bmp) = capture_gdi(px, py, pw as i32, ph as i32) {
                return Ok((bmp, px, py, pw, ph)); // capture_gdi 已直接产出完整 BMP（无二次拷贝）
            }
        }
        // 退回 xcap（抓光标所在屏），仍用该屏矩形定位
        let img = capture_at(cx, cy)?;
        let (w, h) = img.dimensions();
        return Ok((
            rgba_to_bmp(w as usize, h as usize, &img.into_raw())?,
            px,
            py,
            w,
            h,
        ));
    }
    // 极端：完全拿不到显示器信息 → xcap + 原点
    let img = capture_at(cx, cy)?;
    let (w, h) = img.dimensions();
    Ok((
        rgba_to_bmp(w as usize, h as usize, &img.into_raw())?,
        0,
        0,
        w,
        h,
    ))
}

/// GDI BitBlt 抓屏矩形 → **完整 BMP 字节**（32-bit top-down，头部就地写在缓冲前 54 字节，
/// GDI 直接把像素写到头之后 → 全程零二次拷贝）。Windows 专用，快。
#[cfg(windows)]
fn capture_gdi(x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };
    if w <= 0 || h <= 0 {
        return Err("尺寸非法".into());
    }
    let pixels = (w as usize) * (h as usize) * 4;
    let mut buf = vec![0u8; 54 + pixels];
    // 就地写 BMP 头（BITMAPFILEHEADER 14 + BITMAPINFOHEADER 40）
    buf[0..2].copy_from_slice(b"BM");
    buf[2..6].copy_from_slice(&((54 + pixels) as u32).to_le_bytes());
    buf[10..14].copy_from_slice(&54u32.to_le_bytes());
    buf[14..18].copy_from_slice(&40u32.to_le_bytes());
    buf[18..22].copy_from_slice(&w.to_le_bytes());
    buf[22..26].copy_from_slice(&(-h).to_le_bytes()); // top-down
    buf[26..28].copy_from_slice(&1u16.to_le_bytes());
    buf[28..30].copy_from_slice(&32u16.to_le_bytes());
    buf[34..38].copy_from_slice(&(pixels as u32).to_le_bytes());
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return Err("GetDC 失败".into());
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        if mem.is_null() || bmp.is_null() {
            if !bmp.is_null() {
                DeleteObject(bmp as _);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
            ReleaseDC(std::ptr::null_mut(), screen);
            return Err("创建 DC/位图失败".into());
        }
        let old = SelectObject(mem, bmp as _);
        let blt = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB as u32;
        // 像素直接写进 buf 头之后
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr().add(54) as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = SelectObject(mem, old);
        DeleteObject(bmp as _);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if blt == 0 || lines == 0 {
            return Err("BitBlt/GetDIBits 失败".into());
        }
        // GetDIBits 第 4 字节可能为 0，强制不透明，避免浏览器把 BMP 当半透明
        for px in buf[54..].chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        Ok(buf)
    }
}

/// （已由原生覆盖层内部 BitBlt 抓屏取代，保留占位以便非覆盖层场景复用）GDI 抓屏矩形 → 原始 BGRA。
#[cfg(windows)]
#[allow(dead_code)]
fn capture_gdi_raw(x: i32, y: i32, w: i32, h: i32) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };
    if w <= 0 || h <= 0 {
        return Err("尺寸非法".into());
    }
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        if screen.is_null() {
            return Err("GetDC 失败".into());
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp as _);
        let blt = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB as u32;
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut _,
            &mut bi,
            DIB_RGB_COLORS,
        );
        let _ = SelectObject(mem, old);
        DeleteObject(bmp as _);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        if blt == 0 || lines == 0 {
            return Err("BitBlt/GetDIBits 失败".into());
        }
        for px in buf.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
    }
    Ok(buf)
}

// ============ 宿主原生截图（headless，取代 pixshot 插件的角色） ============

/// 宿主截图全局热键的状态（供全局快捷键 handler 辨认 + 换绑时撤旧）。
#[derive(Default)]
pub struct ScreenshotState {
    pub shortcut: Mutex<Option<tauri_plugin_global_shortcut::Shortcut>>,
}

/// 该 shortcut id 是否是宿主截图热键。
pub fn is_screenshot_hotkey(app: &tauri::AppHandle, id: u32) -> bool {
    app.try_state::<ScreenshotState>()
        .and_then(|s| s.shortcut.lock().ok().and_then(|g| g.map(|sc| sc.id() == id)))
        .unwrap_or(false)
}

/// 注册/换绑宿主截图全局热键（默认 ctrl+shift+a，可在 iTools 设置里改）。
pub fn register_screenshot_hotkey(app: &tauri::AppHandle, accel: &str) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let sc = crate::hotkey::parse_hotkey(accel).ok_or_else(|| format!("无效快捷键：{accel}"))?;
    let st = app.state::<ScreenshotState>();
    // 撤旧键（换绑）
    if let Some(old) = st.shortcut.lock().unwrap().take() {
        if old.id() != sc.id() {
            let _ = app.global_shortcut().unregister(old);
        }
    }
    app.global_shortcut()
        .register(sc)
        .map_err(|e| format!("注册截图热键失败（可能被占用）：{e}"))?;
    *st.shortcut.lock().unwrap() = Some(sc);
    Ok(())
}

/// 后台异步跑一次截图（热键 handler 调用，不阻塞）。
pub fn trigger_screenshot(app: &tauri::AppHandle, full: bool) {
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_screenshot(app2, full).await {
            crate::logging::ilog!("[shot] 截图失败：{e}");
        }
    });
}

/// 宿主原生截图全流程：原生覆盖层框选/标注 → 按用户选的动作在 Rust 里落地（复制/保存/贴图/OCR）。
pub async fn run_screenshot(app: tauri::AppHandle, full: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let (vx, vy, vw, vh) = virtual_rect();
        let res = tauri::async_runtime::spawn_blocking(move || {
            super::native_overlay::run(vx, vy, vw, vh, full)
        })
        .await
        .map_err(|e| e.to_string())?;
        let r = match res {
            Some(r) => r,
            None => return Ok(()), // 用户取消
        };
        do_screenshot_action(&app, r).await?;
    }
    #[cfg(not(windows))]
    {
        let _ = (app, full);
    }
    Ok(())
}

#[cfg(windows)]
async fn do_screenshot_action(app: &tauri::AppHandle, r: super::native_overlay::NativeResult) -> Result<(), String> {
    match r.action.as_str() {
        "save" => {
            let png = rgba_to_png(r.w as usize, r.h as usize, &r.rgba)?;
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let name = format!("Shot_{ts}.png");
            tauri::async_runtime::spawn_blocking(move || {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(&name)
                    .add_filter("PNG 图片", &["png"])
                    .save_file()
                {
                    let _ = std::fs::write(path, &png);
                }
            })
            .await
            .map_err(|e| e.to_string())?;
        }
        "pin" => {
            let png = rgba_to_png(r.w as usize, r.h as usize, &r.rgba)?;
            let pins = app.state::<super::pin::Pins>();
            super::pin::create_pin_bytes(app, &pins, png, 1.0)?;
        }
        "ocr" => {
            let png = rgba_to_png(r.w as usize, r.h as usize, &r.rgba)?;
            let text =
                tauri::async_runtime::spawn_blocking(move || super::ocr::ocr_png(&png, None))
                    .await
                    .map_err(|e| e.to_string())??;
            if !text.trim().is_empty() {
                set_clipboard_text(&text)?;
            }
        }
        _ => {
            // copy（默认）
            set_clipboard_image(r.w, r.h, r.rgba)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn set_clipboard_image(w: u32, h: u32, rgba: Vec<u8>) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(rgba),
    })
    .map_err(|e| e.to_string())
}

#[cfg(windows)]
fn set_clipboard_text(s: &str) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(s).map_err(|e| e.to_string())
}

/// 整个虚拟桌面矩形（所有屏，物理像素）。
fn virtual_rect() -> (i32, i32, i32, i32) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
    #[cfg(not(windows))]
    {
        (0, 0, 1920, 1080)
    }
}

/// 找出包含物理点 (cx,cy) 的 Tauri 显示器（用于覆盖窗定位，Tauri Monitor 是 Send）。
fn tauri_monitor_at(app: &tauri::AppHandle, cx: i32, cy: i32) -> Option<tauri::Monitor> {
    if let Ok(mons) = app.available_monitors() {
        for m in &mons {
            let p = m.position();
            let s = m.size();
            if cx >= p.x && cx < p.x + s.width as i32 && cy >= p.y && cy < p.y + s.height as i32 {
                return Some(m.clone());
            }
        }
    }
    app.primary_monitor().ok().flatten()
}

#[derive(Serialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub is_primary: bool,
}

/// 列出所有显示器（虚拟屏坐标 + 缩放）。需 screen-capture 授权。
#[tauri::command]
pub fn plugin_list_displays(
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<DisplayInfo>, String> {
    require_capture(&registry, &settings)?;
    let monitors = xcap::Monitor::all().map_err(|e| e.to_string())?;
    let out = monitors
        .into_iter()
        .map(|m| DisplayInfo {
            id: m.id().unwrap_or(0),
            name: m.friendly_name().or_else(|_| m.name()).unwrap_or_default(),
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            scale: m.scale_factor().unwrap_or(1.0),
            is_primary: m.is_primary().unwrap_or(false),
        })
        .collect();
    Ok(out)
}

/// 全屏截图：指定 display_id（缺省主屏）→ base64 PNG（桥接层解回 ArrayBuffer）。需 screen-capture 授权。
#[tauri::command]
pub fn plugin_capture_full(
    display_id: Option<u32>,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    require_capture(&registry, &settings)?;
    let img = {
        let mon = match display_id {
            Some(want) => xcap::Monitor::all()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|m| m.id().map(|i| i == want).unwrap_or(false))
                .ok_or_else(|| format!("找不到显示器 id={want}"))?,
            None => primary_monitor()?,
        };
        mon.capture_image().map_err(|e| format!("截屏失败: {e}"))?
    };
    let (w, h) = img.dimensions();
    let png = rgba_to_png(w as usize, h as usize, &img.into_raw())?;
    Ok(png_to_b64(&png))
}

// ==================== 区域框选 + 就地标注（PixPin 风格覆盖层） ====================

/// 覆盖层回传的结果：已合成的最终 PNG（base64）+ 用户点的动作（copy/save/pin/ocr）。
/// 标注与裁剪都在覆盖层里完成，后端只透传。
#[derive(Debug, Clone, Deserialize)]
pub struct RegionResult {
    pub b64: String,
    pub action: String,
}

/// captureRegion 的返回：动作 + 最终图片 base64。
#[derive(Serialize)]
pub struct RegionOut {
    pub action: String,
    pub b64: String,
}

/// 区域截图流程的运行期状态（managed）：
/// - `frozen`：冻结的目标屏 RGBA，覆盖层经 `/frozen.png` 显示（就地标注 + 合成都在覆盖层里做）。
/// - `sender`：等待覆盖层结果的 oneshot 发送端（report 时取走并发送）。
/// - `busy`：在途标志，防止并发两次 captureRegion 互相踩踏同一 slot/窗口。
#[derive(Default)]
pub struct CaptureFlow {
    /// 冻结图的**预编码 BMP 字节**（无压缩，近乎瞬时）。协议 /frozen.png 直接吐。
    frozen: Mutex<Option<Vec<u8>>>,
    sender: Mutex<Option<oneshot::Sender<Option<RegionResult>>>>,
    busy: AtomicBool,
    /// 每次截图递增，用作冻结图 URL 的 cache-bust（复用覆盖窗时强制重拉新图）。
    bust: AtomicU64,
}

impl CaptureFlow {
    /// 供 itoverlay 协议服务冻结图（已是 PNG 字节，直接返回）。
    pub fn frozen_png(&self) -> Option<Vec<u8>> {
        self.frozen.lock().ok()?.clone()
    }
}

/// 区域截图（PixPin 风格）：隐藏面板 → 冻结「光标所在屏」→ 打开透明置顶覆盖层（框选 + 就地标注 + 悬浮工具栏）
/// → 覆盖层合成最终图 + 用户选的动作回传。`full=true` 则覆盖层开局即选中整屏。
/// 需 screen-capture 授权。用户取消返回 Err("__cancelled__")（桥接层映射为 null）。
#[tauri::command]
pub async fn plugin_capture_region(
    full: Option<bool>,
    app: tauri::AppHandle,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    flow: State<'_, CaptureFlow>,
) -> Result<RegionOut, String> {
    require_capture(&registry, &settings)?;
    // 在途守卫：已有一次 captureRegion 在跑就拒绝（避免并发踩同一 slot/覆盖窗）
    if flow.busy.swap(true, Ordering::SeqCst) {
        return Err("截图正在进行中".to_string());
    }
    let plugin_win = app.get_webview_window("plugin");
    // 面板截图前是否可见：热键唤起时面板通常已隐藏 → 全程不显示它（不闪面板），事后也不弹出来
    let was_visible = plugin_win
        .as_ref()
        .map(|w| w.is_visible().unwrap_or(false))
        .unwrap_or(false);
    // Windows：走**原生 GDI 覆盖层**（即时显示、跨所有屏、就地标注）；其他平台退回 WebView 覆盖层
    #[cfg(windows)]
    let outcome = {
        let _ = &flow;
        capture_region_native(&app, full.unwrap_or(false), plugin_win.clone(), was_visible).await
    };
    #[cfg(not(windows))]
    let outcome =
        capture_region_inner(&app, &flow, full.unwrap_or(false), plugin_win.clone(), was_visible)
            .await;
    // 统一收尾：清状态、**隐藏**覆盖窗（常驻复用，不销毁）、复位 busy；仅当截图前面板本就可见才恢复显示
    *flow.sender.lock().unwrap() = None;
    let _ = flow.frozen.lock().unwrap().take();
    if let Some(w) = app.get_webview_window("capture-overlay") {
        let _ = w.hide();
    }
    if was_visible {
        if let Some(w) = &plugin_win {
            let _ = w.show();
            let _ = w.set_focus();
        }
    }
    flow.busy.store(false, Ordering::SeqCst);
    outcome
}

/// 原生 GDI 覆盖层区域截图（Windows）：藏面板 → 抓**整个虚拟桌面(所有屏)** → 原生覆盖层
/// (框选 + 就地标注 + 悬浮工具栏) → 合成 PNG + 动作回传。取消返回 Err("__cancelled__")。
#[cfg(windows)]
async fn capture_region_native(
    _app: &tauri::AppHandle,
    full: bool,
    plugin_win: Option<tauri::WebviewWindow>,
    was_visible: bool,
) -> Result<RegionOut, String> {
    // 面板可见才藏它 + 等一帧让它从画面消失（否则会被抓进冻结图）
    if was_visible {
        if let Some(w) = &plugin_win {
            let _ = w.hide();
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
    let (vx, vy, vw, vh) = virtual_rect();
    // 覆盖层自带消息循环 + 自己 BitBlt 抓屏（窗口与循环同线程，Win32 要求），放 blocking 线程跑
    let res = tauri::async_runtime::spawn_blocking(move || {
        super::native_overlay::run(vx, vy, vw, vh, full)
    })
    .await
    .map_err(|e| e.to_string())?;
    match res {
        Some(r) => {
            let png = rgba_to_png(r.w as usize, r.h as usize, &r.rgba)?;
            Ok(RegionOut {
                action: r.action,
                b64: png_to_b64(&png),
            })
        }
        None => Err("__cancelled__".to_string()),
    }
}

#[allow(dead_code)]
async fn capture_region_inner(
    app: &tauri::AppHandle,
    flow: &CaptureFlow,
    full: bool,
    plugin_win: Option<tauri::WebviewWindow>,
    was_visible: bool,
) -> Result<RegionOut, String> {
    // 仅当面板当前可见才藏它 + 等一帧让它从画面消失；热键唤起面板本就隐藏 → 省掉这段延迟与闪烁
    if was_visible {
        if let Some(w) = &plugin_win {
            let _ = w.hide();
        }
        tokio::time::sleep(Duration::from_millis(180)).await;
    }

    // 冻结「光标所在」的显示器：优先 GDI BitBlt（返回 BGRA→直封 BMP，无压缩无转换），退回 xcap。
    let (cx, cy) = cursor_pos();
    let (bmp, px, py, pw, ph) = freeze_target(app, cx, cy)?;
    let iw = pw;
    let ih = ph;

    let (tx, rx) = oneshot::channel::<Option<RegionResult>>();
    {
        *flow.frozen.lock().unwrap() = Some(bmp);
        *flow.sender.lock().unwrap() = Some(tx);
    }
    // 覆盖窗物理矩形直接用 freeze_target 给的目标屏矩形（px,py,pw,ph 物理像素）

    let bust = flow.bust.fetch_add(1, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window("capture-overlay") {
        // **复用**已存在的覆盖窗（页面已热）：移到目标屏 + eval __begin 重置状态并重载新冻结图。
        // 这一步省掉了每次重建 WebView2 + 重新加载页面的几百 ms（这是原来 2 秒延迟的大头）。
        let _ = win.set_position(tauri::PhysicalPosition::new(px, py));
        let _ = win.set_size(tauri::PhysicalSize::new(pw, ph));
        let _ = win.eval(&format!("window.__begin && window.__begin({}, {})", full, bust));
    } else {
        // 首次：建**隐藏**窗；页面初始化脚本自会 __begin(载图→就绪→capture_overlay_ready 显示)
        let url_str = format!(
            "itoverlay://localhost/overlay.html?full={}&b={}",
            if full { 1 } else { 0 },
            bust
        );
        let url: tauri::Url = url_str.parse().map_err(|e| format!("URL 解析失败: {e}"))?;
        let build = tauri::WebviewWindowBuilder::new(
            app,
            "capture-overlay",
            tauri::WebviewUrl::External(url),
        )
        .title("截图")
        .position(px as f64, py as f64)
        .inner_size(pw.max(1) as f64, ph.max(1) as f64)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .build();
        if let Err(e) = build {
            return Err(format!("创建覆盖窗失败: {e}"));
        }
        if let Some(win) = app.get_webview_window("capture-overlay") {
            let _ = win.set_position(tauri::PhysicalPosition::new(px, py));
            let _ = win.set_size(tauri::PhysicalSize::new(pw, ph));
            // Destroyed 兜底只在首次创建时挂一次（此后窗口常驻，仅 hide/show）
            let app2 = app.clone();
            win.on_window_event(move |ev| {
                if matches!(
                    ev,
                    tauri::WindowEvent::Destroyed | tauri::WindowEvent::CloseRequested { .. }
                ) {
                    if let Some(flow) = app2.try_state::<CaptureFlow>() {
                        if let Some(tx) = flow.sender.lock().unwrap().take() {
                            let _ = tx.send(None);
                        }
                    }
                }
            });
        }
    }
    // 兜底：ready 没来就 1.5s 后强制显示——但仅当本次截图仍在进行（sender 还在），避免已取消后又冒出来
    {
        let app3 = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if let Some(flow) = app3.try_state::<CaptureFlow>() {
                let still = flow.sender.lock().map(|g| g.is_some()).unwrap_or(false);
                if still {
                    if let Some(w) = app3.get_webview_window("capture-overlay") {
                        let _ = w.show();
                        let _ = w.set_focus();
                    }
                }
            }
        });
    }

    // 等覆盖层结果（sender 被 drop 视为取消）
    let result = rx.await.unwrap_or(None);
    match result {
        Some(r) if r.b64.len() > 8 => Ok(RegionOut {
            action: r.action,
            b64: r.b64,
        }),
        _ => Err("__cancelled__".to_string()),
    }
}

/// 覆盖层回调：上报最终结果（None = 取消）。由 capture-overlay 窗口调用。
#[tauri::command]
pub fn capture_region_report(result: Option<RegionResult>, flow: State<'_, CaptureFlow>) {
    if let Some(tx) = flow.sender.lock().unwrap().take() {
        let _ = tx.send(result);
    }
}

/// 覆盖层就绪回调：冻结图已加载好，显示并聚焦覆盖窗（此前隐藏，避免黑屏）。由 capture-overlay 窗口调用。
#[tauri::command]
pub fn capture_overlay_ready(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("capture-overlay") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}
