//! 正式版音视频录制：mp4 录屏 + 系统内录（录电脑正在播放的声音）。
//!
//! 与 `record.rs`（GIF 录屏）/ `audio.rs`（麦克风录音）的关系：那两个是宿主自行编码的
//! **轻量版**（GIF 无声、约 5fps、≤150 帧；WAV 只录麦克风）。本文件是"正式版"——真·mp4、
//! 真·帧率、可选混入系统声音/麦克风，供需要保存可分享视频的插件用（教程录制、bug 复现录屏、
//! 会议/直播片段留存……）。二者并存：GIF 版留给"快速录一小段发群里"的轻场景，本文件用于
//! "要一份能长期保存、能拖进剪辑软件的正式录像"。
//!
//! # 为什么 mp4 编码必须外包给 ffmpeg，而不是宿主自己写
//!
//! H.264/mp4 涉及帧内/帧间预测、运动估计、熵编码、码率控制、mp4 容器封装（moov/moof 原子、
//! 音视频轨道交织与时间戳对齐）——这是数十万行、经过十几年打磨的工程量，业界没有人会在一个
//! 效率工具的插件宿主里重新发明它。ffmpeg 是唯一在正确性、性能、编解码器覆盖度上经得起用的
//! 选择，而且项目已经在 `runtime_api.rs` 里把它做成"宿主托管的运行时"（下载 + SHA-256 校验 +
//! 受控调用），本模块只是**消费**那个已装好的 ffmpeg 可执行文件——具体做法是把 xcap 抓到的每帧
//! RGBA 像素通过管道喂给 `ffmpeg -f rawvideo -i -` 的标准输入，音频（若有）单独攒成 WAV，
//! 录制结束后再用 ffmpeg 做一次快速的"视频 copy + 音频 aac 混流"。
//!
//! 注意：本模块**不走** `plugin_runtime_exec`/`plugin_runtime_exec_stream` 那条命令通道——
//! 那条通道是为"插件自己拼 ffmpeg 参数、宿主做参数体检"设计的（`validate_args` 只挡最直白的
//! 滥用形态）；本模块里喂给 ffmpeg 的每一个参数都是宿主代码自己拼出来的（宽高/帧率/文件路径
//! 全部来自受控计算，不是插件传来的原始字符串直接拼命令行），因此不复用、也不需要那层参数
//! 校验与执行期会话表，只在这里直接 `Command::new(ffmpeg_exe).args([...])`。
//!
//! # ffmpeg 未安装时的行为：明确报错，绝不悄悄产出坏文件
//!
//! [`plugin_record_video_start`] 一开始就检查 ffmpeg 是否已由 `runtime_api` 装好；没装**直接
//! 拒绝启动**，错误信息里明确写"请先调用 `itools.runtime.ensure('ffmpeg')`"，不会先开始抓帧
//! 再在 stop 时才发现编码不了。
//!
//! # 隐私：屏幕与系统声音都高度敏感
//!
//! [`is_active`] / [`active_sessions`] 供主控做状态栏/托盘"正在被录制"指示器；
//! [`stop_all_for_session`] 供插件被禁用/卸载/权限被收回时，主控立即强制掐断，不等插件自己
//! 调 stop。这张登记表**只覆盖本模块**（系统内录 + mp4 录屏）两类会话，不包含 `record.rs` 的
//! GIF 录屏与 `audio.rs` 的麦克风录音——那两个模块是私有函数边界，本文件按边界约束不能去改
//! 它们；如需统一到同一张隐私登记表，需要主控在那两个模块也补上等价的登记/摘除调用。
//!
//! # 已知局限（如实写明，不夸大能力）
//!
//! - **音视频弱同步**：喂给 ffmpeg 的视频是"尽量按目标帧率取帧、拿不到新帧就复用上一帧"节奏
//!   写入的 rawvideo 流，`-framerate` 是名义帧率而非逐帧真实时间戳；音频轨则是独立线程全程
//!   实时采样、录制结束后再与视频用 `-shortest` 对齐做一次性混流。这意味着：系统卡顿/丢帧、
//!   或音频设备的实际采样率与标称值有细微漂移时，长录制（几十分钟以上）可能出现音画轻微
//!   不同步，这是当前实现的真实边界，不是"极端情况才会碰到"的稀有 bug。
//! - **窗口（`hwnd`）录制没有事件驱动的高效路径**：xcap 0.9 只给 `Monitor` 提供了
//!   `video_recorder()`（Windows.Graphics.Capture，帧到达即回调，效率与画质都最好）；`Window`
//!   只有一次性的 `capture_image()`，因此窗口录制退化为"定时轮询截图"（同 `record.rs` 的 GIF
//!   录屏思路），CPU 占用更高，且目标窗口被遮挡/最小化时会截屏失败——本实现选择的处理方式是
//!   "截一次失败就体面结束录制"，不是无限重试卡死。
//! - **混合系统声音+麦克风时的混音**：两路各自录成独立 WAV，混流阶段交给 `ffmpeg` 的
//!   `amix` 滤镜做混音与必要的重采样，不是本模块手写的数字信号处理——这是"该交给 ffmpeg 的
//!   都交给它"这条设计原则的延续。
//! - **音频全程缓存在内存**：与 `audio.rs` 同样的实现方式——采样先攒进 `Vec<i16>`，录制结束
//!   才编码落盘，没有做分段流式写盘。因此嵌入视频录制的音频线程默认上限设为 1 小时
//!   （`AUDIO_MAX_SECS`），超长录制会有明显内存占用，这是已知取舍，不是遗漏。
//! - **`RecordArea` 的坐标系**：`x`/`y`/`w`/`h` 是相对**所选显示器左上角**的局部像素坐标，
//!   不是虚拟桌面的全局坐标——与 `displayId` 搭配使用。
//!
//! # 需要主控配合的一处改动
//!
//! 本文件需要拿到 ffmpeg 的本机可执行文件路径，但 `runtime_api.rs::find_spec` /
//! `resolve_installed_exe` 都是模块私有函数，按边界约束本文件不能去改那个文件。**需要主控在
//! `runtime_api.rs` 里补一个 `pub(crate)` 的小函数**（实现只需转发到已有的两个私有函数）：
//!
//! ```ignore
//! pub(crate) fn resolved_exe_path(name: &str) -> Option<PathBuf> {
//!     let spec = find_spec(name).ok()?;
//!     resolve_installed_exe(spec)
//! }
//! ```
//!
//! 本文件通过 `super::runtime_api::resolved_exe_path("ffmpeg")` 调用它。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};
use tokio::sync::oneshot;

use super::capture::require_capture;
use super::commands::{caller_session, plugin_granted, png_to_b64};
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 嵌入视频录制的音频采集线程存活上限（秒）；见模块头"音频全程缓存在内存"一节。
const AUDIO_MAX_SECS: u64 = 3600;
/// 独立系统内录（[`plugin_audio_loopback_start`]）的存活上限（秒），与 `audio.rs` 的麦克风
/// 录音上限保持一致的量级。
const STANDALONE_LOOPBACK_MAX_SECS: u64 = 600;
/// mp4 录制允许的帧率范围；过高的帧率对实时截屏管线没有意义，只会加重系统负担。
const FPS_RANGE: (u32, u32) = (1, 30);
/// 等待音频设备就绪（cpal 建流+播放成功）的超时——与 `audio.rs::plugin_start_audio_record`
/// 的 5 秒预算一致。
const AUDIO_READY_TIMEOUT: Duration = Duration::from_secs(5);

// ==================== 通用小工具 ====================

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 日志脱敏：同 `audio.rs`/`runtime_api.rs::who`，只记"谁"，调试会话加前缀区分。
fn who(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

fn new_id(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{now_ns:x}-{n:x}")
}

/// 校验**发起调用的窗口**当前的插件会话已获 audio-capture 授权，并返回**该会话**。
///
/// `audio.rs::require_audio` 的等价重实现——那是模块私有函数，跨模块拿不到；同
/// `runtime_api.rs::emit_to_plugin` 的先例，两处各自维护一份很短的等价实现比强行打通私有
/// 边界更简单。
fn require_audio_capture(
    webview: &tauri::Webview,
    registry: &PluginRegistry,
    settings: &SettingsStore,
) -> Result<ActiveSession, String> {
    let session = caller_session(webview, registry)?;
    if !plugin_granted(settings, registry, &session, "audio-capture") {
        return Err("插件未获授权录音（请在「插件管理」里授权 audio-capture）".to_string());
    }
    Ok(session)
}

/// 把一条事件推给**发起调用的插件窗口**。插件页拿不到 `window.__TAURI__`，必须走
/// `webview.eval` → `window.__itoolsEmit`，照抄 `exec.rs::emit_to_plugin`（模块私有，跨模块
/// 拿不到，这里维护一份等价实现，同 `runtime_api.rs` 的先例）。后台常驻插件的窗口 label 是
/// `plugin-bg-<插件id>` 而非 `plugin`——本函数从不写死 label，一律取自调用方传入的
/// `webview.label()`。
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

#[cfg(windows)]
fn no_window_command(exe: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(exe);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn no_window_command(exe: &Path) -> Command {
    Command::new(exe)
}

fn recordings_dir() -> PathBuf {
    crate::paths::data_root().join("plugin-recordings")
}

fn timestamped_filename(record_id: &str) -> String {
    let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
    format!("record_{ts}_{record_id}.mp4")
}

// ==================== 隐私登记表（is_active / active_sessions / stop_all_for_session） ====================

/// 一条被登记的"正在使用屏幕/系统声音"的会话。
struct TrackedSession {
    owner: ActiveSession,
    /// 给用户看的一句话描述。
    label: &'static str,
    stop: Arc<AtomicBool>,
    /// mp4 录制持有 ffmpeg 子进程句柄，强制停止时需要直接 kill——只置 `stop` 标志不足以让
    /// 阻塞在管道写入里的喂帧线程立刻退出。纯音频（内录）会话没有子进程，为 `None`。
    child: Option<Arc<Mutex<Child>>>,
}

fn tracked() -> &'static Mutex<HashMap<String, TrackedSession>> {
    static REG: OnceLock<Mutex<HashMap<String, TrackedSession>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 是否有任何插件正在用本模块录屏和/或录系统声音——供主控在状态栏/托盘画一个
/// "正在被录制"的指示器（例如常驻一个红点图标）。
pub fn is_active() -> bool {
    !lock(tracked()).is_empty()
}

/// 当前所有活跃会话的**完整身份**（id + dev），已按会话去重。
///
/// ⚠ 这里曾经返回的是 `"<插件 id> 正在<label>"` 这种**人类可读描述**，而 `indicator.rs`
/// 把它整句当成 plugin_id 用了。后果有两层：托盘文案变成「apitest 正在录屏 正在使用录屏 / 录音」；
/// 更严重的是「点击停止」拿这个假 id 去 `same_as` 匹配**永远匹配不上**，于是录屏/内录
/// 根本停不掉——一个看着能用、点了不生效的控件。所以这里只给身份，文案由调用方自己拼。
pub fn active_sessions() -> Vec<(ActiveSession, &'static str)> {
    let mut out: Vec<(ActiveSession, &'static str)> = Vec::new();
    for t in lock(tracked()).values() {
        // 同一会话可能同时在内录和录屏，那是两条各自独立的使用记录，不该合并成一条：
        // 用户要看见的是「它现在到底占着哪些设备」。所以按 (会话, 用途) 去重而不是只按会话。
        if !out.iter().any(|(s, l)| s.same_as(&t.owner) && *l == t.label) {
            out.push((t.owner.clone(), t.label));
        }
    }
    out
}

/// 强制停止某个会话（按插件 id + dev 标志）名下本模块管理的所有录制/内录——供插件被禁用/
/// 卸载/`screen-capture`/`audio-capture` 授权被收回时，主控立即调用掐断，不等插件自己调
/// stop。只置停止标志 + kill 已知子进程，不在这里 join 工作线程：工作线程发现 stop 标志或
/// 管道断开后会在很短时间内自行退出并回收自己持有的资源，调用方不需要、也不应该在这个
/// （大概率是 UI 相关的）调用点上等待它们收尾。
pub fn stop_all_for_session(session: &ActiveSession) {
    let mut g = lock(tracked());
    g.retain(|id, t| {
        if !t.owner.same_as(session) {
            return true;
        }
        t.stop.store(true, Ordering::Relaxed);
        if let Some(child) = &t.child {
            let _ = lock(child).kill();
        }
        ilog!(
            "[iTools][plugin] record_av 强制停止: 会话={}, 记录={id}",
            who(session)
        );
        false
    });
}

// ==================== 音频采集（复用/对齐 audio.rs 的结构，见模块头说明） ====================

struct Recorded {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

/// i16 PCM 采样 → WAV 字节（RIFF/PCM 16bit）。`audio.rs::encode_wav` 的等价重实现（私有函数，
/// 跨模块拿不到）。
fn encode_wav(samples: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits = 16u16;
    let byte_rate = sample_rate * channels as u32 * (bits / 8) as u32;
    let block_align = channels * (bits / 8);
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// 系统内录（WASAPI loopback）线程主体：把**输出设备**当输入设备打开——cpal 的 WASAPI 后端
/// 探测到这是一个 render（输出）方向的设备时会自动叠加 `AUDCLNT_STREAMFLAGS_LOOPBACK`
/// （见 cpal 源码 `host/wasapi/mod.rs` 头部注释 "If you use a WASAPI output device as an
/// input device it will transparently enable loopback mode"），因此这里只是把 `audio.rs`
/// 里的麦克风采集流程整体替换成"默认输出设备"，其余结构（建流→播放→采样收集→停止）完全对齐。
fn loopback_worker(
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    max_secs: u64,
) -> Result<Recorded, String> {
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(d) => d,
        None => {
            let _ = ready_tx.send(Err("找不到系统扬声器/输出设备".to_string()));
            return Err("找不到系统扬声器/输出设备".to_string());
        }
    };
    let default_cfg = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("读取输出设备配置失败: {e}")));
            return Err(format!("读取输出设备配置失败: {e}"));
        }
    };
    let sample_rate = default_cfg.sample_rate().0;
    let channels = default_cfg.channels();
    let sample_format = default_cfg.sample_format();
    let config: cpal::StreamConfig = default_cfg.into();

    let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
    let s_cb = samples.clone();
    let err_fn = |e| eprintln!("[iTools] 系统内录流错误: {e}");

    // 以输出设备身份调用 build_input_stream：cpal WASAPI 后端据此自动启用 loopback 模式。
    let stream_res = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &_| {
                let mut b = s_cb.lock().unwrap();
                for &s in data {
                    b.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &_| {
                s_cb.lock().unwrap().extend_from_slice(data);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _: &_| {
                let mut b = s_cb.lock().unwrap();
                for &s in data {
                    b.push((s as i32 - 32768) as i16);
                }
            },
            err_fn,
            None,
        ),
        other => {
            let _ = ready_tx.send(Err(format!("不支持的采样格式: {other:?}")));
            return Err(format!("不支持的采样格式: {other:?}"));
        }
    };
    let stream = match stream_res {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("建立系统内录流失败: {e}")));
            return Err(format!("建立系统内录流失败: {e}"));
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("启动系统内录失败: {e}")));
        return Err(format!("启动系统内录失败: {e}"));
    }
    let _ = ready_tx.send(Ok(()));

    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if start.elapsed().as_secs() > max_secs {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
    let data = samples.lock().unwrap().clone();
    Ok(Recorded {
        samples: data,
        sample_rate,
        channels,
    })
}

/// 麦克风采集线程主体，`audio.rs::record_worker` 的等价重实现（默认输入设备）——供
/// mp4 录制的 `includeMic` 选项复用；本模块内 [`loopback_worker`] 走同样的结构，两处一起看
/// 更容易确认没有语义偏差。
fn mic_worker(
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Result<(), String>>,
    max_secs: u64,
) -> Result<Recorded, String> {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            let _ = ready_tx.send(Err("找不到麦克风输入设备".to_string()));
            return Err("找不到麦克风输入设备".to_string());
        }
    };
    let default_cfg = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("读取输入配置失败: {e}")));
            return Err(format!("读取输入配置失败: {e}"));
        }
    };
    let sample_rate = default_cfg.sample_rate().0;
    let channels = default_cfg.channels();
    let sample_format = default_cfg.sample_format();
    let config: cpal::StreamConfig = default_cfg.into();

    let samples = Arc::new(Mutex::new(Vec::<i16>::new()));
    let s_cb = samples.clone();
    let err_fn = |e| eprintln!("[iTools] 麦克风采集流错误: {e}");

    let stream_res = match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _: &_| {
                let mut b = s_cb.lock().unwrap();
                for &s in data {
                    b.push((s.clamp(-1.0, 1.0) * 32767.0) as i16);
                }
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _: &_| {
                s_cb.lock().unwrap().extend_from_slice(data);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _: &_| {
                let mut b = s_cb.lock().unwrap();
                for &s in data {
                    b.push((s as i32 - 32768) as i16);
                }
            },
            err_fn,
            None,
        ),
        other => {
            let _ = ready_tx.send(Err(format!("不支持的采样格式: {other:?}")));
            return Err(format!("不支持的采样格式: {other:?}"));
        }
    };
    let stream = match stream_res {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(format!("建立麦克风采集流失败: {e}")));
            return Err(format!("建立麦克风采集流失败: {e}"));
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("启动麦克风采集失败: {e}")));
        return Err(format!("启动麦克风采集失败: {e}"));
    }
    let _ = ready_tx.send(Ok(()));

    let start = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if start.elapsed().as_secs() > max_secs {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stream);
    let data = samples.lock().unwrap().clone();
    Ok(Recorded {
        samples: data,
        sample_rate,
        channels,
    })
}

// ==================== plugin_audio_loopback_start / stop：独立系统内录 ====================

struct LoopbackSession {
    /// 隐私登记表 [`tracked`] 里对应条目的 key，stop 时用来摘除。
    key: String,
    stop: Arc<AtomicBool>,
    owner: ActiveSession,
    handle: JoinHandle<Result<Recorded, String>>,
}

/// [`plugin_audio_loopback_start`] / [`plugin_audio_loopback_stop`] 的运行期状态（managed）。
/// 同一时刻只允许一路独立系统内录（与 `audio.rs::AudioState` 的单会话模型一致）；如果同时还在
/// 用 [`plugin_record_video_start`] 的 `includeSystemAudio` 选项录屏带声音，那一路是独立的
/// 内部音频线程，不占用这张表，两者互不冲突、可以同时存在。
#[derive(Default)]
pub struct AudioLoopbackState {
    session: Mutex<Option<LoopbackSession>>,
}

/// 开始系统内录（录电脑正在播放的声音，非麦克风）。已在录则报错；自然终止（`600` 秒兜底）的
/// 旧会话会被回收后再启。需 `audio-capture` 授权。
///
/// # Errors
/// - 调用窗口没有正在运行的插件会话 / 未获 `audio-capture` 授权
/// - 已经有一路内录在进行
/// - 找不到系统输出设备 / 建流失败 / 5 秒内未就绪
#[tauri::command]
pub async fn plugin_audio_loopback_start(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, AudioLoopbackState>,
) -> Result<(), String> {
    let owner = require_audio_capture(&webview, &registry, &settings)?;
    {
        let mut g = lock(&state.session);
        if g.as_ref().map(|s| s.handle.is_finished()).unwrap_or(false) {
            if let Some(old) = g.take() {
                lock(tracked()).remove(&old.key);
            }
        }
        if g.is_some() {
            return Err("已经在录系统声音了".to_string());
        }
    }
    let key = new_id("loopback");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
    let handle =
        std::thread::spawn(move || loopback_worker(stop_t, ready_tx, STANDALONE_LOOPBACK_MAX_SECS));
    // 立刻登记会话——这样即使随后等待超时，stop 也总能把线程关掉（不再泄漏输出设备句柄）。
    {
        *lock(&state.session) = Some(LoopbackSession {
            key: key.clone(),
            stop: stop.clone(),
            owner: owner.clone(),
            handle,
        });
    }
    lock(tracked()).insert(
        key.clone(),
        TrackedSession {
            owner: owner.clone(),
            label: "系统内录（回环声卡）",
            stop: stop.clone(),
            child: None,
        },
    );

    let teardown = |state: &State<'_, AudioLoopbackState>, key: &str| {
        if let Some(s) = lock(&state.session).take() {
            s.stop.store(true, Ordering::Relaxed);
        }
        lock(tracked()).remove(key);
    };
    match tokio::time::timeout(AUDIO_READY_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => {
            ilog!(
                "[iTools][plugin] audio_loopback_start: 来自 {}",
                who(&owner)
            );
            Ok(())
        }
        Ok(Ok(Err(e))) => {
            teardown(&state, &key);
            Err(e)
        }
        Ok(Err(_)) => {
            teardown(&state, &key);
            Err("系统内录线程提前退出".to_string())
        }
        Err(_) => {
            teardown(&state, &key);
            Err("系统内录启动超时".to_string())
        }
    }
}

/// 停止系统内录，返回 base64 WAV（16-bit PCM）。带归属校验；join + 编码不在 UI 线程做。
///
/// # Errors
/// - 当前没有内录在进行 / 该内录会话不属于本插件
/// - 内录线程异常退出
#[tauri::command]
pub async fn plugin_audio_loopback_stop(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    state: State<'_, AudioLoopbackState>,
) -> Result<String, String> {
    let cur = caller_session(&webview, &registry)?;
    let session = {
        let mut g = lock(&state.session);
        match g.as_ref() {
            None => return Err("当前没有系统内录".to_string()),
            Some(s) if !s.owner.same_as(&cur) => return Err("该内录会话不属于本插件".to_string()),
            _ => g.take().unwrap(),
        }
    };
    lock(tracked()).remove(&session.key);
    session.stop.store(true, Ordering::Relaxed);
    let wav = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let rec = session
            .handle
            .join()
            .map_err(|_| "系统内录线程异常".to_string())??;
        Ok(encode_wav(&rec.samples, rec.sample_rate, rec.channels))
    })
    .await
    .map_err(|e| e.to_string())??;
    ilog!("[iTools][plugin] audio_loopback_stop: 来自 {}", who(&cur));
    Ok(png_to_b64(&wav))
}

// ==================== mp4 录屏：捕获目标解析 ====================

/// `plugin_record_video_start` 的录制区域，相对**所选显示器左上角**的局部像素坐标
/// （见模块头"`RecordArea` 的坐标系"一节）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordArea {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// `plugin_record_video_start` 的可选参数。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordVideoOptions {
    /// 录制区域（相对所选显示器左上角）；缺省录整个显示器。与 `hwnd` 二选一，`hwnd` 优先。
    pub area: Option<RecordArea>,
    /// 目标显示器 id（`plugin_list_displays` 返回的 id）；缺省用主显示器。
    pub display_id: Option<u32>,
    /// 目标窗口句柄（与 `window_api.rs::WindowItem.hwnd` 同一枚举出的整数值，即
    /// `plugin_win_list` 返回的 `hwnd`）；给了就录这个窗口，忽略 `area`/`displayId`。
    pub hwnd: Option<isize>,
    /// 目标帧率，缺省 15，范围 1~30（见 [`FPS_RANGE`]）。
    pub fps: Option<u32>,
    /// 是否混入系统正在播放的声音。
    pub include_system_audio: Option<bool>,
    /// 是否混入麦克风声音。
    pub include_mic: Option<bool>,
}

/// 解析出的捕获目标：显示器（可选裁剪区域）或窗口，附带最终喂给 ffmpeg 的宽高
/// （已按需裁剪、并向下取偶——H.264 的 `yuv420p` 要求宽高为偶数）。
enum CaptureTarget {
    Monitor {
        monitor: xcap::Monitor,
        /// 相对显示器左上角的裁剪矩形；`None` = 整屏（尺寸已是偶数，无需裁剪）。
        crop: Option<(u32, u32, u32, u32)>,
        out_w: u32,
        out_h: u32,
    },
    Window {
        window: xcap::Window,
        out_w: u32,
        out_h: u32,
    },
}

impl CaptureTarget {
    fn out_size(&self) -> (u32, u32) {
        match self {
            CaptureTarget::Monitor { out_w, out_h, .. } => (*out_w, *out_h),
            CaptureTarget::Window { out_w, out_h, .. } => (*out_w, *out_h),
        }
    }
}

fn even_size(w: u32, h: u32) -> (u32, u32) {
    (w & !1, h & !1)
}

fn resolve_capture_target(
    hwnd: Option<isize>,
    display_id: Option<u32>,
    area: Option<&RecordArea>,
) -> Result<CaptureTarget, String> {
    if let Some(want) = hwnd {
        let win = xcap::Window::all()
            .map_err(|e| format!("枚举窗口失败: {e}"))?
            .into_iter()
            .find(|w| w.id().map(|i| i as isize == want).unwrap_or(false))
            .ok_or_else(|| format!("找不到窗口 hwnd={want}（可能已关闭）"))?;
        let w = win.width().map_err(|e| format!("读取窗口宽度失败: {e}"))?;
        let h = win.height().map_err(|e| format!("读取窗口高度失败: {e}"))?;
        let (out_w, out_h) = even_size(w, h);
        if out_w < 2 || out_h < 2 {
            return Err("窗口尺寸过小，无法录制".to_string());
        }
        return Ok(CaptureTarget::Window {
            window: win,
            out_w,
            out_h,
        });
    }

    let monitors = xcap::Monitor::all().map_err(|e| format!("枚举显示器失败: {e}"))?;
    let monitor = match display_id {
        Some(want) => monitors
            .into_iter()
            .find(|m| m.id().map(|i| i == want).unwrap_or(false))
            .ok_or_else(|| format!("找不到显示器 id={want}"))?,
        None => {
            let mut chosen: Option<xcap::Monitor> = None;
            for m in monitors {
                if m.is_primary().unwrap_or(false) {
                    chosen = Some(m);
                    break;
                }
                if chosen.is_none() {
                    chosen = Some(m);
                }
            }
            chosen.ok_or_else(|| "未找到任何显示器".to_string())?
        }
    };
    let mon_w = monitor
        .width()
        .map_err(|e| format!("读取显示器宽度失败: {e}"))?;
    let mon_h = monitor
        .height()
        .map_err(|e| format!("读取显示器高度失败: {e}"))?;

    let (x, y, raw_w, raw_h) = match area {
        Some(a) => {
            if a.w == 0 || a.h == 0 {
                return Err("录制区域宽高不能为 0".to_string());
            }
            let x = a.x.max(0) as u32;
            let y = a.y.max(0) as u32;
            if x >= mon_w || y >= mon_h || x + a.w > mon_w || y + a.h > mon_h {
                return Err(format!("录制区域超出显示器边界（显示器 {mon_w}x{mon_h}）"));
            }
            (x, y, a.w, a.h)
        }
        None => (0, 0, mon_w, mon_h),
    };
    let (out_w, out_h) = even_size(raw_w, raw_h);
    if out_w < 2 || out_h < 2 {
        return Err("录制区域过小（不足 2x2 像素）".to_string());
    }
    let crop = if x == 0 && y == 0 && out_w == mon_w && out_h == mon_h {
        None
    } else {
        Some((x, y, out_w, out_h))
    };
    Ok(CaptureTarget::Monitor {
        monitor,
        crop,
        out_w,
        out_h,
    })
}

// ==================== mp4 录屏：喂帧线程 ====================

/// 显示器录制的喂帧线程：从 `Monitor::video_recorder()` 的事件驱动 `Receiver<Frame>` 里按
/// 目标帧率取帧（取不到新帧就复用上一帧，保证输出时长与墙钟时间一致，见模块头"音视频弱同步"
/// 一节），必要时裁剪到 `crop` 矩形，写入 ffmpeg 的标准输入（rawvideo/RGBA8）。
fn spawn_monitor_frame_thread(
    rx: Receiver<xcap::Frame>,
    crop: Option<(u32, u32, u32, u32)>,
    fps: u32,
    mut stdin: ChildStdin,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let interval = Duration::from_secs_f64(1.0 / fps as f64);
        let mut last_frame: Option<image::RgbaImage> = None;
        let mut next_tick = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            next_tick += interval;
            let now = Instant::now();
            let got = if next_tick > now {
                rx.recv_timeout(next_tick - now).ok()
            } else {
                rx.try_recv().ok()
            };
            let img: image::RgbaImage = match got {
                Some(f) => {
                    let full = match image::RgbaImage::from_raw(f.width, f.height, f.raw) {
                        Some(i) => i,
                        None => break, // 帧数据尺寸不自洽，理论不会发生，出现就体面结束
                    };
                    let cur = match crop {
                        Some((x, y, w, h)) => {
                            image::imageops::crop_imm(&full, x, y, w, h).to_image()
                        }
                        None => full,
                    };
                    last_frame = Some(cur.clone());
                    cur
                }
                // 还没等到任何一帧：跳过这一格，不写出全零黑帧占位。
                None if last_frame.is_none() => continue,
                // 超时/管道已断开（stop() 已调用 video_recorder.stop()）：复用上一帧撑住时长，
                // 下一轮循环顶部的 stop 检查会在断开后很快退出。
                None => last_frame.clone().unwrap_or_default(),
            };
            if stdin.write_all(img.as_raw()).is_err() {
                break; // ffmpeg 已退出/管道断开
            }
            frames.fetch_add(1, Ordering::Relaxed);
        }
        drop(stdin); // 关闭管道 = 给 ffmpeg 发 EOF，触发它收尾写文件
    })
}

/// 窗口录制的喂帧线程：定时轮询 `Window::capture_image()`（见模块头"窗口录制没有事件驱动的
/// 高效路径"一节），失败即体面结束（不是无限重试卡死）。
fn spawn_window_frame_thread(
    window: xcap::Window,
    out_w: u32,
    out_h: u32,
    fps: u32,
    mut stdin: ChildStdin,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicU64>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let target_ms = (1000 / fps.max(1)) as u64;
        while !stop.load(Ordering::Relaxed) {
            let t0 = Instant::now();
            let img = match window.capture_image() {
                Ok(i) => i,
                Err(_) => break, // 窗口被关闭/遮挡导致抓取失败：结束录制，不重试卡死
            };
            let (w, h) = img.dimensions();
            let final_img = if w != out_w || h != out_h {
                image::imageops::crop_imm(&img, 0, 0, out_w.min(w), out_h.min(h)).to_image()
            } else {
                img
            };
            if stdin.write_all(final_img.as_raw()).is_err() {
                break;
            }
            frames.fetch_add(1, Ordering::Relaxed);
            let spent = t0.elapsed().as_millis() as u64;
            if spent < target_ms {
                std::thread::sleep(Duration::from_millis(target_ms - spent));
            }
        }
        drop(stdin);
    })
}

/// ffmpeg 视频编码子进程的 stderr 尾部（最后约 4000 字符），供编码失败时诊断用；用一个
/// 后台线程持续读取，避免子进程因为没人读 stderr 管道而被系统缓冲区打满卡死。
fn spawn_stderr_tail(mut pipe: impl Read + Send + 'static) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut g) = buf2.lock() {
                        g.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if g.len() > 8000 {
                            let cut = g.len() - 4000;
                            *g = g.split_off(cut);
                        }
                    }
                }
            }
        }
    });
    buf
}

fn spawn_progress_thread(
    app: tauri::AppHandle,
    label: String,
    record_id: String,
    started: Instant,
    frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    watch_path: PathBuf,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            let size_bytes = std::fs::metadata(&watch_path).map(|m| m.len()).unwrap_or(0);
            emit_to_plugin(
                &app,
                &label,
                EVT_RECORD_PROGRESS,
                &RecordProgressEvt {
                    record_id: record_id.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    frames: frames.load(Ordering::Relaxed),
                    size_bytes,
                },
            );
        }
    })
}

const EVT_RECORD_PROGRESS: &str = "plugin-record-video-progress";

/// mp4 录制进度事件（`plugin-record-video-progress`），约每 500ms 推一次。`sizeBytes` 是对
/// 静默视频临时文件（或无音频时的最终文件）的实时 `stat`，属于近似值——ffmpeg 内部有自己的
/// 编码/写盘缓冲，不代表"已经安全落盘不丢"的严格保证。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RecordProgressEvt {
    record_id: String,
    elapsed_ms: u64,
    frames: u64,
    size_bytes: u64,
}

fn wait_with_timeout(
    child: &Arc<Mutex<Child>>,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Ok(Some(s)) = lock(child).try_wait() {
            return Some(s);
        }
        if start.elapsed() >= timeout {
            let _ = lock(child).kill();
            return lock(child).wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 跑一次 ffmpeg 并等它结束（用于混流这种一次性、非流式的调用），带超时兜底；
/// 返回 `(退出码, stderr 文本)`。风格对齐 `runtime_api.rs::run_runtime_and_collect`。
fn run_ffmpeg_blocking(
    exe: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<(i32, String), String> {
    let mut cmd = no_window_command(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动 ffmpeg 失败: {e}"))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "拿不到 ffmpeg 标准错误".to_string())?;
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr_bytes = err_handle.join().unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    Ok((code, String::from_utf8_lossy(&stderr_bytes).into_owned()))
}

// ==================== plugin_record_video_start / stop ====================

enum StopHook {
    Monitor(xcap::VideoRecorder),
    None,
}

struct VideoSession {
    /// 录制会话的归属者（插件 id + dev 标志）——见 `record.rs`/`audio.rs` 模块头关于
    /// "归属校验用的是完整会话身份"的说明，`plugin_record_video_stop` 靠它做归属校验。
    owner: ActiveSession,
    stop: Arc<AtomicBool>,
    stop_hook: StopHook,
    frame_handle: JoinHandle<()>,
    ffmpeg_child: Arc<Mutex<Child>>,
    stderr_tail: Arc<Mutex<String>>,
    sys_audio: Option<JoinHandle<Result<Recorded, String>>>,
    mic_audio: Option<JoinHandle<Result<Recorded, String>>>,
    video_only_path: PathBuf,
    final_path: PathBuf,
    ffmpeg_exe: PathBuf,
}

/// `plugin_record_video_start` / `plugin_record_video_stop` 的运行期状态（managed）。支持多路
/// 并发录制（不同插件同时录不同区域/窗口），以 `recordId` 区分，风格对齐
/// `runtime_api.rs::RuntimeExecState`。
#[derive(Default)]
pub struct RecordVideoState {
    sessions: Mutex<HashMap<String, VideoSession>>,
}

/// 启动前失败时的清理：置停止标志、（若是显示器捕获）关闭 WGC 捕获会话、kill 已起的 ffmpeg
/// 子进程。不 join 任何线程——它们会在停止标志/管道断开后很快自行退出。
fn abort_start(
    stop: &Arc<AtomicBool>,
    stop_hook: &StopHook,
    child: &Arc<Mutex<Child>>,
    video_only_path: &Path,
) {
    stop.store(true, Ordering::Relaxed);
    if let StopHook::Monitor(vr) = stop_hook {
        let _ = vr.stop();
    }
    let _ = lock(child).kill();
    let _ = std::fs::remove_file(video_only_path);
}

/// 开始 mp4 录屏。区域/显示器录制走 xcap 的 `Monitor::video_recorder()`
/// （Windows.Graphics.Capture，事件驱动）；窗口录制走轮询 `Window::capture_image()`（见模块头
/// 局限说明）。视频始终经 ffmpeg 编码为 h264/mp4；勾选的音频（系统声音/麦克风）各自独立采集，
/// 停止时再与视频混流。需 `screen-capture` 授权；若勾选了任一音频源，还需同时具备
/// `audio-capture` 授权（录屏+录音要求两个权限都在场）。
///
/// # Errors
/// - 未获所需授权（`screen-capture`，以及需要音频时的 `audio-capture`）
/// - ffmpeg 尚未由宿主安装（提示先调用 `itools.runtime.ensure('ffmpeg')`）
/// - `hwnd`/`displayId` 指向的窗口/显示器不存在，或 `area` 超出显示器边界/尺寸过小
/// - 启动 ffmpeg / 建立屏幕捕获会话失败
/// - 勾选的音频源 5 秒内未就绪（设备不存在/被占用/格式不支持）
#[tauri::command]
pub async fn plugin_record_video_start(
    opts: RecordVideoOptions,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, RecordVideoState>,
) -> Result<String, String> {
    let owner = require_capture(&webview, &registry, &settings)?;
    let include_sys = opts.include_system_audio.unwrap_or(false);
    let include_mic = opts.include_mic.unwrap_or(false);
    if (include_sys || include_mic)
        && !plugin_granted(&settings, &registry, &owner, "audio-capture")
    {
        return Err(
            "录屏+录音需要同时授权 screen-capture 与 audio-capture（请在「插件管理」里补授权 audio-capture）"
                .to_string(),
        );
    }

    let ffmpeg_exe = super::runtime_api::resolved_exe_path("ffmpeg").ok_or_else(|| {
        "未找到宿主托管的 ffmpeg，请先调用 itools.runtime.ensure('ffmpeg') 安装后再开始录制"
            .to_string()
    })?;

    let fps = opts.fps.unwrap_or(15).clamp(FPS_RANGE.0, FPS_RANGE.1);

    let record_id = new_id("rec");
    let out_dir = recordings_dir();
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("创建录制输出目录失败: {e}"))?;
    let need_audio = include_sys || include_mic;
    let final_path = out_dir.join(timestamped_filename(&record_id));
    let video_only_path = if need_audio {
        out_dir.join(format!("_tmp_video_{record_id}.mp4"))
    } else {
        final_path.clone()
    };

    let stop = Arc::new(AtomicBool::new(false));

    // ---- 第一步：先把音频源（若勾选）建起来并等就绪，全部成功后才去动屏幕捕获/ffmpeg ----
    // 这样任何一步失败时，前面已经起的线程只靠共享的 `stop` 标志收尾，不用另外记一堆句柄。
    let mut sys_audio = None;
    let mut mic_audio = None;
    if include_sys {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        let stop_t = stop.clone();
        let h = std::thread::spawn(move || loopback_worker(stop_t, tx, AUDIO_MAX_SECS));
        match tokio::time::timeout(AUDIO_READY_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => sys_audio = Some(h),
            Ok(Ok(Err(e))) => {
                stop.store(true, Ordering::Relaxed);
                return Err(format!("系统内录启动失败: {e}"));
            }
            Ok(Err(_)) => {
                stop.store(true, Ordering::Relaxed);
                return Err("系统内录线程提前退出".to_string());
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err("系统内录启动超时".to_string());
            }
        }
    }
    if include_mic {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        let stop_t = stop.clone();
        let h = std::thread::spawn(move || mic_worker(stop_t, tx, AUDIO_MAX_SECS));
        match tokio::time::timeout(AUDIO_READY_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => mic_audio = Some(h),
            Ok(Ok(Err(e))) => {
                stop.store(true, Ordering::Relaxed);
                return Err(format!("麦克风录音启动失败: {e}"));
            }
            Ok(Err(_)) => {
                stop.store(true, Ordering::Relaxed);
                return Err("麦克风录音线程提前退出".to_string());
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                return Err("麦克风录音启动超时".to_string());
            }
        }
    }

    // 屏幕捕获目标的解析**必须放在所有 .await 之后**：xcap 的 Monitor / Window 不是 Send，
    // 只要它跨越一个 await 挂起点，整个命令的 future 就不再是 Send，Tauri 的 generate_handler!
    // 直接编译不过。这不是风格问题，是硬约束——挪回去会立刻炸。
    let target = resolve_capture_target(opts.hwnd, opts.display_id, opts.area.as_ref())?;
    let (out_w, out_h) = target.out_size();

    // ---- 第二步：起 ffmpeg（视频编码，纯视频、无声）；此后到函数返回之间不再有 .await，
    // 避免让 xcap 的 COM/WinRT 句柄需要跨越异步挂起点（见文件顶部关于 Send 的设计取舍）。----
    let mut ffmpeg_cmd = no_window_command(&ffmpeg_exe);
    ffmpeg_cmd
        .args([
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "rgba",
            "-video_size",
            &format!("{out_w}x{out_h}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "-",
            "-an",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-preset",
            "veryfast",
            "-movflags",
            "+faststart",
        ])
        .arg(&video_only_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = match ffmpeg_cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            return Err(format!("启动 ffmpeg（视频编码）失败: {e}"));
        }
    };
    let Some(stdin) = child.stdin.take() else {
        stop.store(true, Ordering::Relaxed);
        let _ = child.kill();
        return Err("拿不到 ffmpeg 标准输入".to_string());
    };
    let stderr_tail = match child.stderr.take() {
        Some(p) => spawn_stderr_tail(p),
        None => Arc::new(Mutex::new(String::new())),
    };
    let ffmpeg_child = Arc::new(Mutex::new(child));

    let frames = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    let (frame_handle, stop_hook) = match target {
        CaptureTarget::Monitor { monitor, crop, .. } => match monitor.video_recorder() {
            Ok((video_recorder, rx)) => {
                let h =
                    spawn_monitor_frame_thread(rx, crop, fps, stdin, stop.clone(), frames.clone());
                if let Err(e) = video_recorder.start() {
                    abort_start(&stop, &StopHook::None, &ffmpeg_child, &video_only_path);
                    return Err(format!("启动屏幕捕获失败: {e}"));
                }
                (h, StopHook::Monitor(video_recorder))
            }
            Err(e) => {
                abort_start(&stop, &StopHook::None, &ffmpeg_child, &video_only_path);
                return Err(format!("创建屏幕捕获会话失败: {e}"));
            }
        },
        CaptureTarget::Window {
            window,
            out_w,
            out_h,
        } => {
            let h = spawn_window_frame_thread(
                window,
                out_w,
                out_h,
                fps,
                stdin,
                stop.clone(),
                frames.clone(),
            );
            (h, StopHook::None)
        }
    };

    let app = webview.app_handle().clone();
    let label = webview.label().to_string();
    spawn_progress_thread(
        app,
        label,
        record_id.clone(),
        started,
        frames.clone(),
        stop.clone(),
        video_only_path.clone(),
    );

    lock(tracked()).insert(
        record_id.clone(),
        TrackedSession {
            owner: owner.clone(),
            label: "屏幕录制（mp4）",
            stop: stop.clone(),
            child: Some(ffmpeg_child.clone()),
        },
    );
    lock(&state.sessions).insert(
        record_id.clone(),
        VideoSession {
            owner: owner.clone(),
            stop,
            stop_hook,
            frame_handle,
            ffmpeg_child,
            stderr_tail,
            sys_audio,
            mic_audio,
            video_only_path,
            final_path,
            ffmpeg_exe,
        },
    );
    ilog!(
        "[iTools][plugin] record_video_start: 来自 {}, record={record_id}, {out_w}x{out_h}@{fps}fps",
        who(&owner)
    );
    Ok(record_id)
}

fn finish_video_session(session: VideoSession) -> Result<String, String> {
    // 帧线程先收尾：它看到 stop 标志/管道断开会 drop 自己那份 stdin，这是 ffmpeg 收到 EOF、
    // 开始收尾写文件（flush + moov atom）的前提。
    let _ = session.frame_handle.join();
    let status = wait_with_timeout(&session.ffmpeg_child, Duration::from_secs(120))
        .ok_or_else(|| "等待 ffmpeg 收尾超时".to_string())?;
    if !status.success() {
        let tail = session
            .stderr_tail
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let _ = std::fs::remove_file(&session.video_only_path);
        return Err(format!(
            "ffmpeg 视频编码失败（退出码 {:?}）：{}",
            status.code(),
            if tail.trim().is_empty() {
                "(无输出)".to_string()
            } else {
                tail
            }
        ));
    }

    let mut audio_wavs: Vec<PathBuf> = Vec::new();
    if let Some(h) = session.sys_audio {
        let rec = h.join().map_err(|_| "系统内录线程异常".to_string())??;
        let p = session.video_only_path.with_file_name(format!(
            "_tmp_sysaudio_{}.wav",
            uniq_stem(&session.video_only_path)
        ));
        std::fs::write(&p, encode_wav(&rec.samples, rec.sample_rate, rec.channels))
            .map_err(|e| format!("写入系统内录临时文件失败: {e}"))?;
        audio_wavs.push(p);
    }
    if let Some(h) = session.mic_audio {
        let rec = h.join().map_err(|_| "麦克风录音线程异常".to_string())??;
        let p = session.video_only_path.with_file_name(format!(
            "_tmp_micaudio_{}.wav",
            uniq_stem(&session.video_only_path)
        ));
        std::fs::write(&p, encode_wav(&rec.samples, rec.sample_rate, rec.channels))
            .map_err(|e| format!("写入麦克风录音临时文件失败: {e}"))?;
        audio_wavs.push(p);
    }

    if audio_wavs.is_empty() {
        // 没有音频：ffmpeg 视频编码阶段已经直接写到 final_path，无需再混流。
        return Ok(session.video_only_path.to_string_lossy().into_owned());
    }

    let mut args: Vec<String> = vec![
        "-y".to_string(),
        "-i".to_string(),
        session.video_only_path.to_string_lossy().into_owned(),
    ];
    for w in &audio_wavs {
        args.push("-i".to_string());
        args.push(w.to_string_lossy().into_owned());
    }
    if audio_wavs.len() >= 2 {
        // 双路（系统声音+麦克风）交给 ffmpeg 的 amix 滤镜做混音与必要的重采样，
        // 不是本模块手写的信号处理（见模块头"该交给 ffmpeg 的都交给它"）。
        args.push("-filter_complex".to_string());
        args.push(format!(
            "amix=inputs={}:duration=first:dropout_transition=0",
            audio_wavs.len()
        ));
    }
    args.extend([
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "aac".to_string(),
        "-shortest".to_string(),
    ]);
    args.push(session.final_path.to_string_lossy().into_owned());

    let (code, stderr) = run_ffmpeg_blocking(&session.ffmpeg_exe, &args, Duration::from_secs(600))?;
    if code != 0 {
        let _ = std::fs::remove_file(&session.final_path);
        let kept: String = audio_wavs
            .iter()
            .map(|p| format!("、{}", p.display()))
            .collect();
        return Err(format!(
            "ffmpeg 混流音频失败（退出码 {code}）：{}。已保留无声视频与音频原始文件：{}{kept}",
            if stderr.trim().is_empty() {
                "(无输出)".to_string()
            } else {
                stderr
            },
            session.video_only_path.display()
        ));
    }
    let _ = std::fs::remove_file(&session.video_only_path);
    for w in &audio_wavs {
        let _ = std::fs::remove_file(w);
    }
    Ok(session.final_path.to_string_lossy().into_owned())
}

fn uniq_stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string())
}

/// 停止 mp4 录屏，返回最终文件的绝对路径（无音频时是纯视频 mp4；有音频时是混流后的最终
/// mp4，过程中产生的静默视频/音频 WAV 临时文件在混流成功后会被清理）。带归属校验；
/// join/编码/混流全部放 blocking 线程池，不阻塞 UI 线程。
///
/// # Errors
/// - `recordId` 不存在或已结束 / 不属于本插件
/// - ffmpeg 视频编码失败，或等待其收尾超时
/// - 勾选了音频但采集线程异常
/// - 混流失败（此时无声视频与音频 WAV 原始文件会保留在磁盘，错误信息里给出路径，不会让用户
///   凭空丢失已经录到的内容）
#[tauri::command]
pub async fn plugin_record_video_stop(
    record_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    state: State<'_, RecordVideoState>,
) -> Result<String, String> {
    let cur = caller_session(&webview, &registry)?;
    let session = {
        let mut g = lock(&state.sessions);
        match g.get(&record_id) {
            None => return Err("录制会话不存在或已结束".to_string()),
            // 归属比对**带 dev 标志**：同名的调试会话与正式会话是两个不同的归属者
            // （见 `record.rs`/`audio.rs` 模块头对 [`ActiveSession::same_as`] 的说明）。
            Some(s) if !s.owner.same_as(&cur) => return Err("该录制会话不属于本插件".to_string()),
            _ => g.remove(&record_id).unwrap(),
        }
    };
    lock(tracked()).remove(&record_id);

    session.stop.store(true, Ordering::Relaxed);
    if let StopHook::Monitor(vr) = &session.stop_hook {
        let _ = vr.stop();
    }

    ilog!(
        "[iTools][plugin] record_video_stop: 来自 {}, record={record_id}",
        who(&cur)
    );
    tauri::async_runtime::spawn_blocking(move || finish_video_session(session))
        .await
        .map_err(|e| format!("停止录制线程异常: {e}"))?
}


#[cfg(test)]
mod tests {
    use super::*;

    /// `active_sessions` 必须给出**会话身份**，不能是给人看的描述串。
    ///
    /// 这条是为一个真实缺陷立的桩：它曾经返回 `"<插件 id> 正在<label>"` 这种整句描述，
    /// 而 `indicator.rs` 把整句当成 plugin_id 用了。后果有两层——托盘文案变成
    /// 「apitest 正在录屏 正在使用录屏 / 录音」；更要命的是托盘那个「点击停止」
    /// 拿这个假 id 去 `same_as` 匹配**永远匹配不上**，录屏/内录根本停不掉。
    /// 谁再把返回值改回字符串，这条测试当场变红。
    #[test]
    fn active_sessions_report_identity_not_prose() {
        let mk = |id: &str, dev: bool, label: &'static str| TrackedSession {
            owner: ActiveSession { id: id.to_string(), dev },
            label,
            stop: Arc::new(AtomicBool::new(false)),
            child: None,
        };
        {
            let mut g = lock(tracked());
            g.clear();
            g.insert("r1".into(), mk("demo", false, "屏幕录制（mp4）"));
            g.insert("r2".into(), mk("demo", false, "系统内录（回环声卡）"));
            // 同名但属于**调试会话**：必须与正式会话区分开，否则掐断会掐错人
            g.insert("r3".into(), mk("demo", true, "屏幕录制（mp4）"));
        }
        let mut got = active_sessions();
        got.sort_by(|a, b| (&a.0.id, a.0.dev, a.1).cmp(&(&b.0.id, b.0.dev, b.1)));

        assert_eq!(got.len(), 3, "同会话的不同用途要各占一条，dev 会话要单独一条：{got:?}");
        for (session, label) in &got {
            assert_eq!(session.id, "demo", "给出的必须是插件 id 本身");
            assert!(!session.id.contains("正在"), "返回值里混进了描述串：{session:?}");
            assert!(!label.is_empty());
        }
        assert!(got.iter().any(|(s, _)| s.dev), "dev 标志必须原样带出来");
        assert!(got.iter().any(|(_, l)| l.contains("内录")));
        assert!(got.iter().any(|(_, l)| l.contains("屏幕录制")));

        lock(tracked()).clear();
    }
}
