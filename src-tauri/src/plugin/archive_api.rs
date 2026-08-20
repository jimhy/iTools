//! 基于「用户授权 scope」的三件套：压缩/解压、目录变化监听（真事件驱动）、系统文件图标。
//!
//! # 与 `fs_api.rs` 的关系（安全核心，务必先读）
//!
//! 本模块**不自己发明**「哪些路径插件能碰」这套判断——那是 `fs_api.rs` 已经做过、经过仔细
//! 推敲（TOCTOU 重新校验、iTools 数据根硬黑名单、scope 归属校验）的安全核心，两份实现互相
//! 独立维护是安全漏洞的经典来源。所以这里全程调用 `fs_api::resolve_scope` /
//! `fs_api::resolve_existing` / `fs_api::resolve_for_write` / `fs_api::scope_relative` /
//! `fs_api::reject_itools_data_root` 这几个函数——它们目前是 `fs_api.rs` 内部私有的，
//! 集成时需要主控把它们改成 `pub(crate)`（本文件按任务边界不允许直接改 `fs_api.rs`）。
//!
//! 权限门禁复用同一个 `fs-user-scope`（不新造权限），字符串常量在两个文件里各写一份字面量
//! ——这不是逻辑重复，只是同一个字符串出现两处，不违反「校验逻辑只有一份」的原则。
//!
//! # 三个子系统
//!
//! 1. **压缩/解压**（`zip` crate，仅 deflate）：解压是本模块唯一自己写路径校验的地方，因为
//!    zip 归档条目名是**字符串**，不是 OS 路径，校验规则与 `fs_api::scope_relative`（校验
//!    `Path` 的 `Component`）天然不同源，不能复用；照抄了 `install.rs::safe_entry_path` 已经
//!    验证过的思路（拒绝绝对路径/盘符/`..`/NUL/结尾空格或点），并加了条目数与解压总量上限
//!    防 zip 炸弹。
//! 2. **目录变化监听**：`watch.rs` 那套 1.5 秒轮询指纹是为「很小的插件目录」设计的，量级放到
//!    用户任选的任意目录（可能几万文件）上会不断做全量遍历，是不可接受的开销。这里改用
//!    Win32 `ReadDirectoryChangesW`（重叠 IO + 事件对象），是**真正的事件驱动**：读线程整段
//!    时间阻塞在 `WaitForMultipleObjects` 上等内核通知，不做任何轮询式的重新扫描/重新 stat；
//!    停止监听走的也是给内核发信号（`SetEvent` 唤醒等待、`CancelIoEx` 取消挂起 IO），不是
//!    "设个标志位下次轮询再看"。变化事件按路径做 300ms 尾部去抖（debounce），同一文件短时间
//!    内的多次写入只会推一条。
//! 3. **系统文件图标**：`SHGetFileInfoW` 拿 `HICON` → `GetIconInfo`/`GetDIBits` 转 32bpp
//!    BGRA → 换通道成 RGBA → PNG。这条链路项目里 `search/icon.rs` 已经踩过坑并跑通
//!    （`windows_sys` 而非 `windows` crate、AND 掩码兜底老式无 alpha 图标），本文件按同样的
//!    调用方式重新实现一遍（不能跨模块 `use` 私有函数，且它是为搜索索引场景写的、不吃
//!    scope 校验），没有走没验证过的新路子。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::fs_api;
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 权限标识，与 `fs_api.rs` 的同名常量是**同一个字符串的两份字面量**（见模块文档），
/// 不是两套独立的权限体系。
const PERMISSION: &str = "fs-user-scope";

/// 打包时单个归档的条目数上限（纯资源保护，打包的是用户自己的数据，不存在"归档说谎"的风险，
/// 只防止一次选中海量文件把归档索引撑得过大）。
const MAX_ZIP_CREATE_ENTRIES: u64 = 200_000;

/// 解压时归档条目数上限——防"百万小文件"型 zip 炸弹。
const MAX_UNZIP_ENTRIES: usize = 200_000;
/// 解压后总体积上限（字节）——防高压缩比 zip 炸弹（一个几十 KB 的归档解出几十 GB）。
const MAX_UNZIP_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// 单文件解压体积上限（字节），在总量上限之内再加一道更早触发的防线。
const MAX_UNZIP_SINGLE_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 同一插件同时开启的目录监听上限（防止插件不清理导致句柄/线程堆积）。
const MAX_WATCHES_PER_SESSION: usize = 20;
/// 变化事件去抖窗口：同一路径在这段时间内的多次变化只在"安静下来"后推一条。
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);
/// 去抖线程检查一次到期事件的节拍。
const DEBOUNCE_TICK: Duration = Duration::from_millis(120);

// ==================== 数据结构 ====================

/// [`plugin_zip_create`] 的返回摘要。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipCreateSummary {
    /// 实际写入归档的文件条目数（不含目录条目）。
    pub entry_count: u64,
    /// 生成的归档文件最终大小（字节）。
    pub archive_bytes: u64,
}

/// [`plugin_unzip`] 的返回摘要。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnzipSummary {
    /// 实际解出的文件数（不含目录）。
    pub extracted_files: u64,
    /// 实际解出的总字节数。
    pub extracted_bytes: u64,
}

/// 推给插件页的目录变化事件（经 `window.__itoolsEmit` 广播，见模块文档）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FsWatchEvent {
    watch_id: String,
    /// "created" | "modified" | "removed" | "renamed" | "error"
    kind: String,
    /// 变化文件/目录的绝对路径；`kind == "error"` 时可能为空字符串。
    path: String,
    /// 仅 `kind == "renamed"` 时有值：重命名前的路径。
    old_path: Option<String>,
    /// 仅 `kind == "error"` 时有值：监听为何终止（此后该 `watch_id` 不会再有事件）。
    message: Option<String>,
}

// ==================== 命令：压缩 ====================

/// 把 scope 内若干文件/目录打成一个 zip 归档（仅 deflate 压缩），写到 scope 内 `out_path`。
///
/// `entries` 的每一项都是 scope 内的相对路径；目录会被递归打包，归档内以该相对路径本身作为
/// 顶层条目名（即"打包一个文件夹"的直觉：归档里能看到这个文件夹本身，而不是它的内容被摊平
/// 到根）。`out_path` 所在目录必须已存在（与 `plugin_fs_write` 同样的"不代插件建目录树"取舍，
/// 见 `fs_api.rs` 模块文档）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `entries` 为空、或其中任一项路径校验失败/不存在（越权写法、越出 scope、命中 iTools 数据
///   根黑名单——判断逻辑见 `fs_api::resolve_existing`）
/// - `out_path` 校验失败（同上、或所在目录不存在——见 `fs_api::resolve_for_write`）
/// - 条目数超过 [`MAX_ZIP_CREATE_ENTRIES`]
/// - 创建归档文件、写入归档内容失败（磁盘 IO 错误）
#[tauri::command]
pub async fn plugin_zip_create(
    scope_id: String,
    entries: Vec<String>,
    out_path: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<ZipCreateSummary, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    if entries.is_empty() {
        return Err("entries 不能为空".to_string());
    }
    let scope = fs_api::resolve_scope(&session, &registry, &scope_id)?;
    let mut resolved: Vec<(String, PathBuf)> = Vec::with_capacity(entries.len());
    for e in &entries {
        // resolve_existing 已经做了 fs_api 那套完整校验（越权写法/越出 scope/数据根黑名单/存在性）。
        let p = fs_api::resolve_existing(&scope, e)?;
        resolved.push((normalize_zip_root_name(e), p));
    }
    let out = fs_api::resolve_for_write(&scope, &out_path)?;
    tauri::async_runtime::spawn_blocking(move || zip_create_blocking(resolved, out))
        .await
        .map_err(|e| format!("打包任务失败: {e}"))?
}

/// 把一个 scope 内相对路径规整成 zip 条目名写法：反斜杠归一成 `/`，去掉多余的 `.`/空段。
/// 输入已经过 `fs_api::scope_relative` 校验（不含 `..`/绝对路径），这里只是纯粹的展示规整。
fn normalize_zip_root_name(sub: &str) -> String {
    let unified = sub.replace('\\', "/");
    unified
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn zip_create_blocking(entries: Vec<(String, PathBuf)>, out: PathBuf) -> Result<ZipCreateSummary, String> {
    let file = std::fs::File::create(&out).map_err(|e| format!("创建归档文件失败: {e}"))?;
    let mut writer = zip::ZipWriter::new(file);
    let options: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut entry_count: u64 = 0;
    for (root_name, path) in entries {
        if root_name.is_empty() {
            return Err("归档条目名不能为空".to_string());
        }
        if path.is_dir() {
            add_dir_to_zip(&mut writer, &path, &root_name, options, &mut entry_count)?;
        } else {
            if entry_count >= MAX_ZIP_CREATE_ENTRIES {
                return Err(format!("归档条目数超过上限 {MAX_ZIP_CREATE_ENTRIES}"));
            }
            writer
                .start_file(root_name.clone(), options)
                .map_err(|e| format!("写入归档条目失败 {root_name}: {e}"))?;
            let mut f = std::fs::File::open(&path).map_err(|e| format!("打开文件失败 {}: {e}", path.display()))?;
            std::io::copy(&mut f, &mut writer).map_err(|e| format!("写入归档内容失败 {root_name}: {e}"))?;
            entry_count += 1;
        }
    }
    let file = writer.finish().map_err(|e| format!("完成归档失败: {e}"))?;
    let archive_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    Ok(ZipCreateSummary { entry_count, archive_bytes })
}

/// 把一个目录连同其自身名字（`root_name`）递归写入归档；空目录也会写一条目录条目
/// （保留归档解压后的空目录结构，不是所有解压工具都会为"没有任何文件的目录"自动补建）。
fn add_dir_to_zip<W: std::io::Write + std::io::Seek>(
    writer: &mut zip::ZipWriter<W>,
    dir: &Path,
    root_name: &str,
    options: zip::write::SimpleFileOptions,
    entry_count: &mut u64,
) -> Result<(), String> {
    writer
        .add_directory(root_name.to_string(), options)
        .map_err(|e| format!("写入目录条目失败 {root_name}: {e}"))?;
    for entry in walkdir::WalkDir::new(dir).into_iter() {
        let entry = entry.map_err(|e| format!("遍历目录失败: {e}"))?;
        if entry.path() == dir {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dir)
            .map_err(|_| "计算相对路径失败".to_string())?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let zip_name = format!("{root_name}/{rel_str}");
        if entry.file_type().is_dir() {
            writer
                .add_directory(zip_name.clone(), options)
                .map_err(|e| format!("写入目录条目失败 {zip_name}: {e}"))?;
        } else if entry.file_type().is_file() {
            if *entry_count >= MAX_ZIP_CREATE_ENTRIES {
                return Err(format!("归档条目数超过上限 {MAX_ZIP_CREATE_ENTRIES}"));
            }
            writer
                .start_file(zip_name.clone(), options)
                .map_err(|e| format!("写入归档条目失败 {zip_name}: {e}"))?;
            let mut f = std::fs::File::open(entry.path())
                .map_err(|e| format!("打开文件失败 {}: {e}", entry.path().display()))?;
            std::io::copy(&mut f, writer).map_err(|e| format!("写入归档内容失败 {zip_name}: {e}"))?;
            *entry_count += 1;
        }
        // 符号链接等其它类型（既非目录也非普通文件）安全跳过，不纳入归档。
    }
    Ok(())
}

// ==================== 命令：解压 ====================

/// 把 scope 内 `zip_path` 处的归档解压到 scope 内 `out_sub` 目录（不存在则创建，见
/// [`resolve_extract_dir`] 的取舍说明）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `zip_path` 校验失败/不存在（同 `plugin_zip_create` 的 entries 校验）
/// - `out_sub` 校验失败、或越出 scope 范围、或命中 iTools 数据根黑名单
/// - 归档不是合法 zip、条目数超过 [`MAX_UNZIP_ENTRIES`]
/// - 任一条目名试图越权（绝对路径/盘符/`..`/NUL/结尾空格或点，见 [`safe_zip_entry_path`]）
///   或是符号链接条目（一律拒绝：解出来在 Windows 上可能是指向任意位置的重解析点）
/// - 单文件或解压总体积超过上限（疑似 zip 炸弹）
/// - 写盘失败
#[tauri::command]
pub async fn plugin_unzip(
    scope_id: String,
    zip_path: String,
    out_sub: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<UnzipSummary, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let scope = fs_api::resolve_scope(&session, &registry, &scope_id)?;
    let zip_file_path = fs_api::resolve_existing(&scope, &zip_path)?;
    let out_dir = resolve_extract_dir(&scope, &out_sub)?;
    tauri::async_runtime::spawn_blocking(move || unzip_blocking(zip_file_path, out_dir))
        .await
        .map_err(|e| format!("解压任务失败: {e}"))?
}

/// 解析解压目标目录：与 `fs_api::resolve_for_write` 的"不代插件建目录树"刻意不同——解压
/// 这个操作的语义本来就是"把归档内容摊开到这个目录"，用户点"解压到 X"天然预期 X 被建出来。
/// 复用的仍是 `fs_api` 的两个安全原语（`scope_relative` 校验写法、`reject_itools_data_root`
/// 过黑名单），只是组装方式为"允许目标不存在、创建后再校验"，不是又发明一套边界判断。
fn resolve_extract_dir(scope: &fs_api::FsScope, sub: &str) -> Result<PathBuf, String> {
    if scope.kind == "file" {
        return Err("该 scope 是单个文件，不能作为解压目标目录".to_string());
    }
    if sub.is_empty() {
        return Err("必须指定 scope 内的解压目标目录".to_string());
    }
    let rel = fs_api::scope_relative(sub)?;
    let base = PathBuf::from(&scope.path);
    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("scope 已失效（原目录不可访问）: {e}"))?;
    fs_api::reject_itools_data_root(&canon_base)?;
    let joined = base.join(rel);
    std::fs::create_dir_all(&joined).map_err(|e| format!("创建解压目标目录失败: {e}"))?;
    let canon = joined
        .canonicalize()
        .map_err(|e| format!("解析解压目标目录失败: {e}"))?;
    if canon != canon_base && !canon.starts_with(&canon_base) {
        return Err("解压目标越出 scope 范围".to_string());
    }
    fs_api::reject_itools_data_root(&canon)?;
    Ok(canon)
}

fn unzip_blocking(zip_path: PathBuf, out_dir: PathBuf) -> Result<UnzipSummary, String> {
    let file = std::fs::File::open(&zip_path).map_err(|e| format!("打开归档失败: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("归档不是有效的 zip: {e}"))?;
    if zip.len() > MAX_UNZIP_ENTRIES {
        return Err(format!(
            "归档条目数 {} 超过上限 {MAX_UNZIP_ENTRIES}，已中止（疑似 zip 炸弹）",
            zip.len()
        ));
    }
    let mut extracted_files: u64 = 0;
    let mut extracted_bytes: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("读取归档条目失败: {e}"))?;
        let raw_name = f.name().to_string();
        if let Some(mode) = f.unix_mode() {
            // 0xA000 = S_IFLNK：符号链接条目，解出来在 Windows 上可能变成指向任意位置的重解析点。
            if mode & 0xF000 == 0xA000 {
                return Err(format!("归档含符号链接条目（不安全）：{raw_name}"));
            }
        }
        let rel = safe_zip_entry_path(&raw_name)?;
        let target = out_dir.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| format!("创建目录失败 {}: {e}", target.display()))?;
            continue;
        }
        if f.size() > MAX_UNZIP_SINGLE_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 声明大小超过单文件上限 {} GB，已中止（疑似 zip 炸弹）",
                MAX_UNZIP_SINGLE_FILE_BYTES / 1024 / 1024 / 1024
            ));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
            // 落点复核（Zip Slip 第二道闸）：父目录 canonicalize 后必须仍在解压根内，
            // 挡住"预先存在的软链接/重解析点"把写入引向根外的情况（同 install.rs 的思路）。
            let cp = parent.canonicalize().map_err(|e| format!("解析落点失败: {e}"))?;
            if !cp.starts_with(&out_dir) {
                return Err(format!("归档条目落点越界（不安全）：{raw_name}"));
            }
        }
        let mut w =
            std::fs::File::create(&target).map_err(|e| format!("写入文件失败 {}: {e}", target.display()))?;
        let remaining_budget = MAX_UNZIP_TOTAL_BYTES.saturating_sub(extracted_bytes);
        let cap = remaining_budget.min(MAX_UNZIP_SINGLE_FILE_BYTES).saturating_add(1);
        // 用完全限定调用而不是 `f.by_ref()`：ZipFile 自身没有 by_ref 方法，
        // 那是 Read trait 提供的，要求 trait 在作用域内。这里 `&mut ZipFile` 本身实现了
        // Read 且是 Sized，直接对它取 take 最省事，也不用为一处调用引入 trait 导入。
        let mut limited = std::io::Read::take(&mut f, cap);
        let written = std::io::copy(&mut limited, &mut w)
            .map_err(|e| format!("写入文件失败 {}: {e}", target.display()))?;
        extracted_bytes += written;
        extracted_files += 1;
        if extracted_bytes > MAX_UNZIP_TOTAL_BYTES {
            return Err(format!(
                "解压总体积超过上限 {} GB，已中止（疑似 zip 炸弹）",
                MAX_UNZIP_TOTAL_BYTES / 1024 / 1024 / 1024
            ));
        }
    }
    Ok(UnzipSummary { extracted_files, extracted_bytes })
}

/// zip 条目名（**字符串**，不是 `Path`）→ scope 内相对路径。这是 Zip Slip 的第一道闸：
/// 拒绝绝对路径、盘符、`..` 段、NUL；把反斜杠也当分隔符处理（Windows 上 `a\..\b` 同样能穿越，
/// 且哪怕不处理反斜杠，后续 `PathBuf::join` 在 Windows 上也会把它解释成路径分隔符——不能只按
/// `/` 分割就假装它是"字面字符"）。逻辑照抄自 `install.rs::safe_entry_path`（已验证有效），
/// 因为两处消费的都是 zip 条目名、校验对象是字符串而非 `Path`，与 `fs_api::scope_relative`
/// 不同源，不构成"同一份安全校验拆成两份"。
fn safe_zip_entry_path(name: &str) -> Result<PathBuf, String> {
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
        if seg.ends_with(' ') || seg.ends_with('.') {
            // Windows 会静默剥掉结尾的空格与点，落点与校验不一致，直接拒绝。
            return Err(format!("归档条目名以空格或点结尾（不安全）：{name}"));
        }
        out.push(seg);
        count += 1;
    }
    if count == 0 {
        return Err(format!("归档条目路径为空：{name}"));
    }
    Ok(out)
}

// ==================== 命令：目录变化监听 ====================

/// 开始监听 scope 内某个目录（或某个文件 scope 所在的那一个文件）的变化，返回 `watch_id`。
/// 目录变化经 `plugin-fs-watch` 事件推给插件页（见模块文档，事件字段见 [`FsWatchEvent`]）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - 当前插件已开启的监听数达到上限（[`MAX_WATCHES_PER_SESSION`]）
/// - `scope_id`/`sub_path` 校验失败（同 `plugin_fs_list` 的路径校验）、目标不是目录
/// - 打开目录句柄失败（如目录已被删除/无权限）
/// - 非 Windows 平台（本能力目前仅支持 Windows）
#[tauri::command]
pub fn plugin_fs_watch_start(
    scope_id: String,
    sub_path: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    if watch_count_for(&session) >= MAX_WATCHES_PER_SESSION {
        return Err(format!(
            "同一插件最多同时开 {MAX_WATCHES_PER_SESSION} 个目录监听，请先停掉不用的"
        ));
    }
    let scope = fs_api::resolve_scope(&session, &registry, &scope_id)?;
    let sub = sub_path.unwrap_or_default();
    let (watch_dir, filter_name, recursive) = if scope.kind == "file" {
        if !sub.is_empty() {
            return Err("该 scope 是单个文件，sub_path 需留空".to_string());
        }
        let file_path = PathBuf::from(&scope.path);
        let parent = file_path
            .parent()
            .ok_or_else(|| "无法定位文件所在目录".to_string())?
            .to_path_buf();
        let name = file_path.file_name().map(|n| n.to_string_lossy().into_owned());
        (parent, name, false)
    } else {
        let dir = fs_api::resolve_existing(&scope, &sub)?;
        if !dir.is_dir() {
            return Err("监听目标不是文件夹".to_string());
        }
        (dir, None, true)
    };
    watch_start_dispatch(watch_dir, filter_name, recursive, session, &webview)
}

/// 平台分发：真正实现是 [`win_watch::start`]（`cfg(windows)`）；非 Windows 上如实报错，不假装
/// 监听已经开始。用两份独立的 item 级 `cfg` 函数定义（而不是函数体内 `cfg` 到块表达式），
/// 是照抄本文件已有先例 `ocr.rs::ocr_png` 的写法——块表达式级 `cfg` 在栈式尾表达式位置的
/// 语义边界不够清晰，没必要冒这个险。
#[cfg(windows)]
fn watch_start_dispatch(
    watch_dir: PathBuf,
    filter_name: Option<String>,
    recursive: bool,
    session: ActiveSession,
    webview: &tauri::Webview,
) -> Result<String, String> {
    win_watch::start(watch_dir, filter_name, recursive, session, webview)
}

#[cfg(not(windows))]
fn watch_start_dispatch(
    watch_dir: PathBuf,
    filter_name: Option<String>,
    recursive: bool,
    session: ActiveSession,
    _webview: &tauri::Webview,
) -> Result<String, String> {
    let _ = (watch_dir, filter_name, recursive, session);
    Err("目录变化监听仅在 Windows 上可用".to_string())
}

/// 停止一个之前开启的目录监听。不存在/不属于当前插件都返回同一句模糊错误（不给插件提供
/// 枚举其它插件 watch_id 是否存在的探测手段，思路同 `fs_api::resolve_scope`）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `watch_id` 未知/不属于当前会话
#[tauri::command]
pub fn plugin_fs_watch_stop(
    watch_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    watch_stop_dispatch(&watch_id, &session)
}

#[cfg(windows)]
fn watch_stop_dispatch(watch_id: &str, session: &ActiveSession) -> Result<(), String> {
    win_watch::stop(watch_id, session)
}

#[cfg(not(windows))]
fn watch_stop_dispatch(watch_id: &str, session: &ActiveSession) -> Result<(), String> {
    let _ = (watch_id, session);
    Err("目录变化监听仅在 Windows 上可用".to_string())
}

/// 当前会话已开启的监听数（跨平台可编译，非 Windows 上恒为 0）。
#[cfg(windows)]
fn watch_count_for(session: &ActiveSession) -> usize {
    win_watch::count_for(session)
}

#[cfg(not(windows))]
fn watch_count_for(session: &ActiveSession) -> usize {
    let _ = session;
    0
}

/// Win32 `ReadDirectoryChangesW` 事件驱动监听的完整实现。整段 `cfg(windows)`：`windows` crate
/// 的 Win32 绑定本身就只在 Windows target 上编译（见 `Cargo.toml` 的
/// `[target.'cfg(windows)'.dependencies]`），非 Windows 下这里的类型根本不存在。
#[cfg(windows)]
mod win_watch {
    use super::*;
    use tauri::Manager as _;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

    /// 一次 `ReadDirectoryChangesW` 读缓冲的大小。64KB 是文档记载的"网络驱动器"安全上限
    /// （本地盘可以更大，但没必要——目录事件都很小，选一个通吃本地/网络盘的值更省心）。
    const READ_BUF_BYTES: usize = 64 * 1024;

    /// 包一层让 `HANDLE`（本质是个不透明的内核对象引用/整数句柄）可以跨线程传递。
    ///
    /// # Safety
    /// 持有一个 `HANDLE` 的值、把它拷贝到另一个线程、再用它发起 Win32 调用，是 Windows 官方
    /// 支持的正常用法（`ReadDirectoryChangesW` 打开的目录句柄本就设计成可以在任意线程使用）。
    /// 这里只是满足 Rust 的 `Send` 检查，真正需要注意的是不让两个线程同时对同一个 `HANDLE`
    /// 发起互斥操作——本模块的用法上，每个句柄的生命周期都由唯一一个"读线程"独占，其余线程
    /// 只通过 `SetEvent`/`CancelIoEx` 这类专为跨线程通知设计的 API 与之交互。
    struct SendHandle(HANDLE);
    // SAFETY: 见上文文档；HANDLE 内部就是一个可安全拷贝的整数/指针值，不引用任何线程本地状态。
    unsafe impl Send for SendHandle {}

    /// 一条监听的运行期记录：谁开的（用于 stop 时的归属校验）+ 停止事件（stop 命令通过它
    /// 唤醒读线程的 `WaitForMultipleObjects`，不是设标志位等下一轮轮询）。
    struct WatchEntry {
        owner: ActiveSession,
        stop_event: SendHandle,
    }

    static WATCHES: OnceLock<StdMutex<HashMap<String, WatchEntry>>> = OnceLock::new();

    fn watches() -> &'static StdMutex<HashMap<String, WatchEntry>> {
        WATCHES.get_or_init(|| StdMutex::new(HashMap::new()))
    }

    /// 拿全局监听表的锁；中毒（poison）后直接取回内部 guard 继续用——非核心辅助锁不该因为一次
    /// 无关 panic 让所有插件的监听操作跟着崩，思路同 `fs_api::scope_lock`。
    fn lock_watches() -> std::sync::MutexGuard<'static, HashMap<String, WatchEntry>> {
        match watches().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(super) fn count_for(session: &ActiveSession) -> usize {
        lock_watches().values().filter(|e| e.owner.same_as(session)).count()
    }

    pub(super) fn start(
        watch_dir: PathBuf,
        filter_name: Option<String>,
        recursive: bool,
        session: ActiveSession,
        webview: &tauri::Webview,
    ) -> Result<String, String> {
        let dir_handle = open_dir_handle(&watch_dir)?;
        // SAFETY: CreateEventW 的三个 None/false 参数都是文档记载的合法默认写法
        // （匿名、手动重置、初始未触发）；调用不写任何调用方内存，失败时返回 Err。
        let stop_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| format!("创建停止信号失败: {e}"))?;

        let watch_id = new_watch_id();
        let app = webview.app_handle().clone();
        let label = webview.label().to_string();

        lock_watches().insert(
            watch_id.clone(),
            WatchEntry { owner: session, stop_event: SendHandle(stop_event) },
        );

        let (tx, rx) = std::sync::mpsc::channel::<RawEvent>();
        let reader_dir_handle = SendHandle(dir_handle);
        let reader_stop_event = SendHandle(stop_event);
        let reader_watch_dir = watch_dir.clone();
        let reader_id = watch_id.clone();
        if std::thread::Builder::new()
            .name(format!("fs-watch-{reader_id}"))
            .spawn(move || {
                reader_loop(reader_dir_handle, reader_stop_event, reader_watch_dir, filter_name, recursive, tx);
            })
            .is_err()
        {
            // 建线程失败（进程资源即将耗尽的极端情况）：清理已分配的句柄与登记，如实报错，
            // 不让调用方以为监听已经在跑。
            // SAFETY: dir_handle/stop_event 都是本函数刚成功创建、尚未被任何其它代码路径持有的句柄。
            unsafe {
                let _ = CloseHandle(dir_handle);
                let _ = CloseHandle(stop_event);
            }
            lock_watches().remove(&watch_id);
            return Err("创建监听线程失败（系统资源不足）".to_string());
        }

        let debounce_id = watch_id.clone();
        if std::thread::Builder::new()
            .name(format!("fs-watch-debounce-{debounce_id}"))
            .spawn(move || debounce_loop(debounce_id, rx, app, label))
            .is_err() {
            ilog!("[iTools] 文件监听 {watch_id} 的去抖线程创建失败，事件将无法送达（读线程仍会正常停止）");
        }

        Ok(watch_id)
    }

    pub(super) fn stop(watch_id: &str, session: &ActiveSession) -> Result<(), String> {
        let guard = lock_watches();
        let Some(entry) = guard.get(watch_id) else {
            return Err("未知的监听（可能已停止）".to_string());
        };
        if !entry.owner.same_as(session) {
            return Err("未知的监听（可能已停止）".to_string());
        }
        // SAFETY: stop_event 是本模块专为该 watch 创建的手动重置事件，SetEvent 幂等、线程安全，
        // 唤醒读线程的 WaitForMultipleObjects 后由读线程自己 CancelIoEx + CloseHandle 收尾。
        unsafe {
            let _ = SetEvent(entry.stop_event.0);
        }
        Ok(())
        // 不在这里从表中移除：真正的清理由去抖线程在收尾时做（它是最后退出的那个线程），
        // 避免和"读线程异常退出时也要清理"这条路径竞态删同一项两次。
    }

    fn open_dir_handle(dir: &Path) -> Result<HANDLE, String> {
        let wide = windows::core::HSTRING::from(dir.to_string_lossy().as_ref());
        // SAFETY: wide 是调用期间存活的合法宽字符串；FILE_FLAG_BACKUP_SEMANTICS 是打开"目录"
        // 句柄（而非普通文件）的官方要求写法；FILE_FLAG_OVERLAPPED 使后续 ReadDirectoryChangesW
        // 可以用重叠 IO + 事件对象等待，不阻塞发起调用的线程。
        unsafe {
            CreateFileW(
                &wide,
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|e| format!("打开目录句柄失败: {e}"))
    }

    fn new_watch_id() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("watch-{n}-{nanos:x}")
    }

    /// 读线程内部传给去抖线程的原始事件（尚未去抖）。
    enum RawEvent {
        Change { kind: &'static str, path: PathBuf },
        Rename { old: PathBuf, new: PathBuf },
        /// 监听因不可恢复的错误终止（如目录被删除/卷被拔出）。
        Error(String),
    }

    /// 读线程主循环：真·事件驱动——整段时间阻塞在 `WaitForMultipleObjects` 上等内核通知
    /// 或停止信号，不做任何"下一轮再看看"式的轮询。收到停止信号立即 `CancelIoEx` 取消挂起的
    /// IO 并退出；`tx` 随本函数返回被丢弃，去抖线程据此（`Receiver` 断开）收尾。
    fn reader_loop(
        dir_handle: SendHandle,
        stop_event: SendHandle,
        watch_dir: PathBuf,
        filter_name: Option<String>,
        recursive: bool,
        tx: std::sync::mpsc::Sender<RawEvent>,
    ) {
        let handle = dir_handle.0;
        let stop = stop_event.0;
        let mut buf = vec![0u32; READ_BUF_BYTES / 4]; // u32 数组保证 4 字节对齐

        loop {
            // SAFETY: 三个 None/false 参数同 open 时的 stop_event，是合法的匿名手动重置事件写法。
            let io_event = match unsafe { CreateEventW(None, true, false, PCWSTR::null()) } {
                Ok(h) => h,
                Err(e) => {
                    let _ = tx.send(RawEvent::Error(format!("创建 IO 完成事件失败: {e}")));
                    break;
                }
            };
            let mut overlapped = OVERLAPPED {
                hEvent: io_event,
                ..Default::default()
            };
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_CREATION;
            let buf_bytes = (buf.len() * 4) as u32;
            // SAFETY: buf 在本轮循环内、直到下面 GetOverlappedResult 完成前都保持存活且不被移动；
            // handle 是本线程独占持有、仍然打开的目录句柄；overlapped.hEvent 已设为本轮的 io_event。
            let issue = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf_bytes,
                    recursive,
                    filter,
                    None,
                    Some(&mut overlapped as *mut OVERLAPPED),
                    None,
                )
            };
            if let Err(e) = issue {
                let _ = tx.send(RawEvent::Error(format!("目录监听已终止: {e}")));
                // SAFETY: io_event 是本轮刚创建、还未被其它代码路径持有的句柄。
                unsafe {
                    let _ = CloseHandle(io_event);
                }
                break;
            }

            // SAFETY: io_event/stop 都是仍然有效的事件句柄；u32::MAX（INFINITE）表示无限等待，
            // 这正是"事件驱动、不轮询"的关键——线程在这里真正阻塞在内核里，直到两者之一被触发。
            let wait = unsafe { WaitForMultipleObjects(&[io_event, stop], false, u32::MAX) };
            let idx = wait.0.wrapping_sub(WAIT_OBJECT_0.0);

            if idx == 1 {
                // 收到停止信号：取消这次挂起的 IO 再退出，不留悬空的内核 IO 请求。
                // SAFETY: handle 仍然有效；overlapped 与本次已发起的 IO 是同一个。
                unsafe {
                    let _ = CancelIoEx(handle, Some(&overlapped as *const OVERLAPPED));
                    let _ = CloseHandle(io_event);
                }
                break;
            }
            if idx != 0 {
                // WAIT_FAILED 等异常：不硬撑，如实终止并通知。
                let _ = tx.send(RawEvent::Error("等待目录事件失败".to_string()));
                unsafe {
                    let _ = CloseHandle(io_event);
                }
                break;
            }

            let mut bytes: u32 = 0;
            // SAFETY: handle/overlapped 对应刚刚完成（或即将完成）的那次 IO；bwait=false 是因为
            // 上面 WaitForMultipleObjects 已经等到了完成信号，这里只是取结果，不会再阻塞。
            let got =
                unsafe { GetOverlappedResult(handle, &overlapped as *const OVERLAPPED, &mut bytes, false) };
            // SAFETY: io_event 是本轮的事件句柄，此后不再使用。
            unsafe {
                let _ = CloseHandle(io_event);
            }
            match got {
                Ok(()) if bytes > 0 => {
                    parse_and_send(&buf, watch_dir.as_path(), filter_name.as_deref(), &tx);
                }
                Ok(()) => {
                    // bytes == 0：变化太多把缓冲区冲爆了（罕见），没法知道具体是哪些文件——
                    // 用一条 "modified" 兜底通知监听根目录本身，让插件知道"有变化但细节丢了"。
                    let _ = tx.send(RawEvent::Change { kind: "modified", path: watch_dir.clone() });
                }
                Err(e) => {
                    let _ = tx.send(RawEvent::Error(format!("目录已不可访问，监听终止: {e}")));
                    break;
                }
            }
        }
        // SAFETY: handle 从此不再被任何代码路径使用（读线程是它唯一的持有者）。
        unsafe {
            let _ = CloseHandle(handle);
        }
    }

    /// 解析一批 `FILE_NOTIFY_INFORMATION` 记录并逐条发给去抖线程；`filter_name` 非空时只放行
    /// 顶层文件名等于它的记录（用于"文件 scope 监听所在目录、但只关心那一个文件"的场景）。
    fn parse_and_send(buf: &[u32], watch_dir: &Path, filter_name: Option<&str>, tx: &std::sync::mpsc::Sender<RawEvent>) {
        let base = buf.as_ptr() as *const u8;
        let mut offset: usize = 0;
        let mut pending_rename_old: Option<PathBuf> = None;
        loop {
            // SAFETY: base 指向的缓冲区由本次 ReadDirectoryChangesW 成功写入；offset 每一步都
            // 严格由上一条记录自带的 NextEntryOffset 推进，Windows 保证它不会越过实际写入的范围
            // （定长头 + 变长文件名的紧凑序列，NextEntryOffset==0 表示最后一条）。
            let rec_ptr = unsafe { base.add(offset) as *const FILE_NOTIFY_INFORMATION };
            // SAFETY: 三个字段都在 FILE_NOTIFY_INFORMATION 定长头部内，rec_ptr 指向的内存有效。
            let next_offset = unsafe { std::ptr::addr_of!((*rec_ptr).NextEntryOffset).read_unaligned() };
            let action = unsafe { std::ptr::addr_of!((*rec_ptr).Action).read_unaligned() };
            let name_len_bytes = unsafe { std::ptr::addr_of!((*rec_ptr).FileNameLength).read_unaligned() } as usize;
            let name_ptr = unsafe { std::ptr::addr_of!((*rec_ptr).FileName) as *const u16 };

            let unit_count = name_len_bytes / 2;
            let mut units = Vec::with_capacity(unit_count);
            for i in 0..unit_count {
                // SAFETY: name_ptr 指向紧跟在定长头部之后的 UTF-16 文件名，FileNameLength 是
                // Windows 填的、按字节计的真实长度，逐个 u16 顺序读取不会越过本条记录的范围。
                units.push(unsafe { name_ptr.add(i).read_unaligned() });
            }
            let rel = String::from_utf16_lossy(&units);
            let pass = match filter_name {
                Some(f) => rel.eq_ignore_ascii_case(f),
                None => true,
            };
            let full = watch_dir.join(rel.replace('\\', "/"));

            if pass {
                if action == FILE_ACTION_ADDED {
                    let _ = tx.send(RawEvent::Change { kind: "created", path: full });
                } else if action == FILE_ACTION_REMOVED {
                    let _ = tx.send(RawEvent::Change { kind: "removed", path: full });
                } else if action == FILE_ACTION_MODIFIED {
                    let _ = tx.send(RawEvent::Change { kind: "modified", path: full });
                } else if action == FILE_ACTION_RENAMED_OLD_NAME {
                    pending_rename_old = Some(full);
                } else if action == FILE_ACTION_RENAMED_NEW_NAME {
                    if let Some(old) = pending_rename_old.take() {
                        let _ = tx.send(RawEvent::Rename { old, new: full });
                    } else {
                        // 没配对上老名字（老/新按 Windows 文档总是紧邻出现，理论上不会走到这里）：
                        // 兜底当创建处理，不丢事件。
                        let _ = tx.send(RawEvent::Change { kind: "created", path: full });
                    }
                }
            } else if action == FILE_ACTION_RENAMED_OLD_NAME {
                pending_rename_old = None;
            }

            if next_offset == 0 {
                break;
            }
            offset += next_offset as usize;
        }
    }

    /// 一条尚未到期（未过 [`DEBOUNCE_WINDOW`]）的去抖记录：同一路径的后续事件会覆盖 `kind`/
    /// `seen`，只保留"安静下来那一刻"的最终状态。
    struct Pending {
        kind: String,
        old_path: Option<String>,
        seen: Instant,
    }

    /// 去抖线程：按路径合并短时间内的多次变化，安静 [`DEBOUNCE_WINDOW`] 后才真正推给插件页；
    /// `error` 事件不经去抖，立即推送。读线程退出（`Sender` 被丢弃）后，把剩余未到期的事件
    /// 立即冲出，再把这条监听从全局表里摘除——这是整条监听生命周期真正结束的地方。
    fn debounce_loop(watch_id: String, rx: std::sync::mpsc::Receiver<RawEvent>, app: tauri::AppHandle, label: String) {
        let mut pending: HashMap<String, Pending> = HashMap::new();
        loop {
            match rx.recv_timeout(DEBOUNCE_TICK) {
                Ok(RawEvent::Change { kind, path }) => {
                    pending.insert(
                        path.to_string_lossy().into_owned(),
                        Pending { kind: kind.to_string(), old_path: None, seen: Instant::now() },
                    );
                }
                Ok(RawEvent::Rename { old, new }) => {
                    pending.insert(
                        new.to_string_lossy().into_owned(),
                        Pending {
                            kind: "renamed".to_string(),
                            old_path: Some(old.to_string_lossy().into_owned()),
                            seen: Instant::now(),
                        },
                    );
                }
                Ok(RawEvent::Error(message)) => {
                    emit_event(
                        &app,
                        &label,
                        &FsWatchEvent {
                            watch_id: watch_id.clone(),
                            kind: "error".to_string(),
                            path: String::new(),
                            old_path: None,
                            message: Some(message),
                        },
                    );
                }
                Err(RecvTimeoutError::Timeout) => {
                    flush_due(&watch_id, &mut pending, &app, &label, false);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    flush_due(&watch_id, &mut pending, &app, &label, true);
                    break;
                }
            }
        }
        lock_watches().remove(&watch_id);
    }

    /// 把已经安静满 [`DEBOUNCE_WINDOW`] 的记录冲出去（`force_all` 时不看时间、全部冲出，
    /// 用于读线程已退出、不会再有新事件的收尾场景）。
    fn flush_due(
        watch_id: &str,
        pending: &mut HashMap<String, Pending>,
        app: &tauri::AppHandle,
        label: &str,
        force_all: bool,
    ) {
        let now = Instant::now();
        let due: Vec<String> = pending
            .iter()
            .filter(|(_, e)| force_all || now.duration_since(e.seen) >= DEBOUNCE_WINDOW)
            .map(|(k, _)| k.clone())
            .collect();
        for path in due {
            if let Some(e) = pending.remove(&path) {
                emit_event(
                    app,
                    label,
                    &FsWatchEvent {
                        watch_id: watch_id.to_string(),
                        kind: e.kind,
                        path,
                        old_path: e.old_path,
                        message: None,
                    },
                );
            }
        }
    }

    /// 把事件推给插件页。插件页拿不到 `window.__TAURI__`，不能用 `app.emit_to`，必须走
    /// `webview.eval` 调 `window.__itoolsEmit`——写法照抄 `exec.rs::emit_to_plugin`。
    fn emit_event(app: &tauri::AppHandle, label: &str, payload: &FsWatchEvent) {
        let Some(win) = app.get_webview_window(label) else {
            return;
        };
        let json = match serde_json::to_string(payload) {
            Ok(j) => j,
            Err(_) => return,
        };
        let js = format!("window.__itoolsEmit && window.__itoolsEmit('plugin-fs-watch', {json})");
        let _ = win.eval(&js);
    }
}

// ==================== 命令：系统文件图标 ====================

/// 取 scope 内某个文件/目录的系统图标（32x32），返回 base64 PNG。
///
/// 例外：本命令只读图标（不读文件内容、不改动任何东西），所以复用 `fs_api::resolve_existing`
/// 的越权/黑名单校验即可，不需要比普通读操作更严的门槛（任务书里"该例外"指的正是这条）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `scope_id`/`path` 校验失败（同 `plugin_fs_stat`）
/// - 系统取图标失败（极少见：路径存在但 shell 无法解析，如设备文件）
/// - 非 Windows 平台（本能力目前仅支持 Windows）
#[tauri::command]
pub async fn plugin_get_file_icon(
    scope_id: String,
    path: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let scope = fs_api::resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    let target = fs_api::resolve_existing(&scope, &sub)?;
    tauri::async_runtime::spawn_blocking(move || icon_dispatch(&target))
        .await
        .map_err(|e| format!("图标任务失败: {e}"))?
}

/// 平台分发：真正实现是 [`win_icon::icon_base64_png`]（`cfg(windows)`）；非 Windows 上如实报错。
#[cfg(windows)]
fn icon_dispatch(path: &Path) -> Result<String, String> {
    win_icon::icon_base64_png(path).ok_or_else(|| "无法获取该文件的系统图标".to_string())
}

#[cfg(not(windows))]
fn icon_dispatch(path: &Path) -> Result<String, String> {
    let _ = path;
    Err("系统文件图标仅在 Windows 上可用".to_string())
}

/// 系统文件图标提取：`SHGetFileInfoW` 拿 `HICON` → `GetIconInfo`/`GetDIBits` 转 32bpp BGRA →
/// 换通道成 RGBA → PNG。用 `windows_sys`（而非 `windows` crate）、AND 掩码兜底老式无 alpha
/// 图标，调用方式照抄项目里已经验证跑通的 `search/icon.rs`（不能跨模块 `use` 它的私有函数，
/// 且它是为搜索索引场景写的、不吃 scope 校验，故在此重新实现一遍同样的调用序列）。
#[cfg(windows)]
mod win_icon {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BITMAP, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    /// 路径 → base64(PNG)。失败返回 `None`（调用方负责转成用户可见的错误信息）。
    pub(super) fn icon_base64_png(path: &Path) -> Option<String> {
        // SAFETY: 仅初始化当前线程的 COM apartment（`spawn_blocking` 派发的线程不保证已初始化），
        // 参数固定合法，已初始化(S_FALSE)/模式冲突都无害，返回值可安全忽略。
        unsafe {
            let _ = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
        }
        // Shell 只认反斜杠：把 '/' 归一为 '\\'，再补结尾 0。
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .map(|c| if c == b'/' as u16 { b'\\' as u16 } else { c })
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: wide 是以 0 结尾的合法宽字符串，调用期间存活。
        unsafe {
            let hicon = hicon_for(&wide)?;
            let rgba = hicon_to_rgba(hicon);
            DestroyIcon(hicon); // 无论成败都释放

            let (w, h, pixels) = rgba?;
            let png = encode_png(w, h, &pixels)?;
            use base64::Engine as _;
            Some(base64::engine::general_purpose::STANDARD.encode(png))
        }
    }

    /// 对给定宽字符串路径取系统图标（32x32）。
    ///
    /// # Safety
    /// `wide` 必须以 0 结尾。返回的 `HICON` 所有权移交调用方，由其 `DestroyIcon`。
    unsafe fn hicon_for(wide: &[u16]) -> Option<HICON> {
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut shfi,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || shfi.hIcon.is_null() {
            return None;
        }
        Some(shfi.hIcon)
    }

    /// `HICON` → (w, h, RGBA)。内部负责释放位图与 DC。
    ///
    /// # Safety
    /// `hicon` 必须是有效图标句柄；调用方负责在之后 `DestroyIcon`。
    unsafe fn hicon_to_rgba(hicon: HICON) -> Option<(u32, u32, Vec<u8>)> {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(hicon, &mut ii) == 0 {
            return None;
        }
        let hbm_color = ii.hbmColor;
        let hbm_mask = ii.hbmMask;

        // 闭包收敛 early-return，末尾统一释放两个 HBITMAP，任何分支都不会漏释放。
        let out = (|| {
            if hbm_color.is_null() {
                return None; // 极老单色图标（仅 mask）不处理，调用方按失败兜底
            }
            let mut bmp: BITMAP = std::mem::zeroed();
            let n = GetObjectW(
                hbm_color as HGDIOBJ,
                std::mem::size_of::<BITMAP>() as i32,
                &mut bmp as *mut BITMAP as *mut core::ffi::c_void,
            );
            if n == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
                return None;
            }
            let (width, height) = (bmp.bmWidth as u32, bmp.bmHeight as u32);

            let hdc: HDC = CreateCompatibleDC(ptr::null_mut());
            if hdc.is_null() {
                return None;
            }

            let mut color = read_dib32(hdc, hbm_color, width, height);
            // 彩色位图无 alpha（全 0）→ 用 AND 掩码合成 alpha（老式图标的常见形态）。
            if let Some(ref mut buf) = color {
                let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
                if !has_alpha {
                    match read_dib32(hdc, hbm_mask, width, height) {
                        Some(mask) => {
                            for (px, m) in buf.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                                px[3] = if m[0] == 0 { 255 } else { 0 }; // 0=不透明
                            }
                        }
                        None => buf.chunks_exact_mut(4).for_each(|px| px[3] = 255),
                    }
                }
            }
            DeleteDC(hdc);

            let mut rgba = color?;
            for px in rgba.chunks_exact_mut(4) {
                px.swap(0, 2); // BGRA -> RGBA
            }
            Some((width, height, rgba))
        })();

        if !hbm_color.is_null() {
            DeleteObject(hbm_color as HGDIOBJ);
        }
        if !hbm_mask.is_null() {
            DeleteObject(hbm_mask as HGDIOBJ);
        }
        out
    }

    /// `GetDIBits`：`HBITMAP` → 32bpp top-down BGRA。
    ///
    /// # Safety
    /// `hdc` 为有效内存 DC，`hbm` 为有效位图且未被 select 进任何 DC。
    unsafe fn read_dib32(hdc: HDC, hbm: HBITMAP, width: u32, height: u32) -> Option<Vec<u8>> {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = width as i32;
        bi.bmiHeader.biHeight = -(height as i32); // 负 = top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;

        let mut buf = vec![0u8; width as usize * height as usize * 4];
        let scanned = GetDIBits(hdc, hbm, 0, height, buf.as_mut_ptr() as *mut core::ffi::c_void, &mut bi, DIB_RGB_COLORS);
        if scanned == 0 {
            None
        } else {
            Some(buf)
        }
    }

    /// RGBA → PNG（`png` crate；`Writer` 在 `finish()`/`Drop` 时写 IEND）。
    fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
        let mut out: Vec<u8> = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().ok()?;
        w.write_image_data(rgba).ok()?;
        w.finish().ok()?;
        Some(out)
    }
}

// ==================== 测试：无需 Win32/Tauri State 的纯逻辑 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_zip_root_name_strips_dot_segments_and_backslashes() {
        assert_eq!(normalize_zip_root_name("docs"), "docs");
        assert_eq!(normalize_zip_root_name("./docs/readme.txt"), "docs/readme.txt");
        assert_eq!(normalize_zip_root_name("docs\\sub\\a.txt"), "docs/sub/a.txt");
    }

    #[test]
    fn safe_zip_entry_path_accepts_normal_nested_paths() {
        let p = safe_zip_entry_path("docs/sub/readme.txt").expect("应当放行");
        assert_eq!(p, PathBuf::from("docs").join("sub").join("readme.txt"));
    }

    #[test]
    fn safe_zip_entry_path_accepts_backslash_as_separator() {
        let p = safe_zip_entry_path("docs\\sub\\readme.txt").expect("应当放行（反斜杠也当分隔符）");
        assert_eq!(p, PathBuf::from("docs").join("sub").join("readme.txt"));
    }

    #[test]
    fn safe_zip_entry_path_rejects_absolute_and_parent_traversal() {
        assert!(safe_zip_entry_path("/etc/passwd").is_err());
        assert!(safe_zip_entry_path("../escape.txt").is_err());
        assert!(safe_zip_entry_path("a/../../b.txt").is_err());
        assert!(safe_zip_entry_path("..").is_err());
        assert!(safe_zip_entry_path("...").is_err());
    }

    #[test]
    fn safe_zip_entry_path_rejects_drive_letter_and_nul() {
        assert!(safe_zip_entry_path("C:\\Windows\\system.ini").is_err());
        assert!(safe_zip_entry_path("a\0b").is_err());
        assert!(safe_zip_entry_path("").is_err());
    }

    #[test]
    fn safe_zip_entry_path_rejects_trailing_space_or_dot() {
        assert!(safe_zip_entry_path("payload.exe.").is_err());
        assert!(safe_zip_entry_path("evil ").is_err());
    }
}
