//! 系统管理类插件能力：进程（结束）、已安装软件（只读）、开机启动项（列出/移除/禁用）、
//! 电源（锁屏/睡眠/关机/重启）。这四类能力都能真实改变系统运行状态或暴露系统隐私信息，
//! 因此**分三档权限**（不合并成一个大权限）：
//! - `process-manage`：枚举 / 结束进程。能结束进程 = 能让任意程序崩溃，风险最高。
//! - `system-read`：只读，查询已安装软件列表（注册表卸载项本就是"控制面板 → 程序与
//!   功能"里公开可见的信息，但仍需插件显式声明，避免插件在用户不知情下枚举装了什么）。
//! - `system-manage`：会**改变系统状态**（移除/禁用启动项、锁屏、睡眠、关机、重启）。
//!
//! # 诚信标注：本模块每个命令的真实副作用
//!
//! - [`plugin_proc_list`] / [`plugin_installed_apps`] / [`plugin_startup_list`]：只读，
//!   不改变任何系统状态。取不到的字段返回 `None`，不用 0 / 空串冒充。
//! - [`plugin_proc_kill`]：**立即、不可逆地结束目标进程**，未保存的数据会丢失。
//! - [`plugin_startup_remove`]：**立即、不可逆地**删除该启动项（注册表 Run 值 / 启动
//!   文件夹里的文件），无回收站、无撤销。
//! - [`plugin_startup_set_enabled`]：注册表启动项走 Windows 官方的
//!   `StartupApproved\Run` 禁用标记（与"任务管理器 → 启动"标签页禁用一个项的效果
//!   完全一致，命令本体保留、只是不在登录时执行，可随时改回来）；启动文件夹里的项
//!   本模块用给文件名加 `.disabled` 后缀的方式模拟"禁用"（Windows 官方对这类文件
//!   没有对应的禁用机制），同样是可逆操作。
//! - [`plugin_power_lock`]：立即锁屏，不影响正在运行的程序。
//! - [`plugin_power_sleep`]：让系统进入睡眠，未保存的数据不会丢失（睡眠会保电保持
//!   内存内容），但外接设备/网络连接会短暂中断。
//! - [`plugin_power_shutdown`] / [`plugin_power_restart`]：**默认非强制**
//!   （`force=false`），会给前台应用弹出"是否保存"的机会；`force=true` 时会强制关闭
//!   所有应用，未保存的数据**会丢失**，调用方必须清楚这一点再传 `true`。
//!
//! # 为什么不提供"添加任意启动项"
//!
//! 本模块只允许**列出**与**移除/禁用已存在**的启动项，刻意不提供"新增启动项"命令。
//! "让程序自己写一条开机自启项"是恶意软件驻留系统最经典的手法之一——一旦开放这个
//! 接口，任何声明了 `system-manage` 权限的插件都能在用户毫无察觉的情况下让别的程序
//! 或自身在每次开机时静默运行。iTools 自身的开机自启（托盘"开机启动"开关）走的是
//! 宿主应用自己的专用逻辑（见 `crate::lib.rs` / `autostart_path_is_stale`），不经过
//! 这层插件 API，也不受此限制影响。
//!
//! # 数据来源与真实性
//!
//! 与 [`super::sysinfo`] 同样的取舍：全部走 Win32 API + 注册表，项目未启用 `sysinfo`
//! crate。进程列表用 `CreateToolhelp32Snapshot`（`Win32_System_Diagnostics_ToolHelp`）
//! 拿 pid/名字/父 pid，用 `QueryFullProcessImageNameW` + `GetProcessMemoryInfo`
//! （`Win32_System_ProcessStatus`）尽力补全路径/内存——**这两步对系统进程/其它用户的
//! 进程大概率会因权限不足而失败，此时 `exePath`/`memoryBytes` 如实留 `None`，不会
//! 伪造成 0 或空字符串，调用方能看出"这条数据没拿到"而不是"这个进程占用为 0"**。
//! 已安装软件读注册表卸载项（含 `WOW6432Node`），启动项读注册表 `Run` 键与两个
//! 启动文件夹，关机/重启走 `ExitWindowsEx`（`Win32_System_Shutdown`），睡眠走
//! `SetSuspendState`（`Win32_System_Power`，项目已启用）。

use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::PluginRegistry;
use crate::settings::SettingsStore;

// ============================================================================
// 数据结构
// ============================================================================

/// 单个进程的信息，见模块文档「诚信标注」——`exe_path` / `memory_bytes` 取不到时为
/// `None`，不是没有这个进程或占用为 0。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcInfo {
    /// 进程 ID。
    pub pid: u32,
    /// 进程可执行文件名（不含路径，如 `"explorer.exe"`），来自进程快照，恒可用。
    pub name: String,
    /// 可执行文件完整路径。需要打开目标进程句柄才能查询，系统进程/其它用户的进程/
    /// 已退出的进程常因权限不足或时序问题查不到，此时为 `None`。
    pub exe_path: Option<String>,
    /// 当前工作集内存占用（字节），原理同 `exe_path`，查不到为 `None`。
    pub memory_bytes: Option<u64>,
    /// 父进程 ID。进程快照恒会报告这个值，但 `0`（无父进程，如 System Idle Process）
    /// 时视为 `None`；父进程已退出、pid 被系统回收复用的情况下这个值可能已经指向了
    /// 一个无关的新进程——这是 Windows 进程快照 API 本身的固有局限，不是本模块的 bug。
    pub parent_pid: Option<u32>,
}

/// 一个已安装软件条目（来自注册表卸载项）。字段取不到时为 `None`，不用空串占位。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApp {
    /// 显示名称（`DisplayName`）。这是唯一必需字段——没有它的条目不会出现在列表里。
    pub name: String,
    /// 版本号（`DisplayVersion`）。
    pub version: Option<String>,
    /// 发布者（`Publisher`）。
    pub publisher: Option<String>,
    /// 安装目录（`InstallLocation`）。
    pub install_location: Option<String>,
    /// 卸载命令行（优先 `UninstallString`，缺失时退回 `QuietUninstallString`）。
    /// 本模块只**读出**这条命令，不负责执行卸载——真要卸载由调用方另行处理
    /// （如走 [`super::exec`]，那边有自己的权限门禁）。
    pub uninstall_command: Option<String>,
    /// 估计占用磁盘大小（字节）。注册表里 `EstimatedSize` 单位是 KB，这里已换算成字节；
    /// 该值是安装程序自报的估计值，不是实测的目录大小，可能不准。
    pub estimated_size_bytes: Option<u64>,
}

/// 一个开机启动项（注册表 `Run` 键值，或启动文件夹里的一个文件）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupItem {
    /// 不透明标识符，[`plugin_startup_remove`] / [`plugin_startup_set_enabled`] 用它
    /// 定位具体是哪一项；内部编码格式不对外承诺稳定，调用方不应自己拼这个字符串，
    /// 只应该用 [`plugin_startup_list`] 返回的原值。
    pub id: String,
    /// 显示名称：注册表项是值名；启动文件夹项是文件名（已禁用状态会剥掉内部使用的
    /// `.disabled` 后缀，不会让用户看到实现细节）。
    pub name: String,
    /// 启动命令行。仅注册表 `Run` 项有值；启动文件夹里的 `.lnk` 快捷方式要解析真实
    /// 目标程序需要额外的 COM 组件（`IShellLinkW`），本模块未接入（见模块文档"数据
    /// 来源"一节的取舍），如实留 `None`，不会拿文件路径冒充"启动命令"。
    pub command: Option<String>,
    /// 来源标签：`"registry:hklm"` / `"registry:hkcu"` / `"folder:user"` /
    /// `"folder:common"`。
    pub source: String,
    /// 当前是否启用（会在下次登录时执行）。
    pub enabled: bool,
}

// ============================================================================
// 常量与纯逻辑（不涉及 Win32 调用，跨平台可编译可测）
// ============================================================================

/// 内部 id 编码用的分隔符：控制字符，正常的注册表值名/文件路径不会包含它。
const ID_SEP: char = '\u{1f}';

/// 注册表 `Run` 键相对路径（HKLM/HKCU 通用）。
const RUN_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run";
/// 「启动项审批」注册表键——Windows 任务管理器"启动"标签页禁用一项时，写的就是
/// 这里的同名二进制值，而不是删除 `Run` 键值本身。本模块复用同一套机制，保证行为
/// 与系统自带工具一致、且可被系统自带工具识别/改回。
const STARTUP_APPROVED_RUN_SUBKEY: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// 一律拒绝结束的系统关键进程名（小写，不含路径）。结束其中任意一个都可能导致蓝屏、
/// 强制注销或系统服务大面积失效，不给插件这个机会。
const CRITICAL_PROCESS_NAMES: [&str; 8] = [
    "system idle process",
    "system",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "winlogon.exe",
    "services.exe",
    "lsass.exe",
];

/// 判断进程名是否命中系统关键进程名单（大小写不敏感）。
fn is_critical_process_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    CRITICAL_PROCESS_NAMES.contains(&lower.as_str())
}

/// 把 Win32 定长宽字符缓冲区（如 `PROCESSENTRY32W::szExeFile`）转成 `String`，
/// 在第一个 `\0` 处截断。
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// 从「pid → (name, ppid)」映射出发，沿父进程链一直往上走，算出「这条链上有哪些
/// pid」（包含起点自身）。用于 [`plugin_proc_kill`] 判断"要杀的目标是不是调用者
/// （iTools 自身进程）的祖先"。
///
/// 最多走 32 跳、遇到已访问过的 pid（自引用/环）或映射里查不到就停止——pid 有被
/// 系统回收复用的可能，这条链天生只是"尽力而为"的近似，但宁可保守多拦一点，也不要
/// 因为漏判而放插件误杀链上的宿主/关键进程。
fn ancestor_chain_pids(
    map: &std::collections::HashMap<u32, (String, u32)>,
    start_pid: u32,
) -> std::collections::HashSet<u32> {
    let mut chain = std::collections::HashSet::new();
    let mut current = start_pid;
    for _ in 0..32 {
        if !chain.insert(current) {
            break;
        }
        let Some((_, ppid)) = map.get(&current) else { break };
        if *ppid == current || *ppid == 0 {
            break;
        }
        current = *ppid;
    }
    chain
}

/// 判断注册表卸载项是否是"系统补丁/更新"而不是一个真正的应用（据此从
/// [`plugin_installed_apps`] 结果里过滤掉，否则列表会被一堆 KB 编号淹没）。
fn looks_like_patch_entry(
    system_component: Option<u32>,
    release_type: Option<&str>,
    has_parent_key: bool,
    key_name: &str,
) -> bool {
    if system_component == Some(1) {
        return true;
    }
    // 有 ParentKeyName 说明这是挂在另一个产品下的子更新条目，不是独立软件
    if has_parent_key {
        return true;
    }
    if let Some(rt) = release_type {
        let rt = rt.trim().to_ascii_lowercase();
        if matches!(
            rt.as_str(),
            "hotfix" | "security update" | "update rollup" | "servicepack" | "update"
        ) {
            return true;
        }
    }
    looks_like_kb_id(key_name)
}

/// 卸载项的注册表子键名是不是形如 `KB1234567` 的补丁编号。
fn looks_like_kb_id(key_name: &str) -> bool {
    let name = key_name.trim();
    let rest = name
        .strip_prefix("KB")
        .or_else(|| name.strip_prefix("kb"))
        .or_else(|| name.strip_prefix("Kb"));
    match rest {
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// 解析 `StartupApproved\Run` 下一个原始二进制值，判断对应启动项是否启用。
/// 约定（微软未公开文档化、但已被广泛验证）：首字节 `0x02` = 已启用，`0x03` = 已禁用；
/// 值不存在（该项从未被任务管理器/设置动过）时按 Windows 默认行为视为已启用。
fn startup_approved_enabled(bytes: &[u8]) -> bool {
    !matches!(bytes.first(), Some(0x03))
}

/// 反过来：把"是否启用"编码成要写进 `StartupApproved\Run` 的 12 字节原始值。
fn startup_approved_bytes(on: bool) -> Vec<u8> {
    let mut bytes = vec![0u8; 12];
    bytes[0] = if on { 0x02 } else { 0x03 };
    bytes
}

fn make_reg_id(hive_label: &str, value_name: &str) -> String {
    format!("reg{ID_SEP}{hive_label}{ID_SEP}{value_name}")
}

fn make_folder_id(path: &str) -> String {
    format!("folder{ID_SEP}{path}")
}

/// 解析出来的启动项 id。
enum StartupId {
    Registry { hive_label: String, value_name: String },
    Folder { path: String },
}

fn parse_startup_id(id: &str) -> Result<StartupId, String> {
    let (kind, rest) = id
        .split_once(ID_SEP)
        .ok_or_else(|| "无效的启动项 id".to_string())?;
    match kind {
        "reg" => {
            let (hive_label, value_name) = rest
                .split_once(ID_SEP)
                .ok_or_else(|| "无效的启动项 id".to_string())?;
            Ok(StartupId::Registry {
                hive_label: hive_label.to_string(),
                value_name: value_name.to_string(),
            })
        }
        "folder" => Ok(StartupId::Folder { path: rest.to_string() }),
        _ => Err("无效的启动项 id".to_string()),
    }
}

/// 电源操作类型（关机 / 重启共用同一套特权提升 + `ExitWindowsEx` 逻辑，只是标志位不同）。
enum PowerAction {
    Shutdown,
    Restart,
}

// ============================================================================
// 进程
// ============================================================================

/// 枚举当前系统上运行的所有进程。
///
/// 需要 `process-manage` 权限。内部跑在阻塞线程池上（进程快照 + 逐进程查询路径/内存
/// 有一定 IO/系统调用开销），不阻塞 UI。
///
/// # Errors
/// - 插件未获 `process-manage` 授权
/// - 创建进程快照失败（极少见的系统资源不足场景）
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_proc_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<ProcInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "process-manage") {
        return Err("插件未获授权管理进程（请在「插件管理」里授权 process-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(list_processes_blocking)
        .await
        .map_err(|e| format!("枚举进程任务异常终止: {e}"))?
}

/// 结束一个进程。**立即、不可逆**，目标进程里未保存的数据会丢失。
///
/// 需要 `process-manage` 权限。在真正调用 `TerminateProcess` 之前会做强制安全校验：
/// 拒绝结束 iTools 自身进程、拒绝结束 iTools 进程的父进程链上的任意进程（如启动
/// iTools 的 shell/守护进程）、拒绝结束固定名单里的系统关键进程（`csrss.exe` /
/// `winlogon.exe` / `services.exe` 等）与 pid `0`/`4`（Idle / System）——命中任一条
/// 都直接报错，不会走到真正的结束调用。除此之外的进程，只要 Windows 权限允许，
/// 就会真的被结束，插件方需自行确认目标 pid 无误。
///
/// # Errors
/// - 插件未获 `process-manage` 授权
/// - 目标是 iTools 自身进程 / 其父进程链上的进程 / 系统关键进程 / 保留 pid
/// - `OpenProcess` 失败（目标已退出，或当前权限不足以打开它，如目标是其它用户的进程）
/// - `TerminateProcess` 失败
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_proc_kill(
    pid: u32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "process-manage") {
        return Err("插件未获授权管理进程（请在「插件管理」里授权 process-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || kill_process_blocking(pid))
        .await
        .map_err(|e| format!("结束进程任务异常终止: {e}"))?
}

#[cfg(windows)]
fn list_processes_blocking() -> Result<Vec<ProcInfo>, String> {
    let raw = snapshot_raw_processes()?;
    Ok(raw
        .into_iter()
        .map(|(pid, name, ppid)| {
            let (exe_path, memory_bytes) = query_process_extra(pid);
            ProcInfo {
                pid,
                name,
                exe_path,
                memory_bytes,
                parent_pid: if ppid == 0 { None } else { Some(ppid) },
            }
        })
        .collect())
}

#[cfg(not(windows))]
fn list_processes_blocking() -> Result<Vec<ProcInfo>, String> {
    Err("进程管理目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn kill_process_blocking(pid: u32) -> Result<(), String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    validate_kill_target(pid)?;

    // SAFETY: pid 是外部传入、已通过 validate_kill_target 校验的目标进程 ID；
    // PROCESS_TERMINATE 是结束进程所需的最小权限；OpenProcess 失败（目标已退出/
    // 权限不足）时直接返回 Err，不会产生需要清理的无效句柄。
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }
        .map_err(|e| format!("打开目标进程失败（可能已退出，或当前权限不足以结束它）: {e}"))?;
    // SAFETY: handle 是刚打开、拥有 PROCESS_TERMINATE 权限的合法句柄；TerminateProcess
    // 只读取该句柄，不改变其生命周期，之后仍由本函数负责关闭。
    let result = unsafe { TerminateProcess(handle, 1) };
    // SAFETY: handle 只在本函数内使用，用完立即关闭且只关闭这一次。
    unsafe {
        let _ = CloseHandle(handle);
    }
    result.map_err(|e| format!("结束进程失败: {e}"))
}

#[cfg(not(windows))]
fn kill_process_blocking(_pid: u32) -> Result<(), String> {
    Err("进程管理目前仅支持 Windows".to_string())
}

/// 结束进程前的强制安全校验，见 [`plugin_proc_kill`] 文档。
#[cfg(windows)]
fn validate_kill_target(pid: u32) -> Result<(), String> {
    use windows::Win32::System::Threading::GetCurrentProcessId;

    // SAFETY: 无参数，纯查询当前进程 ID。
    let current = unsafe { GetCurrentProcessId() };
    if pid == current {
        return Err("拒绝结束 iTools 自身进程".to_string());
    }
    if pid == 0 || pid == 4 {
        return Err(
            "拒绝结束系统关键进程（该 PID 为系统保留进程，结束会导致系统崩溃）".to_string(),
        );
    }

    let raw = snapshot_raw_processes()?;
    let map: std::collections::HashMap<u32, (String, u32)> =
        raw.into_iter().map(|(pid, name, ppid)| (pid, (name, ppid))).collect();

    if let Some((name, _)) = map.get(&pid) {
        if is_critical_process_name(name) {
            return Err(format!(
                "拒绝结束系统关键进程「{name}」（可能导致系统崩溃或强制注销所有用户）"
            ));
        }
    }

    let ancestors = ancestor_chain_pids(&map, current);
    if ancestors.contains(&pid) {
        return Err(
            "拒绝结束 iTools 自身或其父进程链上的进程（可能导致 iTools 或其宿主进程异常退出）"
                .to_string(),
        );
    }

    // 同名的**另一个** iTools 实例也要保护，而不只是当前这个进程。
    //
    // 真机验收时踩到过：机器上同时跑着正式版与 debug 版，插件从 proc.list 里按名字找
    // 「itools」取第一个去 kill，命中的是另一个实例——前面所有基于「当前 pid / 父进程链」
    // 的判断全都放行了，因为那确实不是自己。但对用户来说，结果就是「装了个插件，
    // 我的 iTools 被关掉了」，中间那点「不是同一个进程」的技术差别毫无意义。
    let self_name = map.get(&current).map(|(n, _)| n.to_ascii_lowercase());
    if let (Some(self_name), Some((name, _))) = (self_name, map.get(&pid)) {
        if name.to_ascii_lowercase() == self_name {
            return Err(
                "拒绝结束另一个 iTools 实例（同一程序的其它实例同样不允许由插件关闭）"
                    .to_string(),
            );
        }
    }
    Ok(())
}

/// 拍一次进程快照，返回 `(pid, 可执行文件名, 父 pid)` 列表。这是「轻量」查询——只用
/// `CreateToolhelp32Snapshot` 一次系统调用拿到所有进程的基础信息，不逐进程
/// `OpenProcess`（那部分开销更大，只有 [`list_processes_blocking`] 真正需要展示
/// 路径/内存时才做，见 [`query_process_extra`])。
#[cfg(windows)]
fn snapshot_raw_processes() -> Result<Vec<(u32, String, u32)>, String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: 第二个参数传 0——TH32CS_SNAPPROCESS 本身就是「全进程快照」，不需要按
    // 某个具体 pid 过滤，这是该 flag 下的标准用法。
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| format!("创建进程快照失败: {e}"))?;

    let mut out = Vec::new();
    let mut entry =
        PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    // SAFETY: entry 是本函数栈上刚按文档要求设置好 dwSize 的缓冲区；Process32FirstW
    // 只会按该固定布局写入自己的字段，不会越界。
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) }.is_ok();
    while ok {
        out.push((entry.th32ProcessID, wide_to_string(&entry.szExeFile), entry.th32ParentProcessID));
        entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        // SAFETY: snap 是本函数持有、仍然有效的快照句柄；entry 同上是本次循环栈上
        // 刚重置好 dwSize 的缓冲区。
        ok = unsafe { Process32NextW(snap, &mut entry) }.is_ok();
    }
    // SAFETY: snap 是本函数用 CreateToolhelp32Snapshot 独占创建的句柄，遍历结束后
    // 立即释放，且只释放这一次。
    unsafe {
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

/// 尽力查询一个进程的可执行文件完整路径与工作集内存占用；`OpenProcess` 失败
/// （目标受保护、属于其它用户、已退出等）是预期情况，此时两个字段都返回 `None`，
/// 不伪造成 0 / 空字符串——见模块文档「诚信标注」。
#[cfg(windows)]
fn query_process_extra(pid: u32) -> (Option<String>, Option<u64>) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };

    // SAFETY: pid 来自刚拿到的进程快照；OpenProcess 失败时直接返回 (None, None)，
    // 不产生需要清理的无效句柄。
    let handle = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
    } {
        Ok(h) => h,
        Err(_) => return (None, None),
    };

    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    // SAFETY: buf 是本函数栈上 1024 元素的宽字符缓冲区，size 初值等于其长度；
    // QueryFullProcessImageNameW 只会在这个长度范围内写入，并把实际写入的字符数
    // 回填进 size，不会越界。
    let exe_path = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut size)
    }
    .ok()
    .map(|()| String::from_utf16_lossy(&buf[..size as usize]));

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: counters 是本函数栈上刚按文档要求设置好 cb 大小字段的结构体；
    // GetProcessMemoryInfo 只会按该固定布局写入自己的字段。
    let memory_bytes = unsafe { GetProcessMemoryInfo(handle, &mut counters, counters.cb) }
        .ok()
        .map(|()| counters.WorkingSetSize as u64);

    // SAFETY: handle 只在本函数内使用，用完立即关闭且只关闭这一次。
    unsafe {
        let _ = CloseHandle(handle);
    }
    (exe_path, memory_bytes)
}

// ============================================================================
// 已安装软件
// ============================================================================

/// 枚举已安装软件（读注册表卸载项，含 32/64 位与当前用户三处来源）。
///
/// 需要 `system-read` 权限。已过滤掉系统补丁/更新条目与没有显示名的条目，见
/// [`looks_like_patch_entry`]。
///
/// # Errors
/// - 插件未获 `system-read` 授权
/// - 非 Windows 平台固定返回"仅支持 Windows"
/// - 当前实现下 Windows 平台其余情况不主动返回 `Err`（单个注册表根键打不开就跳过，
///   不影响其余来源的枚举结果）
#[tauri::command]
pub async fn plugin_installed_apps(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<InstalledApp>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-read") {
        return Err("插件未获授权读取系统信息（请在「插件管理」里授权 system-read）".to_string());
    }
    tauri::async_runtime::spawn_blocking(scan_installed_apps_blocking)
        .await
        .map_err(|e| format!("枚举已安装软件任务异常终止: {e}"))?
}

#[cfg(windows)]
fn scan_installed_apps_blocking() -> Result<Vec<InstalledApp>, String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    const NATIVE: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const WOW6432: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

    let roots = [(HKEY_LOCAL_MACHINE, NATIVE), (HKEY_LOCAL_MACHINE, WOW6432), (HKEY_CURRENT_USER, NATIVE)];

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (hive, subkey) in roots {
        let Ok(uninstall) = RegKey::predef(hive).open_subkey(subkey) else { continue };
        for key_name in uninstall.enum_keys().filter_map(Result::ok) {
            let Ok(entry) = uninstall.open_subkey(&key_name) else { continue };
            let Some(app) = read_installed_app(&entry, &key_name) else { continue };
            let dedup_key =
                format!("{}\u{1f}{}", app.name.to_lowercase(), app.version.as_deref().unwrap_or(""));
            if seen.insert(dedup_key) {
                out.push(app);
            }
        }
    }
    Ok(out)
}

#[cfg(not(windows))]
fn scan_installed_apps_blocking() -> Result<Vec<InstalledApp>, String> {
    Err("已安装软件查询目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn read_installed_app(entry: &winreg::RegKey, key_name: &str) -> Option<InstalledApp> {
    let name: String = entry.get_value("DisplayName").ok()?;
    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }

    let system_component: Option<u32> = entry.get_value("SystemComponent").ok();
    let release_type: Option<String> = entry.get_value("ReleaseType").ok();
    let has_parent_key = entry.get_value::<String, _>("ParentKeyName").is_ok();
    if looks_like_patch_entry(system_component, release_type.as_deref(), has_parent_key, key_name) {
        return None;
    }

    let trimmed_opt = |v: std::io::Result<String>| v.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let version = trimmed_opt(entry.get_value("DisplayVersion"));
    let publisher = trimmed_opt(entry.get_value("Publisher"));
    let install_location = trimmed_opt(entry.get_value("InstallLocation"));
    let uninstall_command = trimmed_opt(entry.get_value("UninstallString"))
        .or_else(|| trimmed_opt(entry.get_value("QuietUninstallString")));
    let estimated_size_bytes: Option<u64> =
        entry.get_value::<u32, _>("EstimatedSize").ok().map(|kb| u64::from(kb) * 1024);

    Some(InstalledApp { name, version, publisher, install_location, uninstall_command, estimated_size_bytes })
}

// ============================================================================
// 启动项
// ============================================================================

/// 枚举开机启动项：注册表 `Run` 键（HKLM + HKCU）与两个启动文件夹（当前用户 + 所有
/// 用户）。**不包含** `RunOnce`（那类项按设计只执行一次就会被系统自动清除，纳入"启动
/// 项管理"会造成误导）。
///
/// 需要 `system-manage` 权限（与 remove/set_enabled 同档，因为启动项信息本身就比
/// "有哪些软件"更敏感，牵涉到"这台机器登录时会跑什么"）。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_startup_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<StartupItem>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(list_startup_items_blocking)
        .await
        .map_err(|e| format!("枚举启动项任务异常终止: {e}"))?
}

/// 移除一个启动项。**立即、不可逆**：注册表项是直接删掉 `Run` 值，启动文件夹项是
/// 直接删掉文件，均无回收站、无撤销。
///
/// 需要 `system-manage` 权限。`id` 必须是 [`plugin_startup_list`] 返回的原值。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - `id` 格式无效或指向的项已不存在
/// - 权限不足（如 HKLM 项需要管理员权限）
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_startup_remove(
    id: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || remove_startup_item_blocking(&id))
        .await
        .map_err(|e| format!("移除启动项任务异常终止: {e}"))?
}

/// 启用/禁用一个启动项（不删除，命令本体保留，随时可以改回来）。见模块文档
/// 「诚信标注」了解注册表项与文件夹项两种不同的禁用实现方式。
///
/// 需要 `system-manage` 权限。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - `id` 格式无效、指向的项已不存在
/// - 文件夹项切换到目标状态时目标文件名已被占用
/// - 权限不足
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_startup_set_enabled(
    id: String,
    on: bool,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || set_startup_enabled_blocking(&id, on))
        .await
        .map_err(|e| format!("切换启动项状态任务异常终止: {e}"))?
}

#[cfg(windows)]
fn list_startup_items_blocking() -> Result<Vec<StartupItem>, String> {
    let mut out = Vec::new();
    list_registry_run_items(&mut out);
    list_startup_folder_items(&mut out);
    Ok(out)
}

#[cfg(not(windows))]
fn list_startup_items_blocking() -> Result<Vec<StartupItem>, String> {
    Err("启动项管理目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn list_registry_run_items(out: &mut Vec<StartupItem>) {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    for (label, hive) in [("HKLM", HKEY_LOCAL_MACHINE), ("HKCU", HKEY_CURRENT_USER)] {
        let Ok(run) = RegKey::predef(hive).open_subkey(RUN_SUBKEY) else { continue };
        let approved = RegKey::predef(hive).open_subkey(STARTUP_APPROVED_RUN_SUBKEY).ok();
        for (name, value) in run.enum_values().filter_map(Result::ok) {
            let enabled = approved
                .as_ref()
                .and_then(|k| k.get_raw_value(&name).ok())
                .map_or(true, |raw| startup_approved_enabled(&raw.bytes));
            out.push(StartupItem {
                id: make_reg_id(label, &name),
                name: name.clone(),
                command: Some(value.to_string()),
                source: format!("registry:{}", label.to_lowercase()),
                enabled,
            });
        }
    }
}

#[cfg(windows)]
fn list_startup_folder_items(out: &mut Vec<StartupItem>) {
    for (label, dir) in startup_folders() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if !path.is_file() {
                // 启动文件夹里的子文件夹不会被登录时执行，不纳入列表
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let path_str = path.to_string_lossy().into_owned();
            let disabled = path_str.len() >= 10 && path_str[path_str.len() - 10..].eq_ignore_ascii_case(".disabled");
            let display_name = if disabled {
                file_name[..file_name.len().saturating_sub(10)].to_string()
            } else {
                file_name.to_string()
            };
            out.push(StartupItem {
                id: make_folder_id(&path_str),
                name: display_name,
                command: None,
                source: format!("folder:{label}"),
                enabled: !disabled,
            });
        }
    }
}

#[cfg(windows)]
fn startup_folders() -> Vec<(&'static str, std::path::PathBuf)> {
    let mut v = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        v.push((
            "user",
            std::path::PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"),
        ));
    }
    if let Some(program_data) = std::env::var_os("ProgramData") {
        v.push((
            "common",
            std::path::PathBuf::from(program_data).join(r"Microsoft\Windows\Start Menu\Programs\Startup"),
        ));
    }
    v
}

#[cfg(windows)]
fn hive_from_label(label: &str) -> Option<winreg::HKEY> {
    match label {
        "HKLM" => Some(winreg::enums::HKEY_LOCAL_MACHINE),
        "HKCU" => Some(winreg::enums::HKEY_CURRENT_USER),
        _ => None,
    }
}

#[cfg(windows)]
fn remove_startup_item_blocking(id: &str) -> Result<(), String> {
    use winreg::enums::KEY_SET_VALUE;
    use winreg::RegKey;

    match parse_startup_id(id)? {
        StartupId::Registry { hive_label, value_name } => {
            let hive = hive_from_label(&hive_label).ok_or_else(|| format!("未知的注册表根键: {hive_label}"))?;
            let run = RegKey::predef(hive)
                .open_subkey_with_flags(RUN_SUBKEY, KEY_SET_VALUE)
                .map_err(|e| format!("打开启动项注册表键失败: {e}"))?;
            run.delete_value(&value_name).map_err(|e| format!("移除启动项失败: {e}"))?;
            // 一并清理对应的"已禁用"标记，避免留下孤儿数据；这一步失败不影响主操作结果。
            if let Ok(approved) =
                RegKey::predef(hive).open_subkey_with_flags(STARTUP_APPROVED_RUN_SUBKEY, KEY_SET_VALUE)
            {
                let _ = approved.delete_value(&value_name);
            }
            Ok(())
        }
        StartupId::Folder { path } => {
            let p = std::path::Path::new(&path);
            if !p.exists() {
                return Err(format!("启动项文件不存在: {path}"));
            }
            std::fs::remove_file(p).map_err(|e| format!("删除启动文件失败: {e}"))
        }
    }
}

#[cfg(not(windows))]
fn remove_startup_item_blocking(_id: &str) -> Result<(), String> {
    Err("启动项管理目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn set_startup_enabled_blocking(id: &str, on: bool) -> Result<(), String> {
    use winreg::enums::{KEY_QUERY_VALUE, REG_BINARY};
    use winreg::{RegKey, RegValue};

    match parse_startup_id(id)? {
        StartupId::Registry { hive_label, value_name } => {
            let hive = hive_from_label(&hive_label).ok_or_else(|| format!("未知的注册表根键: {hive_label}"))?;
            // 先确认这个启动项确实存在，避免给一个不存在的值建"已禁用"标记。
            RegKey::predef(hive)
                .open_subkey_with_flags(RUN_SUBKEY, KEY_QUERY_VALUE)
                .and_then(|k| k.get_raw_value(&value_name))
                .map_err(|e| format!("启动项不存在或无法访问: {e}"))?;
            let (approved, _) = RegKey::predef(hive)
                .create_subkey(STARTUP_APPROVED_RUN_SUBKEY)
                .map_err(|e| format!("打开「启动项审批」注册表键失败: {e}"))?;
            let value = RegValue { bytes: startup_approved_bytes(on), vtype: REG_BINARY };
            approved.set_raw_value(&value_name, &value).map_err(|e| format!("写入启用/禁用状态失败: {e}"))
        }
        StartupId::Folder { path } => set_folder_item_enabled(&path, on),
    }
}

#[cfg(not(windows))]
fn set_startup_enabled_blocking(_id: &str, _on: bool) -> Result<(), String> {
    Err("启动项管理目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn set_folder_item_enabled(path: &str, on: bool) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return Err(format!("启动项文件不存在: {path}"));
    }
    let currently_disabled = path.len() >= 10 && path[path.len() - 10..].eq_ignore_ascii_case(".disabled");
    if on != currently_disabled {
        // 已经是目标状态
        return Ok(());
    }
    let target = if on {
        path[..path.len() - 10].to_string()
    } else {
        format!("{path}.disabled")
    };
    if std::path::Path::new(&target).exists() {
        return Err(format!("目标文件名已存在，无法切换启用状态: {target}"));
    }
    std::fs::rename(p, &target).map_err(|e| format!("重命名启动文件失败: {e}"))
}

// ============================================================================
// 电源
// ============================================================================

/// 立即锁定工作站（等价于按下 Win+L）。需要 `system-manage` 权限。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - `LockWorkStation` 调用失败
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_power_lock(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(lock_workstation_blocking)
        .await
        .map_err(|e| format!("锁定工作站任务异常终止: {e}"))?
}

/// 让系统进入睡眠。未保存的数据不会丢失（睡眠期间保电维持内存内容），但外接设备/
/// 网络连接会短暂中断。需要 `system-manage` 权限。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - `SetSuspendState` 调用失败（部分设备/驱动不支持挂起，或被系统电源策略阻止）
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_power_sleep(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    tauri::async_runtime::spawn_blocking(suspend_system_blocking)
        .await
        .map_err(|e| format!("使系统睡眠任务异常终止: {e}"))?
}

/// 关机。`force` 缺省为 `false`（不强制）：会给前台应用弹出"是否保存"的机会，某个
/// 应用阻塞也可能导致关机被推迟或取消；`force=true` 会强制关闭所有应用，**未保存的
/// 数据会丢失**，调用方必须清楚这一点再传 `true`。需要 `system-manage` 权限。
///
/// # Errors
/// - 插件未获 `system-manage` 授权
/// - 启用关机特权（`SeShutdownPrivilege`）失败
/// - `ExitWindowsEx` 调用失败
/// - 非 Windows 平台固定返回"仅支持 Windows"
#[tauri::command]
pub async fn plugin_power_shutdown(
    force: Option<bool>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    let force = force.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || power_action_blocking(PowerAction::Shutdown, force))
        .await
        .map_err(|e| format!("关机任务异常终止: {e}"))?
}

/// 重启。`force` 语义同 [`plugin_power_shutdown`]。需要 `system-manage` 权限。
///
/// # Errors
/// 同 [`plugin_power_shutdown`]。
#[tauri::command]
pub async fn plugin_power_restart(
    force: Option<bool>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "system-manage") {
        return Err("插件未获授权管理系统（请在「插件管理」里授权 system-manage）".to_string());
    }
    let force = force.unwrap_or(false);
    tauri::async_runtime::spawn_blocking(move || power_action_blocking(PowerAction::Restart, force))
        .await
        .map_err(|e| format!("重启任务异常终止: {e}"))?
}

#[cfg(windows)]
fn lock_workstation_blocking() -> Result<(), String> {
    use windows::Win32::System::Shutdown::LockWorkStation;
    // SAFETY: 无参数，纯系统调用，不涉及任何调用方缓冲区。
    unsafe { LockWorkStation() }.map_err(|e| format!("锁定工作站失败: {e}"))
}

#[cfg(not(windows))]
fn lock_workstation_blocking() -> Result<(), String> {
    Err("锁定工作站目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn suspend_system_blocking() -> Result<(), String> {
    use windows::Win32::System::Power::SetSuspendState;
    // SAFETY: 三个参数均按值传入的布尔量，无指针；hibernate=false 表示睡眠而非休眠，
    // force=false 表示不强制挂起有未保存数据的应用，disable_wake_event=false 表示
    // 不禁止唤醒事件（否则用户可能被"锁"在睡眠状态里叫不醒）。
    let ok = unsafe { SetSuspendState(false, false, false) };
    if ok {
        Ok(())
    } else {
        Err("使系统进入睡眠失败（部分设备/驱动不支持挂起，或被系统电源策略阻止）".to_string())
    }
}

#[cfg(not(windows))]
fn suspend_system_blocking() -> Result<(), String> {
    Err("系统睡眠目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn power_action_blocking(action: PowerAction, force: bool) -> Result<(), String> {
    use windows::Win32::System::Shutdown::{
        ExitWindowsEx, EWX_FORCE, EWX_REBOOT, EWX_SHUTDOWN, SHTDN_REASON_FLAG_PLANNED,
        SHTDN_REASON_MAJOR_APPLICATION, SHTDN_REASON_MINOR_MAINTENANCE,
    };

    enable_privilege("SeShutdownPrivilege")?;

    let base = match action {
        PowerAction::Shutdown => EWX_SHUTDOWN,
        PowerAction::Restart => EWX_REBOOT,
    };
    let flags = if force { base | EWX_FORCE } else { base };
    let reason = SHTDN_REASON_MAJOR_APPLICATION | SHTDN_REASON_MINOR_MAINTENANCE | SHTDN_REASON_FLAG_PLANNED;
    let action_name = match action {
        PowerAction::Shutdown => "关机",
        PowerAction::Restart => "重启",
    };
    // SAFETY: 两个参数都是按值传入的标志位组合，无指针/缓冲区。
    unsafe { ExitWindowsEx(flags, reason) }.map_err(|e| format!("{action_name}失败: {e}"))
}

#[cfg(not(windows))]
fn power_action_blocking(_action: PowerAction, _force: bool) -> Result<(), String> {
    Err("关机/重启目前仅支持 Windows".to_string())
}

/// 在当前进程访问令牌上启用指定特权（如 `SeShutdownPrivilege`）——默认进程令牌里这类
/// 特权是"持有但未启用"状态，`ExitWindowsEx` 在特权未启用时会直接失败，这是微软官方
/// 文档要求的标准前置步骤。
#[cfg(windows)]
fn enable_privilege(name: &str) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE(std::ptr::null_mut());
    // SAFETY: GetCurrentProcess() 返回的是伪句柄（不需要关闭）；token 是本函数栈上的
    // 输出参数，OpenProcessToken 只在成功时写入一个合法句柄。
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) }
        .map_err(|e| format!("打开进程访问令牌失败: {e}"))?;

    let mut luid = LUID::default();
    // SAFETY: name 转成的 HSTRING 在本次调用期间存活；luid 是栈上输出参数，
    // LookupPrivilegeValueW 只在成功时写入。
    let lookup_result = unsafe { LookupPrivilegeValueW(None, &HSTRING::from(name), &mut luid) };
    if lookup_result.is_err() {
        // SAFETY: token 是本函数刚打开、独占持有的句柄，这里只关闭一次。
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err(format!("查找系统特权「{name}」失败"));
    }

    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: token 是刚打开、拥有 TOKEN_ADJUST_PRIVILEGES 权限的合法句柄；privileges
    // 是栈上结构体，其地址在本次调用期间一直有效，缓冲区大小与结构体自身大小一致。
    let adjust_result = unsafe { AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) };
    // AdjustTokenPrivileges 即便返回成功，也可能因组策略剥夺了该特权而"悄悄不生效"——
    // 微软文档要求额外检查 GetLastError() 是否为 ERROR_NOT_ALL_ASSIGNED 才能确认真的
    // 启用了。
    // SAFETY: 无参数，纯查询上一次调用留下的错误码。
    let really_enabled = adjust_result.is_ok() && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED;
    // SAFETY: token 只在本函数内使用，用完立即关闭且只关闭这一次。
    unsafe {
        let _ = CloseHandle(token);
    }

    if really_enabled {
        Ok(())
    } else {
        Err(format!("启用系统特权「{name}」失败（当前账户策略可能不允许，关机/重启需要该特权）"))
    }
}

// ============================================================================
// 测试（纯逻辑部分，不涉及真实 Win32 调用/注册表写入）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_critical_process_name_matches_case_insensitively() {
        assert!(is_critical_process_name("csrss.exe"));
        assert!(is_critical_process_name("CSRSS.EXE"));
        assert!(is_critical_process_name("System Idle Process"));
        assert!(!is_critical_process_name("notepad.exe"));
        assert!(!is_critical_process_name("svchost.exe"));
    }

    #[test]
    fn wide_to_string_truncates_at_nul() {
        let mut buf = [0u16; 8];
        for (i, c) in "abc".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(wide_to_string(&buf), "abc");
    }

    #[test]
    fn ancestor_chain_includes_self_and_walks_up() {
        let mut map = std::collections::HashMap::new();
        map.insert(3u32, ("c.exe".to_string(), 2u32));
        map.insert(2u32, ("b.exe".to_string(), 1u32));
        map.insert(1u32, ("a.exe".to_string(), 0u32));
        let chain = ancestor_chain_pids(&map, 3);
        assert!(chain.contains(&3));
        assert!(chain.contains(&2));
        assert!(chain.contains(&1));
        assert_eq!(chain.len(), 3);
    }

    #[test]
    fn ancestor_chain_stops_on_cycle_without_looping_forever() {
        let mut map = std::collections::HashMap::new();
        map.insert(1u32, ("a.exe".to_string(), 2u32));
        map.insert(2u32, ("b.exe".to_string(), 1u32));
        let chain = ancestor_chain_pids(&map, 1);
        assert_eq!(chain, std::collections::HashSet::from([1, 2]));
    }

    #[test]
    fn looks_like_kb_id_matches_expected_forms() {
        assert!(looks_like_kb_id("KB5001234"));
        assert!(looks_like_kb_id("kb123"));
        assert!(!looks_like_kb_id("KB"));
        assert!(!looks_like_kb_id("KBabc"));
        assert!(!looks_like_kb_id("MyApp"));
    }

    #[test]
    fn looks_like_patch_entry_filters_system_component() {
        assert!(looks_like_patch_entry(Some(1), None, false, "SomeApp"));
        assert!(!looks_like_patch_entry(Some(0), None, false, "SomeApp"));
        assert!(!looks_like_patch_entry(None, None, false, "SomeApp"));
    }

    #[test]
    fn looks_like_patch_entry_filters_release_type_and_parent_key() {
        assert!(looks_like_patch_entry(None, Some("Security Update"), false, "X"));
        assert!(looks_like_patch_entry(None, Some("Hotfix"), false, "X"));
        assert!(looks_like_patch_entry(None, None, true, "X"));
        assert!(!looks_like_patch_entry(None, Some("Application"), false, "X"));
    }

    #[test]
    fn looks_like_patch_entry_filters_kb_key_name() {
        assert!(looks_like_patch_entry(None, None, false, "KB5001234"));
        assert!(!looks_like_patch_entry(None, None, false, "{12345678-1234-1234-1234-123456789012}"));
    }

    #[test]
    fn startup_approved_enabled_defaults_to_true_when_missing() {
        assert!(startup_approved_enabled(&[]));
    }

    #[test]
    fn startup_approved_enabled_reads_first_byte() {
        assert!(startup_approved_enabled(&[0x02, 0, 0]));
        assert!(!startup_approved_enabled(&[0x03, 0, 0]));
    }

    #[test]
    fn startup_approved_bytes_roundtrip() {
        assert!(startup_approved_enabled(&startup_approved_bytes(true)));
        assert!(!startup_approved_enabled(&startup_approved_bytes(false)));
        assert_eq!(startup_approved_bytes(true).len(), 12);
    }

    #[test]
    fn make_and_parse_registry_id_roundtrip() {
        let id = make_reg_id("HKCU", "OneDrive");
        match parse_startup_id(&id).expect("应能解析出合法 id") {
            StartupId::Registry { hive_label, value_name } => {
                assert_eq!(hive_label, "HKCU");
                assert_eq!(value_name, "OneDrive");
            }
            StartupId::Folder { .. } => panic!("应解析为 Registry 变体"),
        }
    }

    #[test]
    fn make_and_parse_folder_id_roundtrip() {
        let id = make_folder_id(r"C:\Users\a\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.lnk");
        match parse_startup_id(&id).expect("应能解析出合法 id") {
            StartupId::Folder { path } => {
                assert_eq!(path, r"C:\Users\a\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.lnk");
            }
            StartupId::Registry { .. } => panic!("应解析为 Folder 变体"),
        }
    }

    #[test]
    fn parse_startup_id_rejects_garbage() {
        assert!(parse_startup_id("not-a-valid-id").is_err());
        assert!(parse_startup_id("reg\u{1f}HKCU").is_err());
    }
}
