//! 录音（cpal → WAV）。默认麦克风输入，采样收集到内存，停止时编码 16-bit PCM WAV 返回 base64。
//! 需 **audio-capture** 授权。cpal 的 Stream 不 Send，故用专用线程持有它，主线程经 AtomicBool 控停。
//!
//! 并发/线程安全（据评审加固）：命令全 async——启动不在 UI 线程上阻塞等设备初始化、也不跨等待持锁；
//! 自然终止（600s 兜底）的旧会话会在下次启动时回收；停止带归属校验（别的插件不能取走本插件的录音）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tauri::State;
use tokio::sync::oneshot;

use super::commands::{current_plugin, plugin_granted, png_to_b64};
use super::PluginRegistry;
use crate::settings::SettingsStore;

struct Session {
    stop: Arc<AtomicBool>,
    owner: String,
    handle: JoinHandle<Result<Recorded, String>>,
}

struct Recorded {
    samples: Vec<i16>,
    sample_rate: u32,
    channels: u16,
}

/// 录音运行期状态（managed）：同一时刻只允许一个录音会话。
#[derive(Default)]
pub struct AudioState {
    session: Mutex<Option<Session>>,
}

fn require_audio(registry: &PluginRegistry, settings: &SettingsStore) -> Result<(), String> {
    let id = current_plugin(registry)?;
    if !plugin_granted(settings, &id, "audio-capture") {
        return Err("插件未获授权录音（请在「插件管理」里授权 audio-capture）".to_string());
    }
    Ok(())
}

/// 录音线程主体：建流→播放→收到停止信号前一直采样→停。ready_tx 回传「流是否建好」。
fn record_worker(
    stop: Arc<AtomicBool>,
    ready_tx: oneshot::Sender<Result<(), String>>,
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
    let err_fn = |e| eprintln!("[iTools] 录音流错误: {e}");

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
            let _ = ready_tx.send(Err(format!("建立录音流失败: {e}")));
            return Err(format!("建立录音流失败: {e}"));
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(format!("启动录音失败: {e}")));
        return Err(format!("启动录音失败: {e}"));
    }
    let _ = ready_tx.send(Ok(()));

    let start = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if start.elapsed().as_secs() > 600 {
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

/// 开始录音（默认麦克风）。已在录则报错；自然终止的旧会话会被回收后再启。
#[tauri::command]
pub async fn plugin_start_audio_record(
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, AudioState>,
) -> Result<(), String> {
    require_audio(&registry, &settings)?;
    let owner = current_plugin(&registry)?;
    // 回收自然终止的旧会话；仍在录则拒绝（不跨 await 持锁）
    {
        let mut g = state.session.lock().unwrap();
        if g.as_ref().map(|s| s.handle.is_finished()).unwrap_or(false) {
            let _ = g.take();
        }
        if g.is_some() {
            return Err("已经在录音了".to_string());
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();
    let handle = std::thread::spawn(move || record_worker(stop_t, ready_tx));
    // 立刻登记会话——这样即使随后等待超时，stop 也总能把线程关掉（不再泄漏麦克风）
    {
        *state.session.lock().unwrap() = Some(Session {
            stop: stop.clone(),
            owner,
            handle,
        });
    }
    // async 等「流已建好」（不阻塞 UI、不持锁），最长 5 秒
    let teardown = |state: &State<'_, AudioState>| {
        if let Some(s) = state.session.lock().unwrap().take() {
            s.stop.store(true, Ordering::Relaxed); // 线程建流完成后会立即看到 stop 而退出并释放设备
        }
    };
    match tokio::time::timeout(Duration::from_secs(5), ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => {
            teardown(&state);
            Err(e)
        }
        Ok(Err(_)) => {
            teardown(&state);
            Err("录音线程提前退出".to_string())
        }
        Err(_) => {
            teardown(&state);
            Err("录音启动超时".to_string())
        }
    }
}

/// 停止录音，返回 base64 WAV（16-bit PCM）。带归属校验；join + 编码不在 UI 线程做。
#[tauri::command]
pub async fn plugin_stop_audio_record(
    registry: State<'_, PluginRegistry>,
    state: State<'_, AudioState>,
) -> Result<String, String> {
    // 取会话 + 归属校验在同一临界区内原子完成（避免 take→check→put-back 之间被并发 start 抢占覆盖）
    let cur = current_plugin(&registry)?;
    let session = {
        let mut g = state.session.lock().unwrap();
        match g.as_ref() {
            None => return Err("当前没有录音".to_string()),
            Some(s) if s.owner != cur => return Err("该录音会话不属于本插件".to_string()),
            _ => g.take().unwrap(),
        }
    };
    session.stop.store(true, Ordering::Relaxed);
    // join（等线程收尾）放到 blocking 线程池，避免阻塞 UI 线程
    let wav = tauri::async_runtime::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let rec = session
            .handle
            .join()
            .map_err(|_| "录音线程异常".to_string())??;
        Ok(encode_wav(&rec.samples, rec.sample_rate, rec.channels))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(png_to_b64(&wav))
}

/// i16 PCM 采样 → WAV 字节（RIFF/PCM 16bit）。
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
