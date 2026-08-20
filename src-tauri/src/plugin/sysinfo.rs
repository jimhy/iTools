//! 系统信息 / 系统标准路径 / 「在资源管理器中定位文件」——面向系统监控、清理助手、
//! 硬件信息面板一类插件的基础能力，全部**只读、无高危副作用**，因此本模块的命令
//! 一律不做权限门禁（对比 [`super::paths_api`]：那边是「给文件系统访问权」，本模块
//! 只是「查询/展示」）。
//!
//! # 数据真实性（本模块最高优先级的约束）
//!
//! 项目未启用 `sysinfo` crate，全部指标靠 Win32 API + 注册表拿。哪些指标真的拿到了、
//! 哪些没做，见下表——拿不到的字段**不出现**在返回结构里（不是返回 0 或猜测值）：
//! - OS 版本 / 主机名 / CPU 型号与逻辑核心数 / 显卡型号 / 磁盘列表：注册表 + `Win32_Storage_FileSystem`。
//! - CPU 占用率：`GetSystemTimes` 两次采样算出（挂在 `Win32_System_Threading`）。
//! - 内存总量/占用：`GlobalMemoryStatusEx`（挂在 `Win32_System_SystemInformation`）。
//! - 电池电量/充电状态/有无电池：`GetSystemPowerStatus`（挂在 `Win32_System_Power`）。
//! - 网络上下行速率：`GetIfTable`（旧版 32 位计数器接口，挂在 `Win32_NetworkManagement_IpHelper`；
//!   更精确的 64 位版 `GetIfTable2` 额外需要 `Win32_NetworkManagement_Ndis` feature，项目未启用，
//!   见 [`read_if_table`] 文档里对这个取舍的说明），两次采样差值算字节/秒。
//!
//! # `plugin_sys_get_path` 不是文件访问入口
//!
//! 这个命令只把一个白名单名字（如 `"desktop"`）翻译成一条**路径字符串**，翻译过程
//! 不触碰磁盘、不创建句柄、不代表宿主给了插件读写这个目录的权限。插件如果真的需要
//! 读写某个系统命名位置，走的是 [`super::paths_api::plugin_paths_resolve`] /
//! `plugin_trash`（受 `fs-named-path` / `fs-trash` 权限门禁），两者白名单的名字**不通用**，
//! 不要把本命令的返回值当成「已授权访问」的凭证。

use std::path::{Path, PathBuf};

/// 单块磁盘的容量信息（`total_bytes` / `free_bytes` 都是查询当时的实时读数，
/// 不是缓存值——每次调用 [`plugin_sys_info`] / [`plugin_sys_usage`] 都会重新查一遍）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskInfo {
    /// 盘符形式的根路径，如 `"C:\\"`。
    pub drive: String,
    /// 总容量（字节）。
    pub total_bytes: u64,
    /// 当前用户可用的空闲容量（字节，即 `GetDiskFreeSpaceExW` 的
    /// `lpFreeBytesAvailableToCaller`，会受磁盘配额影响，比全局空闲量更准确反映
    /// 「这个用户还能写多少」）。
    pub free_bytes: u64,
    /// 文件系统名（如 `"NTFS"`）；极少数情况下 `GetVolumeInformationW` 会失败，此时为
    /// `None`，不用空字符串占位。
    pub file_system: Option<String>,
    /// 驱动器类型：`"fixed"` / `"removable"` / `"remote"` / `"cdrom"` / `"ramdisk"`。
    /// 未挂载卷（`DRIVE_NO_ROOT_DIR`）与无法判定的（`DRIVE_UNKNOWN`）不会出现在列表里。
    pub drive_type: String,
}

/// 静态系统信息（机器这次开机期间基本不会变化的那部分）。
///
/// 所有字段均为 `Option` / `Vec`：某项在当前系统上确实取不到时是 `None` /
/// 空数组，绝不用占位值冒充。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SysInfo {
    /// 形如 `"Windows 11 专业版 23H2 (Build 22631.3007)"`；来自注册表
    /// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`，各分段（版本名/功能更新号/
    /// 内部版本号）只要缺了某一段就跳过那一段，不补假值。
    pub os_version: Option<String>,
    /// 主机名（取自 `COMPUTERNAME` 环境变量，GUI 会话下由系统保证存在）。
    pub hostname: Option<String>,
    /// CPU 型号字符串，来自注册表
    /// `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0\ProcessorNameString`。
    pub cpu_name: Option<String>,
    /// **逻辑**核心数（`std::thread::available_parallelism()`，即超线程/多核算总线程数，
    /// 不是物理核心数——命名里写明 logical 避免误解）。
    pub cpu_logical_cores: Option<usize>,
    /// 显卡型号（可能不止一块，集显+独显都会列出）；来自注册表显示适配器类
    /// `{4d36e968-e325-11ce-bfc1-08002be10318}` 各子键的 `DriverDesc`。取不到任何一块
    /// 时为空数组。
    pub gpu_names: Vec<String>,
    /// 物理内存总量（字节），来自 `GlobalMemoryStatusEx` 的 `ullTotalPhys`。
    pub memory_total_bytes: Option<u64>,
    /// 是否有系统电池——`GetSystemPowerStatus` 的 `BatteryFlag` 明确报告「无电池」时为
    /// `Some(false)`（可视为台式机/无电池设备），明确报告有电池时为 `Some(true)`
    /// （可视为笔记本/平板），`BatteryFlag` 本身返回「未知」时为 `None`（不猜）。
    pub has_battery: Option<bool>,
    /// 磁盘列表，见 [`DiskInfo`]。
    pub disks: Vec<DiskInfo>,
}

/// 动态系统信息（需要采样/实时查询才有意义的那部分）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SysUsage {
    /// 系统整体 CPU 占用率（0.0~100.0）。算法：调用两次 `GetSystemTimes`，中间
    /// 间隔 [`CPU_SAMPLE_INTERVAL`]，用两次读数的差值算「非空闲时间占比」——
    /// **不是**单次瞬时读数。这个间隔与 `down_bytes_per_sec`/`up_bytes_per_sec` 的采样
    /// 共用同一次 `sleep`，不会为了凑指标叠加等待时间。若两次 `GetSystemTimes` 有一次
    /// 失败则为 `None`。
    pub cpu_percent: Option<f64>,
    /// 物理内存已用量（字节）= `ullTotalPhys - ullAvailPhys`，来自单次
    /// `GlobalMemoryStatusEx` 读数（内存占用不需要采样差值，一次调用就是真实瞬时值）。
    pub memory_used_bytes: Option<u64>,
    /// 物理内存可用量（字节），即 `ullAvailPhys`。
    pub memory_available_bytes: Option<u64>,
    /// 内存占用百分比（0~100），直接取 `GlobalMemoryStatusEx` 自己算好的
    /// `dwMemoryLoad`，不是我们自己用 used/total 二次估算的。
    pub memory_used_percent: Option<u32>,
    /// 电池电量百分比（0~100）。`BatteryLifePercent` 为 `255`（未知）时为 `None`；
    /// 没有电池的机器上，`GetSystemPowerStatus` 一般也会报未知，此时同样是 `None`
    /// （不会显示成 0%）。
    pub battery_percent: Option<u8>,
    /// 是否正在充电，来自 `BatteryFlag` 的「正在充电」位；`BatteryFlag` 本身未知时为
    /// `None`。
    pub is_charging: Option<bool>,
    /// 下行速率（字节/秒），两次采样 [`CPU_SAMPLE_INTERVAL`] 间隔内所有非回环网卡
    /// `InOctets` 增量之和除以耗时。见 [`read_if_table`] 文档：用的是旧版 32 位计数器
    /// 接口，仅在采样窗口极短的前提下才不受计数器回绕影响。
    pub down_bytes_per_sec: Option<u64>,
    /// 上行速率（字节/秒），算法同 `down_bytes_per_sec`，取 `OutOctets`。
    pub up_bytes_per_sec: Option<u64>,
    /// 各盘当前空闲/总容量的实时读数，结构同 [`SysInfo::disks`]。
    pub disks: Vec<DiskInfo>,
}

/// [`plugin_sys_usage`] 里两次采样之间的间隔——CPU 占用率与网速共用同一次
/// `sleep`，不叠加。太短会让噪声占比过大，太长会拖慢命令响应；200ms 是两者之间
/// 常见的折中值。
#[cfg(windows)]
const CPU_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// 查询静态系统信息：OS 版本、主机名、CPU 型号与逻辑核心数、显卡型号、内存总量、
/// 是否有电池、磁盘列表。
///
/// 不需要插件声明任何权限——这些信息在系统属性/设备管理器里本就是公开可见的。
/// 内部跑在阻塞线程池上（注册表查询可能有轻微 IO 延迟），不阻塞 UI。
///
/// # Errors
/// 当前实现下 Windows 平台不会主动返回 `Err`（单项取不到就在结构体里留 `None`/空数组）；
/// 非 Windows 平台固定返回「仅支持 Windows」；仅当 `spawn_blocking` 所在的线程被异常
/// 终止（极端情况，如进程被强制杀死一部分）时才会向上传播错误。
#[tauri::command]
pub async fn plugin_sys_info() -> Result<SysInfo, String> {
    tauri::async_runtime::spawn_blocking(sys_info_blocking)
        .await
        .map_err(|e| format!("系统信息采集任务异常终止: {e}"))?
}

#[cfg(windows)]
fn sys_info_blocking() -> Result<SysInfo, String> {
    Ok(SysInfo {
        os_version: read_os_version(),
        hostname: read_hostname(),
        cpu_name: read_cpu_name(),
        cpu_logical_cores: std::thread::available_parallelism().ok().map(std::num::NonZeroUsize::get),
        gpu_names: read_gpu_names(),
        memory_total_bytes: read_memory_status().map(|m| m.ullTotalPhys),
        has_battery: read_power_status().and_then(|p| has_battery_of(&p)),
        disks: list_disks(),
    })
}

#[cfg(not(windows))]
fn sys_info_blocking() -> Result<SysInfo, String> {
    Err("系统信息目前仅支持 Windows".to_string())
}

/// 查询动态系统信息：CPU 占用率、内存占用、电池状态、网络上下行速率（均为采样/实时
/// 读数）+ 各盘当前可用空间。
///
/// 不需要权限门禁。会话内部会真实 sleep 约 [`CPU_SAMPLE_INTERVAL`]（跑在阻塞线程池，
/// 不占 Tokio 运行时的异步任务槽；CPU 与网速两项采样共用这一次 sleep），所以本命令
/// 耗时略高于一次简单查询，属预期行为。
///
/// # Errors
/// 非 Windows 平台固定返回「仅支持 Windows」；Windows 上本身不返回 `Err`
/// （单项采样/读取失败时对应字段为 `None`，不是整体报错）；仅当采样任务所在线程被
/// 异常终止时才会向上传播错误。
#[tauri::command]
pub async fn plugin_sys_usage() -> Result<SysUsage, String> {
    tauri::async_runtime::spawn_blocking(sys_usage_blocking)
        .await
        .map_err(|e| format!("系统用量采样任务异常终止: {e}"))?
}

#[cfg(windows)]
fn sys_usage_blocking() -> Result<SysUsage, String> {
    // 内存/电池不需要采样，一次读数即真实瞬时值；CPU 与网速需要差值，
    // 两者共用下面同一次 sleep，不叠加等待时间。
    let mem = read_memory_status();
    let power = read_power_status();

    let times0 = sample_system_times();
    let net0 = read_if_table();
    std::thread::sleep(CPU_SAMPLE_INTERVAL);
    let times1 = sample_system_times();
    let net1 = read_if_table();

    let net_speed = net_speed_from_samples(net0, net1, CPU_SAMPLE_INTERVAL);

    Ok(SysUsage {
        cpu_percent: cpu_percent_from_samples(times0, times1),
        memory_used_bytes: mem.map(|m| m.ullTotalPhys.saturating_sub(m.ullAvailPhys)),
        memory_available_bytes: mem.map(|m| m.ullAvailPhys),
        memory_used_percent: mem.map(|m| m.dwMemoryLoad),
        battery_percent: power.as_ref().and_then(battery_percent_of),
        is_charging: power.as_ref().and_then(is_charging_of),
        down_bytes_per_sec: net_speed.map(|(down, _)| down),
        up_bytes_per_sec: net_speed.map(|(_, up)| up),
        disks: list_disks(),
    })
}

#[cfg(not(windows))]
fn sys_usage_blocking() -> Result<SysUsage, String> {
    Err("系统信息目前仅支持 Windows".to_string())
}

/// 查询系统标准路径。见模块文档「`plugin_sys_get_path` 不是文件访问入口」一节。
///
/// `name` 白名单：`home` `appData` `localAppData` `temp` `desktop` `documents`
/// `downloads` `pictures` `videos` `music` `fonts` `startup` `programFiles`。
///
/// # Errors
/// - `name` 不在白名单内：返回「不支持的路径名」并列出完整白名单
/// - 该命名位置在当前系统配置下无法确定（极少见，如损坏的用户 profile）：
///   返回「无法确定系统「xxx」目录」
#[tauri::command]
pub async fn plugin_sys_get_path(name: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || get_named_path(&name))
        .await
        .map_err(|e| format!("路径解析任务异常终止: {e}"))?
}

const NAMED_PATH_WHITELIST: [&str; 13] = [
    "home",
    "appData",
    "localAppData",
    "temp",
    "desktop",
    "documents",
    "downloads",
    "pictures",
    "videos",
    "music",
    "fonts",
    "startup",
    "programFiles",
];

fn get_named_path(name: &str) -> Result<String, String> {
    let path: Option<PathBuf> = match name {
        "home" => dirs::home_dir(),
        "appData" => dirs::data_dir(),
        "localAppData" => dirs::data_local_dir(),
        "temp" => Some(std::env::temp_dir()),
        "desktop" => dirs::desktop_dir(),
        "documents" => dirs::document_dir(),
        "downloads" => dirs::download_dir(),
        "pictures" => dirs::picture_dir(),
        "videos" => dirs::video_dir(),
        "music" => dirs::audio_dir(),
        // `dirs` crate 在 Windows 上没有实现 font_dir（恒为 None），字体/启动项/
        // Program Files 这三个改走 Win32 SHGetKnownFolderPath。
        "fonts" | "startup" | "programFiles" => platform_known_folder(name),
        _ => {
            return Err(format!(
                "不支持的路径名：{name}（白名单：{}）",
                NAMED_PATH_WHITELIST.join(" / ")
            ))
        }
    };
    path.map(|p| normalize_dir(&p))
        .ok_or_else(|| format!("无法确定系统「{name}」目录（该命名位置在当前系统上不可用）"))
}

/// 把目录路径规范成**不带尾部分隔符**的形式。
///
/// 各来源的约定并不统一：`std::env::temp_dir()` 在 Windows 上返回 `...\Temp\`（带尾斜杠），
/// 而 `dirs::download_dir()` 返回 `...\Downloads`（不带）。同一个 `getPath` 因为 name 不同
/// 返回两种形态，插件作者拼路径时必然踩坑——真机验收时正是这么踩到的：
/// `getPath("temp") + "\x.txt"` 拼出 `...\Temp\x.txt`，PowerShell 写不进去。
///
/// 盘符根（`C:\`）例外必须保留尾斜杠：去掉会变成「C 盘当前目录」这个完全不同的含义。
pub(crate) fn normalize_dir(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().into_owned();
    let trimmed = s.trim_end_matches(['\\', '/']);
    // 形如 `C:` 说明原串是盘符根，补回分隔符
    if trimmed.len() < s.len() && trimmed.ends_with(':') {
        return format!("{trimmed}\\");
    }
    if trimmed.is_empty() {
        return s;
    }
    trimmed.to_string()
}

#[cfg(windows)]
fn platform_known_folder(name: &str) -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Fonts, FOLDERID_ProgramFiles, FOLDERID_Startup};

    let rfid = match name {
        "fonts" => FOLDERID_Fonts,
        "startup" => FOLDERID_Startup,
        "programFiles" => FOLDERID_ProgramFiles,
        _ => return None,
    };
    known_folder_path(rfid).map(PathBuf::from)
}

#[cfg(not(windows))]
fn platform_known_folder(_name: &str) -> Option<PathBuf> {
    None
}

/// 用 `SHGetKnownFolderPath` 解析一个 Known Folder GUID 为绝对路径字符串。
#[cfg(windows)]
fn known_folder_path(rfid: windows::core::GUID) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    // SAFETY: `rfid` 是按值传入、调用期间持续有效的 GUID；`htoken` 传 None 表示用当前
    // 进程的访问令牌，是该 API 的标准用法；返回值失败时 `.ok()?` 直接短路，不触碰
    // 未初始化内存。
    let pwstr = unsafe { SHGetKnownFolderPath(&rfid, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: `pwstr` 是 shell32 刚用 CoTaskMemAlloc 分配、以 NUL 结尾的宽字符串，
    // 在下面 `CoTaskMemFree` 之前一直有效，`to_string` 只会读到 NUL 为止，不会越界。
    let text = unsafe { pwstr.to_string() }.ok();
    // SAFETY: `pwstr.0` 是本函数刚从 `SHGetKnownFolderPath` 拿到、尚未释放过的
    // CoTaskMem 指针，按 Win32 约定用 `CoTaskMemFree` 释放且只释放这一次，之后
    // 不再有任何代码路径使用它。
    unsafe { CoTaskMemFree(Some(pwstr.0 as *const _)) };
    text.filter(|s| !s.is_empty())
}

/// 在系统资源管理器里打开文件/文件夹所在目录，并高亮选中该项。
///
/// 走 `explorer.exe /select,<path>`：Windows 官方支持、行为等价于
/// `SHOpenFolderAndSelectItems` 的命令行方式。之所以不直接调用
/// `SHOpenFolderAndSelectItems`：它依赖的 `ITEMIDLIST` 挂在 `windows` crate 的
/// `Win32_UI_Shell_Common` feature 下，项目当前未启用这个 feature，而改 `Cargo.toml`
/// 超出本次任务边界；`explorer /select,` 达到完全相同的用户可见效果，且不需要任何
/// COM/PIDL 操作，复杂度更低。
///
/// # Errors
/// - `path` 为空字符串，或该路径在文件系统上不存在：直接报错，不会带着一个空/野路径
///   去拉起 explorer.exe
/// - 拉起 `explorer.exe` 失败（如系统进程创建失败）
#[tauri::command]
pub async fn plugin_show_item_in_folder(path: String) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("path 不能为空".to_string());
    }
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在：{path}"));
    }
    tauri::async_runtime::spawn_blocking(move || show_in_explorer(&p))
        .await
        .map_err(|e| format!("打开资源管理器任务异常终止: {e}"))?
}

/// 把一段文本包装成显式加引号的 Windows 命令行片段（`CommandLineToArgvW` /
/// `explorer.exe` 自定义解析器都认的格式）。
///
/// 之所以不能直接用 `std::process::Command::arg` 让标准库自动决定要不要加引号：
/// 标准库只保证「整段参数」在**标准 argv 拆分**下语义等价，但 `explorer.exe` 对
/// `/select,` 的解析是它自己写的一套简化扫描逻辑，被验证过能正常工作的形式是
/// `/select,"<path>"`——引号紧跟在逗号后面、只包住路径本体，`/select,` 前缀留在引号
/// 外面不加引号（这是 Chromium `ShowItemInFolder` 等业界参考实现采用的确切写法）。
/// 如果让标准库把 `/select,` 和路径当成同一个参数一起自动加引号，引号会跑到最前面
/// 变成 `"/select,C:\..."`——虽然按标准 argv 拆分规则解出来的字符串内容相同，但
/// `explorer.exe` 自己的扫描逻辑不保证认这种形态，所以这里手工拼、手工转义，
/// 经 [`std::os::windows::process::CommandExt::raw_arg`] 原样交给 `CreateProcess`，
/// 跳过标准库的自动加引号。
///
/// 转义规则严格照抄 Win32 命令行转义算法：结尾如果有连续反斜杠，要在闭引号前翻倍
/// （否则会被解释成「转义闭引号」而不是「路径末尾的反斜杠 + 结束引号」）。文件系统
/// 路径不可能包含双引号字符（NTFS/FAT 都禁止），所以不需要处理内部转义引号。
#[cfg(windows)]
fn quote_path_for_command_line(path: &str) -> String {
    let trailing_backslashes = path.chars().rev().take_while(|&c| c == '\\').count();
    let mut quoted = String::with_capacity(path.len() + trailing_backslashes + 2);
    quoted.push('"');
    quoted.push_str(path);
    quoted.push_str(&"\\".repeat(trailing_backslashes));
    quoted.push('"');
    quoted
}

#[cfg(windows)]
fn show_in_explorer(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // "/select," 前缀不加引号、紧贴着后面手工转义好的带引号路径，整段作为一个
    // raw_arg 交给 CreateProcess——见 quote_path_for_command_line 文档，
    // 这是为了匹配 explorer.exe 自身解析器验证过能用的确切格式，不依赖标准库的
    // 自动加引号（那种形态未必被 explorer.exe 认）。
    let arg = format!("/select,{}", quote_path_for_command_line(&path.display().to_string()));
    Command::new("explorer.exe")
        .raw_arg(&arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 explorer.exe 失败: {e}"))
}

#[cfg(not(windows))]
fn show_in_explorer(_path: &Path) -> Result<(), String> {
    Err("在文件管理器中定位文件目前仅支持 Windows".to_string())
}

/// 读取 OS 版本字符串。来源：`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`，
/// `ProductName` 缺失就直接判定取不到（返回 `None`），其余分段（`DisplayVersion` /
/// `ReleaseId` / `CurrentBuildNumber` / `UBR`）缺哪段就跳过哪段，不补假值。
#[cfg(windows)]
fn read_os_version() -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion").ok()?;
    let product_name: String = key.get_value("ProductName").ok()?;

    let mut s = product_name.trim().to_string();
    let display_version: Option<String> =
        key.get_value("DisplayVersion").ok().or_else(|| key.get_value("ReleaseId").ok());
    if let Some(dv) = display_version.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        s.push(' ');
        s.push_str(dv);
    }
    let build: Option<String> = key.get_value("CurrentBuildNumber").ok();
    if let Some(b) = build.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let ubr: Option<u32> = key.get_value("UBR").ok();
        s.push_str(" (Build ");
        s.push_str(b);
        if let Some(u) = ubr {
            s.push('.');
            s.push_str(&u.to_string());
        }
        s.push(')');
    }
    Some(s)
}

/// 读取主机名：直接取 `COMPUTERNAME` 环境变量（GUI 会话下系统保证已设置，不需要
/// 额外调用 Win32 API）。
#[cfg(windows)]
fn read_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok().filter(|s| !s.trim().is_empty())
}

/// 读取 CPU 型号字符串。来源：
/// `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0\ProcessorNameString`。
#[cfg(windows)]
fn read_cpu_name() -> Option<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = hklm.open_subkey(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0").ok()?;
    let name: String = key.get_value("ProcessorNameString").ok()?;
    let name = name.trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// 读取显卡型号列表。来源：显示适配器注册表类
/// `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}`
/// 下各编号子键（`0000` / `0001` / …）的 `DriverDesc` 值；这是显卡驱动安装时写入的
/// 官方友好名，不需要 WMI（项目未启用 WMI 相关 feature）。子键最多探测到 `0009`——
/// 一台机器挂 10 块以上显示适配器的情况极为罕见，足够覆盖常见的集显+独显组合。
#[cfg(windows)]
fn read_gpu_names() -> Vec<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let mut names = Vec::new();
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(class_key) =
        hklm.open_subkey(r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}")
    else {
        return names;
    };
    for i in 0..10u32 {
        let Ok(sub) = class_key.open_subkey(format!("{i:04}")) else { continue };
        let Ok(desc) = sub.get_value::<String, _>("DriverDesc") else { continue };
        let desc = desc.trim().to_string();
        if !desc.is_empty() && !names.contains(&desc) {
            names.push(desc);
        }
    }
    names
}

/// 枚举本地磁盘并读取容量信息。逻辑：`GetLogicalDrives` 拿盘符位图 → 对每个存在的
/// 盘符用 `GetDriveTypeW` 判定类型（跳过 `DRIVE_UNKNOWN` / `DRIVE_NO_ROOT_DIR`）→
/// `GetDiskFreeSpaceExW` 读容量（失败大概率是无介质的光驱/未就绪的盘，直接跳过，
/// 不伪造容量）→ `GetVolumeInformationW` 读文件系统名（失败则该字段为 `None`，
/// 不影响其余字段）。
#[cfg(windows)]
fn list_disks() -> Vec<DiskInfo> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    // DRIVE_* 常量实际挂在 WindowsProgramming 模块（不是 Storage::FileSystem），
    // 该 feature 项目已启用（见 Cargo.toml 里 Win32_System_WindowsProgramming 的注释）。
    use windows::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

    let mut out = Vec::new();
    // SAFETY: 无输入参数，纯读系统当前的逻辑盘符位图，无越界/生命周期风险。
    let mask = unsafe { GetLogicalDrives() };

    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let root_h = HSTRING::from(root.as_str());

        // SAFETY: `root_h` 是本次循环栈上持有、调用期间存活的合法宽字符串；
        // `GetDriveTypeW` 只读取该字符串，不写任何输出。
        let drive_type = unsafe { GetDriveTypeW(&root_h) };
        let drive_type_str = match drive_type {
            DRIVE_FIXED => "fixed",
            DRIVE_REMOVABLE => "removable",
            DRIVE_REMOTE => "remote",
            DRIVE_CDROM => "cdrom",
            DRIVE_RAMDISK => "ramdisk",
            // DRIVE_UNKNOWN / DRIVE_NO_ROOT_DIR：未挂载或无法判定，跳过，不伪造条目
            _ => continue,
        };

        let mut free_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        // SAFETY: `free_bytes` / `total_bytes` 是本次循环栈上的 u64 变量，
        // 三个输出指针的类型与 `GetDiskFreeSpaceExW` 声明的签名精确匹配，
        // 函数只会向这两个变量各写入一次 u64，不会越界。
        let got_space = unsafe {
            GetDiskFreeSpaceExW(&root_h, Some(&mut free_bytes), Some(&mut total_bytes), None).is_ok()
        };
        if !got_space {
            // 大概率是无介质的光驱 / 未就绪的可移动盘：不伪造容量，直接跳过这块盘
            continue;
        }

        let mut fs_buf = [0u16; 64];
        // SAFETY: `fs_buf` 是本次循环栈上 64 元素的 u16 缓冲区，作为切片传入后
        // `GetVolumeInformationW` 按缓冲区实际长度写入，不会越界；调用失败时
        // 直接返回 None，不读取可能未完整写入的缓冲区内容。
        let file_system = unsafe {
            GetVolumeInformationW(&root_h, None, None, None, None, Some(&mut fs_buf))
                .ok()
                .map(|()| String::from_utf16_lossy(&fs_buf).trim_end_matches('\0').to_string())
                .filter(|s| !s.is_empty())
        };

        out.push(DiskInfo { drive: root, total_bytes, free_bytes, file_system, drive_type: drive_type_str.to_string() });
    }
    out
}

/// 对 [`FILETIME`](windows::Win32::Foundation::FILETIME) 三元组做一次系统时间采样
/// （空闲/内核/用户态时间，单位 100ns，自系统启动起累计）。
#[cfg(windows)]
fn sample_system_times() -> Option<(u64, u64, u64)> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 三个输出指针均指向本函数栈上的 `FILETIME` 变量，大小与
    // `GetSystemTimes` 期望的输出缓冲区精确匹配；调用失败时直接返回 `None`，
    // 不使用可能未被完整写入的值。
    let ok = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).is_ok() };
    if !ok {
        return None;
    }
    let to_u64 = |t: FILETIME| ((u64::from(t.dwHighDateTime)) << 32) | u64::from(t.dwLowDateTime);
    Some((to_u64(idle), to_u64(kernel), to_u64(user)))
}

/// 用两次 `sample_system_times` 读数算系统整体 CPU 占用率（纯计算，不做 sleep——
/// 调用方负责在两次采样之间自己 sleep，方便和别的采样共用同一个等待窗口）。
/// `lpKernelTime` 按 Win32 约定已包含空闲时间，故总时间 = Δkernel + Δuser，
/// 忙碌占比 = (总时间 − Δidle) / 总时间。
#[cfg(windows)]
fn cpu_percent_from_samples(before: Option<(u64, u64, u64)>, after: Option<(u64, u64, u64)>) -> Option<f64> {
    let (idle0, kernel0, user0) = before?;
    let (idle1, kernel1, user1) = after?;

    let d_idle = idle1.saturating_sub(idle0);
    let d_kernel = kernel1.saturating_sub(kernel0);
    let d_user = user1.saturating_sub(user0);
    let total = d_kernel + d_user;
    if total == 0 {
        return None;
    }
    let busy = total.saturating_sub(d_idle);
    Some((busy as f64 / total as f64) * 100.0)
}

/// 一次性读取内存状态（`GlobalMemoryStatusEx`）。返回原始 `MEMORYSTATUSEX`，
/// 由调用方按需要取字段——总量给 [`plugin_sys_info`]，占用/可用给 [`plugin_sys_usage`]。
/// 这不是采样值，一次调用即当下的真实瞬时状态，不需要两次采样做差值。
#[cfg(windows)]
fn read_memory_status() -> Option<windows::Win32::System::SystemInformation::MEMORYSTATUSEX> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status =
        MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: `status` 是本函数栈上刚初始化的 `MEMORYSTATUSEX`，`dwLength` 已按文档
    // 要求设为结构体自身大小；`GlobalMemoryStatusEx` 只会按该固定布局写入自己的字段，
    // 不会越界。
    let ok = unsafe { GlobalMemoryStatusEx(&mut status).is_ok() };
    if ok {
        Some(status)
    } else {
        None
    }
}

/// 一次性读取电源状态（`GetSystemPowerStatus`）。返回原始 `SYSTEM_POWER_STATUS`，
/// 由 [`has_battery_of`] / [`is_charging_of`] / [`battery_percent_of`] 解读具体字段。
#[cfg(windows)]
fn read_power_status() -> Option<windows::Win32::System::Power::SYSTEM_POWER_STATUS> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `status` 是本函数栈上的 `SYSTEM_POWER_STATUS` 变量，`GetSystemPowerStatus`
    // 只会按该固定布局写入自己的字段，不会越界。
    let ok = unsafe { GetSystemPowerStatus(&mut status).is_ok() };
    if ok {
        Some(status)
    } else {
        None
    }
}

/// `BatteryFlag` 取值含义（MSDN `SYSTEM_POWER_STATUS`）：`1`=电量高 `2`=偏低
/// `4`=严重偏低 `8`=正在充电 `128`=系统无电池 `255`=状态未知。
#[cfg(windows)]
fn has_battery_of(status: &windows::Win32::System::Power::SYSTEM_POWER_STATUS) -> Option<bool> {
    match status.BatteryFlag {
        255 => None,       // 状态未知，不猜
        128 => Some(false), // 明确报告：无系统电池
        _ => Some(true),
    }
}

#[cfg(windows)]
fn is_charging_of(status: &windows::Win32::System::Power::SYSTEM_POWER_STATUS) -> Option<bool> {
    match status.BatteryFlag {
        255 => None,
        flag => Some(flag & 0x08 != 0),
    }
}

#[cfg(windows)]
fn battery_percent_of(status: &windows::Win32::System::Power::SYSTEM_POWER_STATUS) -> Option<u8> {
    // 255 = BATTERY_PERCENTAGE_UNKNOWN（含「本机没有电池」的情况，不显示成 0%）
    if status.BatteryLifePercent == 255 {
        None
    } else {
        Some(status.BatteryLifePercent)
    }
}

/// 读取各网卡累计收发字节数（`(接口索引, 累计收, 累计发)`），已过滤回环接口。
///
/// 用的是**旧版** `GetIfTable`（`MIB_IFROW`，32 位 `dwInOctets`/`dwOutOctets`
/// 计数器），不是更精确的 64 位 `GetIfTable2`（`MIB_IF_ROW2`）——`GetIfTable2`
/// 依赖的 `MIB_IF_TABLE2`/`MIB_IF_ROW2` 类型额外挂在 `Win32_NetworkManagement_Ndis`
/// feature 下，项目当前未启用该 feature（只启用了 `Win32_NetworkManagement_IpHelper`），
/// 改 `Cargo.toml` 超出本次任务边界。
///
/// 32 位计数器在网卡持续跑满带宽的情况下会在数小时到数天量级发生一次回绕，但本函数
/// 只用来做两次相隔 [`CPU_SAMPLE_INTERVAL`]（约 200ms）的**短窗口差值**：单个网卡要在
/// 200ms 内传输超过 4GB（约合 170Gbit/s 持续吞吐）才可能在这个窗口内绕回，这在当前
/// 消费级硬件上不现实。即便真的发生，`net_speed_from_samples` 也用 `saturating_sub`
/// 兜底为 0，不会算出离谱的负数或超大值。
#[cfg(windows)]
fn read_if_table() -> Option<Vec<(u32, u64, u64)>> {
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{GetIfTable, MIB_IFROW, MIB_IFTABLE, MIB_IF_TYPE_LOOPBACK};

    let mut size: u32 = 0;
    // SAFETY: 传 None + size=0 只是探测所需缓冲区大小，这是该 API 文档规定的标准
    // 第一步调用，不写任何内存、不解引用任何指针。
    let probe = unsafe { GetIfTable(None, &mut size, false) };
    if probe != ERROR_INSUFFICIENT_BUFFER.0 || size == 0 {
        return None;
    }

    let mut buf: Vec<u8> = vec![0u8; size as usize];
    let table_ptr = buf.as_mut_ptr().cast::<MIB_IFTABLE>();
    // SAFETY: `buf` 是刚按上一步探测出的确切大小分配、已清零的字节缓冲区；
    // `GetIfTable` 只会在 `size` 范围内写入自己的数据，不会越界；写入前的清零内容
    // 不会被当成已初始化数据读取（下面只在 GetIfTable 返回成功后才读它）。
    let ret = unsafe { GetIfTable(Some(table_ptr), &mut size, false) };
    if ret != ERROR_SUCCESS.0 {
        return None;
    }

    // SAFETY: 走到这里说明上一步 `GetIfTable` 已成功把数据写进 `buf`；
    // `dwNumEntries` 是它自己报告的真实条目数，`table` 字段地址就是 C 结构体里
    // 变长数组的起始地址（`#[repr(C)]`，字段偏移与 Win32 头文件一致）；`buf` 的
    // 大小已按 `GetIfTable` 要求的 `size` 分配，按 `dwNumEntries` 个 `MIB_IFROW`
    // （POD、`#[repr(C)]`）读取不会越界。
    let rows: Vec<MIB_IFROW> = unsafe {
        let num_entries = (*table_ptr).dwNumEntries as usize;
        let first = std::ptr::addr_of!((*table_ptr).table).cast::<MIB_IFROW>();
        std::slice::from_raw_parts(first, num_entries).to_vec()
    };

    Some(
        rows.into_iter()
            .filter(|row| row.dwType != MIB_IF_TYPE_LOOPBACK)
            .map(|row| (row.dwIndex, u64::from(row.dwInOctets), u64::from(row.dwOutOctets)))
            .collect(),
    )
}

/// 用两次 [`read_if_table`] 读数（按接口索引对齐）算下行/上行速率（字节/秒）。
/// 两次读数有任意一次失败、或 `elapsed` 为零，都返回 `None`（不编造速率）。
#[cfg(windows)]
fn net_speed_from_samples(
    before: Option<Vec<(u32, u64, u64)>>,
    after: Option<Vec<(u32, u64, u64)>>,
    elapsed: std::time::Duration,
) -> Option<(u64, u64)> {
    let before = before?;
    let after = after?;
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return None;
    }

    let mut down_bytes: u64 = 0;
    let mut up_bytes: u64 = 0;
    for &(idx, in1, out1) in &after {
        if let Some(&(_, in0, out0)) = before.iter().find(|&&(i, _, _)| i == idx) {
            down_bytes = down_bytes.saturating_add(in1.saturating_sub(in0));
            up_bytes = up_bytes.saturating_add(out1.saturating_sub(out0));
        }
    }
    Some(((down_bytes as f64 / secs) as u64, (up_bytes as f64 / secs) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只在 Windows CI/开发机上跑；断言口径宽松（这是真实机器数据，不同机器差异很大），
    /// 只验证「结构没崩、类型合理」而不是具体数值。
    #[cfg(windows)]
    #[test]
    fn sys_info_blocking_returns_plausible_shape() {
        let info = sys_info_blocking().expect("Windows 上应能取到系统信息");
        // 逻辑核心数如果拿到了，至少应该 >= 1
        if let Some(cores) = info.cpu_logical_cores {
            assert!(cores >= 1, "逻辑核心数不应为 0");
        }
        // 当前进程所在的系统盘一定在磁盘列表里（否则整个采集逻辑大概率是错的）
        assert!(!info.disks.is_empty(), "至少应枚举到一块磁盘（含运行本测试的系统盘）");
        for d in &info.disks {
            assert!(d.total_bytes > 0, "磁盘总容量不应为 0：{}", d.drive);
        }
        // 正常开发机/CI 机器一定能读到内存总量；真读不到时也不强制失败（虚拟化环境可能受限）
        if let Some(total) = info.memory_total_bytes {
            assert!(total > 0, "内存总量不应为 0");
        }
    }

    #[cfg(windows)]
    #[test]
    fn cpu_percent_from_two_real_samples_in_valid_range() {
        let t0 = sample_system_times();
        std::thread::sleep(CPU_SAMPLE_INTERVAL);
        let t1 = sample_system_times();
        if let Some(p) = cpu_percent_from_samples(t0, t1) {
            assert!((0.0..=100.0).contains(&p), "CPU 占用率应落在 0~100 之间，实际: {p}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn memory_used_plus_available_matches_total() {
        if let Some(m) = read_memory_status() {
            let used = m.ullTotalPhys.saturating_sub(m.ullAvailPhys);
            assert_eq!(used + m.ullAvailPhys, m.ullTotalPhys, "已用+可用应等于总量");
            assert!(m.dwMemoryLoad <= 100, "内存占用百分比不应超过 100");
        }
    }

    #[cfg(windows)]
    #[test]
    fn net_speed_from_samples_computes_delta_over_elapsed() {
        let before = Some(vec![(1u32, 1_000u64, 2_000u64)]);
        let after = Some(vec![(1u32, 3_000u64, 2_500u64)]);
        let (down, up) =
            net_speed_from_samples(before, after, std::time::Duration::from_secs(2)).expect("应能算出速率");
        assert_eq!(down, 1_000, "2000 字节增量 / 2 秒 = 1000 字节/秒");
        assert_eq!(up, 250, "500 字节增量 / 2 秒 = 250 字节/秒");
    }

    #[cfg(windows)]
    #[test]
    fn net_speed_from_samples_none_when_either_side_missing() {
        assert!(net_speed_from_samples(None, Some(vec![]), std::time::Duration::from_secs(1)).is_none());
        assert!(net_speed_from_samples(Some(vec![]), None, std::time::Duration::from_secs(1)).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn quote_path_for_command_line_handles_spaces_and_commas() {
        // 含空格：必须整体加引号
        assert_eq!(quote_path_for_command_line(r"C:\a b\c.txt"), r#""C:\a b\c.txt""#);
        // 含逗号但不含空格：加引号也无妨（explorer 只关心 /select, 之后到字符串结尾/下一个
        // 未转义空白为止，逗号本身不是它的分隔符）
        assert_eq!(quote_path_for_command_line(r"C:\a,b\c.txt"), r#""C:\a,b\c.txt""#);
        // 结尾反斜杠：闭引号前必须翻倍，否则会被解释成转义引号
        assert_eq!(quote_path_for_command_line(r"C:\a b\"), r#""C:\a b\\""#);
    }

    #[test]
    fn get_named_path_rejects_unknown_name() {
        let err = get_named_path("not-a-real-name").expect_err("未知名字应报错");
        assert!(err.contains("不支持的路径名"), "错误信息应指出名字不合法: {err}");
    }

    #[test]
    fn get_named_path_temp_always_resolves() {
        // temp 恒有值（std::env::temp_dir() 不依赖 dirs crate），跨平台都应成功
        let p = get_named_path("temp").expect("temp 应始终可解析");
        assert!(!p.is_empty());
    }
}
