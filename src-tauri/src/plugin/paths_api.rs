//! 「命名路径白名单」+「送回收站」：给磁盘清理类插件开放系统位置访问，但不给任意路径访问。
//!
//! # 为什么是白名单命名位置，而不是开放任意路径
//!
//! 磁盘清理插件确实需要碰 `C:\Windows\Temp`、回收站、浏览器缓存这些系统位置，但如果直接给
//! 插件一个「任意路径读写」的口子，等于把整台机器的文件系统都暴露给了第三方 JS 代码——
//! 一个写歪了的清理插件（或者干脆是恶意插件）就能删用户的文档、改系统文件。
//! 所以这里只开放一组**写死在 Rust 里**的命名位置（`temp` / `windowsTemp` / `downloads` / …），
//! 插件只能传一个名字换路径，**不能**自己拼一个路径进来当作「命名位置」使用；
//! [`plugin_trash`] 在真正执行删除前还会用这份白名单反向校验目标路径必须落在某个命名位置之内。
//!
//! # 为什么只给「送回收站」，不给「真删」
//!
//! 回收站可撤销，真删不可逆。插件的判断逻辑再谨慎也可能有 bug（扫描逻辑算错、正则误匹配、
//! 用户没看清就点了清理），一旦是真删，后果就是用户数据永久丢失且无法追责到具体是哪次误判。
//! 送回收站保留了「用户后悔了还能从回收站捞回来」这条退路，真删的最终确认交给宿主弹窗，
//! 不下放给插件——这条规则没有例外，本文件不提供、也不会提供任何真删 API。
//!
//! # 权限门禁
//!
//! - [`plugin_paths_resolve`] / [`plugin_paths_scan`]：需要插件声明并被授权 `fs-named-path`
//! - [`plugin_trash`]：需要插件声明并被授权 `fs-trash`（与上面分开，读位置 ≠ 允许删东西）
//!
//! # 硬黑名单
//!
//! 无论走哪个命令，目标路径只要落在 [`crate::paths::data_root`]（iTools 自己的数据根，
//! 内含所有插件的 `plugin-data/`）之内，一律拒绝——这条不接受例外，也不受任何白名单命名
//! 位置的覆盖（因为 `appData`/`localAppData` 在某些用户环境下可能与之存在路径重叠）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::PluginRegistry;
use crate::logging::ilog;
use crate::settings::SettingsStore;

/// 支持的命名位置清单（唯一真源，`resolve_named` 与文档都从这里对齐）。
const NAMED_PATH_NAMES: [&str; 9] = [
    "temp",
    "windowsTemp",
    "downloads",
    "desktop",
    "documents",
    "appData",
    "localAppData",
    "recycleBin",
    "browserCache",
];

/// 单个命名位置解析结果（`browserCache` 这类可能有多个候选，故命令层一律返回 `Vec`）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamedPathInfo {
    /// 候选的可读标签：普通命名位置就是 name 本身；`browserCache` 下是具体浏览器名
    /// （如 "Chrome" / "Firefox (xxxxxxxx.default-release)"）。
    pub label: String,
    /// 绝对路径（不保证存在，见 `exists`）。
    pub path: String,
    /// 该路径当前是否存在于磁盘上。
    pub exists: bool,
    /// 是否具备写权限（通过实际创建+删除一个探测文件判定，不存在时恒为 false）。
    pub writable: bool,
}

/// 查询某个命名位置的绝对路径（白名单内的名字，不接受任意值）。
///
/// 支持的 `name`：`temp` / `windowsTemp` / `downloads` / `desktop` / `documents` /
/// `appData` / `localAppData` / `recycleBin` / `browserCache`。`browserCache` 可能识别到
/// 多个浏览器，故返回值恒为数组（其余命名位置也统一包成单元素数组，接口形状一致）。
///
/// # Errors
/// - 插件未被授权 `fs-named-path` 时返回错误说明
/// - `name` 不在白名单内时返回「未知的命名路径」
/// - 部分系统目录（如 `downloads`）在极少数系统配置下可能无法被 `dirs` crate 定位，此时返回
///   「无法确定系统「xxx」目录」
#[tauri::command]
pub fn plugin_paths_resolve(
    name: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<NamedPathInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "fs-named-path") {
        return Err("插件未获授权访问系统位置（请在「插件管理」里授权 fs-named-path）".to_string());
    }
    let candidates = resolve_named(&name)?;
    Ok(candidates
        .into_iter()
        .map(|(label, path)| describe(label, path))
        .collect())
}

/// 扫描选项：限制递归深度与返回条数，避免在 `C:\Windows\Temp` 这类超大目录上跑到卡死。
#[derive(Deserialize, Default)]
// 入参字段统一用 camelCase：这些结构体是**插件页直接传进来的 JSON**，JS 侧的自然写法就是
// camelCase，且 Tauri 对命令顶层参数本来就做 camelCase 转换。若这里不标，插件传 `maxDepth`
// 会因为对不上 `max_depth` 而被静默忽略、退回默认值——不报错、不生效，最难查的那类问题。
#[serde(rename_all = "camelCase")]
pub struct ScanOptions {
    /// 递归深度上限（含义同 `walkdir` 的 `max_depth`），缺省 3，硬上限 8。
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// 返回的条目数上限，缺省 2000，硬上限 20000；超出部分仍计入 `file_count`/`total_size`，
    /// 但不会出现在 `items` 里，此时 `truncated` 为 true。
    #[serde(default)]
    pub max_items: Option<u32>,
}

/// 单个扫描到的条目（文件或目录）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    /// 绝对路径。
    pub path: String,
    /// 字节数；目录恒为 0（不做递归求和，避免和 `total_size` 重复计数）。
    pub size: u64,
    /// 是否为目录。
    pub is_dir: bool,
}

/// 只读扫描结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// 扫描到的文件总数（不含目录），受 `max_depth` 限制，不受 `max_items` 限制。
    pub file_count: u64,
    /// 扫描到的文件总字节数，统计口径同 `file_count`。
    pub total_size: u64,
    /// 具体条目列表，最多 `max_items` 条。
    pub items: Vec<ScanItem>,
    /// `items` 是否因触达 `max_items` 而被截断。
    ///
    /// 注意：这个标志**只反映条数截断**，不反映深度截断——如果某个子目录因为超过
    /// `max_depth` 而没有被展开，这里不会体现，调用方如需完整体积应自行加大 `max_depth`
    /// 并接受相应的耗时。
    pub truncated: bool,
}

const SCAN_DEFAULT_DEPTH: u32 = 3;
const SCAN_MAX_DEPTH_CEILING: u32 = 8;
const SCAN_DEFAULT_ITEMS: u32 = 2_000;
const SCAN_MAX_ITEMS_CEILING: u32 = 20_000;

/// 只读枚举某个命名位置下的内容，返回文件数、总体积、可清理项列表，供插件自己做 UI。
///
/// 纯只读，不删除、不移动任何东西；`browserCache` 这类多候选命名位置会把所有存在的候选
/// 目录合并统计。跑在 `spawn_blocking` 里，不阻塞 UI 线程。
///
/// # Errors
/// - 插件未被授权 `fs-named-path` 时返回错误说明
/// - `name` 不在白名单内时返回「未知的命名路径」
/// - 后台扫描任务 panic（极少见，如遇到损坏的文件系统条目）时返回「扫描任务异常」
#[tauri::command]
pub async fn plugin_paths_scan(
    name: String,
    opts: Option<ScanOptions>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<ScanResult, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "fs-named-path") {
        return Err("插件未获授权访问系统位置（请在「插件管理」里授权 fs-named-path）".to_string());
    }
    let candidates = resolve_named(&name)?;
    let opts = opts.unwrap_or_default();
    let max_depth = opts
        .max_depth
        .unwrap_or(SCAN_DEFAULT_DEPTH)
        .clamp(1, SCAN_MAX_DEPTH_CEILING);
    let max_items = opts
        .max_items
        .unwrap_or(SCAN_DEFAULT_ITEMS)
        .clamp(1, SCAN_MAX_ITEMS_CEILING);

    tauri::async_runtime::spawn_blocking(move || scan_locations(&candidates, max_depth, max_items))
        .await
        .map_err(|e| format!("扫描任务异常: {e}"))?
}

/// 单条送回收站的结果（批量接口，逐条报告，不因某一条失败就放弃其余条目）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrashOutcome {
    /// 原样回传调用方传入的路径字符串，便于调用方对账。
    pub path: String,
    /// 是否成功送入回收站。
    pub ok: bool,
    /// `ok` 为 false 时的错误说明。
    pub error: Option<String>,
}

/// 把指定路径**送进回收站**（可从「回收站」还原，不是真删）。
///
/// 每个路径都会先校验确实落在某个白名单命名位置之内（且不在 [`crate::paths::data_root`]
/// 硬黑名单内），校验不通过或系统调用失败都只让**这一条**记为失败，不影响批次里的其它路径。
/// 跑在 `spawn_blocking` 里，不阻塞 UI 线程。
///
/// # Errors
/// - 插件未被授权 `fs-trash` 时整体返回错误说明（此时不执行任何删除）
/// - 单条路径的失败体现在返回的 [`TrashOutcome::error`] 里，不会导致整个调用返回 `Err`
#[tauri::command]
pub async fn plugin_trash(
    paths: Vec<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<TrashOutcome>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "fs-trash") {
        return Err("插件未获授权送入回收站（请在「插件管理」里授权 fs-trash）".to_string());
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    tauri::async_runtime::spawn_blocking(move || trash_paths(&paths))
        .await
        .map_err(|e| format!("回收站任务异常: {e}"))
}

// ---------- 命名位置解析 ----------

/// 把命名位置解析为一组 `(标签, 绝对路径)`。绝大多数名字只返回一条；`browserCache`
/// 可能返回多条（每识别到一个浏览器一条）。
fn resolve_named(name: &str) -> Result<Vec<(String, PathBuf)>, String> {
    match name {
        "temp" => Ok(vec![("temp".to_string(), std::env::temp_dir())]),
        "windowsTemp" => Ok(vec![("windowsTemp".to_string(), windows_dir().join("Temp"))]),
        "downloads" => single("downloads", dirs::download_dir()),
        "desktop" => single("desktop", dirs::desktop_dir()),
        "documents" => single("documents", dirs::document_dir()),
        "appData" => single("appData", dirs::data_dir()),
        "localAppData" => single("localAppData", dirs::data_local_dir()),
        "recycleBin" => Ok(vec![("recycleBin".to_string(), recycle_bin_dir())]),
        "browserCache" => Ok(browser_cache_candidates()),
        other => Err(format!(
            "未知的命名路径「{other}」，仅支持: {}",
            NAMED_PATH_NAMES.join(" / ")
        )),
    }
}

/// 单候选命名位置的通用包装：`dirs` crate 取不到就是明确的错误，不用假路径顶替。
fn single(label: &str, dir: Option<PathBuf>) -> Result<Vec<(String, PathBuf)>, String> {
    dir.map(|p| vec![(label.to_string(), p)])
        .ok_or_else(|| format!("无法确定系统「{label}」目录"))
}

/// Windows 安装目录（一般是 `C:\Windows`）：优先取 `SystemRoot` 环境变量，取不到才回退硬编码。
fn windows_dir() -> PathBuf {
    std::env::var("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Windows"))
}

/// 回收站在磁盘上的真实落点：系统盘根目录下的 `$Recycle.Bin`（隐藏系统文件夹，
/// 内部按用户 SID 分子目录）。这是一个真实存在的磁盘路径，不是 Shell 虚拟命名空间，
/// 所以能直接用 `std::fs` 枚举/校验；当前登录用户通常只对自己 SID 的子目录有权限，
/// 这一点在 `writable` 探测里会如实体现（探测失败就是 false，不伪造）。
fn recycle_bin_dir() -> PathBuf {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    PathBuf::from(format!("{drive}\\$Recycle.Bin"))
}

/// 主流浏览器缓存目录探测：能识别几个就返回几个，识别不到的浏览器不会出现在结果里
/// （不会返回一堆全是 `exists=false` 的假条目占位）。
fn browser_cache_candidates() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Some(local) = dirs::data_local_dir() else {
        return out;
    };

    let chrome = local
        .join("Google")
        .join("Chrome")
        .join("User Data")
        .join("Default")
        .join("Cache");
    if chrome.exists() {
        out.push(("Chrome".to_string(), chrome));
    }

    let edge = local
        .join("Microsoft")
        .join("Edge")
        .join("User Data")
        .join("Default")
        .join("Cache");
    if edge.exists() {
        out.push(("Edge".to_string(), edge));
    }

    // Firefox 的配置目录名带随机后缀（如 xxxxxxxx.default-release），需要枚举
    // Profiles 目录，挑出名字里带 "default" 的那些（Firefox 官方命名惯例）。
    let ff_profiles = local.join("Mozilla").join("Firefox").join("Profiles");
    if let Ok(entries) = std::fs::read_dir(&ff_profiles) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains("default") {
                continue;
            }
            let cache = path.join("cache2");
            if cache.exists() {
                out.push((format!("Firefox ({name})"), cache));
            }
        }
    }

    out
}

/// 把 `(标签, 路径)` 变成对外的 [`NamedPathInfo`]：真实做 exists/writable 探测，不伪造。
fn describe(label: String, path: PathBuf) -> NamedPathInfo {
    let exists = path.exists();
    let writable = exists && probe_writable(&path);
    NamedPathInfo {
        label,
        // 与 `itools.sys.getPath` 用同一套规范化：各来源对尾部分隔符的约定不统一
        // （`std::env::temp_dir()` 带、`dirs::*` 不带），同一个 API 返回两种形态会让
        // 插件拼路径时拼出 `...\Temp\\x.txt` 这种双分隔符的串。真机验收时正是这么踩到的。
        path: crate::plugin::sysinfo::normalize_dir(&path),
        exists,
        writable,
    }
}

/// 通过实际创建 + 删除一个探测文件判定目录是否可写；不做「看权限位猜」这种不可靠判断
/// （尤其 Windows 的 ACL 保护目录，只读属性位并不能反映真实可写性）。
fn probe_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".itools-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ---------- 只读扫描 ----------

/// 合并扫描一组候选根目录，聚合成一份结果。
fn scan_locations(
    candidates: &[(String, PathBuf)],
    max_depth: u32,
    max_items: u32,
) -> Result<ScanResult, String> {
    let mut file_count: u64 = 0;
    let mut total_size: u64 = 0;
    let mut items = Vec::new();
    let mut truncated = false;

    for (_, root) in candidates {
        if !root.exists() {
            continue;
        }
        // min_depth(1) 跳过根目录自身，只枚举根目录下的内容；单个条目读取失败（权限不足、
        // 文件在扫描期间被移走等）用 filter_map(Result::ok) 跳过，不因个别条目中断整体扫描。
        let walker = walkdir::WalkDir::new(root)
            .max_depth(max_depth as usize)
            .min_depth(1)
            .into_iter()
            .filter_map(std::result::Result::ok);

        for entry in walker {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let is_dir = meta.is_dir();
            let size = if is_dir { 0 } else { meta.len() };
            if !is_dir {
                file_count += 1;
                total_size += size;
            }
            if items.len() < max_items as usize {
                items.push(ScanItem {
                    path: entry.path().to_string_lossy().to_string(),
                    size,
                    is_dir,
                });
            } else {
                truncated = true;
            }
        }
    }

    Ok(ScanResult {
        file_count,
        total_size,
        items,
        truncated,
    })
}

// ---------- 送回收站 ----------

/// 逐条校验并送回收站，单条失败不影响其余条目。
fn trash_paths(paths: &[String]) -> Vec<TrashOutcome> {
    let allowed_roots = match all_allowed_roots() {
        Ok(roots) => roots,
        Err(e) => {
            return paths
                .iter()
                .map(|p| TrashOutcome {
                    path: p.clone(),
                    ok: false,
                    error: Some(e.clone()),
                })
                .collect();
        }
    };

    let mut out: Vec<TrashOutcome> = Vec::with_capacity(paths.len());
    // 原生实现失败的那些，攒起来整批走回退（回退要起 PowerShell 进程，逐条起受不了）
    let mut retry: Vec<(usize, PathBuf)> = Vec::new();

    for (i, p) in paths.iter().enumerate() {
        match validate_and_trash(p, &allowed_roots) {
            Ok(()) => out.push(TrashOutcome {
                path: p.clone(),
                ok: true,
                error: None,
            }),
            Err(e) => {
                // 只有「校验通过、但系统调用失败」才值得回退；路径不在白名单/落在数据根里
                // 这类**安全拒绝**必须原样保留，绝不能被回退路径绕过去。
                let denied = e.contains("拒绝访问") || e.contains("不在允许") || e.contains("路径不存在");
                if !denied {
                    if let Ok(canon) = std::fs::canonicalize(p) {
                        retry.push((i, strip_extended_prefix(&canon)));
                    }
                }
                out.push(TrashOutcome {
                    path: p.clone(),
                    ok: false,
                    error: Some(e),
                });
            }
        }
    }

    // 走原生还是走回退，从返回值上**完全看不出来**——两条路都只回一个 ok:true。
    // 原生这条路真机上栽过一次（扩展长度前缀，见 strip_extended_prefix），修好之后
    // 如果它又悄悄坏了，PowerShell 回退会把它兜住，于是「功能正常」而每次删除都多起一个进程。
    // 所以这里必须留一行日志，把实际走的分支钉进 itools.log，否则这种退化永远没人发现。
    let native_ok = out.iter().filter(|o| o.ok).count();
    if retry.is_empty() {
        ilog!(
            "[iTools][plugin] trash: 请求 {} 条，原生 IFileOperation 成功 {} 条，未触发回退",
            paths.len(),
            native_ok
        );
    } else {
        ilog!(
            "[iTools][plugin] trash: 请求 {} 条，原生成功 {} 条，{} 条转 PowerShell 回退；首条原生报错：{}",
            paths.len(),
            native_ok,
            retry.len(),
            out[retry[0].0].error.clone().unwrap_or_default()
        );
    }

    if !retry.is_empty() {
        let targets: Vec<PathBuf> = retry.iter().map(|(_, p)| p.clone()).collect();
        match recycle_via_powershell(&targets) {
            Ok(()) => {
                for (i, p) in &retry {
                    // 回退成功也要复核文件确实没了，不能只信退出码
                    if !p.exists() {
                        out[*i].ok = true;
                        out[*i].error = None;
                    }
                }
            }
            Err(e) => {
                for (i, _) in &retry {
                    let prev = out[*i].error.clone().unwrap_or_default();
                    out[*i].error = Some(format!("{prev}；回退方案亦失败：{e}"));
                }
            }
        }
    }
    out
}

/// 收集所有白名单命名位置当前解析出的根路径（含 `browserCache` 的多个候选），
/// 用于校验 [`plugin_trash`] 的目标路径是否落在允许范围内。
///
/// 个别命名位置解析失败（如系统定位不到「下载」目录）就跳过它，不让这一条失败拖累
/// 其它命名位置的校验能力。
fn all_allowed_roots() -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    for name in NAMED_PATH_NAMES {
        if let Ok(candidates) = resolve_named(name) {
            roots.extend(candidates.into_iter().map(|(_, p)| p));
        }
    }
    if roots.is_empty() {
        return Err("无法确定任何允许访问的系统位置".to_string());
    }
    Ok(roots)
}

/// 校验单条路径确实落在白名单命名位置内、且不在 iTools 数据根黑名单内，通过后执行送回收站。
fn validate_and_trash(path_str: &str, allowed_roots: &[PathBuf]) -> Result<(), String> {
    let target = PathBuf::from(path_str);
    if !target.exists() {
        return Err("路径不存在".to_string());
    }
    let canon = std::fs::canonicalize(&target).map_err(|e| format!("路径解析失败: {e}"))?;

    // 硬黑名单：iTools 自己的数据根（含所有插件的 plugin-data/）一律拒绝，不接受例外。
    // 挡 release 与 debug **两个**数据根：只挡当前构建的话，同机装了两种构建时
    // 一边的插件就能把另一边的插件数据整个送进回收站。
    for data_root in crate::paths::all_data_roots() {
        if let Ok(canon_data_root) = std::fs::canonicalize(&data_root) {
            if canon == canon_data_root
                || canon.starts_with(&canon_data_root)
                || canon_data_root.starts_with(&canon)
            {
                return Err(
                    "拒绝访问：该路径位于 iTools 自身数据目录内，任何插件都不允许触碰".to_string(),
                );
            }
        }
    }

    // 白名单：必须是某个命名位置根目录的**严格子路径**（不能是根目录本身，避免插件把
    // 「下载」「桌面」这类特殊 Shell 目录整个送进回收站）。
    let permitted = allowed_roots.iter().any(|root| match std::fs::canonicalize(root) {
        Ok(canon_root) => canon != canon_root && canon.starts_with(&canon_root),
        Err(_) => false,
    });
    if !permitted {
        return Err(format!(
            "路径不在允许的命名位置白名单内（须为以下命名位置的子路径: {}）",
            NAMED_PATH_NAMES.join(" / ")
        ));
    }

    // 传给 shell 之前必须摘掉 `\?\` 前缀，理由见 strip_extended_prefix
    send_to_recycle_bin(&strip_extended_prefix(&canon))
}

/// 去掉 Windows 扩展长度路径前缀（`\?\` / `\?\UNC\`）。
///
/// # 这是四次失败的共同根因
///
/// `std::fs::canonicalize` 在 Windows 上返回的是**扩展长度形式**：
/// `\?\C:\Users\...`。这个前缀对 Win32 文件 API 是合法的，但 **shell 层与 .NET 都不认**：
///
/// - `SHCreateItemFromParsingName` → `E_INVALIDARG (0x80070057)`
/// - `SHFileOperationW` → `124 (DE_INVALIDFILES)`
/// - `Microsoft.VisualBasic.FileIO.FileSystem::DeleteFile` → 「不支持给定路径的格式」
///
/// 真机验收时先后换了「专用 STA 线程」「裸 PCWSTR 取代 HSTRING」「老 API SHFileOperationW」
/// 「PowerShell 回退」四种实现，全都卡在同一处——因为它们收到的都是同一个带前缀的路径。
/// 校验逻辑必须用 canonicalize（要解析符号链接、防绕过），所以只能在**真正调用 shell 之前**
/// 把前缀摘掉。
pub(crate) fn strip_extended_prefix(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        // UNC 形式：`\\?\UNC\server\share` → `\\server\share`
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
}

/// 用 PowerShell 的 `Microsoft.VisualBasic.FileIO.FileSystem` 把一批路径送进回收站。
///
/// # 这是回退路径，不是首选
///
/// 首选是 [`recycle_in_sta`] 里的原生 `IFileOperation`。但真机验收时它在本机稳定失败
/// （`SHCreateItemFromParsingName` 返回 `E_INVALIDARG`；换老 API `SHFileOperationW` 是
/// `DE_INVALIDFILES`；换裸 `PCWSTR`、换专用 STA 线程，结果都一样），而「送回收站」这个
/// 能力本身必须真的能用——磁盘清理类插件全指着它，一个「调了不报错但文件还在」的实现
/// 比没有更糟。
///
/// 所以保留一条不依赖 shell COM 绑定的通路：`VisualBasic.FileIO` 是 .NET 自带的成熟实现，
/// `SendToRecycleBin` 语义明确、同步返回。代价是要起一个 PowerShell 进程，因此
/// **整批只起一次**——磁盘清理动辄几百个文件，逐条起进程是不能接受的。
///
/// 它处理的仍然只是**已经过白名单与数据根黑名单校验**的路径，安全边界不因换实现而放宽。
#[cfg(windows)]
fn recycle_via_powershell(paths: &[PathBuf]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::windows::process::CommandExt;

    if paths.is_empty() {
        return Ok(());
    }

    // 路径**写进临时文件**再让 PowerShell 读，而不是经 stdin 或拼进命令行：
    // 前者踩过坑（`-Command` 带多行脚本 + `[Console]::In.ReadToEnd()` 组合下读不到内容，
    // 表现为「脚本跑了但一条都没删」），后者要处理引号/空格/中文的转义。
    // 文件路径本身不可能包含换行，所以「一行一条」是安全的分隔方式。
    let list_path = std::env::temp_dir().join(format!(".itools-trash-{}.txt", std::process::id()));
    {
        let mut f = std::fs::File::create(&list_path)
            .map_err(|e| format!("创建回收站清单失败: {e}"))?;
        for p in paths {
            writeln!(f, "{}", p.to_string_lossy()).map_err(|e| format!("写入回收站清单失败: {e}"))?;
        }
    }

    // 两个必须这么写的细节，都是真机验收踩出来的：
    //
    // 1. 脚本压成**单行**（语句间用 `;`）——多行脚本作为单个命令行参数传给 `-Command` 时，
    //    参数里的换行会让 PowerShell 解析出问题。
    // 2. UI 选项只能填 `OnlyErrorDialogs` 或 `AllDialogs`——`UIOption` 这个枚举**没有**
    //    「完全不弹框」的取值（一度想当然写成 `DoNotShowDialogs`，运行期直接报枚举名无效）。
    //    取 `OnlyErrorDialogs` 是两者里更安静的那个。
    let script = format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; $fail=0; foreach ($p in (Get-Content -LiteralPath '{}' -Encoding UTF8)) {{ if ([string]::IsNullOrWhiteSpace($p)) {{ continue }}; try {{ if (Test-Path -LiteralPath $p -PathType Container) {{ [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory($p,'OnlyErrorDialogs','SendToRecycleBin') }} else {{ [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile($p,'OnlyErrorDialogs','SendToRecycleBin') }} }} catch {{ Write-Output ('ERR:' + $_.Exception.Message); $fail=$fail+1 }} }}; exit $fail",
        list_path.to_string_lossy()
    );

    // 不闪黑框
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 捕获 stdout/stderr：失败时把 PowerShell 自己的报错原样带回去。
    // 只报「N 个路径未能送入回收站」等于把唯一的线索丢掉了——排查时无从下手。
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    let _ = std::fs::remove_file(&list_path);

    let out = out.map_err(|e| format!("启动回收站回退进程失败: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        code => {
            let so = String::from_utf8_lossy(&out.stdout);
            let se = String::from_utf8_lossy(&out.stderr);
            let detail: String = format!("{so} {se}").trim().chars().take(300).collect();
            Err(format!(
                "回退方案失败（code={code:?}）：{}",
                if detail.is_empty() { "无输出".into() } else { detail }
            ))
        }
    }
}

#[cfg(not(windows))]
fn recycle_via_powershell(_paths: &[PathBuf]) -> Result<(), String> {
    Err("回收站功能仅在 Windows 上可用".to_string())
}

/// 真正把一个路径送进 Windows 回收站（可撤销/可还原，不是真删）。
///
/// # 为什么要专门起一个线程
///
/// `IFileOperation` 要求调用方线程处于 **STA**。本函数的调用链跑在 `spawn_blocking`
/// 的线程池上，那些 OS 线程会跨任务复用，可能早已被别的任务（如 OCR）初始化成 MTA；
/// 此时再调 `CoInitializeEx(COINIT_APARTMENTTHREADED)` 只会返回 `RPC_E_CHANGED_MODE`
/// 而不会改变现状，后续 shell 调用随即失灵。
///
/// 真机验收时这个坑踩得很实：先是 `IFileOperation` 在 `SHCreateItemFromParsingName`
/// 稳定返回 `E_INVALIDARG (0x80070057)`；换成老 API `SHFileOperationW` 后又返回
/// `124 (DE_INVALIDFILES)`——两条路都断在同一个根因上。所以这里起一个**全新的**线程，
/// 它的 apartment 状态由我们自己决定，用完即退，不受线程池历史状态影响。
///
/// 代价是每次删除多一次线程创建（微秒级），对一个由用户手动触发的操作完全可以接受。
/// 这类问题**编译期完全看不出来**，只有真机跑一次才会暴露。
#[cfg(windows)]
fn send_to_recycle_bin(path: &Path) -> Result<(), String> {
    let owned = path.to_path_buf();
    std::thread::spawn(move || recycle_in_sta(&owned))
        .join()
        .map_err(|_| "回收站线程异常终止".to_string())?
}

/// 在一个**全新 STA 线程**内完成整套 COM 调用。只应由 [`send_to_recycle_bin`] 调用。
#[cfg(windows)]
fn recycle_in_sta(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName, FOF_NOCONFIRMATION,
        FOF_NOERRORUI, FOF_SILENT, FOFX_RECYCLEONDELETE,
    };

    // SAFETY: 本线程刚创建、从未初始化过 COM，这里必定拿到干净的 STA。
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if init.is_err() {
        return Err(format!("COM 初始化失败: {init:?}"));
    }
    // 无论下面走哪条路径都要配对 CoUninitialize，所以用闭包把主体裹起来，出口只有一个。
    let run = || -> Result<(), String> {
        let e2s = |e: windows::core::Error| format!("送入回收站失败: {e}");

        // SAFETY: 在已初始化 COM 的线程上创建进程内 shell 对象，标准用法。
        let op: IFileOperation =
            unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(e2s)?;

        // FOFX_RECYCLEONDELETE 才是「进回收站」的现代写法（老 API 用 FOF_ALLOWUNDO）；
        // 静默 + 不弹错误 UI，因为这是插件代表用户发起的批量操作，不该一条弹一次框。
        // SAFETY: op 是刚创建的有效接口。
        unsafe {
            op.SetOperationFlags(
                FOFX_RECYCLEONDELETE | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT,
            )
        }
        .map_err(e2s)?;

        // 用裸 Vec<u16> + PCWSTR，不经 HSTRING：HSTRING 是 WinRT 字符串类型，
        // 它到 PCWSTR 的转换在这个 shell API 上实测拿到 E_INVALIDARG。
        // 这里直接给一个 NUL 结尾的 UTF-16 缓冲，语义上不留任何歧义。
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: wide 在本次调用期间存活且以 NUL 结尾；pbc 传 None 是该 API 的标准用法。
        let item: IShellItem =
            unsafe { SHCreateItemFromParsingName(windows::core::PCWSTR(wide.as_ptr()), None) }
                .map_err(|e| format!("解析路径失败: {e}（路径 {} 字符）", wide.len() - 1))?;

        // SAFETY: item 是刚创建的有效 IShellItem；不需要逐条进度回调。
        unsafe { op.DeleteItem(&item, None) }.map_err(e2s)?;
        // SAFETY: 执行已排队的操作。
        unsafe { op.PerformOperations() }.map_err(e2s)?;

        // SAFETY: 查询只读状态位。
        let aborted = unsafe { op.GetAnyOperationsAborted() }.map_err(e2s)?;
        if aborted.as_bool() {
            return Err("回收站操作被系统中止（可能是权限不足或路径正被占用）".to_string());
        }
        Ok(())
    };
    let r = run();
    // SAFETY: 与本函数开头成功的 CoInitializeEx 严格配对。
    unsafe { CoUninitialize() };
    r
}

#[cfg(not(windows))]
fn send_to_recycle_bin(_path: &Path) -> Result<(), String> {
    Err("回收站功能仅在 Windows 上可用".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 扩展长度前缀被剥离() {
        // 真机验收里最难定位的一个坑：canonicalize 返回的扩展长度形式会让 shell 层与 .NET
        // 全部拒绝（E_INVALIDARG / DE_INVALIDFILES / 「不支持给定路径的格式」），
        // 前后换了四种删除实现都断在这里。
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\C:\Users\x\a.txt")),
            PathBuf::from(r"C:\Users\x\a.txt")
        );
    }

    #[test]
    fn unc_形式还原成双反斜杠() {
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\UNC\server\share\f.txt")),
            PathBuf::from(r"\\server\share\f.txt")
        );
    }

    #[test]
    fn 普通路径原样返回() {
        assert_eq!(
            strip_extended_prefix(Path::new(r"D:\data\x.bin")),
            PathBuf::from(r"D:\data\x.bin")
        );
    }
}
