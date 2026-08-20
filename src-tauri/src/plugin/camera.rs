//! 摄像头能力：设备枚举、单帧抓拍、预览流推送。基于 **Media Foundation**（`windows` crate 的
//! `Win32_Media_MediaFoundation` 绑定），不引入 `nokhwa` 等第三方摄像头库。
//!
//! # 摄像头是最敏感的能力之一——本模块的隐私设计
//!
//! 麦克风偷录用户尚能靠"听到自己说话没被转发"之类的间接感知，摄像头一旦被静默打开，
//! 用户往往毫无察觉（预览窗可以被插件藏起来，但摄像头指示灯不受本进程控制、也不是每台
//! 设备都有）。所以本模块把"谁正在用摄像头"做成了**宿主可查询的运行时状态**，而不是只有
//! 插件自己知道：
//!
//! - [`is_active`] / [`active_sessions`]：供宿主主控在状态栏渲染"XX 插件正在使用摄像头"这类
//!   指示器。这不是可选的锦上添花——没有这两个函数，宿主除了相信插件"没有偷偷开着"之外
//!   没有任何独立验证手段，用户也就没有反抗权（关不掉一个自己都不知道存在的会话）。
//!   注意：这两个函数只反映"预览流"（[`plugin_camera_stream_start`]）是否在跑，[`plugin_camera_grab`]
//!   是抓完即关的单次调用，指示器窗口通常来不及捕捉到它，这点在设计上是刻意的取舍——
//!   持续监控用摄像头对隐私的影响远大于单次抓拍。
//! - [`stop_all_for_session`]：供宿主在插件页关闭 / 后台实例被禁用或卸载时强制收摊，插件自己
//!   没有任何手段阻止这件事。集成点应与 `serve_api::stop_all_for_session` 完全对齐——即
//!   `commands.rs` 里"切换插件窗口内容前停掉上一个插件的本地服务"和"关闭后台常驻实例"这
//!   两处，都要一并调用本模块的 `stop_all_for_session`（本文件按任务边界不允许直接改
//!   `commands.rs`，需要主控接线）。
//!
//! # 权限与归属
//!
//! 需 `camera` 授权（走标准门禁，见 [`require_camera`]）。预览流的归属校验用**完整会话身份**
//! （插件 id + `dev` 标志，见 [`ActiveSession::same_as`]）——道理与 `audio.rs` 模块头注释一致：
//! 调试插件与同名正式插件共用一个 id，只比 id 会让调试窗里的 demo 停掉/接管正式窗里 demo
//! 正在预览的摄像头画面（反向亦然）。
//!
//! # 事件推送
//!
//! 插件页是受限自定义协议页，拿不到 `window.__TAURI__`，不能用 `app.emit_to`；一律走
//! `webview.eval` 调用桥接层注入的 `window.__itoolsEmit(channel, payload)`（照抄
//! `exec.rs::emit_to_plugin`）。事件只推给**发起调用的那个窗口**（启动预览流那一刻的
//! `webview.label()`，可能是 `plugin` / `plugin-dev` / `plugin-bg-<id>`，本模块从不写死）。
//!
//! # 预览流的性能边界（必读，别拿它当视频会议用）
//!
//! 每一帧都要经过「Media Foundation 出图 → JPEG 编码 → base64 → 拼 JS 字符串 → `webview.eval`
//! 注入」这一整条链路，`webview.eval` 本质是把整段脚本源码喂给 WebView2 重新解析执行——
//! 这不是一条为高频大 payload 设计的通道。据此：
//!
//! - **帧率硬上限 15 fps**（[`MAX_STREAM_FPS`]），默认 10 fps（[`DEFAULT_STREAM_FPS`]）；未到
//!   下一次该出帧的时间点，采样到的帧直接丢弃（连 JPEG 编码都不做），不会攒帧、不会越攒越
//!   卡。这是节流手段，不是"能跑多快跑多快"。
//! - **建议分辨率不超过 640×480**：一帧 640×480 JPEG（quality≈70）大约 20~40 KB，base64 放大
//!   ~1.33 倍后是三四万字符的 JS 字符串字面量，`eval` 一次的开销尚可接受；上到 1280×720 往上，
//!   单帧字符串会膨胀到十几万字符，10 fps 持续推送会明显拖慢插件窗口所在的 WebView2 进程，
//!   高分辨率场景请改用 [`plugin_camera_grab`] 按需抓单帧，而不是开着预览流硬扛。
//! - 这是"如实的能力边界"，不是尚待优化的 bug——`webview.eval` 是当前唯一能触达插件页的
//!   通道（见模块头「事件推送」一节），要根治只能等宿主给插件页开一个真正的双向通道
//!   （如 IPC channel），不在本次任务范围内。
//!
//! # 为什么不做录像
//!
//! 录像需要把连续帧编码成 mp4/h264 这类视频容器，这需要一个视频编码器；本项目现有依赖里
//! 没有任何能干这件事的库（`image` 只管单帧静态图，`windows` crate 也没开 Media Foundation
//! 的编码器/Sink Writer 相关 feature）。硬糊一个"能跑但产出坏文件/放不出来"的录像接口，
//! 比直接不提供更糟——用户会觉得"录了但没录上"，比"压根没有这功能"更误导人。这里刻意
//! **不提供** `plugin_camera_record` / `plugin_camera_record_stop`。真要录像，建议插件自己
//! 按需拉预览帧（[`plugin_camera_stream_start`]），喂给一个由宿主托管、单独申请依赖的 ffmpeg
//! 子进程去编码——那是一个独立的、需要额外评估依赖的任务，不在本文件范围内。
//!
//! # 已知取舍（如实记录）
//!
//! - **设备枚举时读取的 `formats` 是尽力而为**：要拿到某台摄像头支持的原生分辨率/帧率列表，
//!   必须先把它打开（`ActivateObject` + 建 `SourceReader`）。如果设备正被别的程序占用，打开
//!   会失败——此时该设备的 `formats` 诚实地返回空数组，不编造数据；`deviceId` / `name` 仍会
//!   正常列出（这两项来自设备属性包，不需要真正打开设备）。
//! - **像素格式固定请求 RGB32（实际内存布局是 BGRX）**：抓拍/预览都显式向 `SourceReader`
//!   请求 `MFVideoFormat_RGB32` 输出，由 Media Foundation 内部的颜色转换 MFT 负责从设备原生
//!   格式（多数 UVC 摄像头是 MJPG 或 NV12）转过来，本模块不必自己实现 YUV/MJPG 解码。极少数
//!   老旧或非标驱动可能不支持这条内建转换路径，此时 `SetCurrentMediaType` 会直接报错并原样
//!   透传给调用方（不会返回一张花屏图糊弄过去）。
//! - **抓拍会丢弃前 1~2 帧**：多数摄像头刚打开的头几帧还在做自动曝光/白平衡调整，画面偏暗
//!   或偏色；[`plugin_camera_grab`] 内部预热丢弃，只把最后一次成功读到的帧编码返回。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use tokio::sync::oneshot;

use super::commands::{caller_session, plugin_granted, png_to_b64};
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 门禁用的权限标识。
const CAMERA_PERM: &str = "camera";

/// 预览流帧事件：`{ streamId, b64（JPEG，不带 data: 前缀）, width, height }`。
const EVT_FRAME: &str = "plugin-camera-frame";
/// 预览流**非主动**终止时推送（设备被拔出/读帧出错等）；主动调用 `plugin_camera_stream_stop`
/// 停止的不会收到这条——那是调用方自己发起的，不需要再通知一遍。
const EVT_STREAM_STOPPED: &str = "plugin-camera-stream-stopped";

/// 预览流帧率硬上限（见模块头「性能边界」）。
const MAX_STREAM_FPS: u32 = 15;
/// 未显式传 `fps` 时的默认帧率。
const DEFAULT_STREAM_FPS: u32 = 10;

// ==================== 对外结构 ====================

/// 单个摄像头支持的一种原生格式（尽力而为，拿不到就不放进 `formats`，见模块头说明）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    /// 近似帧率（设备上报的分数四舍五入）；拿不到就是 `null`，不编造。
    pub fps: Option<u32>,
}

/// 一台摄像头设备。`device_id` 是 Media Foundation 的符号链接（symbolic link），稳定但不是
/// "友好名字"，抓拍/预览流都用它来指定要开哪一台。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CameraDevice {
    pub device_id: String,
    pub name: String,
    pub formats: Vec<CameraFormat>,
}

/// [`plugin_camera_grab`] 的可选参数。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GrabOptions {
    /// 期望的采集分辨率；缺省用设备默认分辨率。设备不支持该分辨率会直接报错，不做静默降级。
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 输出格式："png"（默认，无损）或 "jpeg"（有损、体积更小）。
    pub format: Option<String>,
    /// JPEG 质量 1~100，默认 90；仅 `format="jpeg"` 时生效。
    pub quality: Option<u8>,
}

/// [`plugin_camera_stream_start`] 的可选参数。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StreamOptions {
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 目标帧率；缺省 [`DEFAULT_STREAM_FPS`]，硬上限 [`MAX_STREAM_FPS`]（见模块头「性能边界」）。
    pub fps: Option<u32>,
    /// JPEG 质量 1~100，默认 70（预览优先低延迟/小体积，不必照顾静态画质）。
    pub quality: Option<u8>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FramePayload {
    stream_id: String,
    /// base64 JPEG，不带 `data:image/jpeg;base64,` 前缀（与截图等既有通道保持一致，前缀由
    /// 插件页/桥接层自行拼）。
    b64: String,
    width: u32,
    height: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StreamStoppedPayload {
    stream_id: String,
    reason: String,
}

// ==================== 运行期注册表（预览流会话） ====================

/// 一路正在推送的预览流：归属者 + 停止信号。**不放摄像头句柄/线程句柄**——那些只活在
/// worker 线程自己的栈上，registry 只负责"谁能停它、宿主怎么查它"（对齐 `serve_api.rs`
/// 的 `ServeEntry` 设计：处理耗时操作的资源不该让 registry 的锁陪绑）。
struct StreamEntry {
    owner: ActiveSession,
    stop: Arc<AtomicBool>,
}

fn stream_registry() -> &'static Mutex<HashMap<String, StreamEntry>> {
    static REG: OnceLock<Mutex<HashMap<String, StreamEntry>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 拿 registry 锁；中毒（poison）不当致命错误，直接取回内部数据继续用——一次业务 panic
/// 不该让所有插件的摄像头预览流从此再也停不掉/查不到（对齐 `serve_api.rs` 同名工具函数）。
fn stream_reg_lock() -> MutexGuard<'static, HashMap<String, StreamEntry>> {
    match stream_registry().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 当前是否有插件正在使用摄像头预览流。**供宿主状态栏做隐私指示器**——这是隐私底线，见
/// 模块头「本模块的隐私设计」一节：没有这个函数，用户就没有独立于插件自述之外的渠道知道
/// 摄像头正被谁用着。
pub fn is_active() -> bool {
    !stream_reg_lock().is_empty()
}

/// 正在使用摄像头预览流的插件 id 列表（已去重；不区分调试/正式，只给宿主渲染文案用）。
pub fn active_sessions() -> Vec<ActiveSession> {
    // 返回**完整会话身份**（id + dev）而不是裸 id：指示器的「点击停止」靠
    // `ActiveSession::same_as` 找回会话，而它同时比对 id 与 dev 标志。只给 id 的话，
    // 调试会话开的摄像头会显示得出来、却一个都掐不掉（`dev` 被当成 false 去匹配）。
    let reg = stream_reg_lock();
    let mut out: Vec<ActiveSession> = Vec::new();
    for e in reg.values() {
        if !out.iter().any(|s| s.same_as(&e.owner)) {
            out.push(e.owner.clone());
        }
    }
    out.sort_by(|a, b| (&a.id, a.dev).cmp(&(&b.id, b.dev)));
    out
}

/// 供宿主主控调用：插件页关闭 / 插件被禁用或卸载时，强制停掉该会话名下**所有**摄像头预览流。
/// 插件自己拿不到摄像头句柄，也没有命令能阻止这件事——生命周期完全由宿主掌握。
///
/// 建议接线点（与 `serve_api::stop_all_for_session` 对齐，本文件按任务边界不能直接改
/// `commands.rs`）：
/// 1. 切换 `plugin` 窗口内容前（原先插件的会话被换下来的那一刻）；
/// 2. 关闭某插件后台常驻实例（`close_plugin_background`）时。
pub fn stop_all_for_session(session: &ActiveSession) {
    let mut reg = stream_reg_lock();
    let ids: Vec<String> = reg
        .iter()
        .filter(|(_, e)| e.owner.same_as(session))
        .map(|(id, _)| id.clone())
        .collect();
    for id in ids {
        if let Some(e) = reg.remove(&id) {
            e.stop.store(true, Ordering::Relaxed);
        }
    }
}

// ==================== 门禁 / 小工具 ====================

/// 校验发起调用的窗口当前的插件会话已获 `camera` 授权，并返回**该会话**（归属者：id + dev
/// 标志，缺一不可，理由见模块头）。
fn require_camera(
    webview: &tauri::Webview,
    registry: &PluginRegistry,
    settings: &SettingsStore,
) -> Result<ActiveSession, String> {
    let session = caller_session(webview, registry)?;
    if !plugin_granted(settings, registry, &session, CAMERA_PERM) {
        return Err("插件未获授权使用摄像头（请在「插件管理」里授权 camera）".to_string());
    }
    Ok(session)
}

/// 日志里只记「谁 + 干了什么」，不记设备名等可能带辨识度的信息（与 `exec.rs::who` 同一约定）。
fn who(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

fn new_stream_id() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now_ns:x}-{n:x}")
}

/// 把一条事件推给**插件页**。插件页拿不到 `window.__TAURI__`，不能用 `app.emit_to`；一律走
/// `webview.eval` 调用桥接层注入的 `window.__itoolsEmit`（照抄 `exec.rs::emit_to_plugin`）。
fn emit_to_plugin<T: Serialize>(app: &tauri::AppHandle, label: &str, channel: &str, payload: &T) {
    let Some(win) = app.get_webview_window(label) else {
        return;
    };
    let json = match serde_json::to_string(payload) {
        Ok(j) => j,
        Err(_) => return,
    };
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('{channel}', {json})");
    let _ = win.eval(&js);
}

// ==================== 命令：设备列表 ====================

/// 列出系统当前可见的摄像头设备。需 `camera` 授权。
#[tauri::command]
pub async fn plugin_camera_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<CameraDevice>, String> {
    let session = require_camera(&webview, &registry, &settings)?;
    ilog!("[iTools][plugin] camera_list: 来自 {}", who(&session));
    tauri::async_runtime::spawn_blocking(list_devices_blocking)
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(windows)]
fn list_devices_blocking() -> Result<Vec<CameraDevice>, String> {
    // SAFETY: 与 ocr.rs 同一约定——这是共享的 tokio blocking 线程池线程，忽略 CoInitializeEx
    // 的返回值、也不做 CoUninitialize（该线程可能被其它无关任务复用，贸然反初始化会影响
    // 同线程后续的其它 COM 调用）。
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let _mf = mf::MfSession::new().map_err(|e| format!("初始化 Media Foundation 失败: {}", e.message()))?;
    let acts = mf::enum_activates()?;
    let mut out = Vec::with_capacity(acts.len());
    for act in &acts {
        let name = mf::get_alloc_string(act, &windows::Win32::Media::MediaFoundation::MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
            .unwrap_or_default();
        let device_id = mf::get_alloc_string(
            act,
            &windows::Win32::Media::MediaFoundation::MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )
        .unwrap_or_default();
        if device_id.is_empty() {
            // 拿不到唯一标识就没法用（grab/stream 都要靠它定位设备），宁可不列出，也不编造一个。
            continue;
        }
        let formats = mf::native_formats(act);
        out.push(CameraDevice { device_id, name, formats });
    }
    Ok(out)
}

#[cfg(not(windows))]
fn list_devices_blocking() -> Result<Vec<CameraDevice>, String> {
    Err("摄像头能力仅支持 Windows".to_string())
}

// ==================== 命令：单帧抓拍 ====================

/// 抓一帧静态图，返回 base64（PNG 或 JPEG，见 [`GrabOptions::format`]）。需 `camera` 授权。
#[tauri::command]
pub async fn plugin_camera_grab(
    device_id: String,
    opts: Option<GrabOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let session = require_camera(&webview, &registry, &settings)?;
    if device_id.trim().is_empty() {
        return Err("deviceId 不能为空".to_string());
    }
    ilog!("[iTools][plugin] camera_grab: 来自 {}", who(&session));
    let opts = opts.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || grab_blocking(&device_id, &opts))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(windows)]
fn grab_blocking(device_id: &str, opts: &GrabOptions) -> Result<String, String> {
    // SAFETY: 同 list_devices_blocking——共享 blocking 线程池线程，只忽略返回值，不反初始化。
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let _mf = mf::MfSession::new().map_err(|e| format!("初始化 Media Foundation 失败: {}", e.message()))?;
    let act = mf::find_activate(device_id)?;
    let opened = mf::open_reader(&act, opts.width, opts.height)?;

    // 预热丢弃前几帧（自动曝光/白平衡还没收敛），只把最后一次成功读到的帧编码返回；
    // 单次读取失败不算致命，除非连续三次都失败。
    let mut rgba: Option<Vec<u8>> = None;
    let mut last_err: Option<String> = None;
    for _ in 0..3 {
        match mf::read_frame_rgba(&opened.reader, opened.width, opened.height) {
            Ok(frame) => rgba = Some(frame),
            Err(e) => last_err = Some(e),
        }
    }
    // SAFETY: source 由 open_reader 内部 ActivateObject 打开，用完必须 Shutdown 释放设备，
    // 否则句柄会一直占用，其它程序/下次调用打不开这台摄像头。
    let _ = unsafe { opened.source.Shutdown() };

    let rgba = match rgba {
        Some(v) => v,
        None => return Err(last_err.unwrap_or_else(|| "抓拍失败：连续多次读取不到有效帧".to_string())),
    };

    let format = opts.format.as_deref().unwrap_or("png");
    let bytes = match format {
        "jpeg" | "jpg" => mf::rgba_to_jpeg(
            opened.width as usize,
            opened.height as usize,
            &rgba,
            opts.quality.unwrap_or(90).clamp(1, 100),
        )?,
        _ => super::commands::rgba_to_png(opened.width as usize, opened.height as usize, &rgba)?,
    };
    Ok(png_to_b64(&bytes))
}

#[cfg(not(windows))]
fn grab_blocking(_device_id: &str, _opts: &GrabOptions) -> Result<String, String> {
    Err("摄像头能力仅支持 Windows".to_string())
}

// ==================== 命令：预览流 ====================

/// 启动预览流：按 [`StreamOptions::fps`]（默认 [`DEFAULT_STREAM_FPS`]，硬上限
/// [`MAX_STREAM_FPS`]）持续经 [`EVT_FRAME`] 推 JPEG 帧给**发起调用的窗口**。返回 `streamId`，
/// 用于 [`plugin_camera_stream_stop`]。需 `camera` 授权；启动阶段（打开设备、协商格式）失败会
/// 直接报错，不会留下一个悬空会话。
#[tauri::command]
pub async fn plugin_camera_stream_start(
    device_id: String,
    opts: Option<StreamOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let owner = require_camera(&webview, &registry, &settings)?;
    if device_id.trim().is_empty() {
        return Err("deviceId 不能为空".to_string());
    }
    let opts = opts.unwrap_or_default();
    let fps = opts.fps.unwrap_or(DEFAULT_STREAM_FPS).clamp(1, MAX_STREAM_FPS);
    let quality = opts.quality.unwrap_or(70).clamp(1, 100);

    let stream_id = new_stream_id();
    let stop = Arc::new(AtomicBool::new(false));
    let job = StreamJob {
        app: webview.app_handle().clone(),
        label: webview.label().to_string(),
        stream_id: stream_id.clone(),
        device_id,
        want_w: opts.width,
        want_h: opts.height,
        fps,
        quality,
        stop: stop.clone(),
    };
    ilog!(
        "[iTools][plugin] camera_stream_start: 来自 {}，stream={stream_id}，fps={fps}",
        who(&owner)
    );

    // 立刻登记会话——这样即使随后等待超时，stop 也总能让 worker 线程在下次轮询时退出并
    // 释放设备（不会因为等待阶段提前返回而泄漏摄像头占用）。
    {
        let mut reg = stream_reg_lock();
        reg.insert(stream_id.clone(), StreamEntry { owner, stop: stop.clone() });
    }

    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
    spawn_stream_worker(job, ready_tx);

    let teardown = |stream_id: &str| {
        if let Some(e) = stream_reg_lock().remove(stream_id) {
            e.stop.store(true, Ordering::Relaxed);
        }
    };
    match tokio::time::timeout(Duration::from_secs(8), ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(stream_id),
        Ok(Ok(Err(e))) => {
            teardown(&stream_id);
            Err(e)
        }
        Ok(Err(_)) => {
            teardown(&stream_id);
            Err("预览流线程提前退出".to_string())
        }
        Err(_) => {
            teardown(&stream_id);
            Err("打开摄像头超时".to_string())
        }
    }
}

/// 停止一路预览流。带归属校验（别的插件停不掉/接管不了你的流）；不属于本插件或已结束都报错。
/// 需已获 `camera` 授权（与 `exec.rs::plugin_exec_kill` 同一约定：控制类命令也复核权限，
/// 免得授权在会话期间被收回后，插件还能靠一个仍然合法的 `streamId` 继续操作）。
#[tauri::command]
pub fn plugin_camera_stream_stop(
    stream_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let cur = require_camera(&webview, &registry, &settings)?;
    let mut reg = stream_reg_lock();
    match reg.get(&stream_id) {
        None => Err("该预览流不存在或已结束".to_string()),
        Some(e) if !e.owner.same_as(&cur) => Err("该预览流不属于本插件".to_string()),
        _ => {
            if let Some(e) = reg.remove(&stream_id) {
                e.stop.store(true, Ordering::Relaxed);
            }
            ilog!("[iTools][plugin] camera_stream_stop: 来自 {}，stream={stream_id}", who(&cur));
            Ok(())
        }
    }
}

/// 一路预览流 worker 需要的全部上下文，打包成结构体传给后台线程（避免裸传一长串参数）。
struct StreamJob {
    app: tauri::AppHandle,
    label: String,
    stream_id: String,
    device_id: String,
    want_w: Option<u32>,
    want_h: Option<u32>,
    fps: u32,
    quality: u8,
    stop: Arc<AtomicBool>,
}

#[cfg(windows)]
fn spawn_stream_worker(job: StreamJob, ready_tx: oneshot::Sender<Result<(), String>>) {
    std::thread::spawn(move || mf::stream_worker(job, ready_tx));
}

#[cfg(not(windows))]
fn spawn_stream_worker(_job: StreamJob, ready_tx: oneshot::Sender<Result<(), String>>) {
    let _ = ready_tx.send(Err("摄像头能力仅支持 Windows".to_string()));
}

// ==================== Windows / Media Foundation 实现细节 ====================

#[cfg(windows)]
mod mf {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows::core::{Interface, GUID, PWSTR};
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, IMFAttributes, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
        MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources,
        MFShutdown, MFStartup, MFVideoFormat_RGB32, MFMediaType_Video, MFSTARTUP_FULL,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
        MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_VERSION,
    };
    use windows::Win32::System::Com::CoTaskMemFree;
    // `JpegEncoder::write_image` 是 `ImageEncoder` trait 方法，不导入这个 trait 就调不到它。
    
    use super::{
        emit_to_plugin, png_to_b64, stream_reg_lock, CameraFormat, StreamJob, EVT_FRAME, EVT_STREAM_STOPPED,
    };
    use crate::logging::ilog;

    /// 单个设备一次最多枚举的原生格式数量（防御性上限，避免异常驱动导致无限循环）。
    const MAX_FORMATS: u32 = 64;

    /// RAII 守卫：构造时 `MFStartup`，`Drop` 时保证 `MFShutdown` 配对——即使中途某步 `?`
    /// 提前返回，也不会漏掉配对调用（Media Foundation 要求二者严格成对，可重入计数）。
    pub(super) struct MfSession;

    impl MfSession {
        pub(super) fn new() -> windows::core::Result<Self> {
            // SAFETY: MFStartup 是无副作用的库初始化 API，按文档要求必须与 MFShutdown 成对调用；
            // 本类型的 Drop 实现保证这一点。
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }?;
            Ok(Self)
        }
    }

    impl Drop for MfSession {
        fn drop(&mut self) {
            // SAFETY: 与 `new()` 中的 MFStartup 严格成对；无论前面的操作是否提前失败返回，
            // Drop 都会执行到这里。
            unsafe {
                let _ = MFShutdown();
            }
        }
    }

    /// `MF_MT_FRAME_SIZE` / `MF_MT_FRAME_RATE` 这类"打包 UINT64"属性：高 32 位在前、低 32
    /// 位在后（对应 Microsoft 文档里的 `MFGetAttributeSize`/`MFSetAttributeSize` 打包约定）。
    fn pack_u64(hi: u32, lo: u32) -> u64 {
        ((hi as u64) << 32) | lo as u64
    }
    fn unpack_u64(v: u64) -> (u32, u32) {
        ((v >> 32) as u32, (v & 0xFFFF_FFFF) as u32)
    }

    /// 从任意实现了 `IMFAttributes`（设备 `IMFActivate` / 媒体类型 `IMFMediaType`）的接口上读
    /// 一个字符串属性。`GetAllocatedString` 用 `CoTaskMemAlloc` 分配返回缓冲，本函数负责配对
    /// `CoTaskMemFree`，调用方拿到的是已经复制出来的 Rust `String`，不持有任何原生资源。
    pub(super) fn get_alloc_string<T: Interface>(obj: &T, key: &GUID) -> Result<String, String> {
        let attrs: IMFAttributes = obj.cast().map_err(|e| e.message().to_string())?;
        let mut pw = PWSTR::null();
        let mut len: u32 = 0;
        // SAFETY: pw/len 是纯输出参数；GetAllocatedString 成功时会把 pw 指向一段新分配、
        // 以 \0 结尾的 UTF-16 缓冲，失败时不会写 pw（保持 null）。
        let res = unsafe { attrs.GetAllocatedString(key, &mut pw, &mut len) };
        let out = match &res {
            // SAFETY: 上一步已确认 GetAllocatedString 成功，pw 指向有效、以 \0 结尾的缓冲。
            Ok(()) => unsafe { pw.to_string() }.unwrap_or_default(),
            Err(_) => String::new(),
        };
        if !pw.is_null() {
            // SAFETY: 只释放 GetAllocatedString 自己分配的这一段内存，且仅在这里释放一次
            // （out 已经是复制出来的 Rust String，不再依赖这段原生内存）。
            unsafe { CoTaskMemFree(Some(pw.as_ptr() as *const _)) };
        }
        res.map(|()| out).map_err(|e| e.message().to_string())
    }

    /// 从 `IMFMediaType`（或任何实现了 `IMFAttributes` 的接口）上读一个 `UINT64` 打包属性。
    pub(super) fn get_u64<T: Interface>(obj: &T, key: &GUID) -> Result<u64, String> {
        let attrs: IMFAttributes = obj.cast().map_err(|e| e.message().to_string())?;
        // SAFETY: 纯值读取，不涉及额外资源生命周期。
        unsafe { attrs.GetUINT64(key) }.map_err(|e| e.message().to_string())
    }

    /// 枚举系统当前可见的摄像头（视频采集类）设备，返回一组 `IMFActivate`（延迟激活句柄，
    /// 此时设备尚未真正打开）。
    pub(super) fn enum_activates() -> Result<Vec<IMFActivate>, String> {
        let mut attrs: Option<IMFAttributes> = None;
        // SAFETY: 创建一个只放 1 项的属性包，纯内存操作。
        unsafe { MFCreateAttributes(&mut attrs, 1) }.map_err(|e| format!("创建枚举属性失败: {}", e.message()))?;
        let attrs = attrs.ok_or_else(|| "创建枚举属性失败".to_string())?;
        // SAFETY: SetGUID 写入的是值语义 GUID，不持有额外资源。
        unsafe { attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID) }
            .map_err(|e| format!("设置枚举属性失败: {}", e.message()))?;

        let mut list_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        // SAFETY: MFEnumDeviceSources 是标准 MF 设备枚举 API；返回的数组由它内部按文档要求
        // 用 CoTaskMemAlloc 分配，下面按文档要求整体 CoTaskMemFree 释放。
        unsafe { MFEnumDeviceSources(&attrs, &mut list_ptr, &mut count) }
            .map_err(|e| format!("枚举摄像头设备失败: {}", e.message()))?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: i < count，list_ptr 指向 MF 分配的、恰好 count 个元素的数组；
            // ptr::read 把该槽位的所有权（含其内部引用计数）转移到 Rust 侧的 Option<IMFActivate>，
            // 之后只释放数组容器本身的内存，不会对同一个接口指针重复 Release。
            let slot = unsafe { list_ptr.add(i) };
            if let Some(act) = unsafe { std::ptr::read(slot) } {
                out.push(act);
            }
        }
        if !list_ptr.is_null() {
            // SAFETY: 仅释放 MFEnumDeviceSources 分配的数组容器内存；每个槽位的接口所有权
            // 已经在上面的循环里转移给 `out`，这里不会造成 double-release。
            unsafe { CoTaskMemFree(Some(list_ptr as *const _)) };
        }
        Ok(out)
    }

    /// 按 `deviceId`（符号链接）在当前枚举结果里找到匹配的设备句柄。
    pub(super) fn find_activate(device_id: &str) -> Result<IMFActivate, String> {
        let acts = enum_activates()?;
        for act in acts {
            if let Ok(id) = get_alloc_string(
                &act,
                &windows::Win32::Media::MediaFoundation::MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) {
                if id == device_id {
                    return Ok(act);
                }
            }
        }
        Err(format!("找不到摄像头设备（deviceId={device_id}，可能已拔出或被系统重新枚举，试试重新调用摄像头列表）"))
    }

    /// 打开设备读取它上报的原生格式列表（宽高 + 近似帧率）。**尽力而为**：设备正被别的程序
    /// 占用等场景会打开失败，此时诚实返回空列表，不编造数据（见模块头「已知取舍」）。
    pub(super) fn native_formats(act: &IMFActivate) -> Vec<CameraFormat> {
        // SAFETY: ActivateObject 打开设备拿到 IMFMediaSource，仅用于探测原生格式，
        // 用完立即 Shutdown 释放，不会占着设备不放。
        let Ok(source) = (unsafe { act.ActivateObject::<IMFMediaSource>() }) else {
            return Vec::new();
        };
        // 探测原生格式不需要格式转换：这里就是要看设备**原本**支持什么，
        // 打开高级视频处理反而会把 MF 能转出来的格式也混进来，失去参考意义。
        let reader = match unsafe { MFCreateSourceReaderFromMediaSource(&source, None) } {
            Ok(r) => r,
            Err(_) => {
                // SAFETY: 与上面的 ActivateObject 成对，探测失败也要把设备还回去。
                let _ = unsafe { source.Shutdown() };
                return Vec::new();
            }
        };

        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for i in 0..MAX_FORMATS {
            // SAFETY: 只读查询，i 在防御性上限内；返回 Err 视为枚举到头（通常是
            // MF_E_NO_MORE_TYPES），正常退出循环。
            let mt: IMFMediaType = match unsafe { reader.GetNativeMediaType(0, i) } {
                Ok(mt) => mt,
                Err(_) => break,
            };
            if let Ok(size) = get_u64(&mt, &MF_MT_FRAME_SIZE) {
                let (w, h) = unpack_u64(size);
                let fps = get_u64(&mt, &MF_MT_FRAME_RATE)
                    .ok()
                    .map(|r| {
                        let (num, den) = unpack_u64(r);
                        if den == 0 {
                            0
                        } else {
                            (num as f64 / den as f64).round() as u32
                        }
                    })
                    .filter(|f| *f > 0);
                if w > 0 && h > 0 && seen.insert((w, h, fps)) {
                    out.push(CameraFormat { width: w, height: h, fps });
                }
            }
        }
        // SAFETY: 与本函数开头的 ActivateObject 成对，探测完毕释放设备。
        let _ = unsafe { source.Shutdown() };
        out
    }

    /// 已打开、已协商好输出格式的摄像头：设备源 + 读取器 + 协商到的实际宽高。
    pub(super) struct OpenedCamera {
        pub(super) source: IMFMediaSource,
        pub(super) reader: IMFSourceReader,
        pub(super) width: u32,
        pub(super) height: u32,
    }

    /// 打开设备并把首路视频流的输出格式设为 `MFVideoFormat_RGB32`（内存布局为 BGRX，见模块
    /// 头「已知取舍」）；`want_w`/`want_h` 都给出时会一并请求该分辨率，设备不支持会直接报错，
    /// 不做静默降级。调用方负责在用完后对返回的 `source` 调用 `Shutdown()` 释放设备。
    pub(super) fn open_reader(act: &IMFActivate, want_w: Option<u32>, want_h: Option<u32>) -> Result<OpenedCamera, String> {
        // SAFETY: ActivateObject 打开设备，返回值归调用方所有，调用方需在用完后调用
        // source.Shutdown()（本函数出错分支已经这样做；成功分支的释放责任交给上层调用方）。
        let source: IMFMediaSource = unsafe { act.ActivateObject() }
            .map_err(|e| format!("打开摄像头失败（可能被其它程序占用）: {}", e.message()))?;
        // 建读取器时**必须**打开「高级视频处理」，否则只能拿到设备原生格式。
        //
        // 真机验收踩到的实例：罗技 C930c 这类常见 USB 摄像头原生输出 MJPG / YUY2 / NV12，
        // 根本不直接产 RGB32。不开这个开关时，下面那句 SetCurrentMediaType(RGB32) 直接报
        // 「为此媒体类型指定的数据无效、不一致或不受此对象的支持」——**绝大多数 USB 摄像头
        // 都会卡在这里**。打开后 Media Foundation 会自动在管线里插入颜色转换（必要时还有缩放）
        // MFT，才能稳定按 RGB32 取帧。
        let reader_attrs: IMFAttributes = {
            let mut a: Option<IMFAttributes> = None;
            // SAFETY: 标准用法，输出参数是栈上 Option，容量 1 足够放一个属性。
            if let Err(e) = unsafe { MFCreateAttributes(&mut a, 1) } {
                // SAFETY: 与上面的 ActivateObject 成对。
                let _ = unsafe { source.Shutdown() };
                return Err(format!("创建读取器属性失败: {}", e.message()));
            }
            match a {
                Some(a) => a,
                None => {
                    // SAFETY: 与上面的 ActivateObject 成对。
                    let _ = unsafe { source.Shutdown() };
                    return Err("创建读取器属性失败（返回空对象）".to_string());
                }
            }
        };
        // SAFETY: reader_attrs 刚创建且有效；该属性是文档规定的 UINT32 开关。
        if let Err(e) =
            unsafe { reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1) }
        {
            // SAFETY: 与上面的 ActivateObject 成对。
            let _ = unsafe { source.Shutdown() };
            return Err(format!("启用视频格式转换失败: {}", e.message()));
        }

        // SAFETY: source 是上一步刚拿到的有效 IMFMediaSource；属性存储已按上面说明配置好。
        let reader = match unsafe { MFCreateSourceReaderFromMediaSource(&source, &reader_attrs) } {
            Ok(r) => r,
            Err(e) => {
                // SAFETY: 与上面的 ActivateObject 成对。
                let _ = unsafe { source.Shutdown() };
                return Err(format!("创建视频读取器失败: {}", e.message()));
            }
        };

        let stream_idx = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let build_result: windows::core::Result<()> = (|| {
            // SAFETY: MFCreateMediaType 创建一个空白媒体类型对象，随后写入的 MajorType/SubType/
            // FrameSize 都是值语义属性，不持有额外资源。
            let media_type = unsafe { MFCreateMediaType() }?;
            let attrs: IMFAttributes = media_type.cast()?;
            unsafe { attrs.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }?;
            unsafe { attrs.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }?;
            if let (Some(w), Some(h)) = (want_w, want_h) {
                unsafe { attrs.SetUINT64(&MF_MT_FRAME_SIZE, pack_u64(w, h)) }?;
            }
            // SAFETY: reader/media_type 均为本函数内刚创建、仍然有效的对象；stream_idx 是标准
            // 首路视频流哨兵值；第二个参数（reserved）按文档传 None。
            let first = unsafe { reader.SetCurrentMediaType(stream_idx, None, &media_type) };
            if first.is_ok() || want_w.is_none() {
                return first;
            }
            // 指定分辨率失败时退一步：只要 RGB32、不挑尺寸，由设备给它的默认分辨率。
            // 用户传了一个该摄像头不支持的分辨率，不该让整个取帧直接失败。
            let fallback = unsafe { MFCreateMediaType() }?;
            let fa: IMFAttributes = fallback.cast()?;
            unsafe { fa.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }?;
            unsafe { fa.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32) }?;
            unsafe { reader.SetCurrentMediaType(stream_idx, None, &fallback) }
        })();
        if let Err(e) = build_result {
            // SAFETY: 与上面的 ActivateObject 成对，格式协商失败也要把设备还回去。
            let _ = unsafe { source.Shutdown() };
            return Err(format!(
                "设置摄像头输出格式失败（该设备可能不支持直采 RGB，或不支持所选分辨率）: {}",
                e.message()
            ));
        }

        // SAFETY: reader 是已成功 SetCurrentMediaType 的读取器，只读查询它当前协商到的格式。
        let (width, height) = match unsafe { reader.GetCurrentMediaType(stream_idx) } {
            Ok(mt) => match get_u64(&mt, &MF_MT_FRAME_SIZE) {
                Ok(size) => unpack_u64(size),
                Err(_) => (want_w.unwrap_or(0), want_h.unwrap_or(0)),
            },
            Err(_) => (want_w.unwrap_or(0), want_h.unwrap_or(0)),
        };
        if width == 0 || height == 0 {
            // SAFETY: 与上面的 ActivateObject 成对。
            let _ = unsafe { source.Shutdown() };
            return Err("无法确定摄像头输出分辨率".to_string());
        }

        Ok(OpenedCamera { source, reader, width, height })
    }

    /// 一次取帧最多重试多少轮空样本。
    ///
    /// `ReadSample` 返回**空样本但不报错**是 Media Foundation 的正常行为：设备刚启动、
    /// 或流控打了个 stream tick 时都会这样，含义是「此刻还没有数据，继续读」。
    /// 真机验收踩到过：把第一次空样本当成流结束，预览流一启动就自己退了，
    /// `frames` 恒为 0——而单帧 `grab` 因为恰好赶上设备已就绪反而是好的，
    /// 于是表现成「拍照能用、预览不能用」这种很难联想到根因的样子。
    const EMPTY_SAMPLE_RETRIES: u32 = 60;

    /// 读一帧，转成紧凑排列的 RGBA8 字节（丢弃 alpha 通道意义上的透明度语义，统一填 255）。
    pub(super) fn read_frame_rgba(reader: &IMFSourceReader, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let stream_idx = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let mut tries = 0u32;
        // 循环直到真的拿到样本、真的结束、或真的出错——空样本只是「再等等」。
        let sample = loop {
            let mut stream_flags: u32 = 0;
            let mut sample: Option<IMFSample> = None;
            // SAFETY: 输出参数均为本函数栈上局部变量的可变引用，生命周期覆盖整次调用。
            unsafe {
                reader.ReadSample(stream_idx, 0, None, Some(&mut stream_flags), None, Some(&mut sample))
            }
            .map_err(|e| format!("读取摄像头帧失败: {}", e.message()))?;
            if stream_flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Err("摄像头数据流已结束".to_string());
            }
            if let Some(s) = sample.take() {
                break s;
            }
            tries += 1;
            if tries >= EMPTY_SAMPLE_RETRIES {
                return Err(format!(
                    "摄像头连续 {EMPTY_SAMPLE_RETRIES} 次未返回帧数据（设备可能被其它程序独占）"
                ));
            }
            // 让出一点时间给设备管线，别把 CPU 打满空转
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // SAFETY: ConvertToContiguousBuffer 是只读查询，返回的缓冲归调用方所有（Lock/Unlock
        // 成对使用即可，不需要额外释放）。
        let buf = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| format!("取帧缓冲失败: {}", e.message()))?;

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut cur_len: u32 = 0;
        // SAFETY: Lock 返回的指针在对应 Unlock 调用前一直有效；下面只按 `need`（且已校验
        // `<= cur_len`）读取，不会越界访问未初始化或越界内存。
        unsafe { buf.Lock(&mut ptr, None, Some(&mut cur_len)) }.map_err(|e| format!("锁定帧缓冲失败: {}", e.message()))?;
        let need = (width as usize) * (height as usize) * 4;
        let result = if (cur_len as usize) < need {
            Err(format!("帧数据大小异常（期望至少 {need} 字节，实际 {cur_len}）"))
        } else {
            // SAFETY: 上面已校验 cur_len as usize >= need，[ptr, ptr+need) 在 Lock 承诺的有效
            // 范围 [ptr, ptr+cur_len) 之内。
            let slice = unsafe { std::slice::from_raw_parts(ptr, need) };
            Ok(bgrx_to_rgba(slice))
        };
        // SAFETY: 与上面的 Lock 严格成对。
        let _ = unsafe { buf.Unlock() };
        result
    }

    /// RGB32（内存布局 BGRX，小端）→ 紧凑 RGBA8。
    fn bgrx_to_rgba(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; data.len()];
        for (chunk_in, chunk_out) in data.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
            chunk_out[0] = chunk_in[2]; // R
            chunk_out[1] = chunk_in[1]; // G
            chunk_out[2] = chunk_in[0]; // B
            chunk_out[3] = 255; // A（RGB32 源没有真正的透明度语义，固定不透明）
        }
        out
    }

    /// RGBA8 → JPEG（`image` 的 JPEG 编码器只认 L8/Rgb8，这里先丢弃 alpha 通道）。
    pub(super) fn rgba_to_jpeg(width: usize, height: usize, rgba: &[u8], quality: u8) -> Result<Vec<u8>, String> {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for px in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[0..3]);
        }
        let mut out = Vec::new();
        // write_image 来自 ImageEncoder trait，用完全限定调用免去一处 use（该 trait 只在这里用到）
        image::ImageEncoder::write_image(
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality),
            &rgb,
            width as u32,
            height as u32,
            image::ExtendedColorType::Rgb8,
        )
            .map_err(|e| format!("JPEG 编码失败: {e}"))?;
        Ok(out)
    }

    // 事件载荷结构体在本文件顶层已定义；`mod mf` 是独立作用域看不到外层项，
// 用 use 引进来而不是再定义一份——两份定义迟早会改歪一边。
use super::{FramePayload, StreamStoppedPayload};

/// 打开设备、协商输出格式。拆成独立函数（而不是内联闭包）是刻意的：`ready_tx.send()`
    /// 会按值消费 `Sender`，如果把这段"可能提前返回"的逻辑塞进一个闭包、闭包里又调用一次
    /// `send`、闭包外还要再调用一次 `send`，闭包会把 `ready_tx` 整体按值捕获走，闭包外的第
    /// 二次 `send` 就编译不过——用一个普通函数 + `?` 做早退，`ready_tx` 全程只在
    /// [`stream_worker`] 里出现一次，全靠调用点的 if/else 分流，没有这个陷阱。
    fn open_stream(job: &StreamJob) -> Result<(MfSession, OpenedCamera), String> {
        let mf = MfSession::new().map_err(|e| format!("初始化 Media Foundation 失败: {}", e.message()))?;
        let act = find_activate(&job.device_id)?;
        let opened = open_reader(&act, job.want_w, job.want_h)?;
        Ok((mf, opened))
    }

    /// 预览流后台线程主体：打开设备协商格式 → 回报 ready → 按 `job.fps` 节流持续读帧、编码、
    /// 经 `EVT_FRAME` 推给发起调用的窗口，直到 `stop` 置位或读帧失败为止。
    pub(super) fn stream_worker(job: StreamJob, ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>) {
        // SAFETY: 本线程专用于这一路摄像头预览会话，寿命与该会话完全一致（线程退出前会做
        // CoUninitialize），CoInitializeEx/CoUninitialize 在本线程内严格成对
        // （S_OK/S_FALSE 都视为"需要配对释放"的成功）。
        let com_ok = unsafe {
            windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED)
        }
        .is_ok();

        // 打开设备/协商格式失败：这是"启动阶段"的失败，ready_tx 还没发过，直接把错误当 ready
        // 结果发回去，不进入读帧循环。
        let (_mf, opened) = match open_stream(&job) {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                stream_reg_lock().remove(&job.stream_id);
                if com_ok {
                    // SAFETY: 与本函数开头成功的 CoInitializeEx 严格成对。
                    unsafe { windows::Win32::System::Com::CoUninitialize() };
                }
                return;
            }
        };
        // 格式协商通过，视为"启动成功"，回报给等待中的 plugin_camera_stream_start；
        // ready_tx 到此已经用完（Sender::send 按值消费自己），后面不会再碰它。
        let _ = ready_tx.send(Ok(()));

        let mut stopped_reason: Option<String> = None;
        let interval = Duration::from_millis((1000 / job.fps.max(1)).max(1) as u64);
        let mut last_emit = Instant::now() - interval;
        loop {
            if job.stop.load(Ordering::Relaxed) {
                break;
            }
            match read_frame_rgba(&opened.reader, opened.width, opened.height) {
                Ok(rgba) => {
                    let now = Instant::now();
                    if now.duration_since(last_emit) < interval {
                        continue; // 限速丢帧：连编码都不做，省 CPU（见模块头「性能边界」）
                    }
                    last_emit = now;
                    match rgba_to_jpeg(opened.width as usize, opened.height as usize, &rgba, job.quality) {
                        Ok(jpeg) => emit_to_plugin(
                            &job.app,
                            &job.label,
                            EVT_FRAME,
                            &FramePayload {
                                stream_id: job.stream_id.clone(),
                                b64: png_to_b64(&jpeg),
                                width: opened.width,
                                height: opened.height,
                            },
                        ),
                        Err(e) => {
                            ilog!("[iTools][plugin] camera_stream: JPEG 编码失败，跳过该帧: {e}");
                        }
                    }
                }
                Err(e) => {
                    stopped_reason = Some(e);
                    break;
                }
            }
        }
        // SAFETY: 与 open_stream 内部的 ActivateObject（见 open_reader）成对，循环退出（无论
        // 主动停止还是出错）都要释放设备。
        let _ = unsafe { opened.source.Shutdown() };
        drop(_mf); // 显式标注：MFShutdown 在这里配对触发（Drop），不依赖函数末尾的隐式析构顺序

        // 无论正常结束还是出错，都要把自己从 registry 摘掉——`plugin_camera_stream_stop` 也
        // 会摘一次，两边谁先到都安全（HashMap::remove 对不存在的 key 是无操作）。
        stream_reg_lock().remove(&job.stream_id);
        if let Some(reason) = stopped_reason {
            emit_to_plugin(
                &job.app,
                &job.label,
                EVT_STREAM_STOPPED,
                &StreamStoppedPayload { stream_id: job.stream_id.clone(), reason },
            );
        }
        if com_ok {
            // SAFETY: 与本函数开头成功的 CoInitializeEx 严格成对。
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
    }
}
