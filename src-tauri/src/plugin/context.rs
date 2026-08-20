//! 上下文感知：让插件知道用户唤起 iTools 时前台是什么窗口，以及能不能进一步问出
//! 浏览器地址栏 URL / 资源管理器当前目录。这是「在 Chrome 里唤起就给下载视频、
//! 在资源管理器里唤起就给清理此目录」这类场景的地基。
//!
//! # 「唤起前的前台窗口」怎么拿——本文件最容易做成假实现的地方
//!
//! iTools 被全局热键唤起时，**自己会成为前台窗口**：如果插件调用本文件的命令时才去问
//! `GetForegroundWindow()`，问到的必然是 iTools 自己的某个窗口（主窗口/插件窗口，同一
//! 进程），什么都问不出来——这是不能拍脑袋直接实现的核心原因。
//!
//! ## 采用方案：后台轮询 + 按进程 ID 过滤自身窗口（自给自足，不需要主控改任何已有文件）
//!
//! 首次有代码路径用到本模块（任意一个 `plugin_context_*` 命令或 [`window_matches`]）时，
//! 惰性起一条后台线程，以 200ms 间隔调用 `GetForegroundWindow()`：
//! - 若返回句柄所属进程 PID == 本进程 PID（`GetCurrentProcessId()`），说明前台正是
//!   iTools 自己的某个窗口（主窗口/插件窗口/管理中心/调试窗口都是同一个进程），跳过、
//!   不覆盖缓存——这一刻的「上一个非自身前台窗口」缓存原样保留。
//! - 否则记下这个窗口的快照（进程名/标题/类名/句柄/矩形），存进一个全局 `Mutex` 缓存。
//!
//! 插件调用 `plugin_context_active_window` 等命令时，先尝试**实时**采一次（万一那一刻
//! 前台恰好不是自身，例如插件是不抢焦点的覆盖层），采不到（正常情况，前台是自身）就
//! 退回读这份缓存——缓存里的就是「唤起 iTools 之前，用户正在看的那个窗口」。
//!
//! ## 为什么不用 `SetWinEventHook`（事件驱动、零延迟的更优方案）
//!
//! `windows` crate 里 `SetWinEventHook` / `IUIAutomation` 等符号都在 `Win32_UI_Accessibility`
//! 这一个 feature 之下，而项目当前**没有启用**它（本次改动被要求只新增此文件、不许碰
//! `Cargo.toml`）。轮询用 200ms 的采样延迟换「不需要新 feature 也能先跑起来」；
//! 而浏览器地址栏读取（见下文）躲不开 `Win32_UI_Accessibility`，所以已经在下面申请了。
//! 等主控加上这个 feature 后，这里的轮询可以后续替换成 `SetWinEventHook`
//! （事件回调需要安装它的线程跑消息循环，改起来比现在的轮询线程更复杂，本次先不做）。
//!
//! ## 需要主控配合的地方（可选，不配合也能工作）
//!
//! 轮询是"自动补上"的，最坏延迟 200ms，对人类的按键节奏来说通常够用、无感。若想把这个
//! 延迟压到 0，主控可以在 `window::toggle()` 里 `show(&win)` **之前**调一次
//! [`refresh_now`]（本文件导出的 `pub fn`，无参数、无副作用，只是"立刻采一次样"）。
//! **这是可选优化，不调用完全不影响正确性**，只是精度从"至多晚 200ms"提升到"零延迟"。
//!
//! # 浏览器 URL：用 UI Automation 读地址栏，读不到就诚实返回 `None`
//!
//! 不猜、不从窗口标题里拆字符串冒充 URL。做法是用 `IUIAutomation` 在目标窗口的控件树里
//! 找地址栏输入框元素、读它的 `ValuePattern.CurrentValue`——这本身就是那个输入框里显示的
//! 真实文本，不是本文件编出来的。Chrome / Edge（同为 Chromium 内核）共享地址栏控件类名
//! `Chrome_OmniboxView`，可靠地按类名定位；Firefox 没有公开稳定的类名，改用
//! AutomationId `urlbar-input`，不同大版本可能改名或找不到，找不到就返回 `None`。
//!
//! **需要主控在 `Cargo.toml` 的 `windows` crate 里追加两个当前未启用的 feature**：
//! `Win32_UI_Accessibility`（`IUIAutomation` 所在模块）、`Win32_System_Variant`
//! （`VARIANT` 所在模块，`IShellWindows::Item` 与 `IUIAutomation::CreatePropertyCondition`
//! 都要用到，且它不会被现有任何一个已启用的 feature 自动带出来）。**`plugin_context_browser_url`
//! 与 `plugin_context_folder_path` 这两个命令在这两个 feature 落地前无法通过编译**
//! （`plugin_context_active_window` / [`window_matches`] 不受影响，零新增 feature 就能跑）。
//! 这是刻意的：与其写一段绕开 feature、编得过但什么都读不到的假实现，不如让编译器诚实地卡住。
//!
//! # 资源管理器目录：用 Shell COM（`IShellWindows`），另需 `Win32_System_Variant`
//!
//! `IShellWindows`（`CLSID_ShellWindows`）会枚举所有打开的资源管理器窗口，每个都能
//! `QueryInterface` 成 `IWebBrowser2`（历史遗留但至今仍受支持的自动化接口，PowerShell 的
//! `(New-Object -ComObject Shell.Application).Windows()` 走的就是它）；按 `HWND` 找到目标
//! 窗口后，读它的 `LocationURL`（`file:///C:/...` 形式）。只对类名是 `CabinetWClass` /
//! `ExploreWClass`（资源管理器窗口）的目标发起 COM 调用，别的窗口直接返回 `None`，不浪费
//! 一次进程外 COM 往返。虚拟命名空间目录（"此电脑"、回收站、控制面板…）没有文件系统路径，
//! `LocationURL` 会是空串，此时如实返回 `None`，不会拼一个看着像路径的假值出来。
//!
//! `IShellWindows`/`IWebBrowser2` 本身在已启用的 `Win32_UI_Shell` + `Win32_System_Com` 下就够，
//! 但 `IShellWindows::Item` 要传一个 `&VARIANT` 当索引——`VARIANT` 单独在 `Win32_System_Variant`
//! feature 之下（不会被 `Win32_UI_Shell`/`Win32_System_Com` 自动带出来），所以这个命令**也**
//! 要等下面申请的 feature 落地才能编译，不是标题说的那么"免费"（写文档时才发现，如实更正）。
//!
//! # 权限门禁
//!
//! 三个命令统一要求插件声明并被授权 `context-read`（`plugin_granted` 门禁，样板与
//! 其它 `plugin_*` 命令一致）。[`window_matches`] 是纯函数，不直接暴露给插件 JS，
//! 供主控在派发 `window` 类型触发器时调用，不在此处做权限判定。

#[cfg(windows)]
use std::path::Path;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
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

/// 一次前台窗口快照：`plugin_context_active_window` 的返回结构，也是
/// [`window_matches`] 用来做匹配判断的数据源。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWindowInfo {
    /// 拥有该窗口的进程可执行文件名（不含路径），如 `"chrome.exe"`；取不到为空串。
    pub app: String,
    /// 窗口标题；取不到为空串。
    pub title: String,
    /// 窗口类名；取不到为空串。
    pub class: String,
    /// 窗口句柄的整数值（`HWND` 的位模式）；非 Windows 平台恒为 0。
    pub hwnd: isize,
    /// 窗口矩形。
    pub rect: WindowRect,
}

/// 取「唤起 iTools 之前，用户正在看的那个窗口」的快照。
///
/// 拿不到时返回 `Ok(None)` 而**不是** `Err`。
///
/// 「此刻没有可报告的前台窗口」是完全正常的状态——iTools 刚启动、后台常驻插件被拉起、
/// 用户锁着屏，都会落到这里。把它当错误抛给插件，等于逼每个插件都去 try/catch 一个
/// 根本不是错误的情况；漏了 catch 还会让整段初始化逻辑中断。真机验收时正是这么暴露的。
/// 文档里也一直写的是「拿不到返回 null，必须判空」，实现要与之一致。
///
/// # Errors
/// 只有一种：插件未被授权 `context-read`。
#[tauri::command]
pub async fn plugin_context_active_window(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Option<ActiveWindowInfo>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "context-read") {
        return Err("插件未获授权读取当前窗口信息（请在「插件管理」里授权 context-read）".to_string());
    }
    tauri::async_runtime::spawn_blocking(current_target)
        .await
        .map_err(|e| format!("读取前台窗口任务异常: {e}"))
}

/// 若当前活动窗口是浏览器（Chrome / Edge / Firefox），读取地址栏 URL；读不到（非浏览器、
/// 找不到地址栏控件、控件没有值）诚实返回 `None`，绝不猜测或拼造。
///
/// # Errors
/// - 插件未被授权 `context-read` 时返回错误说明
#[tauri::command]
pub async fn plugin_context_browser_url(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Option<String>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "context-read") {
        return Err("插件未获授权读取当前窗口信息（请在「插件管理」里授权 context-read）".to_string());
    }
    let Some(win) = current_target() else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || browser_url_for(&win))
        .await
        .map_err(|e| format!("读取浏览器地址栏任务异常: {e}"))
}

/// 若当前活动窗口是资源管理器，读取它正在浏览的目录；虚拟命名空间目录（此电脑/回收站/
/// 控制面板等，没有文件系统路径）或非资源管理器窗口，诚实返回 `None`。
///
/// # Errors
/// - 插件未被授权 `context-read` 时返回错误说明
#[tauri::command]
pub async fn plugin_context_folder_path(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<Option<String>, String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "context-read") {
        return Err("插件未获授权读取当前窗口信息（请在「插件管理」里授权 context-read）".to_string());
    }
    let Some(win) = current_target() else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || folder_path_for(&win))
        .await
        .map_err(|e| format!("读取资源管理器目录任务异常: {e}"))
}

/// 判断「当前活动窗口」（与 `plugin_context_active_window` 同一份数据源）是否匹配给定条件。
/// 供主控实现 plugin.json 里新的 `window` 类型触发器；本函数只做纯匹配判断，不接触
/// 搜索/触发链路，也不做权限判定（触发器本身不是插件发起的 IPC 调用）。
///
/// - `app`：进程可执行文件名，大小写不敏感精确匹配；传空串 `""` 表示不限制。
/// - `title_pat`：标题的正则表达式（`regex` crate 语法）；`None` 不限制；正则本身编译
///   失败按"不匹配"处理，不会 panic。
/// - `class`：窗口类名，大小写不敏感精确匹配；`None` 不限制。
///
/// 拿不到当前窗口快照时返回 `false`。
pub fn window_matches(app: &str, title_pat: Option<&str>, class: Option<&str>) -> bool {
    let Some(win) = current_target() else {
        return false;
    };
    matches_snapshot(&win, app, title_pat, class)
}

/// [`window_matches`] 的纯匹配逻辑，抽出来是为了能在单元测试里喂假快照，
/// 不必依赖 `current_target()`——那个函数会读真实桌面状态（当前前台窗口），
/// 在 `cargo test` 的进程里跑，读到的必然是测试进程之外某个真实窗口（比如
/// 跑测试的终端/IDE），跟这里要断言的"匹配规则本身对不对"没有关系，混在一起
/// 测会得到一个看似能过、实则断言的是宿主机桌面状态而不是代码逻辑的假测试。
fn matches_snapshot(win: &ActiveWindowInfo, app: &str, title_pat: Option<&str>, class: Option<&str>) -> bool {
    if !app.is_empty() && !win.app.eq_ignore_ascii_case(app) {
        return false;
    }
    if let Some(pat) = title_pat {
        match regex::Regex::new(pat) {
            Ok(re) => {
                if !re.is_match(&win.title) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    if let Some(c) = class {
        if !win.class.eq_ignore_ascii_case(c) {
            return false;
        }
    }
    true
}

/// 立即采样一次前台窗口（若它不是 iTools 自己）并更新缓存。主控可选调用点见模块文档；
/// 不调用也不影响正确性，后台轮询线程最多 200ms 后会自动补上。
#[cfg(windows)]
pub fn refresh_now() {
    if let Some(win) = capture_if_foreign() {
        *last_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(win);
    }
}

/// 非 Windows 平台没有任何窗口系统可查，空实现。
#[cfg(not(windows))]
pub fn refresh_now() {}

// ---------- 内部：「唤起前窗口」缓存与后台轮询 ----------

#[cfg(windows)]
static LAST_FOREGROUND: OnceLock<Mutex<Option<ActiveWindowInfo>>> = OnceLock::new();

#[cfg(windows)]
fn last_slot() -> &'static Mutex<Option<ActiveWindowInfo>> {
    LAST_FOREGROUND.get_or_init(|| Mutex::new(None))
}

/// 取「唤起 iTools 之前的前台窗口」：先实时采一次样（万一此刻前台真的不是自身），
/// 采不到就退回读缓存。副作用：确保后台轮询线程已经在跑。
#[cfg(windows)]
fn current_target() -> Option<ActiveWindowInfo> {
    ensure_watcher();
    if let Some(live) = capture_if_foreign() {
        return Some(live);
    }
    last_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[cfg(not(windows))]
fn current_target() -> Option<ActiveWindowInfo> {
    None
}

/// 确保后台轮询线程已启动；整个进程生命周期只会真正起一条线程（`OnceLock` 去重）。
#[cfg(windows)]
fn ensure_watcher() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| loop {
            refresh_now();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
    });
}

#[cfg(not(windows))]
fn ensure_watcher() {}

/// 若当前前台窗口不是 iTools 自己，采一份快照；是自己（或没有前台窗口）返回 `None`。
#[cfg(windows)]
fn capture_if_foreign() -> Option<ActiveWindowInfo> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId,
    };

    // SAFETY: 无参数、无前置条件的只读查询；返回句柄可能为空（当前没有前台窗口，如锁屏），
    // 下面立刻判空处理，不解引用。
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return None;
    }

    let mut pid: u32 = 0;
    // SAFETY: hwnd 刚由 GetForegroundWindow 返回；lpdwprocessid 指向本函数栈上有效的 u32。
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: 无参数、无副作用的只读查询。
    if pid == unsafe { GetCurrentProcessId() } {
        // 前台正是 iTools 自己的某个窗口（主窗口/插件窗口/管理中心/调试窗口都在同一进程），
        // 不是「唤起前」的窗口，跳过、不覆盖缓存。
        return None;
    }

    let mut title_buf = [0u16; 512];
    // SAFETY: hwnd 有效；title_buf 是本函数栈上的定长缓冲，其长度如实作为切片长度传入，
    // 该 API 保证只写入不超过切片长度的内容。
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title_buf) }.max(0) as usize;
    let title = String::from_utf16_lossy(&title_buf[..title_len.min(title_buf.len())]);

    let mut class_buf = [0u16; 256];
    // SAFETY: 同上，class_buf 是本函数栈上的定长缓冲。
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_buf) }.max(0) as usize;
    let class = String::from_utf16_lossy(&class_buf[..class_len.min(class_buf.len())]);

    let mut raw_rect = RECT::default();
    // SAFETY: hwnd 有效；raw_rect 指向本函数栈上有效的 RECT，失败时按全零矩形处理。
    let rect = if unsafe { GetWindowRect(hwnd, &mut raw_rect) }.is_ok() {
        WindowRect {
            left: raw_rect.left,
            top: raw_rect.top,
            right: raw_rect.right,
            bottom: raw_rect.bottom,
        }
    } else {
        WindowRect::default()
    };

    Some(ActiveWindowInfo {
        app: process_exe_name(pid).unwrap_or_default(),
        title,
        class,
        hwnd: hwnd.0 as isize,
        rect,
    })
}

/// 按进程 ID 查它的可执行文件名（不含路径）；查不到（进程已退出、权限不足等）返回 `None`。
#[cfg(windows)]
fn process_exe_name(pid: u32) -> Option<String> {
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

#[cfg(not(windows))]
fn process_exe_name(_pid: u32) -> Option<String> {
    None
}

// ---------- 内部：资源管理器目录（Shell COM） ----------

/// 只对资源管理器窗口发起 COM 调用；找到匹配 `HWND` 的窗口后读 `LocationURL` 并转成本地路径。
/// 找不到匹配窗口、目标是虚拟命名空间目录（没有 `file://` URL）都诚实返回 `None`。
#[cfg(windows)]
fn folder_path_for(win: &ActiveWindowInfo) -> Option<String> {
    // 只有传统资源管理器窗口才可能是这两个类名之一；不是就不必发起一次进程外 COM 往返。
    if win.class != "CabinetWClass" && win.class != "ExploreWClass" {
        return None;
    }

    use windows::core::Interface;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};

    // SAFETY: 只是尝试把本线程 COM 初始化为 STA；已初始化（含被别的任务设成别的模式）时会
    // 返回错误，不当致命错误处理——紧接着的 CoCreateInstance 用 CLSCTX_LOCAL_SERVER 走进程外
    // 激活，对象运行在 Shell 自己的 STA 里，调用方线程处于哪种 apartment 不影响正确性
    // （与 paths_api.rs::send_to_recycle_bin 同一处理方式）。
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // SAFETY: rclsid 传入静态已知的 ShellWindows CLSID；用 CLSCTX_LOCAL_SERVER 是因为该对象
    // 由资源管理器所在进程提供，不是进程内 DLL。
    let shell_windows: IShellWindows =
        unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER) }.ok()?;

    // SAFETY: shell_windows 是刚创建的有效接口指针，纯只读查询。
    let count = unsafe { shell_windows.Count() }.ok()?;

    for i in 0..count {
        let index = VARIANT::from(i);
        // SAFETY: shell_windows 有效；index 是本函数刚构造、生命周期覆盖本次调用的 VARIANT。
        let item_result = unsafe { shell_windows.Item(&index) };
        let Ok(item) = item_result else { continue };

        // 有的枚举项可能是旧版 IE 窗口或已关闭的残留条目，QueryInterface 失败就跳过这一条，
        // 不当成整体失败。
        let Ok(browser) = item.cast::<IWebBrowser2>() else {
            continue;
        };

        // SAFETY: browser 是刚 QueryInterface 出的有效接口指针。
        let hwnd_result = unsafe { browser.HWND() };
        let Ok(browser_hwnd) = hwnd_result else { continue };
        if browser_hwnd.0 != win.hwnd {
            continue;
        }

        // SAFETY: 同上，browser 仍然有效。
        let url = unsafe { browser.LocationURL() }.ok()?;
        return file_url_to_path(&url.to_string());
    }
    None
}

#[cfg(not(windows))]
fn folder_path_for(_win: &ActiveWindowInfo) -> Option<String> {
    None
}

/// 把 `file:///C:/Users/...`（本地盘符）或 `file://server/share/...`（UNC）形式的
/// URL 转成 Windows 路径；空串、非 `file:` 协议（虚拟命名空间目录没有文件系统路径时
/// `LocationURL` 就是空串）一律返回 `None`，不拼造假路径。
fn file_url_to_path(url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    let rest = url.strip_prefix("file:///").or_else(|| url.strip_prefix("file://"))?;
    if rest.is_empty() {
        return None;
    }
    let decoded = percent_decode(rest);
    if decoded.as_bytes().get(1) == Some(&b':') {
        // "C:/Users/xxx" —— 本地盘符路径
        Some(decoded.replace('/', "\\"))
    } else {
        // 剩余形如 "server/share/xxx" —— UNC 路径
        Some(format!("\\\\{}", decoded.replace('/', "\\")))
    }
}

/// 极简 percent-decode：只处理 `%XX` 转义，纯字节级操作（不做 `str` 切片），
/// 遇到格式不对的 `%` 原样保留，不会因切到 UTF-8 字符中间而 panic。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------- 内部：浏览器地址栏（UI Automation） ----------
//
// ⚠ 本节用到 `windows::Win32::UI::Accessibility` 与 `windows::Win32::System::Variant`，
// 均需 `Cargo.toml` 新增 feature（见模块顶部文档），当前尚未启用，
// 这一节在 feature 落地前无法通过编译——这是刻意的，理由见模块文档。

/// 定位地址栏输入框的方式：Chromium 内核（Chrome / Edge）共享稳定类名，
/// Firefox 没有公开稳定类名，改用 AutomationId。
#[cfg(windows)]
enum AddressBarLocator {
    ClassName(&'static str),
    AutomationId(&'static str),
}

/// 按前台窗口所属进程名判断是否是已支持的浏览器，是就尝试用 UI Automation 读地址栏；
/// 不是已知浏览器、找不到地址栏控件、控件没有值，一律诚实返回 `None`。
#[cfg(windows)]
fn browser_url_for(win: &ActiveWindowInfo) -> Option<String> {
    let app = win.app.to_ascii_lowercase();
    let locator = match app.as_str() {
        "chrome.exe" | "msedge.exe" => AddressBarLocator::ClassName("Chrome_OmniboxView"),
        // Firefox 没有公开稳定的地址栏类名，用 AutomationId；不同大版本可能改名，
        // 找不到就走下面统一的「找不到即 None」路径，不用别的手段硬凑。
        "firefox.exe" => AddressBarLocator::AutomationId("urlbar-input"),
        _ => return None,
    };
    browser_url_via_uia(win.hwnd, locator)
}

#[cfg(windows)]
fn browser_url_via_uia(hwnd: isize, locator: AddressBarLocator) -> Option<String> {
    use windows::core::{Interface, BSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationValuePattern, TreeScope_Descendants,
        UIA_AutomationIdPropertyId, UIA_ClassNamePropertyId, UIA_ValuePatternId,
    };

    // SAFETY: 微软文档要求 UI Automation 客户端线程为 STA；已初始化（S_FALSE）时忽略即可，
    // 不涉及任何指针/内存操作。
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // SAFETY: CUIAutomation 是系统内置的进程内 COM 组件（uiautomationcore.dll 提供），
    // CLSCTX_INPROC_SERVER 与其注册方式一致；失败时返回 Err，不产生悬垂指针。
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.ok()?;

    // SAFETY: hwnd 来自最近一次前台窗口快照；即便目标窗口此刻已被关闭，ElementFromHandle
    // 也只会返回 Err，不会解引用非法内存。
    let root = unsafe { automation.ElementFromHandle(HWND(hwnd as *mut core::ffi::c_void)) }.ok()?;

    let (property, value) = match locator {
        AddressBarLocator::ClassName(name) => (UIA_ClassNamePropertyId, BSTR::from(name)),
        AddressBarLocator::AutomationId(id) => (UIA_AutomationIdPropertyId, BSTR::from(id)),
    };
    let value = VARIANT::from(value);
    // SAFETY: automation 是刚创建的有效接口指针；property/value 均为本函数栈上的合法值。
    let condition = unsafe { automation.CreatePropertyCondition(property, &value) }.ok()?;

    // SAFETY: root/condition 均为本函数刚创建的有效对象，在目标窗口的控件子树里查找匹配元素。
    let element = unsafe { root.FindFirst(TreeScope_Descendants, &condition) }.ok()?;

    // SAFETY: element 是上一步成功返回的有效对象；查询它是否支持 ValuePattern 并取回该模式。
    let pattern_unk = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) }.ok()?;
    let pattern: IUIAutomationValuePattern = pattern_unk.cast().ok()?;

    // SAFETY: pattern 来自刚成功的接口查询，读取只读的当前文本值。
    let text = unsafe { pattern.CurrentValue() }.ok()?.to_string();
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(not(windows))]
fn browser_url_for(_win: &ActiveWindowInfo) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_to_path_local_drive() {
        assert_eq!(
            file_url_to_path("file:///C:/Users/%E6%A1%8C%E9%9D%A2"),
            Some("C:\\Users\\桌面".to_string())
        );
    }

    #[test]
    fn file_url_to_path_unc() {
        assert_eq!(
            file_url_to_path("file://server/share/folder"),
            Some("\\\\server\\share\\folder".to_string())
        );
    }

    #[test]
    fn file_url_to_path_empty_is_none() {
        // 虚拟命名空间目录（此电脑/回收站等）没有文件系统路径，LocationURL 为空串。
        assert_eq!(file_url_to_path(""), None);
    }

    fn sample_window() -> ActiveWindowInfo {
        ActiveWindowInfo {
            app: "chrome.exe".to_string(),
            title: "示例页面 - Google Chrome".to_string(),
            class: "Chrome_WidgetWin_1".to_string(),
            hwnd: 12345,
            rect: WindowRect::default(),
        }
    }

    #[test]
    fn matches_snapshot_empty_app_is_wildcard() {
        assert!(matches_snapshot(&sample_window(), "", None, None));
    }

    #[test]
    fn matches_snapshot_app_case_insensitive() {
        assert!(matches_snapshot(&sample_window(), "CHROME.EXE", None, None));
        assert!(!matches_snapshot(&sample_window(), "msedge.exe", None, None));
    }

    #[test]
    fn matches_snapshot_title_regex() {
        assert!(matches_snapshot(&sample_window(), "", Some("示例.*"), None));
        assert!(!matches_snapshot(&sample_window(), "", Some("^不会匹配$"), None));
    }

    #[test]
    fn matches_snapshot_invalid_regex_is_false_not_panic() {
        // 正则语法错误必须按"不匹配"处理，不能 panic，也不能悄悄当成"不限制"放行。
        assert!(!matches_snapshot(&sample_window(), "", Some("("), None));
    }

    #[test]
    fn matches_snapshot_class_case_insensitive() {
        assert!(matches_snapshot(&sample_window(), "", None, Some("chrome_widgetwin_1")));
        assert!(!matches_snapshot(&sample_window(), "", None, Some("CabinetWClass")));
    }
}
