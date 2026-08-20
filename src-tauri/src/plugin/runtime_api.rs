//! 宿主托管的外部运行时（ffmpeg / adb / yt-dlp …）。
//!
//! # 为什么插件不能自带这些可执行文件
//!
//! 插件分发规范（`doc/插件系统/插件分发规范.md` 第七节第 2 条）对插件包里的可执行/脚本
//! 扩展名（`.exe` / `.dll` / `.bat` / `.ps1` / … 共 23 种）**整包拒绝**，包体本身也有 32 MB
//! 上限——一个静态链接的 ffmpeg.exe 随便就是几十上百 MB，物理上塞不进插件包，也绝不该让
//! 插件自己夹带任意二进制（那等于彻底放弃「插件是纯前端资源」这条安全前提）。
//!
//! 解法是**宿主统一托管**：宿主按这里内置的官方清单下载、校验、解压、管理版本，插件只能
//! 通过 `plugin_runtime_*` 这组受控接口按「运行时名字 + 参数」调用，**拿不到裸路径**，
//! 更没有办法把任意文件伪装成运行时塞进来。
//!
//! # 为什么哈希校验是硬性前提
//!
//! 下载源（GitHub Releases / Google 官方 CDN）理论上可信，但「理论上可信的下载源」历史上
//! 也出现过供应链投毒、CDN 劫持、镜像篡改——一旦这个二进制被换成别的东西，它是以「宿主
//! 主进程的权限」跑起来的，风险等级和执行任意代码没有区别。项目在 `mirror.rs` 里对插件包
//! 本身就是这个立场（"镜像可篡改内容，有可信哈希时必须校验，不匹配即拒绝"），这里延续同一
//! 条红线，而且标准更严：**没有可信哈希，压根不下载/不落地**，不存在"先装上、回头再补校验"
//! 这种中间状态。
//!
//! ## 清单里的哈希是本机实算的，不是抄来的
//!
//! [`RUNTIME_MANIFEST`] 里三项的 `sha256` 均为**本机实际下载后计算所得**，不是抄来的：
//! ffmpeg 的值与该 release 自带的 `checksums.sha256` 逐字符一致（两个独立来源交叉验证），
//! adb / yt-dlp 的体积与上游声明值吻合。[`plugin_runtime_ensure`] 在 `sha256` 为 `None` 时
//! **直接拒绝安装**——宁可某个运行时暂时装不上，也不放行未经校验的可执行文件。
//!
//! # 为什么三个 URL 都钉了具体版本
//!
//! 早先用的是 `latest` 形式的滚动地址，那样哈希根本钉不住：上游一发新构建就失配。
//! 现在三条都指向不可变产物，但**各自有不同的保鲜期**，维护时要知道：
//!
//! - **ffmpeg**：BtbN 只保留最近约两周的「日构建」tag，更早的仅保留每月最后一天那个快照。
//!   所以这里钉的是月末快照 `autobuild-2026-07-31-14-10`（按现行保留策略可存活约两年），
//!   **不能钉日构建**——两周后就 404。注意这只是 BtbN 事实上的保留习惯，不是不可变承诺；
//!   要真正长期可靠，应把该 zip 镜像到自己可控的存储再引用。
//! - **adb**：Google 保留历史版本（实测 2023 年的 r34.0.5 至今仍可下载），版本化 URL 较稳。
//!   但 **文件名后缀从 r36 起由 `-windows.zip` 改为 `-win.zip`**，升级版本号时不要沿用旧模板，
//!   应先从 `repository2-3.xml` 取准确文件名。
//! - **yt-dlp**：按日期打 tag，历史 release 长期保留，最稳。
//!
//! 升级任一运行时时，**必须同时更新 `download_url`、`version` 与 `sha256` 三项**，
//! 且哈希要自己下载后实算，不要只抄上游页面上的声明值。
//!
//! # 权限边界：`runtime` ≠ `runCommand`
//!
//! `runCommand`（见 `exec.rs`）能跑插件指定的**任意程序**，风险很高；`runtime` 只能跑
//! [`RUNTIME_MANIFEST`] 里这几个**宿主下载、校验过**的固定程序，风险低得多，两者是完全独立
//! 的权限声明，不能互相替代、也不共享授权表。
//!
//! # args 校验的真实边界（如实说明，不夸大）
//!
//! 子进程始终不经 shell 启动（`Command::new(exe).args(args)`），所以 shell 元字符
//! （`&`/`|`/`>` 等）不构成注入面。[`validate_args`] 在此基础上额外挡了「参数里出现路径
//! 分隔符 + 常见可执行/脚本扩展名」这种最直白的"把运行时当跳板拉起另一个程序"尝试。
//! **但这做不到彻底防护**：ffmpeg/adb 本身就是功能强大的 CLI，`-i`/`push`/`pull` 这类参数
//! 天然允许读写宿主进程有权限触达的任意文件，ffmpeg 的 `-f lavfi`、各种 `protocol` 也留有
//! 读取本地资源的口子——这些不是"没想到"，是没有安全、通用的方式在参数层面**完全**堵死同时
//! 还保留正常功能，本模块做的是「挡明显的、常见的滥用形态」，不是「形式化证明安全」，
//! UI/文档需要如实告知用户：授权 `runtime` 等于允许插件以宿主进程权限调用 ffmpeg/adb，
//! 而不是一个完全沙盒化的执行环境。
//!
//! # 事件总线
//!
//! 插件页拿不到 `window.__TAURI__`（受限自定义协议页），所有事件一律走
//! `webview.eval` → `window.__itoolsEmit(channel, payload)`，照抄 `exec.rs::emit_to_plugin`
//! 的做法（两处各自维护一份小的私有实现，理由见 [`emit_to_plugin`] 处的注释）。
//! 后台常驻插件的窗口 label 是 `plugin-bg-<插件id>` 而非 `plugin`——本模块从不写死 label，
//! 一律取自发起调用的那个 `tauri::Webview` 自身（`webview.label()`），谁调用就推给谁。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{Manager, State};
use walkdir::WalkDir;

use super::commands::{caller_session, plugin_granted};
use super::mirror;
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 高危能力名——见模块头「权限边界」一节，不与 `runCommand` 共用授权表。
const PERM_RUNTIME: &str = "runtime";

// ==================== 运行时清单 ====================

/// 一个可被宿主托管的外部运行时的解压/落盘方式。
enum Archive {
    /// zip 归档。解压后**到处按文件名搜**（不写死内部相对路径）：官方构建内部目录层级
    /// 常与构建号/日期绑定（如 BtbN 的产物），写死一条固定相对路径会在上游随便一次改版
    /// 后悄悄失效；按文件名搜索更皮实，代价是要求同一压缩包内该文件名唯一（这三项都满足）。
    Zip { exe_file_name: &'static str },
    /// 单个可执行文件直接下载（如 yt-dlp 官方发布的独立 exe），无需解压。
    RawExe { file_name: &'static str },
}

/// 一条运行时清单条目。见模块头「哈希校验」一节：`sha256` 为 `None` 时
/// [`plugin_runtime_ensure`] 拒绝安装。
struct RuntimeSpec {
    /// 供插件引用的稳定标识（如 `"ffmpeg"`），同时也是宿主本地存放目录名。
    name: &'static str,
    display_name: &'static str,
    /// 面向用户展示的版本描述。三条 `download_url` 均已钉在不可变产物上，所以这里写的
    /// 就是**实际会下载到的那一个版本**（真机实测：`ffmpeg -version` / `yt-dlp --version`
    /// 的输出与本字段逐字符一致）。升级时三项必须同步改，见模块头。
    version: &'static str,
    description: &'static str,
    download_url: &'static str,
    archive: Archive,
    /// 官方分发文件的 SHA-256（小写十六进制）。`None` = 清单未提供，拒绝安装。
    sha256: Option<&'static str>,
}

/// 内置运行时清单：目前支持 ffmpeg / adb（必需）与 yt-dlp（可选）。
///
/// 三项的 `sha256` 均已填入**本机下载后实算**的值（ffmpeg 那条另与上游 `checksums.sha256`
/// 交叉验证过），因此三项都可正常安装；见模块头「哈希校验」一节。
static RUNTIME_MANIFEST: &[RuntimeSpec] = &[
    RuntimeSpec {
        name: "ffmpeg",
        display_name: "FFmpeg",
        version: "n8.1.2-34-g9b6c8969e0（BtbN autobuild-2026-07-31-14-10 月末快照）",
        description: "音视频转码/剪辑/截帧命令行工具（BtbN 官方 Windows 静态构建，GPL 版含 libx264，zip 格式）。",
        // 钉在**月末快照** tag 上，不用 `latest` 也不用日构建：
        // BtbN 只保留最近约两周的日构建，更早的仅留每月最后一天那个快照（可存活约两年）。
        // 钉日构建的话两周后就 404 了。
        download_url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n8.1.2-34-g9b6c8969e0-win64-gpl-8.1.zip",
        archive: Archive::Zip { exe_file_name: "ffmpeg.exe" },
        // 本机实测：下载 167,405,723 字节后自行计算所得，且与该 release 自带的
        // checksums.sha256 声明值逐字符一致（两个独立来源交叉验证）。
        sha256: Some("cc4156d51387566ea8ba653fc3a04897bdf812fddf652428d9030bbf7ae24835"),
    },
    RuntimeSpec {
        name: "adb",
        display_name: "Android Debug Bridge",
        version: "r37.0.1（Google 官方 Platform-Tools）",
        description: "Android 设备调试桥：连接设备、查日志、推拉文件、跑 shell（Google 官方 Platform Tools）。",
        // ⚠ 后缀是 `-win.zip` 而不是 `-windows.zip`——Google 从 r36 起改了命名，
        // 沿用旧模板拼出来的 `platform-tools_rXX-windows.zip` 会直接 404。
        // 版本号与文件名的权威来源是 https://dl.google.com/android/repository/repository2-3.xml。
        download_url: "https://dl.google.com/android/repository/platform-tools_r37.0.1-win.zip",
        archive: Archive::Zip { exe_file_name: "adb.exe" },
        // 本机实测下载 8,044,989 字节后自行计算（Google 清单只声明 SHA-1，故此处的
        // SHA-256 由本地实算得出；体积与清单声明一致，可作交叉印证）。
        sha256: Some("45f4d63113e895ebde0c90f194099a4676b6ac653bd28d54314a9e022bbc1a99"),
    },
    RuntimeSpec {
        name: "yt-dlp",
        display_name: "yt-dlp",
        version: "2026.08.19",
        description: "视频/音频下载工具，支持数千站点（yt-dlp 官方 GitHub Releases，单文件 exe）。",
        download_url: "https://github.com/yt-dlp/yt-dlp/releases/download/2026.08.19/yt-dlp.exe",
        archive: Archive::RawExe { file_name: "yt-dlp.exe" },
        // 本机实测下载 17,840,399 字节后自行计算。
        sha256: Some("66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a"),
    },
];

/// 进度推送的最小间隔与最小字节增量——**两个条件都满足**才推一次。
///
/// 只按时间节流在真机上是无效的：`webview.eval` 的单次开销就超过了常见的 150 ms 阈值，
/// 导致每个数据块都会触发一次推送，下载速度被打到 1/90（实测 166 KB/s vs curl 15.4 MB/s）。
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(400);
const PROGRESS_MIN_BYTES: u64 = 4 * 1024 * 1024;

fn find_spec(name: &str) -> Result<&'static RuntimeSpec, String> {
    RUNTIME_MANIFEST
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("未知的运行时: {name}"))
}

// ==================== 本地存放位置 ====================

/// 所有宿主托管运行时的根目录：`<数据根>/host-runtimes`。
///
/// 放在 [`crate::paths::data_root`] 之下是刻意的：`fs_api.rs::reject_itools_data_root`
/// 已经把**整个数据根**（含所有子目录，不只是 `plugin-data/`）列入插件文件能力的硬黑名单，
/// 运行时目录因此自动被插件的读写能力挡在外面，不需要为它单独再开一条黑名单规则。
fn runtime_root() -> PathBuf {
    crate::paths::data_root().join("host-runtimes")
}

fn runtime_dir(name: &str) -> PathBuf {
    runtime_root().join(name)
}

/// 在某运行时目录下按文件名找到已安装的可执行文件（见 [`Archive::Zip`] 的说明：
/// 不依赖固定相对路径）。未安装或目录不存在返回 `None`。
fn resolve_installed_exe(spec: &RuntimeSpec) -> Option<PathBuf> {
    let dir = runtime_dir(spec.name);
    if !dir.is_dir() {
        return None;
    }
    let file_name = match &spec.archive {
        Archive::Zip { exe_file_name } => *exe_file_name,
        Archive::RawExe { file_name } => *file_name,
    };
    WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().is_file() && e.file_name().to_str() == Some(file_name))
        .map(|e| e.into_path())
}

/// 递归统计某目录的总字节数（用于 `sizeBytes`），读不到的条目直接跳过，不因个别文件失败整体报错。
fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

// ==================== plugin_runtime_list ====================

/// 单个运行时的清单信息 + 本机状态，供插件展示"是否已安装/多大"。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub installed: bool,
    /// 已安装时为该运行时目录的总大小；未安装为 `None`（不是 0——0 会被误读成"装了但是空的"）。
    pub size_bytes: Option<u64>,
    pub description: String,
    /// 清单是否提供了可信哈希——为 `false` 时 [`plugin_runtime_ensure`] 一定会失败，
    /// UI 应据此禁用"安装"按钮并提示原因，而不是让用户点了才发现装不上。
    pub installable: bool,
}

fn info_for(spec: &RuntimeSpec) -> RuntimeInfo {
    let installed_exe = resolve_installed_exe(spec);
    let installed = installed_exe.is_some();
    RuntimeInfo {
        name: spec.name.to_string(),
        display_name: spec.display_name.to_string(),
        version: spec.version.to_string(),
        installed,
        size_bytes: if installed {
            Some(dir_size(&runtime_dir(spec.name)))
        } else {
            None
        },
        description: spec.description.to_string(),
        installable: spec.sha256.is_some(),
    }
}

/// 列出内置运行时清单及本机安装状态。需已获 `runtime` 授权。
///
/// # Errors
/// - 调用窗口没有正在运行的插件会话
/// - 插件未获 `runtime` 授权
#[tauri::command]
pub fn plugin_runtime_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<RuntimeInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    Ok(RUNTIME_MANIFEST.iter().map(info_for).collect())
}

// ==================== plugin_runtime_ensure：下载 + 校验 + 安装 ====================

/// 下载进度事件（`plugin-runtime-progress`）。
fn emit_runtime_progress(
    webview: &tauri::Webview,
    name: &str,
    received: u64,
    total: Option<u64>,
    done: bool,
    error: Option<&str>,
) {
    let payload = serde_json::json!({
        "name": name,
        "received": received,
        "total": total,
        "done": done,
        "error": error,
    });
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('plugin-runtime-progress', {payload})");
    let _ = webview.eval(&js);
}

/// 单次下载的体积硬上限（1 GB）：这几个运行时正常都在几十到一百多 MB，
/// 给够余量的同时防一个响应异常的下载源把内存吃爆。
const RUNTIME_DOWNLOAD_CAP: u64 = 1024 * 1024 * 1024;

/// 本次下载按什么顺序试哪些源，返回 `(来源展示名, URL)`。
///
/// 策略本身收在 [`mirror::download_sources`] 一处（更新器走的是同一套），这里只是转调，
/// 免得两个模块各写一份「有代理走官方、没代理走镜像」的判断，日后改一处漏一处。
///
/// # 为什么运行时下载可以放心用第三方镜像
///
/// 「镜像可以篡改内容」是 `mirror.rs` 反复强调的红线。本模块之所以不受它限制：
/// 清单里每一项都带**本机实算的 sha256**，下载完必须逐字节校验，不匹配就丢弃且**从不落盘**
/// （见 [`download_and_verify`]）。镜像最多能让下载失败，无法让一个被替换的可执行文件落地。
fn download_sources(url: &str) -> Vec<(String, String)> {
    mirror::download_sources(url)
}

/// 下载整个响应体到内存并边下边算 SHA-256；哈希与 `expected_hex`（大小写不敏感）不一致时
/// **直接返回错误、不返回任何字节给调用方**——调用方因此不可能把未经校验的内容写到磁盘上，
/// 这就是模块头承诺的"校验失败绝不落地"在代码层面的体现（不是"写了再删"，而是"从不写"）。
///
/// 按 [`download_sources`] 给出的顺序逐个源尝试，某个源失败就换下一个；全都失败时把**每个源
/// 各自的失败原因**一起带回去（只报「下载失败」等于把排查线索全丢了）。
/// 注意哈希校验失败**不换源重试**：那说明拿到的内容确实不是清单里那一个，换个源再下一遍
/// 既不会让它变对，也可能把一个正在投毒的源的错误掩盖成「网络问题」。
fn download_and_verify(
    webview: &tauri::Webview,
    name: &str,
    url: &str,
    expected_hex: &str,
) -> Result<Vec<u8>, String> {
    let sources = download_sources(url);
    let last = sources.len().saturating_sub(1);
    let mut errors: Vec<String> = Vec::new();
    for (i, (label, candidate)) in sources.iter().enumerate() {
        ilog!(
            "[iTools][plugin] runtime {name}: 尝试源 {}/{} = {label}",
            i + 1,
            sources.len()
        );
        match download_once(webview, name, candidate, expected_hex) {
            Ok(bytes) => {
                ilog!(
                    "[iTools][plugin] runtime {name}: 下载完成，来源 {label}，{} 字节",
                    bytes.len()
                );
                return Ok(bytes);
            }
            Err(e) if e.starts_with(HASH_MISMATCH_PREFIX) => {
                ilog!("[iTools][plugin] runtime {name}: 来源 {label} 内容哈希不符，已丢弃且不再换源");
                return Err(e);
            }
            Err(e) => {
                ilog!("[iTools][plugin] runtime {name}: 来源 {label} 失败：{e}");
                errors.push(format!("{label}: {e}"));
                if i == last {
                    return Err(format!("全部 {} 个下载源都失败——{}", sources.len(), errors.join("；")));
                }
            }
        }
    }
    Err("没有可用的下载源".to_string())
}

/// 哈希不符错误的固定前缀：[`download_and_verify`] 靠它把「内容不对」与「网络失败」区分开，
/// 前者绝不换源重试。用常量而不是在两处各写一遍字面量，免得改了一处忘了另一处。
const HASH_MISMATCH_PREFIX: &str = "SHA-256 校验失败";

/// 从**一个**指定源完整下载并校验。失败原因原样上抛，由 [`download_and_verify`] 决定要不要换源。
fn download_once(
    webview: &tauri::Webview,
    name: &str,
    url: &str,
    expected_hex: &str,
) -> Result<Vec<u8>, String> {
    let resp = crate::http::request("GET", url)
        .call()
        .map_err(|e| format!("下载失败: {e}"))?;
    let total = resp.header("Content-Length").and_then(|s| s.parse::<u64>().ok());
    if let Some(t) = total {
        if t > RUNTIME_DOWNLOAD_CAP {
            return Err(format!(
                "文件超过 {} GB 下载上限",
                RUNTIME_DOWNLOAD_CAP / 1024 / 1024 / 1024
            ));
        }
    }
    let mut reader = resp.into_reader();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    let mut last_emit = Instant::now();
    let mut last_emit_bytes: u64 = 0;
    emit_runtime_progress(webview, name, 0, total, false, None);
    loop {
        let n = reader.read(&mut chunk).map_err(|e| format!("读取失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() as u64 > RUNTIME_DOWNLOAD_CAP {
            return Err(format!(
                "文件超过 {} GB 下载上限",
                RUNTIME_DOWNLOAD_CAP / 1024 / 1024 / 1024
            ));
        }
        // 进度节流必须**同时**看时间和字节数，只看时间会失效。
        //
        // 真机实测：`webview.eval` 单次往返的开销比 150 ms 还大，于是「距上次推送超过
        // 150 ms 就推」这个条件在每个数据块循环回来时都成立——等于每块都推一次，下载
        // 整个被进度推送拖垮：160 MB 的包实测只有 **166 KB/s**，而同机 curl 是 15.4 MB/s，
        // 差 93 倍。加上「至少又下了 4 MB」这个条件后，推送次数从上千次降到几十次。
        let enough_bytes = buf.len() as u64 >= last_emit_bytes + PROGRESS_MIN_BYTES;
        if enough_bytes && last_emit.elapsed() >= PROGRESS_MIN_INTERVAL {
            emit_runtime_progress(webview, name, buf.len() as u64, total, false, None);
            last_emit = Instant::now();
            last_emit_bytes = buf.len() as u64;
        }
    }
    let digest = hasher.finalize();
    let actual_hex = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        emit_runtime_progress(
            webview,
            name,
            buf.len() as u64,
            total,
            true,
            Some("SHA-256 校验失败，下载内容已丢弃，未写入任何文件"),
        );
        return Err(format!(
            "{HASH_MISMATCH_PREFIX}（期望 {expected_hex}，实际 {actual_hex}），下载内容已丢弃，未落盘"
        ));
    }
    emit_runtime_progress(webview, name, buf.len() as u64, total, true, None);
    Ok(buf)
}

// ---- zip 安全解压（本模块自用的简化版本，理由见下） ----
//
// `install.rs::extract_zip` 也做安全解压，但它的策略是"整包含可执行/脚本扩展名就拒绝"——
// 那正是本模块要下载的东西（ffmpeg.exe / adb.exe），策略互斥，不能直接复用那个函数
// （它也是模块私有项，跨模块本就拿不到）。这里保留 Zip Slip 防护（条目路径校验、
// 落点复核、体积/条目数上限），只是不做可执行文件拒收。

const MAX_RUNTIME_ENTRIES: usize = 4000;
const MAX_RUNTIME_FILE_BYTES: u64 = 400 * 1024 * 1024;
const MAX_RUNTIME_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;

/// 校验并规范化一个 zip 条目名，返回可安全 join 的相对路径（Zip Slip 第一道闸）。
fn safe_entry_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("归档中存在空路径条目".to_string());
    }
    if name.contains('\0') {
        return Err(format!("归档条目路径非法：{name}"));
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(format!("归档含绝对路径条目（不安全）：{name}"));
    }
    let mut out = PathBuf::new();
    let mut count = 0usize;
    for seg in unified.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg.chars().all(|c| c == '.') {
            return Err(format!("归档条目含上跳路径（..）：{name}"));
        }
        let b = seg.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return Err(format!("归档条目含盘符（不安全）：{name}"));
        }
        out.push(seg);
        count += 1;
    }
    if count == 0 {
        return Err(format!("归档条目路径为空：{name}"));
    }
    Ok(out)
}

/// 把 zip 字节流安全解压到 `dest`（会先创建该目录）。Zip Slip 防护 + 体积/条目数上限，
/// 详见各分支注释；**不做可执行文件拒收**（本模块存在的目的就是落地 ffmpeg.exe/adb.exe）。
fn safe_extract_zip(bytes: Vec<u8>, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建解压目录失败: {e}"))?;
    let canon_dest = dest.canonicalize().map_err(|e| format!("解析解压目录失败: {e}"))?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("归档不是有效的 zip: {e}"))?;
    if zip.len() > MAX_RUNTIME_ENTRIES {
        return Err(format!("归档条目数 {} 超过上限 {MAX_RUNTIME_ENTRIES}，已中止", zip.len()));
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("读取归档条目失败: {e}"))?;
        let raw_name = f.name().to_string();
        if let Some(mode) = f.unix_mode() {
            if mode & 0xF000 == 0xA000 {
                return Err(format!("归档含符号链接条目（不安全）：{raw_name}"));
            }
        }
        let rel = safe_entry_path(&raw_name)?;
        let out = canon_dest.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("创建目录失败 {}: {e}", out.display()))?;
            continue;
        }
        if f.size() > MAX_RUNTIME_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 声明大小超过单文件上限 {} MB，已中止",
                MAX_RUNTIME_FILE_BYTES / 1024 / 1024
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
            let cp = parent.canonicalize().map_err(|e| format!("解析落点失败: {e}"))?;
            if !cp.starts_with(&canon_dest) {
                return Err(format!("归档条目落点越界（不安全）：{raw_name}"));
            }
        }
        let mut w = std::fs::File::create(&out).map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        let written = std::io::copy(&mut f.by_ref().take(MAX_RUNTIME_FILE_BYTES + 1), &mut w)
            .map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        if written > MAX_RUNTIME_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 实际大小超过单文件上限 {} MB，已中止",
                MAX_RUNTIME_FILE_BYTES / 1024 / 1024
            ));
        }
        total += written;
        if total > MAX_RUNTIME_TOTAL_BYTES {
            return Err(format!(
                "解压总体积超过上限 {} MB，已中止（疑似 zip 炸弹）",
                MAX_RUNTIME_TOTAL_BYTES / 1024 / 1024
            ));
        }
    }
    Ok(())
}

fn install_bytes(spec: &RuntimeSpec, bytes: Vec<u8>) -> Result<(), String> {
    let dir = runtime_dir(spec.name);
    match &spec.archive {
        Archive::Zip { .. } => safe_extract_zip(bytes, &dir),
        Archive::RawExe { file_name } => {
            std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
            std::fs::write(dir.join(file_name), &bytes).map_err(|e| format!("写入文件失败: {e}"))
        }
    }
}

/// 确保某运行时已安装：已装则直接返回现状；未装则下载 → 校验 SHA-256 → 解压/落盘，
/// 过程中经 `plugin-runtime-progress` 事件回报下载进度。需已获 `runtime` 授权。
///
/// # Errors
/// - 调用窗口没有正在运行的插件会话 / 未获 `runtime` 授权
/// - `name` 不在 [`RUNTIME_MANIFEST`] 里
/// - 清单该项 `sha256` 为 `None`——拒绝安装，不发起任何网络请求。当前清单三项均已填入
///   实算哈希，这条是给「日后新增运行时但还没算出哈希」留的闸门
/// - 网络下载失败，或体积超过 [`RUNTIME_DOWNLOAD_CAP`]
/// - SHA-256 校验失败（下载内容已被丢弃，不会写入磁盘）
/// - 解压/写入失败，或解压后按清单文件名找不到期望的可执行文件（会清理已解压的残留目录）
#[tauri::command]
pub async fn plugin_runtime_ensure(
    name: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<RuntimeInfo, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let spec = find_spec(&name)?;
    if resolve_installed_exe(spec).is_some() {
        return Ok(info_for(spec));
    }
    let Some(expected_hex) = spec.sha256 else {
        return Err(format!(
            "运行时 {} 的官方分发清单尚未填入可信 SHA-256（维护者待补全），\
             出于安全考虑拒绝安装未经校验的可执行文件，未发起任何下载",
            spec.display_name
        ));
    };

    ilog!(
        "[iTools][plugin] runtime_ensure 开始下载: 来自 {}, runtime={name}",
        who(&session)
    );

    let url = spec.download_url.to_string();
    let name_for_task = name.clone();
    let wv = webview.clone();
    let bytes = tauri::async_runtime::spawn_blocking(move || {
        download_and_verify(&wv, &name_for_task, &url, expected_hex)
    })
    .await
    .map_err(|e| format!("下载线程异常: {e}"))??;

    install_bytes(spec, bytes)?;

    if resolve_installed_exe(spec).is_none() {
        let _ = std::fs::remove_dir_all(runtime_dir(spec.name));
        return Err(
            "解压后未找到期望的可执行文件（清单登记的文件名可能与官方包结构不再匹配），已清理残留"
                .to_string(),
        );
    }
    ilog!("[iTools][plugin] runtime_ensure 完成: runtime={name}");
    Ok(info_for(spec))
}

/// 删除已下载的运行时，释放磁盘。需已获 `runtime` 授权。
///
/// # Errors
/// - 调用窗口没有正在运行的插件会话 / 未获 `runtime` 授权
/// - `name` 不在清单里
/// - 删除失败（目录不存在视为成功，不是错误）
#[tauri::command]
pub fn plugin_runtime_remove(
    name: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let spec = find_spec(&name)?;
    match std::fs::remove_dir_all(runtime_dir(spec.name)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除运行时失败: {e}")),
    }
}

// ==================== args 校验 ====================

/// 明显可被当作"另一个可执行文件"的扩展名（小写，不含点）。
const DENY_ARG_EXTS: [&str; 9] = ["exe", "bat", "cmd", "com", "scr", "ps1", "vbs", "msi", "sh"];

/// 对传给运行时的参数做基本校验。见模块头「args 校验的真实边界」——这只挡最直白的滥用形态，
/// 不是完整的沙盒化方案。
///
/// # Errors
/// - 某个参数含 NUL 字节
/// - 某个参数同时满足"含路径分隔符"与"以常见可执行/脚本扩展名结尾"（疑似指向另一个程序）
fn validate_args(args: &[String]) -> Result<(), String> {
    for a in args {
        if a.as_bytes().contains(&0) {
            return Err("参数包含非法空字符".to_string());
        }
        let has_sep = a.contains('/') || a.contains('\\');
        if has_sep {
            if let Some((_, ext)) = a.rsplit_once('.') {
                let ext_lower = ext.to_ascii_lowercase();
                if DENY_ARG_EXTS.contains(&ext_lower.as_str()) {
                    return Err(format!("参数疑似指向其它可执行文件，已拒绝：{a}"));
                }
            }
        }
    }
    Ok(())
}

// ==================== 子进程构造（不经 shell，同 exec.rs 思路） ====================

#[cfg(windows)]
fn new_runtime_command(program: &str, args: &[String], new_group: bool) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    let mut cmd = Command::new(program);
    cmd.args(args);
    let mut flags = CREATE_NO_WINDOW;
    if new_group {
        flags |= CREATE_NEW_PROCESS_GROUP;
    }
    cmd.creation_flags(flags);
    cmd
}

#[cfg(not(windows))]
fn new_runtime_command(program: &str, args: &[String], _new_group: bool) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd
}

/// `plugin_runtime_exec` / `plugin_runtime_exec_stream` 的可选参数。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExecOptions {
    /// 子进程工作目录；缺省继承 iTools 主进程当前目录。
    pub cwd: Option<String>,
    /// 超时（毫秒）；到点仍未结束就强制终止。缺省不设超时。
    pub timeout_ms: Option<u64>,
}

/// 单路管道读取上限（16 MB），超过即截断并标记——理由同 `exec.rs::EXEC_OUTPUT_LIMIT`。
const RUNTIME_EXEC_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

fn read_capped(pipe: &mut impl Read) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return (buf, false),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= RUNTIME_EXEC_OUTPUT_LIMIT {
                    buf.truncate(RUNTIME_EXEC_OUTPUT_LIMIT);
                    return (buf, true);
                }
            }
            Err(_) => return (buf, false),
        }
    }
}

// ==================== plugin_runtime_exec：一次性执行 ====================

/// [`plugin_runtime_exec`] 的返回结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// 输出是否因超过单路 16 MB 上限被截断。
    ///
    /// 与 `itools.exec` 的同名字段一一对应：没有它的话截断就是**静默**的——
    /// 调用方拿到半截输出却以为拿全了，据此下「没有匹配项」之类结论就是错的。
    pub truncated: bool,
}

fn run_runtime_and_collect(
    program: &str,
    args: &[String],
    opts: &RuntimeExecOptions,
    timeout_ms: Option<u64>,
) -> Result<RuntimeExecResult, String> {
    let mut cmd = new_runtime_command(program, args, false);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = opts.cwd.as_deref() {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {e}"))?;
    let mut stdout_pipe = child.stdout.take().ok_or_else(|| "拿不到子进程 stdout".to_string())?;
    let mut stderr_pipe = child.stderr.take().ok_or_else(|| "拿不到子进程 stderr".to_string())?;
    let out_handle = std::thread::spawn(move || read_capped(&mut stdout_pipe));
    let err_handle = std::thread::spawn(move || read_capped(&mut stderr_pipe));

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        if let Some(t) = timeout_ms {
            if start.elapsed().as_millis() as u64 >= t {
                timed_out = true;
                let _ = child.kill();
                break child.wait().ok();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let (stdout_bytes, out_cut) = out_handle.join().unwrap_or_default();
    let (stderr_bytes, err_cut) = err_handle.join().unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);
    Ok(RuntimeExecResult {
        code,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
        truncated: out_cut || err_cut,
    })
}

/// 执行某宿主运行时并等它跑完，拿到 `{ code, stdout, stderr }`。需已获 `runtime` 授权，
/// 且该运行时须已通过 [`plugin_runtime_ensure`] 安装。
///
/// 输出按 UTF-8 宽松解码（`from_utf8_lossy`）：ffmpeg/adb 的输出以 ASCII/UTF-8 为主，
/// 这里不像 `exec.rs::plugin_exec` 那样做 GBK 探测——如果未来接入的运行时确实需要，
/// 再按 `exec.rs` 的做法扩展，本次先如实按当前范围（ffmpeg/adb/yt-dlp）实现。
///
/// # Errors
/// - 未获 `runtime` 授权 / 会话无效
/// - `name` 不在清单里，或尚未安装（提示先调用 `runtimeEnsure`）
/// - `args` 未通过 [`validate_args`]
/// - 启动子进程失败
#[tauri::command]
pub async fn plugin_runtime_exec(
    name: String,
    args: Vec<String>,
    opts: Option<RuntimeExecOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<RuntimeExecResult, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let spec = find_spec(&name)?;
    let exe = resolve_installed_exe(spec)
        .ok_or_else(|| format!("运行时 {} 尚未安装，请先调用 runtimeEnsure", spec.display_name))?;
    validate_args(&args)?;
    let opts = opts.unwrap_or_default();
    let timeout_ms = opts.timeout_ms;
    let exe_str = exe.to_string_lossy().into_owned();
    tauri::async_runtime::spawn_blocking(move || run_runtime_and_collect(&exe_str, &args, &opts, timeout_ms))
        .await
        .map_err(|e| format!("执行线程异常: {e}"))?
}

// ==================== plugin_runtime_exec_stream：流式执行 ====================

const EVT_STDOUT: &str = "plugin-runtime-stdout";
const EVT_STDERR: &str = "plugin-runtime-stderr";
const EVT_EXIT: &str = "plugin-runtime-exit";
const EVT_FFMPEG_PROGRESS: &str = "plugin-runtime-ffmpeg-progress";

/// 把一条事件推给**发起调用的插件窗口**。必须走 `webview.eval` → `window.__itoolsEmit`
/// 这条总线（插件页拿不到 `window.__TAURI__`，原生 `app.emit_to` 送不到）。
///
/// 这是 `exec.rs::emit_to_plugin` 的等价重实现：那边是模块私有函数，本模块拿不到，
/// 两处各自维护一份（都很短）比强行打通私有边界更简单，逻辑上完全一致。
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

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RuntimeLineEvt {
    stream_id: String,
    data: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RuntimeExitEvt {
    stream_id: String,
    code: i32,
    timed_out: bool,
}

/// ffmpeg 进度事件：解析自 stderr 里 `frame=… fps=… time=… bitrate=… speed=…` 这类进度行。
/// 各字段独立 `Option`——不是每一行都会带全部字段（比如结尾的 `Lsize=` 汇总行没有 `frame`）。
#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct FfmpegProgress {
    stream_id: String,
    frame: Option<u64>,
    fps: Option<f64>,
    bitrate: Option<String>,
    /// 原始 `HH:MM:SS.ff` 文本。
    time: Option<String>,
    /// 上面那个时间换算成的秒数，方便调用方直接算百分比进度。
    time_secs: Option<f64>,
    /// 原始 speed 文本（已去掉末尾的 `x`，如 `"0.998"`）。
    speed: Option<String>,
}

/// 惰性编译一次的 `key=value` 提取正则；万一编译失败（理论不会——静态字面量），
/// 返回 `None` 而不是 panic，此时只是拿不到结构化 ffmpeg 进度，原始 stderr 文本仍会正常推送，
/// 不影响主功能。这是本模块避免在 lib 代码里用 `unwrap`/`expect` 的具体处理方式。
fn ffmpeg_kv_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\w+)=\s*(\S+)").ok()).as_ref()
}

/// `"HH:MM:SS.ff"` → 秒数。
fn parse_ffmpeg_time(v: &str) -> Option<f64> {
    let mut parts = v.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// 从一行 ffmpeg stderr 输出里抽取结构化进度；不含 `time=` 的行（非进度行）返回 `None`。
fn parse_ffmpeg_progress_line(stream_id: &str, line: &str) -> Option<FfmpegProgress> {
    if !line.contains("time=") {
        return None;
    }
    let re = ffmpeg_kv_regex()?;
    let mut p = FfmpegProgress {
        stream_id: stream_id.to_string(),
        ..Default::default()
    };
    for cap in re.captures_iter(line) {
        let key = &cap[1];
        let val = &cap[2];
        match key {
            "frame" => p.frame = val.parse().ok(),
            "fps" => p.fps = val.parse().ok(),
            "bitrate" => p.bitrate = Some(val.to_string()),
            "time" => {
                p.time_secs = parse_ffmpeg_time(val);
                p.time = Some(val.to_string());
            }
            "speed" => p.speed = Some(val.trim_end_matches('x').to_string()),
            _ => {}
        }
    }
    p.time.is_some().then_some(p)
}

/// 把一段增量字节流按 `\r` / `\n` 切成"行"（ffmpeg 的进度输出用 `\r` 覆写同一行，
/// 普通日志用 `\n`，两种都要能触发一次完整的"行"事件）。跨块的半截行留在 `carry` 里。
struct LineSplitter {
    carry: String,
}

impl LineSplitter {
    fn new() -> Self {
        Self { carry: String::new() }
    }

    /// 按 UTF-8 宽松解码追加新数据，吐出所有已经凑成完整的行（空行跳过）。
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.carry.push_str(&String::from_utf8_lossy(chunk));
        let mut lines = Vec::new();
        while let Some(i) = self.carry.find(['\r', '\n']) {
            let line: String = self.carry.drain(..i).collect();
            self.carry.drain(..1); // 吃掉这一个分隔符本身
            if !line.is_empty() {
                lines.push(line);
            }
        }
        lines
    }

    /// 流结束时把剩余（没有分隔符收尾的）尾巴也吐出来，不悄悄丢最后一行。
    fn finish(&mut self) -> Option<String> {
        if self.carry.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.carry))
        }
    }
}

fn spawn_runtime_reader_thread(
    app: tauri::AppHandle,
    label: String,
    stream_id: String,
    event_name: &'static str,
    mut pipe: impl Read + Send + 'static,
    parse_ffmpeg: bool,
) {
    std::thread::spawn(move || {
        let mut splitter = LineSplitter::new();
        let mut buf = [0u8; 8192];
        let emit_line = |line: &str| {
            emit_to_plugin(
                &app,
                label.as_str(),
                event_name,
                &RuntimeLineEvt {
                    stream_id: stream_id.clone(),
                    data: line.to_string(),
                },
            );
            if parse_ffmpeg {
                if let Some(p) = parse_ffmpeg_progress_line(&stream_id, line) {
                    emit_to_plugin(&app, label.as_str(), EVT_FFMPEG_PROGRESS, &p);
                }
            }
        };
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    for line in splitter.feed(&buf[..n]) {
                        emit_line(&line);
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(tail) = splitter.finish() {
            emit_line(&tail);
        }
    });
}

/// 一条运行时流式执行会话的运行期状态。
struct RuntimeStreamSession {
    owner: ActiveSession,
    child: Arc<Mutex<Child>>,
}

/// `plugin_runtime_exec_stream` 的运行期状态（managed）。
///
/// **需要主控在 `lib.rs` 里注册**：`.manage(runtime_api::RuntimeExecState::default())`，
/// 本文件按边界约束不能自己去改 `lib.rs`。
#[derive(Default)]
pub struct RuntimeExecState {
    sessions: Mutex<HashMap<String, RuntimeStreamSession>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 日志脱敏：同 `exec.rs::who`，只记"谁"，调试会话加前缀区分。
fn who(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

fn new_stream_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now_ns:x}-{n:x}")
}

fn spawn_runtime_waiter_thread(
    app: tauri::AppHandle,
    label: String,
    stream_id: String,
    child: Arc<Mutex<Child>>,
    timeout_ms: Option<u64>,
) {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut timed_out = false;
        let status = loop {
            {
                let mut guard = lock(&child);
                match guard.try_wait() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => {}
                    Err(_) => {
                        let _ = guard.kill();
                        break None;
                    }
                }
            }
            if let Some(t) = timeout_ms {
                if !timed_out && start.elapsed().as_millis() as u64 >= t {
                    timed_out = true;
                    let mut guard = lock(&child);
                    let _ = guard.kill();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        emit_to_plugin(
            &app,
            label.as_str(),
            EVT_EXIT,
            &RuntimeExitEvt {
                stream_id: stream_id.clone(),
                code,
                timed_out,
            },
        );
        if let Some(state) = app.try_state::<RuntimeExecState>() {
            let mut sessions = lock(&state.sessions);
            sessions.remove(&stream_id);
        }
    });
}

/// 启动一条流式执行会话，返回 `stream_id`；stdout/stderr 增量经 [`EVT_STDOUT`]/[`EVT_STDERR`]
/// 推给发起调用的窗口，进程结束推 [`EVT_EXIT`]。当 `name == "ffmpeg"` 时，额外解析 stderr
/// 里的进度行并经 [`EVT_FFMPEG_PROGRESS`] 推结构化进度。需已获 `runtime` 授权且已安装。
///
/// # Errors
/// 同 [`plugin_runtime_exec`]，另加：启动后拿不到子进程的 stdout/stderr 管道。
#[tauri::command]
pub fn plugin_runtime_exec_stream(
    name: String,
    args: Vec<String>,
    opts: Option<RuntimeExecOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, RuntimeExecState>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let spec = find_spec(&name)?;
    let exe = resolve_installed_exe(spec)
        .ok_or_else(|| format!("运行时 {} 尚未安装，请先调用 runtimeEnsure", spec.display_name))?;
    validate_args(&args)?;
    let opts = opts.unwrap_or_default();
    let timeout_ms = opts.timeout_ms;

    let mut cmd = new_runtime_command(&exe.to_string_lossy(), &args, true);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = opts.cwd.as_deref() {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动进程失败: {e}"))?;
    let stdout = child.stdout.take().ok_or_else(|| "拿不到子进程 stdout".to_string())?;
    let stderr = child.stderr.take().ok_or_else(|| "拿不到子进程 stderr".to_string())?;

    let stream_id = new_stream_id();
    let app = webview.app_handle().clone();
    let label = webview.label().to_string();
    let child_arc = Arc::new(Mutex::new(child));
    {
        let mut sessions = lock(&state.sessions);
        sessions.insert(
            stream_id.clone(),
            RuntimeStreamSession {
                owner: session.clone(),
                child: child_arc.clone(),
            },
        );
    }
    ilog!(
        "[iTools][plugin] runtime_exec_stream 启动: 来自 {}, runtime={name}, argc={}, stream={stream_id}",
        who(&session),
        args.len()
    );

    let parse_ffmpeg = spec.name == "ffmpeg";
    spawn_runtime_reader_thread(app.clone(), label.clone(), stream_id.clone(), EVT_STDOUT, stdout, false);
    spawn_runtime_reader_thread(app.clone(), label.clone(), stream_id.clone(), EVT_STDERR, stderr, parse_ffmpeg);
    spawn_runtime_waiter_thread(app, label, stream_id.clone(), child_arc, timeout_ms);

    Ok(stream_id)
}

fn take_owned_runtime_child(
    state: &RuntimeExecState,
    stream_id: &str,
    session: &ActiveSession,
) -> Result<Arc<Mutex<Child>>, String> {
    let sessions = lock(&state.sessions);
    match sessions.get(stream_id) {
        None => Err("执行会话不存在或已结束".to_string()),
        Some(s) if !s.owner.same_as(session) => Err("该执行会话不属于本插件".to_string()),
        Some(s) => Ok(s.child.clone()),
    }
}

/// 强制结束一条流式执行会话（`TerminateProcess`）。需已获 `runtime` 授权 + 会话归属本插件。
///
/// # Errors
/// - 未获 `runtime` 授权
/// - `stream_id` 不存在，或不属于当前调用会话
/// - 系统结束进程调用失败
#[tauri::command]
pub fn plugin_runtime_exec_kill(
    stream_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, RuntimeExecState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let child = take_owned_runtime_child(&state, &stream_id, &session)?;
    ilog!("[iTools][plugin] runtime_exec_kill: 来自 {}, stream={stream_id}", who(&session));
    let mut guard = lock(&child);
    guard.kill().map_err(|e| format!("强制结束进程失败: {e}"))
}

#[cfg(windows)]
fn quit_runtime_process(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    let pid = lock(child).id();
    win_ctrl::send_ctrl_break(pid)
}

#[cfg(not(windows))]
fn quit_runtime_process(child: &Arc<Mutex<Child>>) -> Result<(), String> {
    lock(child).kill().map_err(|e| format!("结束进程失败: {e}"))
}

/// 礼貌地请一条流式执行会话自己退出（Windows 上是 `CTRL_BREAK`，不保证程序会真的优雅处理
/// ——同 `exec.rs::plugin_exec_quit` 的边界：系统默认动作通常是终止，只是给了它一个走清理
/// 路径的机会）。需已获 `runtime` 授权 + 会话归属本插件。
///
/// # Errors
/// 同 [`plugin_runtime_exec_kill`]，另加：`AttachConsole`/`GenerateConsoleCtrlEvent` 失败
/// （常见于目标进程已经退出）。
#[tauri::command]
pub fn plugin_runtime_exec_quit(
    stream_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, RuntimeExecState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERM_RUNTIME) {
        return Err("插件未获授权使用宿主运行时（请在「插件管理」里授权 runtime）".to_string());
    }
    let child = take_owned_runtime_child(&state, &stream_id, &session)?;
    ilog!("[iTools][plugin] runtime_exec_quit: 来自 {}, stream={stream_id}", who(&session));
    quit_runtime_process(&child)
}

/// 给子进程发 `CTRL_BREAK` 请求它自行退出（`exec.rs::win_ctrl` 的等价重实现，
/// 同理由：那边是模块私有项，跨模块拿不到，这里维护一份等价的最小实现）。
#[cfg(windows)]
mod win_ctrl {
    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
        fn FreeConsole() -> i32;
        fn GenerateConsoleCtrlEvent(dw_ctrl_event: u32, dw_process_group_id: u32) -> i32;
    }

    const CTRL_BREAK_EVENT: u32 = 1;

    /// 给 `pid` 所在的进程组发 `CTRL_BREAK`。要求该进程创建时带了 `CREATE_NEW_PROCESS_GROUP`
    /// （本模块所有流式子进程都满足，见 [`super::new_runtime_command`]），
    /// 这样事件只会送到这一个进程组，不会波及宿主自身或其它进程。
    pub fn send_ctrl_break(pid: u32) -> Result<(), String> {
        // SAFETY：三个 Win32 调用都只操作"调用线程当前 attach 的控制台"这个进程级状态，
        // 不接受/返回需要手动管理生命周期的资源；pid 取自 `std::process::Child::id()`
        // 且此刻仍在会话表里，即便该进程已提前退出，`AttachConsole` 对不存在的进程
        // 也只会返回失败（0），不会有 UB。
        unsafe {
            let _ = FreeConsole();
            if AttachConsole(pid) == 0 {
                return Err(format!("附加目标进程控制台失败（pid={pid}，可能已退出）"));
            }
            let ok = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid);
            let _ = FreeConsole();
            if ok == 0 {
                return Err(format!("发送 CTRL_BREAK 失败（pid={pid}）"));
            }
        }
        Ok(())
    }
}

/// 取某个已安装运行时的可执行文件真实路径。
///
/// **只给宿主内部用**（`record_av` 要把帧喂给 ffmpeg 的 stdin），不经任何命令暴露给插件——
/// 插件拿到裸路径就等于绕开了 `plugin_runtime_exec` 的参数校验去跑任意东西，
/// 托管运行时「插件拿不到真实路径」这条约束会当场作废。
pub(crate) fn resolved_exe_path(name: &str) -> Option<std::path::PathBuf> {
    let spec = find_spec(name).ok()?;
    resolve_installed_exe(spec)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 清单三项都必须带可信 sha256，且形态合法。
    ///
    /// 这条是**安全闸门**：正因为每一项都有本机实算的哈希，才敢让运行时下载走第三方镜像
    /// （镜像最多能让下载失败，改不动内容）。哪天有人加了一项却没填哈希，
    /// 它既装不上（`plugin_runtime_ensure` 会拒绝），也会让「镜像可用」这个前提失效——
    /// 所以在这里拦住，而不是等到用户点安装才发现。
    #[test]
    fn every_runtime_has_trusted_hash_and_https_url() {
        assert!(!RUNTIME_MANIFEST.is_empty());
        for spec in RUNTIME_MANIFEST {
            let hex = spec
                .sha256
                .unwrap_or_else(|| panic!("运行时 {} 没有 sha256：走镜像的安全前提就不成立了", spec.name));
            assert_eq!(hex.len(), 64, "运行时 {} 的 sha256 长度不对", spec.name);
            assert!(
                hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "运行时 {} 的 sha256 应为小写十六进制",
                spec.name
            );
            assert!(
                spec.download_url.starts_with("https://"),
                "运行时 {} 的下载地址必须是 https",
                spec.name
            );
        }
    }
}
