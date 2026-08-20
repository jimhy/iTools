//! 插件下载：带进度、可取消的文件下载（落插件沙盒目录）。
//!
//! # 为什么不让插件用 `itools.fetch` 下文件
//!
//! `fetch` 把整个响应体读进内存再返回给页面（二进制还要 base64 膨胀 4/3），下一个几百 MB
//! 的文件会直接把插件窗口拖垮，而且**全程没有进度**——用户只能对着一个不动的界面干等。
//! 视频下载、素材批量拉取这类插件必须有边下边写 + 进度回报，所以单独开这条通道。
//!
//! # 设计要点
//!
//! - **落盘不入内存**：64 KB 一块边读边写，内存占用与文件大小无关。
//! - **进度节流**：按「至少 400 ms **且** 至少 2 MB」双条件推，不是每块都推。
//!   只按时间节流是不够的——真机实测 `webview.eval` 单次往返的开销就超过百毫秒级阈值，
//!   于是每个数据块回来时时间条件都已成立，退化成每块一推，下载速度被打到 1/90。
//! - **id 由调用方生成并传入**，不是下载开始后再返回。因为命令是 async 的，等它返回
//!   id 时下载可能已经跑完了，那样「取消」这个功能形同虚设。调用方先造 id，
//!   随时可以拿这个 id 去 `plugin_download_cancel`。
//! - **取消表按会话身份隔离**：别的插件不能取消不属于自己的下载。用完整会话身份
//!   （插件 id + dev 标志）而不是裸 id——调试插件与同名正式插件共用 id，只比 id
//!   会让调试窗里的下载被正式窗掐掉（反之亦然）。
//! - **落点限插件沙盒**：复用 `sandbox_relative` 的相对路径校验，禁绝对路径、盘符与 `..`。
//!   要下到用户指定的目录，等 `fs_api` 的 scope 能力接进来后再扩展，不在这里开口子。

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::State;

use super::commands::{caller_session, plugin_granted, sandbox_relative, session_files_dir};
use super::PluginRegistry;
use crate::settings::SettingsStore;

/// 单个文件的下载上限（4 GB）。不设上限的话，一个恶意插件用一条 URL 就能把用户磁盘写满；
/// 设得太小又会挡住正当的大文件下载（视频、镜像），4 GB 是个够用且不至于失控的界。
const DOWNLOAD_LIMIT: u64 = 4 * 1024 * 1024 * 1024;

/// 进度推送的最小间隔。见模块文档「进度节流」。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);

/// 进度推送的最小字节增量——与 [`PROGRESS_INTERVAL`] **同时**满足才推。
/// 只按时间节流会因为 `webview.eval` 本身的开销而失效，详见推送处的注释。
const PROGRESS_MIN_BYTES: u64 = 2 * 1024 * 1024;

/// 读写块大小。
const CHUNK: usize = 64 * 1024;

/// 已被请求取消的下载：`"<会话身份>\u{1}<下载 id>"` 的集合。
///
/// 用「会话身份 + id」而不是裸 id 作键，是为了让取消请求只能命中发起方自己的下载。
fn cancel_set() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 取消表的键：会话身份 + 下载 id。
fn cancel_key(scope: &str, id: &str) -> String {
    format!("{scope}\u{1}{id}")
}

/// 下载完成后的返回。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadResult {
    /// 落盘的绝对路径。
    pub path: String,
    /// 实际写入的字节数。
    pub size: u64,
}

/// 把一条进度推给插件页（事件总线见 `bridge.js` 的 `window.__itoolsEmit`）。
///
/// `total` 为 `None` 表示服务端没给 `Content-Length`——这时进度条只能显示已下载量，
/// 不能显示百分比。如实传 null，不要为了界面好看编一个假的总量出来。
fn emit_progress(
    webview: &tauri::Webview,
    id: &str,
    received: u64,
    total: Option<u64>,
    done: bool,
    error: Option<&str>,
) {
    let payload = serde_json::json!({
        "id": id,
        "received": received,
        "total": total,
        "done": done,
        "error": error,
    });
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('download-progress', {payload})");
    let _ = webview.eval(&js);
}

/// 下载一个 http/https 文件到插件沙盒目录，过程中经 `download-progress` 事件回报进度。
///
/// 需 `network` 授权（与 `itools.fetch` 同一个权限，都是出站流量）。
#[tauri::command]
pub async fn plugin_download(
    url: String,
    dest: String,
    id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<DownloadResult, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "network") {
        return Err("插件未获授权联网（请在「插件管理」里授权 network）".to_string());
    }
    let lower = url.trim().to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("download 只支持 http/https".to_string());
    }
    if id.trim().is_empty() {
        return Err("download 需要一个非空的 id（用于取消与进度回报）".to_string());
    }

    // 落点：插件沙盒内的相对路径。先算好绝对路径，父目录不存在就建出来。
    let rel = sandbox_relative(&dest)?.to_path_buf();
    let target = session_files_dir(&session, &registry).join(&rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }

    let scope = session.scope_key();
    let key = cancel_key(&scope, &id);
    // 进入下载前先清一次同键的残留：上一轮下载被取消后若没走到清理（进程被杀等），
    // 这个键会留在表里，让本轮一启动就被判为已取消。
    if let Ok(mut set) = cancel_set().lock() {
        set.remove(&key);
    }

    let wv = webview.clone();
    let id_for_task = id.clone();
    let key_for_task = key.clone();
    let target_for_task = target.clone();

    let outcome = tauri::async_runtime::spawn_blocking(move || -> Result<DownloadResult, String> {
        // 不给整体下载设截止时间——大文件本来就要下很久，20 秒那种整体超时会把正常下载掐断。
        // 建连超时由 crate::http 的 Agent 统一配置（见 src/http.rs），这里不再叠加。
        let resp = crate::http::request("GET", &url)
            .call()
            .map_err(|e| e.to_string())?;
        let total = resp
            .header("Content-Length")
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(t) = total {
            if t > DOWNLOAD_LIMIT {
                return Err(format!(
                    "文件超过 {} GB 下载上限",
                    DOWNLOAD_LIMIT / 1024 / 1024 / 1024
                ));
            }
        }

        let mut reader = resp.into_reader();
        let mut file =
            std::fs::File::create(&target_for_task).map_err(|e| format!("创建文件失败: {e}"))?;
        let mut buf = vec![0u8; CHUNK];
        let mut received: u64 = 0;
        let mut last_emit = Instant::now();
        let mut last_emit_bytes: u64 = 0;
        emit_progress(&wv, &id_for_task, 0, total, false, None);

        loop {
            // 取消检查放在每块之前：用户点了取消就别再多写一块。
            let cancelled = cancel_set()
                .lock()
                .map(|s| s.contains(&key_for_task))
                .unwrap_or(false);
            if cancelled {
                drop(file);
                // 取消要把半截文件删掉——留着一个不完整的文件比没有更糟，
                // 用户或插件很容易把它当成下载好的东西用。
                let _ = std::fs::remove_file(&target_for_task);
                return Err("下载已取消".to_string());
            }

            let n = reader.read(&mut buf).map_err(|e| format!("读取失败: {e}"))?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])
                .map_err(|e| format!("写入失败: {e}"))?;
            received += n as u64;

            // 服务端没给 Content-Length 时，上限只能靠边下边判来兜。
            if received > DOWNLOAD_LIMIT {
                drop(file);
                let _ = std::fs::remove_file(&target_for_task);
                return Err(format!(
                    "文件超过 {} GB 下载上限",
                    DOWNLOAD_LIMIT / 1024 / 1024 / 1024
                ));
            }

            // 时间与字节数**两个条件都要满足**才推进度。
            // 只按时间节流在真机上是无效的：`webview.eval` 单次往返的开销比 150 ms 还大，
            // 于是每个数据块循环回来时时间条件都已成立，等于每块都推一次，下载被进度
            // 推送拖垮（runtime 那边实测把 15.4 MB/s 打到 166 KB/s）。
            if received >= last_emit_bytes + PROGRESS_MIN_BYTES
                && last_emit.elapsed() >= PROGRESS_INTERVAL
            {
                emit_progress(&wv, &id_for_task, received, total, false, None);
                last_emit = Instant::now();
                last_emit_bytes = received;
            }
        }

        file.flush().map_err(|e| format!("写入失败: {e}"))?;
        emit_progress(&wv, &id_for_task, received, total, true, None);
        Ok(DownloadResult {
            path: target_for_task.to_string_lossy().to_string(),
            size: received,
        })
    })
    .await
    .map_err(|e| e.to_string())?;

    // 无论成败都要把取消标记清掉，否则同一个 id 下次复用会一启动就被判为已取消。
    if let Ok(mut set) = cancel_set().lock() {
        set.remove(&key);
    }
    if let Err(e) = &outcome {
        emit_progress(&webview, &id, 0, None, true, Some(e));
    }
    outcome
}

/// 取消一个进行中的下载。只能取消**本插件会话**发起的下载。
///
/// 返回是否真的标记成功（下载已结束或 id 不对时返回 false，不当成错误——
/// 用户手快点两下取消不该弹报错）。
#[tauri::command]
pub fn plugin_download_cancel(
    id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<bool, String> {
    let session = caller_session(&webview, &registry)?;
    let key = cancel_key(&session.scope_key(), &id);
    let mut set = cancel_set()
        .lock()
        .map_err(|_| "取消表加锁失败".to_string())?;
    Ok(set.insert(key))
}
