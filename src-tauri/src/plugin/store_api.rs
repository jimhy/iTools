//! 插件三项能力：**加密存储**（`plugin_crypto_*`）、**附件存储**（`plugin_attach_*`）、
//! **定时任务**（`plugin_schedule_*` + [`start_scheduler`] / [`stop_for_session`]）。
//!
//! ## 会话隔离
//! 三类数据一律按**完整会话身份**（插件 id + `dev` 标志）隔离，写法直接对齐
//! `commands.rs`：加密 KV 复用同一个 `Db`（调试会话落 `registry.dev.db`，另一份物理文件），
//! 只是多套了一层「命名空间」（`crypto:<id>`）避免和 `itools.db.*`（`plugin_db_*`）撞车；
//! 附件与定时任务的持久化文件都落在 [`super::commands::session_files_dir`] 的**同级目录**
//! （`<...>/<id>/attachments/`、`<...>/<id>/schedule.json`），该函数本身已经把调试会话
//! 与正式会话分到两个物理不同的根目录下，无需再自己判 `dev`。
//!
//! ## 加密存储：Windows DPAPI（`CryptProtectData` / `CryptUnprotectData`）
//! 选它是因为密钥完全由操作系统托管、绑定到"当前登录的 Windows 用户账户"，我们不接触、
//! 不生成、不存储任何密钥材料——这是"最省事且真实有效"的加密，而不是自己拍一个 XOR
//! 或写死密钥的假加密（那种东西比不加密更糟：它会让用户误以为数据安全）。
//!
//! **必须如实讲清防护边界**（诚信红线，不能吹成"绝对安全"）：
//! - **防得住**：别的 Windows 用户账户登录后来读这份数据；有人把 `itools.db` 文件整个拷走
//!   （拷到别的机器、别的账户下）想离线解密——DPAPI 的主密钥派生自该账户的登录凭据，脱离
//!   这个账户上下文就解不开。
//! - **防不住**：**当前登录账户下运行的其它程序**。任何以同一用户身份跑起来的进程，都能调用
//!   一模一样的 `CryptUnprotectData` 把密文解开——这不是 iTools 的实现缺陷，是 DPAPI 这套机制
//!   本身保护的是"账户边界"而非"进程边界"。所以这套存储只能定位为"防拷库 / 防换人"，
//!   不能当成"防同机其它软件窥探"的强隔离，UI/文档措辞上不得夸大。
//!
//! ## 附件存储
//! 二进制大对象（图片/音频/导出文件），按会话隔离落盘在插件数据目录下，单个附件上限 32MB，
//! 超限直接报错（不静默截断——静默截断会让插件拿到半截数据却以为写完整了）。
//!
//! ## 定时任务
//! 只做**固定间隔**（`everySecs`）。`atCron` 参数**不支持**：项目没有引入 cron 表达式解析
//! 依赖，也不该为了这一个字段专门造轮子或引新库——传了 `atCron` 直接报错拒绝，不会被
//! 静默忽略成"看似接受、实则什么也没排"的假成功。任务持久化在插件自己的数据目录下
//! （不含下一次触发时间——重启/重新装载后从"此刻起再走一个整间隔"计时，不做错过重排的
//! 追赶式补发，这是简单且诚实的行为，好过假装能精确追回停机期间错过的每一次触发）。
//!
//! 到点后**只推事件给该插件页**（`window.__itoolsEmit`），绝不主动把窗口唤出来——后台常驻
//! 插件的窗口本来就是隐藏的，唤出来只会突兀地在任务栏之外冒出一个用户没点过的窗口。
//! 找接收窗口时按**会话**匹配（[`resolve_window_label`]），不是写死 `"plugin"`：后台常驻插件
//! 跑在 `plugin-bg-<id>` 窗口而非共享的 `plugin` 窗口，定时任务的接收方多半正是前者。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::db::Db;
use crate::dev::DEV_WINDOW_LABEL;
use crate::logging::ilog;
use crate::settings::SettingsStore;

use super::commands::{bg_label, caller_session, plugin_granted, session_files_dir, PLUGIN_WINDOW_LABEL};
use super::{ActiveSession, PluginRegistry};

/// 容忍锁中毒（持锁线程 panic）：拿到内部值继续用，道理同 `exec.rs::lock`。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 日志里只记「谁」，不记 key / 内容——这些可能是插件在处理的用户数据，道理同 `exec.rs::who`。
fn who(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 把一条事件推给**插件页**。照抄 `exec.rs::emit_to_plugin`：插件页拿不到 `window.__TAURI__`，
/// 必须走 `webview.eval` → `window.__itoolsEmit(channel, payload)` 这条自建总线，不能用
/// `app.emit_to` 的 Tauri 原生事件（原生事件在插件页里没有接收方）。
fn emit_to_plugin<T: Serialize>(app: &AppHandle, label: &str, channel: &str, payload: &T) {
    let Some(win) = app.get_webview_window(label) else {
        return;
    };
    let Ok(json) = serde_json::to_string(payload) else {
        return;
    };
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('{channel}', {json})");
    let _ = win.eval(&js);
}

/// 校验一个「不参与文件系统路径拼接的 KV key / 附件 id」：非空、不超长、不含路径分隔符 /
/// 盘符 / NUL、不含 `..`。加密 KV 的 key 本身不落文件系统（存在 SQLite 列里），但附件 id
/// 会被直接拼成文件名——两者共用同一条校验规则，图省心也图安全（严格的对不严格的场景
/// 无害，反过来才会出事）。
fn check_key(kind: &str, s: &str) -> Result<(), String> {
    const MAX_LEN: usize = 256;
    let bad = s.is_empty()
        || s.len() > MAX_LEN
        || s.contains(['/', '\\', ':', '\0'])
        || s.contains("..");
    if bad {
        return Err(format!(
            "非法的{kind}「{s}」：不能为空、不超过 {MAX_LEN} 字符，且不能含路径分隔符 / \\ : 或 .."
        ));
    }
    Ok(())
}

// ============================================================================
// 加密存储：plugin_crypto_*
// ============================================================================

/// DPAPI 封装：真实加密，不是自造的假加密。密钥/算法全由操作系统托管，我们只负责调用。
#[cfg(windows)]
mod dpapi {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// 明文字节 → DPAPI 密文字节。`CRYPTPROTECT_UI_FORBIDDEN`：禁止系统弹任何 UI（某些策略下
    /// 会弹密码确认框，把一次纯后台调用卡死），我们要的是"静默成功或明确失败"。
    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: input.pbData 指向本函数参数 `plain`（本调用期间全程存活），cbData 与其
        // 长度精确匹配，只被 CryptProtectData 只读地读取；output 全零初始化，由系统调用
        // 成功时自行 LocalAlloc 分配并填充，我们不预先准备缓冲区；其余可选参数一律传
        // None（不要弹窗描述、不要额外熵、不要提示结构）。
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        }
        // SAFETY: 上一步已成功返回，output.pbData 是系统用 LocalAlloc 分配、长度恰为
        // output.cbData 的有效缓冲区；这里只在其生命周期内只读拷出一份，随后立即
        // LocalFree 释放，不产生悬垂指针，也不会重复释放（只释放这一次）。
        let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        }
        Ok(bytes)
    }

    /// DPAPI 密文字节 → 明文字节。
    pub fn unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: cipher.len() as u32,
            pbData: cipher.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: 同 protect()——input 指向仍存活的 `cipher` 切片；ppszDataDescr 传 None
        // 表示不取回加密时附带的描述字符串；output 由系统调用负责分配填充。
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 解密失败（密文可能已损坏，或是在别的 Windows 账户下加密的）: {e}"))?;
        }
        let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        }
        Ok(bytes)
    }
}

/// 非 Windows 平台没有 DPAPI：诚实拒绝，不假装加密成功。
#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
        Err("加密存储仅支持 Windows（依赖系统 DPAPI）".to_string())
    }
    pub fn unprotect(_cipher: &[u8]) -> Result<Vec<u8>, String> {
        Err("加密存储仅支持 Windows（依赖系统 DPAPI）".to_string())
    }
}

/// 加密 KV 的命名空间：与 `plugin_db_*`（`itools.db.*`）共用同一张 `plugin_kv` 表但不同
/// `plugin_id` 前缀，物理上互不可见，也不需要为此另开一张表 / 另一个文件。
fn crypto_ns(id: &str) -> String {
    format!("crypto:{id}")
}

/// 本次调用该用哪个库：调试会话落**独立测试库**，正式会话落正式库——与 `commands.rs::session_db`
/// 同一套判定（该函数是私有的，这里按同样逻辑本地重写一份，两处不共享状态、互不影响）。
fn crypto_db<'a>(session: &ActiveSession, registry: &'a PluginRegistry, db: &'a Arc<Db>) -> &'a Arc<Db> {
    if session.dev {
        &registry.dev.db
    } else {
        db
    }
}

/// 写一条加密 KV（覆盖）。`value` 用 DPAPI 加密后落盘，明文不落地。
#[tauri::command]
pub fn plugin_crypto_set(
    key: String,
    value: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    check_key("加密存储 key", &key)?;
    let cipher = dpapi::protect(value.as_bytes())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(cipher);
    crypto_db(&session, &registry, &db).pkv_set(&crypto_ns(&session.id), &key, &b64)
}

/// 读一条加密 KV 并解密；不存在返回 `None`。
#[tauri::command]
pub fn plugin_crypto_get(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<Option<String>, String> {
    let session = caller_session(&webview, &registry)?;
    check_key("加密存储 key", &key)?;
    let Some(b64) = crypto_db(&session, &registry, &db).pkv_get(&crypto_ns(&session.id), &key) else {
        return Ok(None);
    };
    let cipher = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| format!("存储的密文已损坏（base64 解码失败）: {e}"))?;
    let plain = dpapi::unprotect(&cipher)?;
    String::from_utf8(plain)
        .map(Some)
        .map_err(|e| format!("解密结果不是合法 UTF-8（数据可能已损坏）: {e}"))
}

/// 删一条加密 KV（不存在视为成功，幂等）。
#[tauri::command]
pub fn plugin_crypto_remove(
    key: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    check_key("加密存储 key", &key)?;
    crypto_db(&session, &registry, &db).pkv_remove(&crypto_ns(&session.id), &key)
}

/// 列本插件加密 KV 的 key（可前缀过滤）。返回的是 key 列表，不含值（值需逐个 get 才解密）。
#[tauri::command]
pub fn plugin_crypto_keys(
    prefix: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    db: State<'_, Arc<Db>>,
) -> Result<Vec<String>, String> {
    let session = caller_session(&webview, &registry)?;
    Ok(crypto_db(&session, &registry, &db).pkv_keys(&crypto_ns(&session.id), &prefix.unwrap_or_default()))
}

// ============================================================================
// 附件存储：plugin_attach_*
// ============================================================================

/// 单个附件上限：32MB。超限直接报错，不静默截断。
const ATTACH_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachMeta {
    mime: String,
    size: u64,
    created_at: u64,
}

/// [`plugin_attach_get`] 的返回结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachGetResult {
    data_b64: String,
    mime: String,
    size: u64,
}

/// [`plugin_attach_list`] 的一条条目。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachInfo {
    id: String,
    mime: String,
    size: u64,
    created_at: u64,
}

/// 附件目录：与 [`session_files_dir`]（`writeFile` 用的沙盒）**同级但不同名**
/// （`<...>/<id>/attachments/` vs `<...>/<id>/files/`），互不干扰，且天然继承同一套
/// 调试/正式物理隔离（`session_files_dir` 本身已经按 `session.dev` 分到不同根）。
fn attachments_dir(session: &ActiveSession, registry: &PluginRegistry) -> PathBuf {
    let files_dir = session_files_dir(session, registry);
    let base = files_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| files_dir.clone());
    base.join("attachments")
}

/// 写入 / 覆盖一个附件。`data_b64` 为附件的 base64 编码内容，解码后超过 32MB 直接报错。
#[tauri::command]
pub fn plugin_attach_put(
    id: String,
    data_b64: String,
    mime: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    check_key("附件 id", &id)?;
    if mime.len() > 256 {
        return Err("mime 过长".to_string());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    if bytes.len() > ATTACH_MAX_BYTES {
        return Err(format!(
            "附件超过单个 32MB 上限（实际 {} 字节），未写入",
            bytes.len()
        ));
    }
    let dir = attachments_dir(&session, &registry);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建附件目录失败: {e}"))?;
    let meta = AttachMeta {
        mime,
        size: bytes.len() as u64,
        created_at: now_secs(),
    };
    std::fs::write(dir.join(format!("{id}.bin")), &bytes).map_err(|e| format!("写入附件失败: {e}"))?;
    let meta_json = serde_json::to_string(&meta).map_err(|e| format!("附件元信息序列化失败: {e}"))?;
    std::fs::write(dir.join(format!("{id}.json")), meta_json)
        .map_err(|e| format!("写入附件元信息失败: {e}"))?;
    Ok(())
}

/// 读取一个附件（base64 内容 + mime）；不存在返回 `None`。
#[tauri::command]
pub fn plugin_attach_get(
    id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<Option<AttachGetResult>, String> {
    let session = caller_session(&webview, &registry)?;
    check_key("附件 id", &id)?;
    let dir = attachments_dir(&session, &registry);
    let Ok(bytes) = std::fs::read(dir.join(format!("{id}.bin"))) else {
        return Ok(None);
    };
    // 元信息文件理论上应与 .bin 同时存在；万一因异常（如强杀导致两次写入不原子）缺失，
    // 退化为默认 mime 而不是把整个附件当成"读取失败"——数据本体还在，不该因为一个
    // 辅助元信息丢了就拒绝把内容还给调用方。
    let meta: AttachMeta = std::fs::read_to_string(dir.join(format!("{id}.json")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(AttachMeta {
            mime: "application/octet-stream".to_string(),
            size: bytes.len() as u64,
            created_at: 0,
        });
    Ok(Some(AttachGetResult {
        data_b64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        mime: meta.mime,
        size: bytes.len() as u64,
    }))
}

/// 删除一个附件（连同其元信息）。不存在视为成功，幂等。
#[tauri::command]
pub fn plugin_attach_remove(
    id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    check_key("附件 id", &id)?;
    let dir = attachments_dir(&session, &registry);
    for name in [format!("{id}.bin"), format!("{id}.json")] {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("删除附件失败: {e}")),
        }
    }
    Ok(())
}

/// 列出本插件的全部附件（id / mime / size / createdAt），不含内容本体。
#[tauri::command]
pub fn plugin_attach_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<Vec<AttachInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    let dir = attachments_dir(&session, &registry);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<AttachMeta>(&s) else {
            continue;
        };
        out.push(AttachInfo {
            id: id.to_string(),
            mime: meta.mime,
            size: meta.size,
            created_at: meta.created_at,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

// ============================================================================
// 定时任务：plugin_schedule_* + start_scheduler / stop_for_session
// ============================================================================

const EVT_SCHEDULE_FIRE: &str = "plugin-schedule-fire";

/// `plugin_schedule_add` 的参数。`code`/`payload` 到点后原样回传给插件页，语义由插件自定。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleOptions {
    /// 固定间隔（秒）。当前唯一支持的调度方式，必须 >= 1。
    #[serde(default)]
    pub every_secs: Option<u64>,
    /// cron 表达式——**暂不支持**（项目未引入 cron 解析依赖）。传了会直接报错，不会被
    /// 静默忽略成"看似接受、实则从不触发"的假成功。
    #[serde(default)]
    pub at_cron: Option<String>,
    pub code: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// [`plugin_schedule_list`] 的一条条目 / 持久化到磁盘的记录（同一个结构复用：磁盘上就是
/// 这份数据的数组，不含运行期才有的 `next_fire_at`——见模块头注释「不做追赶式补发」）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleInfo {
    pub task_id: String,
    pub every_secs: u64,
    pub code: String,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// 运行期任务：磁盘记录 + 归属会话 + 下一次到点的 unix 秒。
#[derive(Clone)]
struct RuntimeTask {
    owner: ActiveSession,
    every_secs: u64,
    code: String,
    payload: Option<serde_json::Value>,
    next_fire_at: u64,
}

/// 定时任务的运行期状态（managed）。由主控在启动时 `.manage(ScheduleState::default())`，
/// 并调用 [`start_scheduler`] 从磁盘装载 + 启动后台 tick 线程。
#[derive(Default)]
pub struct ScheduleState {
    tasks: Mutex<HashMap<String, RuntimeTask>>,
}

/// 定时任务持久化文件：与 [`attachments_dir`] 同级（`<...>/<id>/schedule.json`），
/// 内容是 `Vec<ScheduleInfo>` 的 JSON 数组。
fn schedule_file(session: &ActiveSession, registry: &PluginRegistry) -> PathBuf {
    let files_dir = session_files_dir(session, registry);
    let base = files_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| files_dir.clone());
    base.join("schedule.json")
}

/// 读取某会话持久化的任务列表；文件不存在或损坏都返回空（不 panic、不把半个 JSON 当成部分成功）。
fn load_persisted(path: &Path) -> Vec<ScheduleInfo> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_persisted(path: &Path, tasks: &[ScheduleInfo]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建定时任务数据目录失败: {e}"))?;
    }
    let json = serde_json::to_string_pretty(tasks).map_err(|e| format!("序列化定时任务失败: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("写入定时任务持久化文件失败: {e}"))
}

fn new_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sched-{now_ns:x}-{n:x}")
}

/// 新增一条固定间隔定时任务，返回 `taskId`。需已获 `background` 授权。
///
/// `atCron` 一旦非空直接报错拒绝（不支持，见模块头注释）；`everySecs` 缺失或为 0 同样报错。
#[tauri::command]
pub fn plugin_schedule_add(
    opts: ScheduleOptions,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, ScheduleState>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "background") {
        return Err("插件未获授权后台定时任务（请在「插件管理」里授权 background）".to_string());
    }
    if opts.at_cron.is_some() {
        return Err("atCron 暂不支持（项目未引入 cron 解析依赖），请改用 everySecs 固定间隔".to_string());
    }
    let every_secs = opts
        .every_secs
        .ok_or_else(|| "缺少 everySecs（当前仅支持固定间隔调度）".to_string())?;
    if every_secs == 0 {
        return Err("everySecs 必须 >= 1".to_string());
    }
    if opts.code.trim().is_empty() {
        return Err("code 不能为空".to_string());
    }

    let task_id = new_task_id();
    let record = ScheduleInfo {
        task_id: task_id.clone(),
        every_secs,
        code: opts.code.clone(),
        payload: opts.payload.clone(),
    };

    let path = schedule_file(&session, &registry);
    let mut list = load_persisted(&path);
    list.push(record);
    save_persisted(&path, &list)?;

    {
        let mut tasks = lock(&state.tasks);
        tasks.insert(
            task_id.clone(),
            RuntimeTask {
                owner: session.clone(),
                every_secs,
                code: opts.code,
                payload: opts.payload,
                next_fire_at: now_secs() + every_secs,
            },
        );
    }
    ilog!(
        "[iTools][plugin] schedule_add: 来自 {}，everySecs={every_secs}，task={task_id}",
        who(&session)
    );
    Ok(task_id)
}

/// 移除一条定时任务。需已获 `background` 授权；只能移除自己会话名下的任务。
#[tauri::command]
pub fn plugin_schedule_remove(
    task_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, ScheduleState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "background") {
        return Err("插件未获授权后台定时任务（请在「插件管理」里授权 background）".to_string());
    }
    {
        let mut tasks = lock(&state.tasks);
        if let Some(t) = tasks.get(&task_id) {
            if !t.owner.same_as(&session) {
                return Err("该定时任务不属于本插件".to_string());
            }
            tasks.remove(&task_id);
        }
        // 内存里没有也继续清持久化文件：可能是刚重启、调度器尚未装载完，
        // 或任务已到期被自动清理——移除操作本身应当幂等。
    }
    let path = schedule_file(&session, &registry);
    let mut list = load_persisted(&path);
    let before = list.len();
    list.retain(|t| t.task_id != task_id);
    if list.len() != before {
        save_persisted(&path, &list)?;
    }
    ilog!("[iTools][plugin] schedule_remove: 来自 {}，task={task_id}", who(&session));
    Ok(())
}

/// 列出本会话名下的全部定时任务（从持久化文件读取，始终反映磁盘上的真实状态）。
#[tauri::command]
pub fn plugin_schedule_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<Vec<ScheduleInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    let path = schedule_file(&session, &registry);
    Ok(load_persisted(&path))
}

/// 事件推给插件页的负载。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ScheduleFirePayload {
    task_id: String,
    code: String,
    payload: Option<serde_json::Value>,
    fired_at: u64,
}

/// 找到某会话**当前**该收事件的窗口：后台常驻插件跑在 `plugin-bg-<id>`，被用户主动打开的
/// 正式插件跑在共享的 `plugin` 窗口，调试插件跑在 `plugin-dev`——依次尝试，找到「窗口确实
/// 存在 + 该窗口当前加载的会话确实是这一个（同 id 同 dev 标志）」的那个就用它，一个都没有
/// 就返回 `None`（调用方据此安静跳过，不主动唤起窗口）。
fn resolve_window_label(app: &AppHandle, session: &ActiveSession) -> Option<String> {
    let registry = app.try_state::<PluginRegistry>()?;
    let candidates: Vec<String> = if session.dev {
        vec![DEV_WINDOW_LABEL.to_string()]
    } else {
        vec![bg_label(&session.id), PLUGIN_WINDOW_LABEL.to_string()]
    };
    candidates.into_iter().find(|label| {
        app.get_webview_window(label).is_some()
            && registry.session_for(label).is_some_and(|s| s.same_as(session))
    })
}

/// 到点触发一个任务：推事件，不唤窗口；找不到接收窗口就安静跳过（下一轮还会再试）。
fn fire(app: &AppHandle, task_id: &str, owner: &ActiveSession, code: &str, payload: &Option<serde_json::Value>) {
    let Some(label) = resolve_window_label(app, owner) else {
        return;
    };
    emit_to_plugin(
        app,
        &label,
        EVT_SCHEDULE_FIRE,
        &ScheduleFirePayload {
            task_id: task_id.to_string(),
            code: code.to_string(),
            payload: payload.clone(),
            fired_at: now_secs(),
        },
    );
}

/// 扫描某根目录下 `<id>/schedule.json`，返回 `(会话, 文件路径)` 列表。
fn scan_schedule_files(root: &Path, dev: bool) -> Vec<(ActiveSession, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let f = dir.join("schedule.json");
        if f.exists() {
            out.push((
                ActiveSession {
                    id: id.to_string(),
                    dev,
                },
                f,
            ));
        }
    }
    out
}

/// 启动时从磁盘装载全部已持久化的定时任务（正式 + 调试两套根都扫），装载前逐条复核
/// `background` 授权——插件被卸载 / 授权被撤销后，磁盘上残留的旧任务不会被装载、更不会触发。
fn load_all_tasks(app: &AppHandle) {
    let (Some(registry), Some(settings), Some(state)) = (
        app.try_state::<PluginRegistry>(),
        app.try_state::<SettingsStore>(),
        app.try_state::<ScheduleState>(),
    ) else {
        return;
    };
    let prod_root = crate::paths::data_root().join("plugin-data");
    let dev_root = registry.dev.files_root.clone();
    let mut found = scan_schedule_files(&prod_root, false);
    found.extend(scan_schedule_files(&dev_root, true));

    let now = now_secs();
    let mut tasks = lock(&state.tasks);
    let mut loaded = 0usize;
    for (session, path) in found {
        if !plugin_granted(&settings, &registry, &session, "background") {
            continue;
        }
        for t in load_persisted(&path) {
            let every_secs = t.every_secs.max(1);
            tasks.insert(
                t.task_id,
                RuntimeTask {
                    owner: session.clone(),
                    every_secs,
                    code: t.code,
                    payload: t.payload,
                    next_fire_at: now + every_secs,
                },
            );
            loaded += 1;
        }
    }
    ilog!("[iTools][plugin] 定时任务已装载: {loaded} 条");
}

/// 每秒检查一次到点任务：到点就触发并重排下一次（`next_fire_at += everySecs`，从当前时刻
/// 起算，不做错过重排的追赶式补发）；顺带复核每条任务的 `background` 授权是否仍然有效，
/// 一旦被撤销就从运行期表中摘除（磁盘上的持久化记录不动，重新授权后需插件再次调用
/// [`plugin_schedule_add`] 或等下次 [`start_scheduler`] 装载）。
fn tick_once(app: &AppHandle) {
    let (Some(state), Some(registry), Some(settings)) = (
        app.try_state::<ScheduleState>(),
        app.try_state::<PluginRegistry>(),
        app.try_state::<SettingsStore>(),
    ) else {
        return;
    };
    let now = now_secs();
    let mut due: Vec<(String, ActiveSession, String, Option<serde_json::Value>)> = Vec::new();
    {
        let mut tasks = lock(&state.tasks);
        let mut drop_ids = Vec::new();
        for (id, t) in tasks.iter_mut() {
            if !plugin_granted(&settings, &registry, &t.owner, "background") {
                drop_ids.push(id.clone());
                continue;
            }
            if now >= t.next_fire_at {
                due.push((id.clone(), t.owner.clone(), t.code.clone(), t.payload.clone()));
                t.next_fire_at = now + t.every_secs.max(1);
            }
        }
        for id in drop_ids {
            tasks.remove(&id);
        }
    }
    for (task_id, owner, code, payload) in due {
        fire(app, &task_id, &owner, &code, &payload);
    }
}

fn spawn_ticker(app: AppHandle) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        tick_once(&app);
    });
}

/// 供主控在启动时调用一次：先从磁盘装载全部已持久化的定时任务，再起一个每秒轮询的
/// 后台线程负责到点触发。调用前必须已 `.manage(ScheduleState::default())`
/// （否则 [`app.try_state`] 拿不到状态，本函数会直接静默跳过，不会 panic）。
pub async fn start_scheduler(app: AppHandle) {
    let app2 = app.clone();
    // 目录扫描 + 文件读取是阻塞 IO，放线程池而不是占用 async 执行器。
    let _ = tauri::async_runtime::spawn_blocking(move || load_all_tasks(&app2)).await;
    spawn_ticker(app);
}

/// 停止某会话名下全部定时任务的**触发**（不删除磁盘上的持久化记录——重新授权 / 插件重新
/// 调用 [`plugin_schedule_add`] 或应用重启后 [`start_scheduler`] 会再次装载）。
///
/// 建议在「禁用插件」「卸载插件」「关闭调试窗口」这些会让会话失效的地方调用；
/// 卸载插件时若同时删除了整个 `plugin-data/<id>` 目录，`schedule.json` 会随之一起清掉，
/// 无需额外处理。
pub fn stop_for_session(app: &AppHandle, session: &ActiveSession) {
    let Some(state) = app.try_state::<ScheduleState>() else {
        return;
    };
    let mut tasks = lock(&state.tasks);
    let before = tasks.len();
    tasks.retain(|_, t| !t.owner.same_as(session));
    let removed = before - tasks.len();
    if removed > 0 {
        ilog!("[iTools][plugin] 定时任务已停止 {removed} 条（会话 {}）", who(session));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "itools-store-api-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&p);
        p
    }

    #[test]
    fn check_key_rejects_traversal_and_separators() {
        for ok in ["demo", "user-settings", "a.b.c", "笔记本"] {
            assert!(check_key("key", ok).is_ok(), "本应合法: {ok}");
        }
        for bad in ["", "a/b", "a\\b", "C:secret", "../etc", "a..b", &"x".repeat(300)] {
            assert!(check_key("key", bad).is_err(), "本应非法: {bad:?}");
        }
    }

    #[test]
    fn crypto_ns_is_distinct_from_plain_kv_namespace() {
        // 加密 KV 的命名空间必须带前缀，绝不能与 `plugin_db_*` 直接用的裸 id 撞车，
        // 否则加密值会和明文值写进同一行、互相覆盖。
        assert_ne!(crypto_ns("demo"), "demo");
        assert_eq!(crypto_ns("demo"), "crypto:demo");
    }

    #[test]
    fn schedule_persist_roundtrip() {
        let dir = tmp("sched");
        let path = dir.join("schedule.json");
        assert!(load_persisted(&path).is_empty(), "文件不存在应返回空列表而不是报错");

        let list = vec![
            ScheduleInfo {
                task_id: "t1".to_string(),
                every_secs: 30,
                code: "ping".to_string(),
                payload: Some(serde_json::json!({"a":1})),
            },
            ScheduleInfo {
                task_id: "t2".to_string(),
                every_secs: 60,
                code: "sync".to_string(),
                payload: None,
            },
        ];
        save_persisted(&path, &list).unwrap();
        let back = load_persisted(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].task_id, "t1");
        assert_eq!(back[0].every_secs, 30);
        assert_eq!(back[1].payload, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schedule_file_corrupted_json_degrades_to_empty() {
        let dir = tmp("sched-bad");
        let path = dir.join("schedule.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(load_persisted(&path).is_empty(), "损坏的持久化文件不应 panic，应退化为空列表");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
