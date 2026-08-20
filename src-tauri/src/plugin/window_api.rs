//! 桌面窗口管理：管理**别的应用**的窗口（置顶 / 移动 / 缩放 / 最小化 / 关闭 / 切前台），
//! 不是 iTools 自己的窗口。Windows 上这类小工具（窗口置顶器、批量排列、Alt-Tab 增强）
//! 需求很大，插件目前完全做不了，本文件补上这块能力。
//!
//! # 安全红线：绝不允许操作 iTools 自己的窗口
//!
//! 插件如果能通过窗口管理把 iTools 主窗口关掉 / 挪出屏幕 / 强制最小化，会造成用户
//! 束手无策的不可恢复状态（热键唤不出来、看不见窗口、找不到进程）。所有会改变窗口状态
//! 的命令（focus/move/resize/set_rect/minimize/maximize/restore/close/set_topmost）
//! 执行前都先用 [`ensure_not_self`] 拿 `GetWindowThreadProcessId` 查出目标窗口所属进程 PID，
//! 与 `GetCurrentProcessId()` 比对，是自己的窗口一律拒绝并返回明确中文错误。
//! 只读的 `plugin_win_list` / `plugin_win_get_foreground` 不受此限制——读取信息本身无害，
//! 且 `plugin_win_get_foreground` 就该如实反映"此刻前台确实是 iTools"这一事实。
//!
//! # 列表过滤：不过滤就是一堆用户根本看不见的窗口，列表毫无用处
//!
//! `plugin_win_list` 枚举顶层窗口后按以下条件过滤：
//! - `IsWindowVisible` 为假的（不可见）
//! - 标题为空的（`GetWindowTextLengthW == 0`）
//! - 带 `WS_EX_TOOLWINDOW` 扩展样式的（工具窗口，如浮动面板，正常不出现在任务栏/Alt-Tab）
//! - `DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, ...)` 非 0 的（见 [`is_cloaked`]）：
//!   `IsWindowVisible` 对这类窗口仍返回真（窗口对象本身处于"可见"状态），但 DWM 合成器
//!   实际不会画出来，用户根本看不见——常见于被最小化到系统托盘、或运行在别的虚拟桌面的
//!   UWP 应用留下的隐藏宿主窗口。该 API 需要 `Win32_Graphics_Dwm` feature，已由主控加入
//!   `Cargo.toml`。
//!
//! # 切前台（focus）不保证 100% 成功——这是 Windows 系统限制，不是本实现的 bug
//!
//! Windows 自 2000 起就有前台窗口抢占锁定（foreground lock）：只有当前拥有前台的线程、
//! 或者已经与它"绑定"输入状态的线程，才能可靠地把前台切给别的窗口；否则
//! `SetForegroundWindow` 常常**静默返回 FALSE**（不抛异常，就是没生效，前台其实没变）。
//! [`focus_window_impl`] 用 `AttachThreadInput` 把本线程与当前前台窗口所属线程的输入状态
//! 临时绑在一起来提升成功率，但这只是缓解、不是消除限制（比如目标进程以管理员权限运行、
//! 而 iTools 不是，UIPI 会挡住），因此 `plugin_win_focus` 的返回值里带 `success: bool`，
//! 老实告诉调用方这次到底切没切成功，不会「调用不报错就当作成功」。
//!
//! # 权限门禁
//!
//! 所有命令统一要求插件声明并被授权 `window-manage`（`plugin_granted` 门禁，样板与
//! `context.rs` / `commands.rs` 里其它 `plugin_*` 命令一致）。
//!
//! # `close` 用 `WM_CLOSE`，不强杀
//!
//! `plugin_win_close` 只是把 `WM_CLOSE` 消息投进目标窗口的消息队列（`PostMessageW`），
//! 让目标应用走自己正常的关闭流程（可能弹"是否保存"对话框、可能自己忽略这条消息）。
//! 这意味着"消息投递成功"不等于"窗口真的关掉了"，本文件对此如实说明，不假装能保证结果。

use serde::{Deserialize, Serialize};
use tauri::State;

use super::commands::{caller_session, plugin_granted};
use super::PluginRegistry;
use crate::settings::SettingsStore;

/// 窗口在屏幕上的矩形（物理像素，左上右下），取不到时四个字段恒为 0。
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// 一个顶层窗口的快照，`plugin_win_list` / `plugin_win_get_foreground` 的返回元素。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowItem {
    /// 窗口句柄的整数值（`HWND` 的位模式），后续 `plugin_win_*` 命令用它定位目标窗口。
    pub hwnd: isize,
    /// 拥有该窗口的进程可执行文件名（不含路径），如 `"notepad.exe"`；取不到为空串。
    pub app: String,
    /// 窗口标题；取不到为空串。
    pub title: String,
    /// 窗口类名。
    pub class: String,
    /// 窗口矩形。
    pub rect: WindowRect,
    /// 是否处于最小化状态（`IsIconic`）。
    pub minimized: bool,
    /// 是否处于最大化状态（`IsZoomed`）。
    pub maximized: bool,
    /// 是否带 `WS_EX_TOPMOST` 扩展样式（当前是否置顶）。
    pub topmost: bool,
}

/// `plugin_win_set_rect` 的入参：把 `x/y/w/h` 收进一个结构体而不是四个独立参数，
/// 单纯是为了不撞上 `clippy::too_many_arguments`（`webview`/`registry`/`settings`/`hwnd`
/// 这四个已经是每条命令的固定开销，再摊开四个数字会超过默认的 7 参阈值）。
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RectInput {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// `plugin_win_focus` 的返回结果：如实反映这次切前台是否真的成功，
/// 不能因为 API 调用没报错就当作成功——`SetForegroundWindow` 会静默失败。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusResult {
    /// 是否确认成功把目标窗口切到了前台。
    pub success: bool,
    /// `success == false` 时的原因说明；成功时为 `None`。
    pub reason: Option<String>,
}

/// 枚举当前所有「用户看得见、有意义」的顶层窗口。过滤规则见模块文档。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 枚举任务本身异常（极少见）时返回错误说明
#[tauri::command]
pub async fn plugin_win_list(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Vec<WindowItem>, String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(list_windows_impl)
        .await
        .map_err(|e| format!("枚举窗口任务异常: {e}"))?
}

/// 取此刻真正的前台窗口快照（与 `context.rs` 的「唤起前的窗口」不同，这个就是当下）。
/// 不排除 iTools 自己——如果此刻前台确实是 iTools，如实返回 iTools 的窗口信息。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 此刻没有前台窗口（如系统被锁屏）时返回错误说明
#[tauri::command]
pub async fn plugin_win_get_foreground(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<WindowItem, String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(foreground_window_impl)
        .await
        .map_err(|e| format!("读取前台窗口任务异常: {e}"))?
        .ok_or_else(|| "此刻没有前台窗口".to_string())
}

/// 激活并前置目标窗口。成功率不是 100%——见模块文档「切前台不保证 100% 成功」一节。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在（句柄已失效）时返回错误说明
#[tauri::command]
pub async fn plugin_win_focus(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
) -> Result<FocusResult, String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, focus_window_impl))
        .await
        .map_err(|e| format!("切前台任务异常: {e}"))?
}

/// 移动目标窗口到 `(x, y)`（屏幕物理像素坐标），不改变其尺寸。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或系统拒绝该操作时返回错误说明
#[tauri::command]
pub async fn plugin_win_move(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
    x: i32,
    y: i32,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || {
        with_guarded_hwnd(hwnd, |h| move_window_impl(h, x, y))
    })
    .await
    .map_err(|e| format!("移动窗口任务异常: {e}"))?
}

/// 调整目标窗口尺寸为 `(w, h)`（物理像素），不改变其左上角位置。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或系统拒绝该操作时返回错误说明
#[tauri::command]
pub async fn plugin_win_resize(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
    w: i32,
    h: i32,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || {
        with_guarded_hwnd(hwnd, |handle| resize_window_impl(handle, w, h))
    })
    .await
    .map_err(|e| format!("调整窗口尺寸任务异常: {e}"))?
}

/// 一次性设置目标窗口的位置与尺寸。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或系统拒绝该操作时返回错误说明
#[tauri::command]
pub async fn plugin_win_set_rect(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
    rect: RectInput,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || {
        with_guarded_hwnd(hwnd, |handle| set_rect_impl(handle, rect.x, rect.y, rect.w, rect.h))
    })
    .await
    .map_err(|e| format!("设置窗口位置尺寸任务异常: {e}"))?
}

/// 最小化目标窗口，并校验确实进入了最小化状态。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或最小化请求未生效时返回错误说明
#[tauri::command]
pub async fn plugin_win_minimize(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, minimize_impl))
        .await
        .map_err(|e| format!("最小化窗口任务异常: {e}"))?
}

/// 最大化目标窗口，并校验确实进入了最大化状态。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或最大化请求未生效时返回错误说明
#[tauri::command]
pub async fn plugin_win_maximize(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, maximize_impl))
        .await
        .map_err(|e| format!("最大化窗口任务异常: {e}"))?
}

/// 还原目标窗口（取消最小化/最大化），并校验确实回到了正常状态。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或还原请求未生效时返回错误说明
#[tauri::command]
pub async fn plugin_win_restore(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, restore_impl))
        .await
        .map_err(|e| format!("还原窗口任务异常: {e}"))?
}

/// 请求关闭目标窗口（投递 `WM_CLOSE`，不强杀进程）。目标应用可能弹确认对话框、
/// 也可能自行忽略这条消息——本命令只保证消息投递成功，不保证窗口最终真的关闭。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或消息队列已满导致投递失败时返回错误说明
#[tauri::command]
pub async fn plugin_win_close(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, close_impl))
        .await
        .map_err(|e| format!("关闭窗口任务异常: {e}"))?
}

/// 开启/关闭目标窗口的置顶状态。
///
/// # Errors
/// - 插件未被授权 `window-manage` 时返回错误说明
/// - 目标窗口是 iTools 自己的窗口时拒绝并返回明确错误
/// - 目标窗口不存在，或系统拒绝该操作时返回错误说明
#[tauri::command]
pub async fn plugin_win_set_topmost(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hwnd: isize,
    on: bool,
) -> Result<(), String> {
    require_permission(&webview, &registry, &settings)?;
    tauri::async_runtime::spawn_blocking(move || with_guarded_hwnd(hwnd, |h| set_topmost_impl(h, on)))
        .await
        .map_err(|e| format!("设置窗口置顶任务异常: {e}"))?
}

/// 统一的权限门禁：所有 `plugin_win_*` 命令都要求插件声明并被授权 `window-manage`。
fn require_permission(
    webview: &tauri::Webview,
    registry: &State<'_, PluginRegistry>,
    settings: &State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = caller_session(webview, registry)?;
    if !plugin_granted(settings, registry, &session, "window-manage") {
        return Err("插件未获授权管理窗口（请在「插件管理」里授权 window-manage）".to_string());
    }
    Ok(())
}

// ---------- Windows 实现 ----------
//
// 下面所有会改变窗口状态的 `*_impl` 函数统一以 `isize`（插件传入的整数句柄）为参数，
// 而不是 `HWND`：这样 [`with_guarded_hwnd`] 与每个 `*_impl` 在 Windows / 非 Windows
// 两侧都能保持完全相同的函数签名（与 `context.rs` 的 `current_target` / `process_exe_name`
// 等函数同一约定），命令层的调用代码不必按平台分叉，非 Windows 平台也能正常类型检查通过
// （虽然运行时 [`with_guarded_hwnd`] 会在真正调用 `op` 之前就直接返回错误）。

#[cfg(windows)]
use windows::Win32::Foundation::HWND;

/// 校验目标窗口存在、且不属于 iTools 自身进程，通过后才把 `hwnd` 交给 `op` 执行真正的操作。
/// 这是所有会改变窗口状态的命令的统一入口，安全红线卡在这一层，任何新增的写操作只要经过
/// 这个函数就不可能漏掉自身窗口保护。
#[cfg(windows)]
fn with_guarded_hwnd<T>(hwnd: isize, op: impl FnOnce(isize) -> Result<T, String>) -> Result<T, String> {
    ensure_not_self(HWND(hwnd as *mut core::ffi::c_void))?;
    op(hwnd)
}

/// 非 Windows 平台没有任何窗口系统可查，直接拒绝，不调用 `op`。
#[cfg(not(windows))]
fn with_guarded_hwnd<T>(_hwnd: isize, _op: impl FnOnce(isize) -> Result<T, String>) -> Result<T, String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 校验目标窗口存在、且不属于 iTools 自身进程；两者有一项不满足就拒绝并返回中文错误。
#[cfg(windows)]
fn ensure_not_self(hwnd: HWND) -> Result<(), String> {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    let mut pid: u32 = 0;
    // SAFETY: hwnd 是调用方传入的整数句柄原样还原而来；即便句柄已失效（窗口已关闭），
    // 该 API 也只会把 lpdwprocessid 写为 0 并返回 0，不会解引用非法内存；
    // pid 指向本函数栈上有效的 u32。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Err("目标窗口不存在或已关闭".to_string());
    }
    // SAFETY: 无参数、无副作用的只读查询。
    let self_pid = unsafe { GetCurrentProcessId() };
    if pid == self_pid {
        return Err("不允许通过窗口管理操作 iTools 自身窗口".to_string());
    }
    Ok(())
}

/// 枚举所有顶层窗口，按模块文档的规则过滤后收集为 [`WindowItem`] 列表。
#[cfg(windows)]
fn list_windows_impl() -> Result<Vec<WindowItem>, String> {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    let mut items: Vec<WindowItem> = Vec::new();
    let lparam = LPARAM(std::ptr::addr_of_mut!(items) as isize);
    // SAFETY: enum_proc 只会在本次 EnumWindows 调用内部、同一线程同步回调，items 的生命周期
    // 覆盖整个枚举过程，lparam 携带的指针在此期间始终指向本函数栈上的有效对象。
    unsafe {
        let _ = EnumWindows(Some(enum_proc), lparam);
    }
    Ok(items)
}

#[cfg(not(windows))]
fn list_windows_impl() -> Result<Vec<WindowItem>, String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// `EnumWindows` 的回调：把符合列表过滤规则的窗口快照追加进 `lparam` 指向的 `Vec`。
///
/// # Safety
/// 仅供 [`list_windows_impl`] 通过 `EnumWindows` 调用；`lparam` 必须是该函数构造、
/// 生命周期覆盖整个枚举过程的 `&mut Vec<WindowItem>` 转换来的指针，不得在别处传入。
#[cfg(windows)]
unsafe extern "system" fn enum_proc(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    // SAFETY: 按函数级 `# Safety` 约定，lparam.0 是 list_windows_impl 里栈上 Vec 的有效指针，
    // 且整个枚举过程都在该函数调用栈内同步完成，不存在指针已失效的情况。
    let items = unsafe { &mut *(lparam.0 as *mut Vec<WindowItem>) };
    if let Some(item) = snapshot_if_listable(hwnd) {
        items.push(item);
    }
    windows::Win32::Foundation::TRUE
}

/// 对枚举到的单个窗口做过滤判断（可见 / 有标题 / 非工具窗口，见模块文档），
/// 通过才构造快照，否则返回 `None`。
#[cfg(windows)]
fn snapshot_if_listable(hwnd: HWND) -> Option<WindowItem> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowTextLengthW, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    // SAFETY: hwnd 来自 EnumWindows 回调，系统保证枚举期间句柄有效；纯只读查询。
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }
    // SAFETY: 同上。
    if unsafe { GetWindowTextLengthW(hwnd) } == 0 {
        return None;
    }
    // SAFETY: 同上，GWL_EXSTYLE 是标准扩展窗口样式索引，无副作用。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    if is_cloaked(hwnd) {
        return None;
    }

    build_window_item(hwnd, ex_style)
}

/// 查 `DWMWA_CLOAKED` 判断是否是被 DWM 隐藏的幽灵窗口——常见于被最小化到系统托盘、
/// 或运行在别的虚拟桌面的 UWP 应用留下的宿主窗口：`IsWindowVisible` 对它们仍返回真
/// （窗口对象本身是"可见"状态），但合成器实际上并不会把它画出来，用户根本看不见。
/// `DwmGetWindowAttribute` 查询失败（极少见，如目标窗口在查询瞬间已被销毁）时按
/// "未被 cloak"处理，不因为查不到状态就武断地把窗口从列表里剔除。
#[cfg(windows)]
fn is_cloaked(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};

    let mut cloaked: u32 = 0;
    // SAFETY: hwnd 来自 EnumWindows 回调，系统保证枚举期间句柄有效；cloaked 指向本函数栈上
    // 有效的 u32，其大小如实通过 cbattribute 传入，该 API 只会按此大小写回数据，不会越界写。
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            std::ptr::addr_of_mut!(cloaked).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && cloaked != 0
}

/// 取此刻真正的前台窗口（`GetForegroundWindow`），不做任何过滤——即便前台是 iTools 自己，
/// 也如实返回，因为这个命令的语义就是"此刻前台到底是谁"。
#[cfg(windows)]
fn foreground_window_impl() -> Option<WindowItem> {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowLongPtrW, GWL_EXSTYLE};

    // SAFETY: 无参数、无前置条件的只读查询；返回句柄可能为空（当前没有前台窗口，如锁屏），
    // 下面立刻判空处理，不解引用。
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }
    // SAFETY: hwnd 刚由 GetForegroundWindow 返回，无副作用的只读查询。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    build_window_item(hwnd, ex_style)
}

#[cfg(not(windows))]
fn foreground_window_impl() -> Option<WindowItem> {
    None
}

/// 采集一个窗口句柄的完整信息并组装成 [`WindowItem`]；`ex_style` 由调用方传入，避免
/// 重复查询一次（[`snapshot_if_listable`] / [`foreground_window_impl`] 都已经查过一次）。
/// 拿不到所属进程 PID（窗口在采集过程中恰好消失）时返回 `None`，不拼造半真半假的数据。
#[cfg(windows)]
fn build_window_item(hwnd: HWND, ex_style: u32) -> Option<WindowItem> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, IsIconic, IsZoomed, WS_EX_TOPMOST,
    };

    let mut pid: u32 = 0;
    // SAFETY: hwnd 由调用方保证在本次调用期间有效；pid 指向本函数栈上有效的 u32。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }

    // SAFETY: 只读查询窗口最小化状态。
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    // SAFETY: 只读查询窗口最大化状态。
    let maximized = unsafe { IsZoomed(hwnd) }.as_bool();
    let topmost = ex_style & WS_EX_TOPMOST.0 != 0;

    Some(WindowItem {
        hwnd: hwnd.0 as isize,
        app: process_exe_name(pid).unwrap_or_default(),
        title: window_text(hwnd),
        class: window_class(hwnd),
        rect: window_rect(hwnd),
        minimized,
        maximized,
        topmost,
    })
}

/// 读取窗口标题；取不到为空串。
#[cfg(windows)]
fn window_text(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;

    let mut buf = [0u16; 512];
    // SAFETY: hwnd 由调用方保证在本次调用期间有效；buf 是本函数栈上的定长缓冲，其长度如实
    // 作为切片长度传入，该 API 保证只写入不超过切片长度的内容。
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
}

/// 读取窗口类名；取不到为空串。
#[cfg(windows)]
fn window_class(hwnd: HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;

    let mut buf = [0u16; 256];
    // SAFETY: 同 window_text，buf 是本函数栈上的定长缓冲。
    let len = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..len.min(buf.len())])
}

/// 读取窗口矩形；取不到时返回全零矩形。
#[cfg(windows)]
fn window_rect(hwnd: HWND) -> WindowRect {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let mut raw = RECT::default();
    // SAFETY: hwnd 由调用方保证在本次调用期间有效；raw 指向本函数栈上有效的 RECT，
    // 失败时按全零矩形处理，不读取未初始化内存。
    if unsafe { GetWindowRect(hwnd, &mut raw) }.is_ok() {
        WindowRect { left: raw.left, top: raw.top, right: raw.right, bottom: raw.bottom }
    } else {
        WindowRect::default()
    }
}

/// 按进程 ID 查它的可执行文件名（不含路径）；查不到（进程已退出、权限不足等）返回 `None`。
/// 与 `context.rs::process_exe_name` 同一实现方式（该函数是私有的，本文件不能复用，独立实现）。
#[cfg(windows)]
fn process_exe_name(pid: u32) -> Option<String> {
    use std::path::Path;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: pid 来自 GetWindowThreadProcessId 的真实返回值；PROCESS_QUERY_LIMITED_INFORMATION
    // 是读取镜像名所需的最低权限，目标进程即便以更高完整性运行通常也不会拒绝这个请求。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    // SAFETY: handle 是上面刚打开的有效句柄；buf 是本函数栈上的定长缓冲，len 如实传入其容量，
    // 该 API 会把实际写入长度回填进 len。
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len)
    };
    // SAFETY: handle 只在这里关闭一次，之后不再使用。
    unsafe {
        let _ = CloseHandle(handle);
    }
    ok.ok()?;
    let path = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    Path::new(&path).file_name().map(|s| s.to_string_lossy().into_owned())
}

/// 激活并前置目标窗口，用 `AttachThreadInput` 提升成功率；如实检测并返回是否真的成功。
#[cfg(windows)]
fn focus_window_impl(hwnd: isize) -> Result<FocusResult, String> {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);

    // 若窗口是最小化状态，先还原——最小化窗口即便 SetForegroundWindow 成功，用户看到的
    // 仍是任务栏图标，体验上等于没生效。
    // SAFETY: 只读查询窗口最小化状态。
    let iconic = unsafe { IsIconic(hwnd) }.as_bool();
    if iconic {
        // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身。
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    } else {
        // SAFETY: 同上。
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
    }

    // SAFETY: 无参数、无前置条件的只读查询；返回句柄可能为空，下面判空处理。
    let fg = unsafe { GetForegroundWindow() };
    let mut attached = false;
    let mut fg_thread = 0u32;
    let cur_thread = unsafe { GetCurrentThreadId() };
    if !fg.0.is_null() && fg != hwnd {
        // SAFETY: fg 刚由 GetForegroundWindow 返回，无副作用的只读查询。
        fg_thread = unsafe { GetWindowThreadProcessId(fg, None) };
        if fg_thread != 0 && fg_thread != cur_thread {
            // SAFETY: fg_thread / cur_thread 均为刚查得的有效线程 ID；true 表示建立绑定，
            // 与下面 false 的解除绑定成对出现，确保不会残留跨线程输入状态共享。
            attached = unsafe { AttachThreadInput(cur_thread, fg_thread, true) }.as_bool();
        }
    }

    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身；此调用可能静默失败，
    // 下面用其返回值如实判断成败，不当成必然成功。
    let ok = unsafe { SetForegroundWindow(hwnd) }.as_bool();

    if attached {
        // SAFETY: 与上面成对的绑定必须在函数返回前解除，否则两个线程的输入状态会一直
        // 共享下去，是常见的资源泄漏坑。
        unsafe {
            let _ = AttachThreadInput(cur_thread, fg_thread, false);
        }
    }

    if ok {
        Ok(FocusResult { success: true, reason: None })
    } else {
        Ok(FocusResult {
            success: false,
            reason: Some(
                "系统拒绝了前台切换请求（Windows 前台窗口抢占限制，非 100% 成功，常见于目标\
                 以更高权限运行或用户近期未与该窗口交互）"
                    .to_string(),
            ),
        })
    }
}

#[cfg(not(windows))]
fn focus_window_impl(_hwnd: isize) -> Result<FocusResult, String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 移动窗口到 `(x, y)`，不改变尺寸（`SWP_NOSIZE`）、不改变 Z 序（`SWP_NOZORDER`）、
/// 不抢占前台（`SWP_NOACTIVATE`）。
#[cfg(windows)]
fn move_window_impl(hwnd: isize, x: i32, y: i32) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身；cx/cy 因 SWP_NOSIZE
    // 被忽略，传 0 占位。
    unsafe { SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| format!("移动窗口失败: {e}"))
}

#[cfg(not(windows))]
fn move_window_impl(_hwnd: isize, _x: i32, _y: i32) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 调整窗口尺寸为 `(w, h)`，不改变位置（`SWP_NOMOVE`）、不改变 Z 序、不抢占前台。
#[cfg(windows)]
fn resize_window_impl(hwnd: isize, w: i32, h: i32) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身；x/y 因 SWP_NOMOVE
    // 被忽略，传 0 占位。
    unsafe { SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| format!("调整窗口尺寸失败: {e}"))
}

#[cfg(not(windows))]
fn resize_window_impl(_hwnd: isize, _w: i32, _h: i32) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 一次性设置位置与尺寸，不改变 Z 序、不抢占前台。
#[cfg(windows)]
fn set_rect_impl(hwnd: isize, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身。
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE) }
        .map_err(|e| format!("设置窗口位置尺寸失败: {e}"))
}

#[cfg(not(windows))]
fn set_rect_impl(_hwnd: isize, _x: i32, _y: i32, _w: i32, _h: i32) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 请求最小化并校验确实进入了最小化状态，不校验就返回成功等于自欺欺人。
#[cfg(windows)]
fn minimize_impl(hwnd: isize) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, ShowWindow, SW_MINIMIZE};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身。
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
    // SAFETY: 只读查询窗口最小化状态。
    if unsafe { IsIconic(hwnd) }.as_bool() {
        Ok(())
    } else {
        Err("窗口未响应最小化请求（可能已关闭或被系统/目标应用拒绝）".to_string())
    }
}

#[cfg(not(windows))]
fn minimize_impl(_hwnd: isize) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 请求最大化并校验确实进入了最大化状态。
#[cfg(windows)]
fn maximize_impl(hwnd: isize) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{IsZoomed, ShowWindow, SW_MAXIMIZE};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身。
    unsafe {
        let _ = ShowWindow(hwnd, SW_MAXIMIZE);
    }
    // SAFETY: 只读查询窗口最大化状态。
    if unsafe { IsZoomed(hwnd) }.as_bool() {
        Ok(())
    } else {
        Err("窗口未响应最大化请求（可能已关闭或被系统/目标应用拒绝）".to_string())
    }
}

#[cfg(not(windows))]
fn maximize_impl(_hwnd: isize) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 请求还原（取消最小化/最大化）并校验确实回到了正常状态。
#[cfg(windows)]
fn restore_impl(hwnd: isize) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsZoomed, ShowWindow, SW_RESTORE};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身。
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    // SAFETY: 只读查询窗口最小化/最大化状态。
    let still_abnormal = unsafe { IsIconic(hwnd) }.as_bool() || unsafe { IsZoomed(hwnd) }.as_bool();
    if still_abnormal {
        Err("窗口未响应还原请求（可能已关闭或被系统/目标应用拒绝）".to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn restore_impl(_hwnd: isize) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 投递 `WM_CLOSE`；只保证消息投递成功，不保证目标应用真的会关闭（见模块文档）。
#[cfg(windows)]
fn close_impl(hwnd: isize) -> Result<(), String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身；WM_CLOSE 不携带
    // 额外参数，wparam/lparam 均为 0。
    unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
        .map_err(|e| format!("投递关闭消息失败: {e}"))
}

#[cfg(not(windows))]
fn close_impl(_hwnd: isize) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}

/// 开启/关闭窗口置顶。
#[cfg(windows)]
fn set_topmost_impl(hwnd: isize, on: bool) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let insert_after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    // SAFETY: hwnd 已由 with_guarded_hwnd 校验存在且非 iTools 自身；x/y/cx/cy 因
    // SWP_NOMOVE|SWP_NOSIZE 被忽略，传 0 占位。
    unsafe { SetWindowPos(hwnd, Some(insert_after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) }
        .map_err(|e| format!("设置窗口置顶状态失败: {e}"))
}

#[cfg(not(windows))]
fn set_topmost_impl(_hwnd: isize, _on: bool) -> Result<(), String> {
    Err("窗口管理仅支持 Windows".to_string())
}
