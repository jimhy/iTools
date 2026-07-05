//! 录屏（GIF）：定时抓「主屏」帧 → 停止时用 image 的 GIF 编码器合成动图。需 **screen-capture** 授权。
//!
//! v1 的轻量实现——宿主内编码，无音频、无 mp4（mp4+声需 ffmpeg，见 README 路线图）。
//! 帧率约 5fps、单帧宽上限 640px、最多 150 帧（约 30s）。xcap::Monitor 不 Send，
//! 故录制线程内每帧新建即弃。
//!
//! 加固（据评审）：stop 全 async——join 与 CPU 密集的 GIF 量化编码都放 blocking 线程池，不冻结 UI；
//! stop 带归属校验；自然满帧终止的旧会话在下次 start 时回收。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tauri::State;

use super::capture::require_capture;
use super::commands::{current_plugin, png_to_b64};
use super::PluginRegistry;
use crate::settings::SettingsStore;

const TARGET_MS: u64 = 200; // ~5fps
const MAX_FRAMES: usize = 150;
const MAX_W: u32 = 640;

struct RecSession {
    stop: Arc<AtomicBool>,
    owner: String,
    handle: JoinHandle<Result<Vec<image::RgbaImage>, String>>,
}

#[derive(Default)]
pub struct RecordState {
    session: Mutex<Option<RecSession>>,
}

/// 开始录屏（主屏，GIF）。已在录则报错；自然满帧终止的旧会话会被回收后再启。需 screen-capture 授权。
#[tauri::command]
pub fn plugin_start_gif_record(
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, RecordState>,
) -> Result<(), String> {
    require_capture(&registry, &settings)?;
    let owner = current_plugin(&registry)?;
    let mut guard = state.session.lock().unwrap();
    if guard.as_ref().map(|s| s.handle.is_finished()).unwrap_or(false) {
        let _ = guard.take();
    }
    if guard.is_some() {
        return Err("已经在录屏了".to_string());
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let handle = std::thread::spawn(move || -> Result<Vec<image::RgbaImage>, String> {
        let mut frames: Vec<image::RgbaImage> = Vec::new();
        while !stop_t.load(Ordering::Relaxed) && frames.len() < MAX_FRAMES {
            let t0 = Instant::now();
            match super::capture::capture_primary_downscaled(MAX_W) {
                Ok(frame) => frames.push(frame),
                Err(_) => break,
            }
            let spent = t0.elapsed().as_millis() as u64;
            if spent < TARGET_MS {
                std::thread::sleep(Duration::from_millis(TARGET_MS - spent));
            }
        }
        Ok(frames)
    });
    *guard = Some(RecSession { stop, owner, handle });
    Ok(())
}

/// 停止录屏，返回 base64 GIF。带归属校验；join + GIF 编码不在 UI 线程做。
#[tauri::command]
pub async fn plugin_stop_gif_record(
    registry: State<'_, PluginRegistry>,
    state: State<'_, RecordState>,
) -> Result<String, String> {
    // 取会话 + 归属校验原子完成（避免与并发 start 的竞态覆盖）
    let cur = current_plugin(&registry)?;
    let session = {
        let mut g = state.session.lock().unwrap();
        match g.as_ref() {
            None => return Err("当前没有录屏".to_string()),
            Some(s) if s.owner != cur => return Err("该录屏会话不属于本插件".to_string()),
            _ => g.take().unwrap(),
        }
    };
    session.stop.store(true, Ordering::Relaxed);
    // join + GIF 量化编码（150 帧可能几秒）都放 blocking 线程池，绝不阻塞 UI 线程
    let gif = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let frames = session
            .handle
            .join()
            .map_err(|_| "录屏线程异常".to_string())??;
        if frames.is_empty() {
            return Err("没有抓到任何帧".to_string());
        }
        encode_gif(frames)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(png_to_b64(&gif))
}

fn encode_gif(frames: Vec<image::RgbaImage>) -> Result<Vec<u8>, String> {
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame};
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)
            .map_err(|e| format!("GIF 循环设置失败: {e}"))?;
        for f in frames {
            let delay = Delay::from_numer_denom_ms(TARGET_MS as u32, 1);
            enc.encode_frame(Frame::from_parts(f, 0, 0, delay))
                .map_err(|e| format!("GIF 编码失败: {e}"))?;
        }
    }
    Ok(out.into_inner())
}
