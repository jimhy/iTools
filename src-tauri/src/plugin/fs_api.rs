//! 「用户选择即授权」的通用文件访问能力（浏览器 File System Access API 的思路搬到桌面）。
//!
//! # 背景
//!
//! 插件默认只能碰自己的沙盒目录（`plugin-data/<id>/files`，相对路径，见 `commands.rs` 的
//! `plugin_read_file` / `plugin_write_file`）。磁盘清理、批量改名这类插件天然要碰用户指定的
//! 任意目录，沙盒模型做不到。本模块给的方案是：**插件不能自己指定路径去读，只能弹一个原生
//! 对话框，由用户亲手选中一个目录/文件；用户选中的那一下就是唯一的授权动作**——插件此后
//! 只持有一个不透明的 `scope_id`，看不到、也改不了自己被授予了哪个真实路径之外的任何东西。
//!
//! # 设计取舍
//!
//! - **scope 要持久**：用户选一次之后，插件下次进来不该被迫重新选——不然这个能力就是聋子的
//!   耳朵。所以 scope 落盘在插件自己的数据目录下（`plugin-data/<id>/fs_scopes.json`，调试会话
//!   落 `<dev_home>/plugin-data/<id>/fs_scopes.json`），**按插件 id + dev/正式 隔离**，A 插件的
//!   scope 文件里没有 B 插件的任何记录，物理上就拿不到。
//! - **scope_id 不透明、不可猜测**：用 sha256(纳秒时间戳 + 进程内自增计数器 + 进程 PID + 一个
//!   堆地址) 生成的 64 位十六进制串。项目里没有引入 `rand`/`uuid`（依赖最小化的要求），
//!   这几路输入合起来的熵对「同机其它插件靠猜 id 撞库」这个威胁模型已经够用；它不是密码学级
//!   随机数生成器，如果之后要把 scope_id 当作能跨机器/跨会话传递的凭证，需要换成真随机源。
//! - **每次操作都重新 canonicalize、重新比对，不缓存「已验证」的结果**：symlink/junction 可以
//!   在 grant 之后被换掉（TOCTOU），所以校验必须在**每一次**实际落盘操作前重新做一遍，而不是
//!   只在用户选择的那一刻做一次就信到底。
//! - **硬黑名单：iTools 的数据根一律拒绝，即使用户在对话框里亲自选中了它**。
//!   挡的是 `crate::paths::all_data_roots()`（release 与 debug 两个），不是只挡当前构建那一个。
//!   这个目录下的 `plugin-data/` 装着**所有插件**的数据（沙盒文件、kv 存储、权限表……）。
//!   一旦某个插件通过「用户选择」拿到了这个目录的访问权，插件间的数据隔离就名存实亡——
//!   哪怕用户本人愿意把这个目录点给某个插件，也不能替其它所有插件"被公开"背书，这不是
//!   用户能单方面同意让渡的东西。所以这条检查在 grant 时和之后每次操作时都会重新做，
//!   不接受任何例外、任何"用户已同意"的旁路。
//! - **不像沙盒那样自动 `create_dir_all`**：`plugin_write_file` 在自己的沙盒里可以放心地把
//!   缺失的父目录一路建出来，因为那本来就是插件自己的一亩三分地；但这里操作的是用户真实的
//!   文件系统，静默建出一整棵新目录树的副作用比在沙盒里大得多，所以 `plugin_fs_write` 只允许
//!   在**已经存在**的目录下新建/覆盖单个文件，不会替插件把目录结构画出来。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::{ActiveSession, PluginRegistry};
use crate::settings::SettingsStore;

/// 门禁用的权限标识（对应插件 `plugin.json` 里的 `permissions`，用户需在「插件管理」授权）。
const PERMISSION: &str = "fs-user-scope";

/// scope 持久化文件名（落在插件自己的数据目录下，见模块文档）。
const SCOPE_STORE_FILE: &str = "fs_scopes.json";

/// 单块流式读取的上限（16MB）：避免插件用超大 `len` 把整份大文件一次吃进内存，
/// 这就失去了「分块读」本来要规避的问题。超限直接拒绝，提示拆成多次读取。
const MAX_CHUNK_LEN: u64 = 16 * 1024 * 1024;

// ==================== 数据结构 ====================

/// 一个用户授权的文件访问范围：要么是一个目录，要么是单个文件。
///
/// `id` 是发给插件的唯一凭证，不透明；`path` 是绝对路径，仅作展示（对话框里用户已经亲眼
/// 看过这个路径，回显给插件不算额外泄露）。插件此后所有 `plugin_fs_*` 调用都传 `id`，
/// 真实路径只在后端内部使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsScope {
    pub id: String,
    /// "dir" | "file"
    pub kind: String,
    pub path: String,
    /// 展示用的短名字（目录/文件的 basename），前端可直接拿去显示。
    pub label: String,
    /// 授权时刻的 Unix 秒。
    pub created: u64,
}

/// 目录枚举 / 单项查询的返回项。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// RFC3339 本地时间；取不到修改时间（极少数文件系统）时为 `None`。
    pub modified: Option<String>,
}

/// `plugin_pick_dir` 的可选参数。
#[derive(Debug, Clone, Default, Deserialize)]
// 入参字段统一用 camelCase：这些结构体是**插件页直接传进来的 JSON**，JS 侧的自然写法就是
// camelCase，且 Tauri 对命令顶层参数本来就做 camelCase 转换。若这里不标，插件传 `maxDepth`
// 会因为对不上 `max_depth` 而被静默忽略、退回默认值——不报错、不生效，最难查的那类问题。
#[serde(rename_all = "camelCase")]
pub struct PickDirOptions {
    /// 对话框标题，缺省用「选择文件夹」。
    pub title: Option<String>,
}

/// `plugin_pick_file` 的一个文件类型过滤器。
#[derive(Debug, Clone, Deserialize)]
pub struct PickFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

/// `plugin_pick_file` 的可选参数。
#[derive(Debug, Clone, Default, Deserialize)]
// 入参字段统一用 camelCase：这些结构体是**插件页直接传进来的 JSON**，JS 侧的自然写法就是
// camelCase，且 Tauri 对命令顶层参数本来就做 camelCase 转换。若这里不标，插件传 `maxDepth`
// 会因为对不上 `max_depth` 而被静默忽略、退回默认值——不报错、不生效，最难查的那类问题。
#[serde(rename_all = "camelCase")]
pub struct PickFileOptions {
    /// 对话框标题，缺省用「选择文件」。
    pub title: Option<String>,
    pub filters: Option<Vec<PickFilter>>,
}

/// scope 存档文件的整体结构（一个插件一份）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ScopeStore {
    scopes: Vec<FsScope>,
}

// ==================== 命令：授权（弹对话框） ====================

/// 弹原生「选择文件夹」对话框；用户选中即视为把该目录授权给当前插件，返回一个持久 scope。
/// 用户取消返回 `Ok(None)`。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - 所选目录不可访问（如中途被删除/无权限）、或校验后发现不是目录
/// - 所选目录落在 iTools 自身数据根内（硬黑名单，见模块文档，无一例外）
/// - scope 存档写盘失败
#[tauri::command]
pub async fn plugin_pick_dir(
    opts: Option<PickDirOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Option<FsScope>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let title = opts
        .and_then(|o| o.title)
        .unwrap_or_else(|| "选择文件夹".to_string());
    let picked = tauri::async_runtime::spawn_blocking(move || {
        rfd::FileDialog::new().set_title(&title).pick_folder()
    })
    .await
    .map_err(|e| format!("对话框任务失败: {e}"))?;
    let Some(path) = picked else {
        return Ok(None);
    };
    let canon = path
        .canonicalize()
        .map_err(|e| format!("所选文件夹不可访问: {e}"))?;
    if !canon.is_dir() {
        return Err("所选路径不是文件夹".to_string());
    }
    reject_itools_data_root(&canon)?;
    let scope = FsScope {
        id: new_scope_id(),
        kind: "dir".to_string(),
        // 对外暴露的路径要去掉 `\?\` 扩展长度前缀：canonicalize 在 Windows 上返回的是
        // 扩展形式，直接透给插件既难看（用户界面上会出现 `\?\F:\...`），又危险——
        // 插件把它再传给 shell 类 API（openPath / showItemInFolder / 回收站）会直接失败，
        // trash 那边就是被同一个前缀坑了整整四种实现（见 paths_api::strip_extended_prefix）。
        // 内部校验仍然用未剥离的 canon，剥离只发生在「交给插件」这一步。
        path: super::paths_api::strip_extended_prefix(&canon)
            .to_string_lossy()
            .into_owned(),
        label: label_of(&canon),
        created: now_secs(),
    };
    persist_new_scope(&session, &registry, scope.clone())?;
    Ok(Some(scope))
}

/// 弹原生「选择文件」对话框；用户选中即视为把该文件授权给当前插件，返回一个持久 scope。
/// 用户取消返回 `Ok(None)`。
///
/// # Errors
/// 同 [`plugin_pick_dir`]，只是判定对象换成单个文件。
#[tauri::command]
pub async fn plugin_pick_file(
    opts: Option<PickFileOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Option<FsScope>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let opts = opts.unwrap_or_default();
    let title = opts.title.unwrap_or_else(|| "选择文件".to_string());
    let filters = opts.filters.unwrap_or_default();
    let picked = tauri::async_runtime::spawn_blocking(move || {
        let mut dlg = rfd::FileDialog::new().set_title(&title);
        for f in &filters {
            let exts: Vec<&str> = f.extensions.iter().map(String::as_str).collect();
            dlg = dlg.add_filter(&f.name, &exts);
        }
        dlg.pick_file()
    })
    .await
    .map_err(|e| format!("对话框任务失败: {e}"))?;
    let Some(path) = picked else {
        return Ok(None);
    };
    let canon = path
        .canonicalize()
        .map_err(|e| format!("所选文件不可访问: {e}"))?;
    if !canon.is_file() {
        return Err("所选路径不是文件".to_string());
    }
    reject_itools_data_root(&canon)?;
    let scope = FsScope {
        id: new_scope_id(),
        kind: "file".to_string(),
        // 对外暴露的路径要去掉 `\?\` 扩展长度前缀：canonicalize 在 Windows 上返回的是
        // 扩展形式，直接透给插件既难看（用户界面上会出现 `\?\F:\...`），又危险——
        // 插件把它再传给 shell 类 API（openPath / showItemInFolder / 回收站）会直接失败，
        // trash 那边就是被同一个前缀坑了整整四种实现（见 paths_api::strip_extended_prefix）。
        // 内部校验仍然用未剥离的 canon，剥离只发生在「交给插件」这一步。
        path: super::paths_api::strip_extended_prefix(&canon)
            .to_string_lossy()
            .into_owned(),
        label: label_of(&canon),
        created: now_secs(),
    };
    persist_new_scope(&session, &registry, scope.clone())?;
    Ok(Some(scope))
}

/// 列出当前插件已持有的全部 scope（供插件启动时判断「已经选过了，不用再弹一次」）。
///
/// # Errors
/// 插件未被授予 `fs-user-scope` 权限。
#[tauri::command]
pub fn plugin_fs_list_scopes(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<FsScope>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let dir = plugin_scope_store_dir(&session, &registry);
    let _guard = scope_lock();
    Ok(load_scopes(&dir).scopes)
}

/// 撤销当前插件持有的一个 scope（用户后悔了/插件用完了可以主动放弃）。不存在则报错（不会静默成功，
/// 避免插件传错 id 却以为撤销生效了）。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `scope_id` 不属于当前插件（未知/已被撤销）
/// - 存档写盘失败
#[tauri::command]
pub fn plugin_fs_revoke_scope(
    scope_id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let dir = plugin_scope_store_dir(&session, &registry);
    let _guard = scope_lock();
    let mut store = load_scopes(&dir);
    let before = store.scopes.len();
    store.scopes.retain(|s| s.id != scope_id);
    if store.scopes.len() == before {
        return Err("未知的 scope（可能已被撤销，或属于其他插件）".to_string());
    }
    save_scopes(&dir, &store)
}

// ==================== 命令：基于 scope 的文件操作 ====================

/// 枚举某 scope（必须是目录 scope）内 `sub_path` 处的目录内容；`sub_path` 缺省时列 scope 根。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `scope_id` 未知/不属于当前插件
/// - scope 是文件 scope（不能枚举）
/// - `sub_path` 含 `..`/绝对路径/盘符等越权写法
/// - 目标不存在、不是目录，或校验后落在 iTools 数据根内
#[tauri::command]
pub fn plugin_fs_list(
    scope_id: String,
    sub_path: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<FsEntry>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    if scope.kind != "dir" {
        return Err("该 scope 是单个文件，不能枚举目录".to_string());
    }
    let sub = sub_path.unwrap_or_default();
    let dir = resolve_existing(&scope, &sub)?;
    if !dir.is_dir() {
        return Err("目标不是文件夹".to_string());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("读取目录失败: {e}"))?;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            // 单个条目读失败（权限被拒等）不该拖垮整个列表，跳过即可。
            Err(_) => continue,
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(FsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.modified().ok().and_then(system_time_to_rfc3339),
        });
    }
    Ok(out)
}

/// 查询 scope 内某个文件/目录的元信息。目录 scope 下 `path` 缺省表示 scope 根本身；
/// 文件 scope 下 `path` 必须缺省（指的就是这个文件本身）。
///
/// # Errors
/// 同 [`plugin_fs_list`]，另外目标不存在时也会报错（stat 不隐藏"不存在"这个事实）。
#[tauri::command]
pub fn plugin_fs_stat(
    scope_id: String,
    path: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<FsEntry, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    // check_sub_for_scope 的校验已内嵌在 resolve_existing 里（见其文档第 1 步），这里不重复调用。
    let target = resolve_existing(&scope, &sub)?;
    let meta = std::fs::metadata(&target).map_err(|e| format!("读取元信息失败: {e}"))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| scope.label.clone());
    Ok(FsEntry {
        name,
        is_dir: meta.is_dir(),
        size: meta.len(),
        modified: meta.modified().ok().and_then(system_time_to_rfc3339),
    })
}

/// 对 scope 内一个文件求哈希（目前只支持 `sha256`，参数留出来是为了以后加算法不必破坏签名）。
/// 全文件流式读取（64KB 缓冲），不会把整份文件一次性载入内存。
///
/// # Errors
/// - `algo` 不是 `sha256`
/// - 同 [`plugin_fs_stat`] 的路径校验错误
/// - 目标不是文件，或读取过程中出错
#[tauri::command]
pub async fn plugin_fs_hash(
    scope_id: String,
    path: Option<String>,
    algo: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    if !algo.eq_ignore_ascii_case("sha256") {
        return Err(format!("不支持的哈希算法「{algo}」，目前仅支持 sha256"));
    }
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    // check_sub_for_scope 的校验已内嵌在 resolve_existing 里（见其文档第 1 步），这里不重复调用。
    let target = resolve_existing(&scope, &sub)?;
    tauri::async_runtime::spawn_blocking(move || {
        if !target.is_file() {
            return Err("目标不是文件".to_string());
        }
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut file = std::fs::File::open(&target).map_err(|e| format!("打开文件失败: {e}"))?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("读取文件失败: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(|e| format!("哈希任务失败: {e}"))?
}

/// 读取 scope 内一个文件的**全部内容**，以 base64 返回（不假设文本编码，二进制文件也能读）。
/// 大文件建议改用 [`plugin_fs_read_chunk`] 分块读，避免一次性占用大量内存。
///
/// # Errors
/// 同 [`plugin_fs_stat`]，另外目标不是文件、读取失败也会报错。
#[tauri::command]
pub async fn plugin_fs_read(
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
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    // check_sub_for_scope 的校验已内嵌在 resolve_existing 里（见其文档第 1 步），这里不重复调用。
    let target = resolve_existing(&scope, &sub)?;
    tauri::async_runtime::spawn_blocking(move || {
        if !target.is_file() {
            return Err("目标不是文件".to_string());
        }
        let bytes = std::fs::read(&target).map_err(|e| format!("读取文件失败: {e}"))?;
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    })
    .await
    .map_err(|e| format!("读取任务失败: {e}"))?
}

/// 按 `offset`/`len` 分块读取 scope 内一个文件（大文件专用，避免整份载入内存），返回 base64。
/// 到文件末尾时实际返回字节数可能小于 `len`（不会报错，正常的"读到头了"）。
///
/// # Errors
/// - `len` 为 0 或超过单块上限（16MB）
/// - 同 [`plugin_fs_stat`] 的路径校验错误
/// - 目标不是文件、定位/读取失败
#[tauri::command]
pub async fn plugin_fs_read_chunk(
    scope_id: String,
    path: Option<String>,
    offset: u64,
    len: u64,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    if len == 0 {
        return Err("len 必须大于 0".to_string());
    }
    if len > MAX_CHUNK_LEN {
        return Err(format!("单次分块最大 {MAX_CHUNK_LEN} 字节，请拆成多次读取"));
    }
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    // check_sub_for_scope 的校验已内嵌在 resolve_existing 里（见其文档第 1 步），这里不重复调用。
    let target = resolve_existing(&scope, &sub)?;
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        if !target.is_file() {
            return Err("目标不是文件".to_string());
        }
        let mut file = std::fs::File::open(&target).map_err(|e| format!("打开文件失败: {e}"))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("定位失败: {e}"))?;
        // len 已在上面校验 <= MAX_CHUNK_LEN（16MB），as usize 在本项目的 64 位目标上不会截断。
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).map_err(|e| format!("读取失败: {e}"))?;
        buf.truncate(n);
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.encode(buf))
    })
    .await
    .map_err(|e| format!("读取任务失败: {e}"))?
}

/// 向 scope 内写入一个文件（base64 内容，覆盖式写）。**不会自动新建目录**：目标所在的目录必须
/// 已经存在（原因见模块文档）；文件 scope 下 `path` 必须缺省，只能写回这一个被授权的文件本身。
///
/// # Errors
/// - 插件未被授予 `fs-user-scope` 权限
/// - `scope_id` 未知/不属于当前插件
/// - `path` 非法（越权写法、目录 scope 下留空）、或所在目录不存在
/// - 校验后落点在 iTools 数据根内
/// - `content_b64` 不是合法 base64、写盘失败
#[tauri::command]
pub async fn plugin_fs_write(
    scope_id: String,
    path: Option<String>,
    content_b64: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, PERMISSION) {
        return Err("插件未获授权访问所选文件夹（请在「插件管理」里授权 fs-user-scope）".to_string());
    }
    let scope = resolve_scope(&session, &registry, &scope_id)?;
    let sub = path.unwrap_or_default();
    let target = resolve_for_write(&scope, &sub)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(content_b64.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::write(&target, bytes).map_err(|e| format!("写文件失败: {e}"))
    })
    .await
    .map_err(|e| format!("写入任务失败: {e}"))?
}

// ==================== 内部辅助：scope 存档 ====================

/// scope 存档读写的进程级互斥锁：同一插件短时间内并发调用 pick/revoke 时，避免「读-改-写」
/// 三步互相踩踏导致后写的那次把先写的那次覆盖掉。锁很轻（只包一段小 JSON 文件 IO），
/// 全插件共用一把即可，不值得为每个插件单独开一把。
static SCOPE_LOCK: StdMutex<()> = StdMutex::new(());

/// 拿存档锁；此锁只保护内存里的临界区，不会因为业务错误 panic，中毒（poison）后直接取回内部
/// guard 继续用即可——总比在这类非核心的辅助锁上 `unwrap()` 让插件的每一次文件操作都跟着崩安全。
fn scope_lock() -> std::sync::MutexGuard<'static, ()> {
    match SCOPE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 某会话的 scope 存档所在目录：**按插件 id + dev/正式 隔离**，与 `commands::plugin_data_dir` /
/// `commands::session_files_dir` 同一套规则（那两个函数是 commands.rs 内的私有函数，本文件按
/// 任务边界只能新增文件、不能改 commands.rs 把它们导出，所以这里按同样的规则独立推导一遍——
/// 两处都只认 `crate::paths::data_root()` 这一个真源，不会出现两份不一致的路径）。
fn plugin_scope_store_dir(session: &ActiveSession, registry: &PluginRegistry) -> PathBuf {
    if session.dev {
        registry.dev.files_root.join(&session.id)
    } else {
        crate::paths::data_root().join("plugin-data").join(&session.id)
    }
}

/// 读取某插件的 scope 存档；文件不存在或损坏（用户手改/磁盘问题）时视为空存档，不让插件因此崩掉，
/// 只是相当于「之前的授权全部丢了，需要用户重新选一次」。
fn load_scopes(dir: &Path) -> ScopeStore {
    let file = dir.join(SCOPE_STORE_FILE);
    match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => ScopeStore::default(),
    }
}

/// 落盘某插件的 scope 存档（整份覆盖写）。
fn save_scopes(dir: &Path, store: &ScopeStore) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建插件数据目录失败: {e}"))?;
    let file = dir.join(SCOPE_STORE_FILE);
    let text = serde_json::to_string_pretty(store).map_err(|e| format!("序列化 scope 存档失败: {e}"))?;
    std::fs::write(&file, text).map_err(|e| format!("写 scope 存档失败: {e}"))
}

/// 追加一个新 scope 并落盘。
fn persist_new_scope(session: &ActiveSession, registry: &PluginRegistry, scope: FsScope) -> Result<(), String> {
    let dir = plugin_scope_store_dir(session, registry);
    let _guard = scope_lock();
    let mut store = load_scopes(&dir);
    store.scopes.push(scope);
    save_scopes(&dir, &store)
}

/// 按 `scope_id` 从当前插件的存档里取出一个 scope；查不到（未知 id / 属于别的插件 / 已被撤销）
/// 一律返回同一句模糊错误，不区分「不存在」和「属于别人」，避免给插件提供枚举其它插件 scope
/// 是否存在的探测手段。
pub(crate) fn resolve_scope(session: &ActiveSession, registry: &PluginRegistry, scope_id: &str) -> Result<FsScope, String> {
    let dir = plugin_scope_store_dir(session, registry);
    let _guard = scope_lock();
    load_scopes(&dir)
        .scopes
        .into_iter()
        .find(|s| s.id == scope_id)
        .ok_or_else(|| "未知的 scope（可能已过期、被撤销，或属于其他插件）".to_string())
}

/// 生成一个不透明、不可预测的 scope id（sha256 十六进制串）。
///
/// 输入组合：高精度纳秒时间戳 + 进程内自增计数器 + 进程 PID + 一次堆分配的地址（ASLR 带来的
/// 额外熵）。这不是密码学随机数生成器（项目未引入 `rand`，见模块文档的依赖最小化取舍），
/// 但对「同机其它插件靠猜测撞出别的插件的 scope id」这个威胁模型已经足够：既没有可预测的
/// 递增规律，也不会在两次调用间重复。
fn new_scope_id() -> String {
    use sha2::{Digest, Sha256};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let heap_marker: Box<u8> = Box::new(0);
    let addr = std::ptr::addr_of!(*heap_marker) as usize;
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(n.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(addr.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

/// 目录/文件的展示短名字：取 basename；取不到（如盘符根 `C:\`）就回退成完整路径。
fn label_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `SystemTime` → 本地 RFC3339 字符串；拿不到（极少数场景，如系统时间早于 UNIX_EPOCH）则 `None`。
fn system_time_to_rfc3339(t: SystemTime) -> Option<String> {
    let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs();
    chrono::DateTime::from_timestamp(secs as i64, 0).map(|d| d.with_timezone(&chrono::Local).to_rfc3339())
}

// ==================== 内部辅助：路径校验（本模块的安全核心） ====================

/// 硬黑名单：iTools 自身的数据根一律拒绝访问，**即使用户在对话框里亲自选中了它**。
///
/// 理由见模块顶部文档：`data_root()/plugin-data/` 装着所有插件的数据，放行这里等于打破
/// 插件间数据隔离；这条判断在 grant 时和之后每一次文件操作前都会重新做一遍（不缓存结论），
/// 防的是「授权时目录还干净、之后被换成/软链到数据根」这类事后腾挪。
pub(crate) fn reject_itools_data_root(path: &Path) -> Result<(), String> {
    // 挡的是**两个构建**的数据根（release 的 itools 与 debug 的 itools-dev），不是只挡当前这个。
    // 只挡当前构建会留下一个真实的横向口子：同机同时装了两种构建时（开发者常态），
    // release 版插件照样能选中 debug 版的数据目录、读走那边所有插件的数据，反之亦然。
    for root in crate::paths::all_data_roots() {
        // 目录可能不存在（比如这台机器只装了 release），canonicalize 失败就跳过这一个——
        // 不存在的目录本来也选不中，不构成放行。
        let Ok(canon_root) = root.canonicalize() else {
            continue;
        };
        if path == canon_root || path.starts_with(&canon_root) {
            return Err(
                "禁止访问 iTools 自身的数据目录（会破坏插件间数据隔离），请重新选择其它文件夹或文件"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// 校验一个「scope 内相对路径」的写法：只允许 `Normal`/`CurDir` 路径组件，拒绝绝对路径、
/// 盘符前缀、根、`..`。空字符串合法，代表 scope 根本身。
///
/// 与 `commands::sandbox_relative` 同一思路（那边管的是插件自己的沙盒），这里管的是用户亲自
/// 授权的外部真实目录，一旦逃逸后果是任意本地文件，校验只能更严、不能更松；两处规则各自独立
/// 维护是因为本文件不允许改 commands.rs 把那个私有函数导出。
pub(crate) fn scope_relative(path: &str) -> Result<&Path, String> {
    let rel = Path::new(path);
    let ok = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
    if !ok {
        return Err("只能访问 scope 内的相对路径（禁绝对路径/盘符/根/..）".to_string());
    }
    Ok(rel)
}

/// 文件 scope 下，业务参数里的子路径必须留空（文件 scope 只对应那一个文件，没有「子路径」这回事）。
fn check_sub_for_scope(scope: &FsScope, sub: &str) -> Result<(), String> {
    if scope.kind == "file" && !sub.is_empty() {
        return Err("该 scope 是单个文件，路径参数需留空（直接指向这个文件）".to_string());
    }
    Ok(())
}

/// 解析一个**必须已存在**的目标路径（用于 list/stat/hash/read/read_chunk）：
/// 1. 校验 `sub` 的写法（见 [`scope_relative`]）
/// 2. 拼到 scope 根下，`canonicalize`（顺带证明它确实存在，解析掉符号链接/`..`）
/// 3. 重新 `canonicalize` scope 根本身（scope 授权之后可能被移动/删除，每次都验，不信任缓存）
/// 4. 校验目标仍在 scope 根之内
/// 5. 过硬黑名单（[`reject_itools_data_root`]）
pub(crate) fn resolve_existing(scope: &FsScope, sub: &str) -> Result<PathBuf, String> {
    check_sub_for_scope(scope, sub)?;
    let rel = scope_relative(sub)?;
    let base = PathBuf::from(&scope.path);
    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("scope 已失效（原目录/文件不可访问）: {e}"))?;
    reject_itools_data_root(&canon_base)?;
    let joined = base.join(rel);
    let canon = joined
        .canonicalize()
        .map_err(|e| format!("路径不存在或不可访问: {e}"))?;
    if canon != canon_base && !canon.starts_with(&canon_base) {
        return Err("路径越出 scope 范围".to_string());
    }
    reject_itools_data_root(&canon)?;
    Ok(canon)
}

/// 解析一个**用于写入**的目标路径：目标文件本身可以不存在（写入即创建），但它所在的目录必须
/// 已经存在——本 API 不会替插件把目录结构建出来（理由见模块文档）。文件 scope 下只能写回
/// 那一个被授权的文件本身（`sub` 必须留空）。
pub(crate) fn resolve_for_write(scope: &FsScope, sub: &str) -> Result<PathBuf, String> {
    let base = PathBuf::from(&scope.path);
    let canon_base = base
        .canonicalize()
        .map_err(|e| format!("scope 已失效（原目录/文件不可访问）: {e}"))?;
    reject_itools_data_root(&canon_base)?;

    if scope.kind == "file" {
        if !sub.is_empty() {
            return Err("该 scope 是单个文件，只能写入这个文件本身（path 留空）".to_string());
        }
        return Ok(canon_base);
    }

    if sub.is_empty() {
        return Err("必须指定 scope 内要写入的文件名".to_string());
    }
    let rel = scope_relative(sub)?;
    let joined = base.join(rel);
    let parent = joined.parent().ok_or_else(|| "非法路径".to_string())?;
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("目标所在目录不存在: {e}"))?;
    if canon_parent != canon_base && !canon_parent.starts_with(&canon_base) {
        return Err("路径越出 scope 范围".to_string());
    }
    reject_itools_data_root(&canon_parent)?;
    let file_name = joined.file_name().ok_or_else(|| "非法路径（缺文件名）".to_string())?;
    Ok(canon_parent.join(file_name))
}
