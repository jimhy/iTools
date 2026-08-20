//! 输入注入（模拟键鼠）与剪贴板变化监听，供翻译/代码片段/表情包/自动化类插件使用。
//!
//! ## 核心取舍
//!
//! - **`plugin_input_type_string` 按“输入法”原理输入，不是逐键模拟**：用 `SendInput` +
//!   `KEYEVENTF_UNICODE`，每个 UTF-16 码元发一次「按下+抬起」；代理对（emoji、生僻字等超出
//!   BMP 的字符）在 UTF-16 里本来就是两个码元，天然按两次发送即可，不需要特殊拼接逻辑。
//!   这条路径完全绕开虚拟键码/键盘布局，能打出任何 Unicode 字符，不要求目标控件支持 IME。
//! - **`plugin_input_key_tap` / 粘贴热键走真实虚拟键码**：因为「按键」这个语义本身依赖 VK
//!   （方向键、功能键、Ctrl+V 组合键等在目标应用眼里必须是「真的按键」，不能用 Unicode 注入
//!   模拟——很多程序只认 `WM_KEYDOWN`/VK，不认 Unicode 字符输入）。单字符键用
//!   `VkKeyScanW` 按当前键盘布局把字符换算成 VK + 所需 Shift 状态；具名键（方向键/功能键/
//!   Enter 等）用固定表。
//! - **剪贴板监听用轮询序列号，不用 `AddClipboardFormatListener`**：后者要接消息循环
//!   （`WM_CLIPBOARDUPDATE`），需要给某个窗口挂消息钩子，实现复杂且容易跟 Tauri/wry 自己的
//!   消息循环打架；`GetClipboardSequenceNumber` 每次剪贴板内容变化都会自增，用一条独立线程
//!   低频轮询（300ms）足够灵敏，且不依赖任何窗口消息，实现和生命周期管理都简单可靠。
//!
//! ## 两条关键限制（必须让插件开发者知道，否则会遇到「时灵时不灵」却查不出原因）
//!
//! 1. **调用方必须先把 iTools 主窗口/插件窗口切到不遮挡目标应用、且不抢焦点的状态，
//!    最好是先隐藏窗口**，本模块只负责把输入事件灌进 Windows 全局输入队列——它会打到当前
//!    「前台窗口」（`GetForegroundWindow` 决定的那个），如果 iTools 自己的窗口还在前台，
//!    注入结果会打在 iTools 自己身上而不是用户期望的目标应用。**隐藏/切焦点窗口不是本模块
//!    职责**（那是窗口管理层的事），本模块的每个注入类命令在文档里都会重复这一点。
//! 2. **Windows UIPI（User Interface Privilege Isolation）限制**：非提权（非管理员）进程
//!    无法向运行在**更高完整性级别**的窗口注入输入——比如以管理员身份运行的程序、UAC 提权
//!    对话框本身、部分系统安全桌面。iTools 若未以管理员运行，向这类目标注入会**静默失败或
//!    部分失败**（`SendInput` 本身通常仍返回“成功发送的事件数”，不代表目标真的收到了）。
//!    这不是本模块的 bug，是 Windows 的强制安全边界，无法在用户态绕过。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use tauri::{AppHandle, Manager, State};

use super::commands::{caller_session, plugin_granted};
use super::{ActiveSession, PluginRegistry};
use crate::settings::SettingsStore;

// ==================== 权限门禁的两个标识 ====================

/// 键鼠注入相关命令统一用的权限标识（新标识，需在插件 `plugin.json` 的 `permissions`
/// 里声明并被用户在「插件管理」里授权，见 `commands.rs::plugin_granted`）。
const PERM_INPUT: &str = "input-inject";
/// 剪贴板变化监听命令用的权限标识（新标识）。
const PERM_CLIPBOARD_WATCH: &str = "clipboard-watch";

/// 校验调用方已获 `input-inject` 授权，返回其会话。
fn require_input(
    webview: &tauri::Webview,
    registry: &PluginRegistry,
    settings: &SettingsStore,
) -> Result<ActiveSession, String> {
    let session = caller_session(webview, registry)?;
    if !plugin_granted(settings, registry, &session, PERM_INPUT) {
        return Err("插件未获授权模拟输入（请在「插件管理」里授权 input-inject）".to_string());
    }
    Ok(session)
}

/// 校验调用方已获 `clipboard-watch` 授权，返回其会话。
fn require_clipboard_watch(
    webview: &tauri::Webview,
    registry: &PluginRegistry,
    settings: &SettingsStore,
) -> Result<ActiveSession, String> {
    let session = caller_session(webview, registry)?;
    if !plugin_granted(settings, registry, &session, PERM_CLIPBOARD_WATCH) {
        return Err(
            "插件未获授权监听剪贴板（请在「插件管理」里授权 clipboard-watch）".to_string(),
        );
    }
    Ok(session)
}

// ==================== 对外事件名 ====================

/// 剪贴板发生变化时推给插件页的事件通道名（经 [`emit_to_plugin`]）。
const EVT_CLIPBOARD_CHANGED: &str = "plugin-clipboard-changed";

/// 把一条事件推给**插件页**（正式窗口 `plugin` 或调试窗口 `plugin-dev`，由 `label` 决定）。
///
/// 必须走 `webview.eval` → `window.__itoolsEmit(channel, payload)` 这条总线，**不能用
/// `app.emit_to` 的 Tauri 原生事件**：插件页是受限自定义协议页，拿不到 `window.__TAURI__`，
/// 原生事件推过去没有接收方，表现是「命令能跑、但插件收不到事件」。写法照抄
/// `exec.rs::emit_to_plugin`：payload 经 `serde_json` 序列化后插入 JS 字面量，保证转义安全。
fn emit_to_plugin<T: serde::Serialize>(app: &AppHandle, label: &str, channel: &str, payload: &T) {
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

// ==================== Win32 键鼠注入 ====================

#[cfg(windows)]
mod win_input {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
        VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12,
        VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT, VK_LEFT,
        VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
        VK_UP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// 一次 `SendInput` 调用失败（返回值小于事件数）时的统一错误文案。
    fn send_input_err(sent: u32, total: usize) -> String {
        format!(
            "输入注入被系统拒绝（已发送 {sent}/{total} 个事件）：目标窗口可能运行在更高权限\
             （如管理员/UAC 对话框），受 Windows UIPI 限制，非提权进程无法向其注入输入"
        )
    }

    /// 发送一批 `INPUT` 结构；返回系统实际接受的事件数，等于 `inputs.len()` 才算全部送达。
    ///
    /// SAFETY: `SendInput` 只读 `inputs` 指向的内存，长度按 `inputs.len()` 精确传入，
    /// 不会越界读写；`inputs` 在调用期间作为局部变量持续存活。
    fn send(inputs: &[INPUT]) -> Result<(), String> {
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(send_input_err(sent, inputs.len()));
        }
        Ok(())
    }

    fn key_unicode_input(code_unit: u16, key_up: bool) -> INPUT {
        let mut flags = KEYEVENTF_UNICODE;
        if key_up {
            flags |= KEYEVENTF_KEYUP;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: code_unit,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// 按“输入法”原理逐 UTF-16 码元输入任意字符串（含代理对/emoji/中文），绕开虚拟键盘布局。
    pub fn type_string(text: &str) -> Result<(), String> {
        let mut inputs = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            inputs.push(key_unicode_input(unit, false));
            inputs.push(key_unicode_input(unit, true));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs)
    }

    fn vk_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
        let flags: KEYBD_EVENT_FLAGS = if key_up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) };
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// 具名键 → 虚拟键码（不区分大小写；未覆盖的具名键直接报错，不猜测）。
    fn named_vk(name: &str) -> Option<VIRTUAL_KEY> {
        Some(match name.to_ascii_lowercase().as_str() {
            "enter" | "return" => VK_RETURN,
            "esc" | "escape" => VK_ESCAPE,
            "tab" => VK_TAB,
            "space" | "spacebar" => VK_SPACE,
            "backspace" => VK_BACK,
            "delete" | "del" => VK_DELETE,
            "insert" | "ins" => VK_INSERT,
            "home" => VK_HOME,
            "end" => VK_END,
            "pageup" | "pgup" => VK_PRIOR,
            "pagedown" | "pgdn" => VK_NEXT,
            "up" | "arrowup" => VK_UP,
            "down" | "arrowdown" => VK_DOWN,
            "left" | "arrowleft" => VK_LEFT,
            "right" | "arrowright" => VK_RIGHT,
            "f1" => VK_F1,
            "f2" => VK_F2,
            "f3" => VK_F3,
            "f4" => VK_F4,
            "f5" => VK_F5,
            "f6" => VK_F6,
            "f7" => VK_F7,
            "f8" => VK_F8,
            "f9" => VK_F9,
            "f10" => VK_F10,
            "f11" => VK_F11,
            "f12" => VK_F12,
            _ => return None,
        })
    }

    /// 单字符键（字母/数字/标点）→ (虚拟键码, 是否需要额外按住 Shift)，按**当前键盘布局**换算。
    fn char_vk(ch: char) -> Option<(VIRTUAL_KEY, bool)> {
        let mut buf = [0u16; 2];
        let units = ch.encode_utf16(&mut buf);
        if units.len() != 1 {
            return None; // 代理对字符没有对应的单个虚拟键，不是「按键」语义能表达的
        }
        // SAFETY: VkKeyScanW 是纯函数式查表调用，只读传入的 u16，不涉及内存生命周期问题。
        let packed = unsafe { VkKeyScanW(units[0]) };
        if packed == -1 {
            return None; // 当前键盘布局按不出这个字符
        }
        let vk = VIRTUAL_KEY((packed as u16) & 0xFF);
        let shift_state = (packed >> 8) & 0xFF;
        Some((vk, shift_state & 0x01 != 0))
    }

    /// 把 `key` 解析为虚拟键码：先查具名表，再按单字符走 `VkKeyScanW`；返回键码与「该键自身
    /// 是否要求 Shift」（会并入调用方显式传入的 modifiers）。
    fn resolve_key(key: &str) -> Result<(VIRTUAL_KEY, bool), String> {
        if let Some(vk) = named_vk(key) {
            return Ok((vk, false));
        }
        let mut chars = key.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else {
            return Err(format!("不支持的按键名：{key}"));
        };
        char_vk(ch).ok_or_else(|| format!("当前键盘布局无法输入按键：{key}"))
    }

    fn modifier_vk(name: &str) -> Option<VIRTUAL_KEY> {
        Some(match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => VK_CONTROL,
            "shift" => VK_SHIFT,
            "alt" | "menu" => VK_MENU,
            "win" | "meta" | "super" => VK_LWIN,
            _ => return None,
        })
    }

    /// 模拟一次「组合键点按」：按顺序按下所有修饰键 → 按下+抬起主键 → 按相反顺序抬起修饰键。
    pub fn key_tap(key: &str, modifiers: &[String]) -> Result<(), String> {
        let (vk, needs_shift) = resolve_key(key)?;
        let mut mod_vks = Vec::with_capacity(modifiers.len() + 1);
        for m in modifiers {
            let mv = modifier_vk(m).ok_or_else(|| format!("不支持的修饰键：{m}"))?;
            if !mod_vks.contains(&mv) {
                mod_vks.push(mv);
            }
        }
        if needs_shift && !mod_vks.contains(&VK_SHIFT) {
            mod_vks.push(VK_SHIFT);
        }

        let mut inputs = Vec::with_capacity(mod_vks.len() * 2 + 2);
        for mv in &mod_vks {
            inputs.push(vk_input(*mv, false));
        }
        inputs.push(vk_input(vk, false));
        inputs.push(vk_input(vk, true));
        for mv in mod_vks.iter().rev() {
            inputs.push(vk_input(*mv, true));
        }
        send(&inputs)
    }

    /// 发一次 Ctrl+V（供“先写剪贴板再粘贴”类命令复用）。
    pub fn ctrl_v() -> Result<(), String> {
        key_tap("v", &["ctrl".to_string()])
    }

    /// 虚拟桌面（含多显示器）矩形：`SendInput` 绝对坐标要求归一化到 0..=65535，且要带
    /// `MOUSEEVENTF_VIRTUALDESK` 才会按整个虚拟桌面（而非主显示器）解释坐标。
    ///
    /// SAFETY: `GetSystemMetrics` 是无副作用的只读查询，无内存安全问题。
    fn virtual_desktop_rect() -> (i32, i32, i32, i32) {
        unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        }
    }

    fn to_absolute(x: i32, y: i32) -> Option<(i32, i32)> {
        let (vx, vy, vw, vh) = virtual_desktop_rect();
        if vw <= 0 || vh <= 0 {
            return None;
        }
        // 归一化到 0..=65535；减 1 是 SendInput 绝对坐标的标准换算方式，避免右/下边缘越界。
        let nx = ((x - vx) as i64 * 65535) / (vw as i64 - 1).max(1);
        let ny = ((y - vy) as i64 * 65535) / (vh as i64 - 1).max(1);
        Some((nx.clamp(0, 65535) as i32, ny.clamp(0, 65535) as i32))
    }

    fn mouse_input(dx: i32, dy: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    /// 移动鼠标到屏幕绝对坐标（物理像素，虚拟桌面坐标系，可跨多显示器）。
    pub fn mouse_move(x: i32, y: i32) -> Result<(), String> {
        let (nx, ny) = to_absolute(x, y).ok_or("无法读取虚拟桌面尺寸".to_string())?;
        send(&[mouse_input(
            nx,
            ny,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    /// 移动到坐标并做一次点击（`double`=true 时点两下；`right`=true 时用右键）。
    pub fn mouse_click(x: i32, y: i32, right: bool, double: bool) -> Result<(), String> {
        let (nx, ny) = to_absolute(x, y).ok_or("无法读取虚拟桌面尺寸".to_string())?;
        let base = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
        let (down, up) = if right {
            (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
        } else {
            (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
        };
        let mut inputs = vec![mouse_input(nx, ny, base)];
        let clicks = if double { 2 } else { 1 };
        for _ in 0..clicks {
            inputs.push(mouse_input(nx, ny, MOUSEEVENTF_ABSOLUTE | down));
            inputs.push(mouse_input(nx, ny, MOUSEEVENTF_ABSOLUTE | up));
        }
        send(&inputs)
    }
}

#[cfg(not(windows))]
mod win_input {
    pub fn type_string(_text: &str) -> Result<(), String> {
        Err("输入注入仅在 Windows 上可用".to_string())
    }
    pub fn key_tap(_key: &str, _modifiers: &[String]) -> Result<(), String> {
        Err("输入注入仅在 Windows 上可用".to_string())
    }
    pub fn ctrl_v() -> Result<(), String> {
        Err("输入注入仅在 Windows 上可用".to_string())
    }
    pub fn mouse_move(_x: i32, _y: i32) -> Result<(), String> {
        Err("输入注入仅在 Windows 上可用".to_string())
    }
    pub fn mouse_click(_x: i32, _y: i32, _right: bool, _double: bool) -> Result<(), String> {
        Err("输入注入仅在 Windows 上可用".to_string())
    }
}

// ==================== 命令：按“输入法”原理输入字符串 ====================

/// 按“输入法”原理输入任意字符串（支持 Emoji、中文等任意 Unicode），不是逐键模拟虚拟键盘。
///
/// **调用方须先把 iTools 窗口切走/隐藏**，否则输入会打到 iTools 自身而不是目标应用
/// （本命令只管注入，不管窗口显隐/焦点，见模块头注释）。
///
/// # Errors
/// - 未授权 `input-inject` 时返回提示文案；
/// - 非 Windows 平台返回「仅在 Windows 上可用」；
/// - 目标窗口运行在更高完整性级别（如管理员进程）时，受 UIPI 限制会返回「被系统拒绝」，
///   这不是本命令的 bug，是 Windows 强制安全边界。
#[tauri::command]
pub fn plugin_input_type_string(
    text: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::type_string(&text)
}

// ==================== 命令：粘贴（先写剪贴板再发 Ctrl+V） ====================

/// 把 `text` 写入剪贴板并发送 Ctrl+V 粘贴到当前前台窗口。
///
/// **调用方须先把 iTools 窗口切走/隐藏**（同 [`plugin_input_type_string`]）。
///
/// # Errors
/// - 未授权 `input-inject`；
/// - 写剪贴板失败（如系统剪贴板被其它进程占用）；
/// - 注入 Ctrl+V 被 UIPI 拒绝（见模块头注释）。
#[tauri::command]
pub fn plugin_input_paste_text(
    text: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("打开剪贴板失败: {e}"))?;
    cb.set_text(text).map_err(|e| format!("写剪贴板文本失败: {e}"))?;
    win_input::ctrl_v()
}

/// 把 `data`（base64，任意 `image` crate 可解码格式）解码为图片写入剪贴板并发送 Ctrl+V。
///
/// **调用方须先把 iTools 窗口切走/隐藏**（同 [`plugin_input_type_string`]）。
///
/// # Errors
/// - 未授权 `input-inject`；
/// - base64 解码 / 图片解码失败；
/// - 写剪贴板失败；
/// - 注入 Ctrl+V 被 UIPI 拒绝。
#[tauri::command]
pub fn plugin_input_paste_image(
    data: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("base64 解码失败: {e}"))?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| format!("图片解码失败: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("打开剪贴板失败: {e}"))?;
    cb.set_image(arboard::ImageData {
        width: w,
        height: h,
        bytes: std::borrow::Cow::Owned(img.into_raw()),
    })
    .map_err(|e| format!("写剪贴板图片失败: {e}"))?;
    win_input::ctrl_v()
}

/// 把 `paths`（本机绝对路径列表）以「文件列表」（`CF_HDROP`）形式写入剪贴板并发送 Ctrl+V，
/// 效果等价于用户在资源管理器里选中这些文件后按 Ctrl+C 再到目标处 Ctrl+V。
///
/// 剪贴板文本/图片格式复用 `arboard`，但 `arboard` 不支持 `CF_HDROP`，这里手写最小 Win32
/// 剪贴板写入：构造 `DROPFILES` 头 + 双 NUL 结尾的 UTF-16 路径列表，`GlobalAlloc` 移动式内存
/// 塞入剪贴板。
///
/// **调用方须先把 iTools 窗口切走/隐藏**（同 [`plugin_input_type_string`]）。
///
/// # Errors
/// - 未授权 `input-inject`；
/// - `paths` 为空；
/// - 打开/写剪贴板失败；
/// - 注入 Ctrl+V 被 UIPI 拒绝。
#[tauri::command]
pub fn plugin_input_paste_file(
    paths: Vec<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    if paths.is_empty() {
        return Err("文件路径列表为空".to_string());
    }
    #[cfg(windows)]
    {
        win_clipboard::set_hdrop(&paths)?;
    }
    #[cfg(not(windows))]
    {
        return Err("输入注入仅在 Windows 上可用".to_string());
    }
    #[cfg(windows)]
    win_input::ctrl_v()
}

// ==================== 命令：按键 / 鼠标 ====================

/// 模拟一次「组合键点按」：`key` 为主键（如 `"a"` / `"Enter"` / `"F5"`），`modifiers` 支持
/// `"ctrl"` / `"shift"` / `"alt"` / `"win"` 任意组合（大小写不敏感）。
///
/// **调用方须先把 iTools 窗口切走/隐藏**（同 [`plugin_input_type_string`]）。
///
/// # Errors
/// - 未授权 `input-inject`；
/// - `key` 或 `modifiers` 中出现无法识别的键名；
/// - 注入被 UIPI 拒绝。
#[tauri::command]
pub fn plugin_input_key_tap(
    key: String,
    modifiers: Vec<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::key_tap(&key, &modifiers)
}

/// 移动鼠标到屏幕绝对坐标（物理像素，虚拟桌面坐标系，可跨多显示器）。
///
/// # Errors
/// 未授权 `input-inject`；无法读取虚拟桌面尺寸；注入被 UIPI 拒绝。
#[tauri::command]
pub fn plugin_input_mouse_move(
    x: i32,
    y: i32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::mouse_move(x, y)
}

/// 移动到 `(x, y)` 并单击左键。
///
/// # Errors
/// 同 [`plugin_input_mouse_move`]。
#[tauri::command]
pub fn plugin_input_mouse_click(
    x: i32,
    y: i32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::mouse_click(x, y, false, false)
}

/// 移动到 `(x, y)` 并双击左键。
///
/// # Errors
/// 同 [`plugin_input_mouse_move`]。
#[tauri::command]
pub fn plugin_input_mouse_double_click(
    x: i32,
    y: i32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::mouse_click(x, y, false, true)
}

/// 移动到 `(x, y)` 并单击右键。
///
/// # Errors
/// 同 [`plugin_input_mouse_move`]。
#[tauri::command]
pub fn plugin_input_mouse_right_click(
    x: i32,
    y: i32,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    require_input(&webview, &registry, &settings)?;
    win_input::mouse_click(x, y, true, false)
}

// ==================== CF_HDROP 剪贴板写入（文件列表） ====================

#[cfg(windows)]
mod win_clipboard {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::UI::Shell::DROPFILES;

    /// `CF_HDROP`（15）：拖放/剪贴板「文件列表」格式，见 WinUser.h。`windows` crate 未导出
    /// 该常量的固定位置，这里按官方文档数值直接定义，避免因 crate 版本差异找不到符号。
    const CF_HDROP: u32 = 15;

    /// 把一批绝对路径以 `CF_HDROP` 格式写入系统剪贴板。
    pub fn set_hdrop(paths: &[String]) -> Result<(), String> {
        // DROPFILES 头 + 每个路径的 UTF-16（含各自的 NUL）+ 末尾额外一个 NUL（双 NUL 结束）。
        let mut wide: Vec<u16> = Vec::new();
        for p in paths {
            wide.extend(p.encode_utf16());
            wide.push(0);
        }
        wide.push(0); // 列表结束的第二个 NUL

        let header_len = std::mem::size_of::<DROPFILES>();
        let payload_len = wide.len() * std::mem::size_of::<u16>();
        let total_len = header_len + payload_len;

        // SAFETY: GlobalAlloc(GMEM_MOVEABLE, total_len) 仅申请一块系统全局内存，不涉及别的
        // 内存的读写；失败时返回空句柄，下面立即检查。
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, total_len) }
            .map_err(|e| format!("分配剪贴板内存失败: {e}"))?;

        // SAFETY: GlobalLock 返回的指针指向刚分配的、大小为 total_len 字节的内存块；下面写入
        // 的 header（DROPFILES 大小）与 payload（wide 的字节长度）之和精确等于 total_len，
        // 不会越界。写入完成后立即 GlobalUnlock，指针不再使用。
        let write_result: Result<(), String> = unsafe {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                Err("锁定剪贴板内存失败".to_string())
            } else {
                let header = DROPFILES {
                    pFiles: header_len as u32,
                    pt: windows::Win32::Foundation::POINT { x: 0, y: 0 },
                    fNC: windows::core::BOOL(0),
                    fWide: windows::core::BOOL(1), // 路径按 UTF-16 (fWide=TRUE) 写入
                };
                std::ptr::write_unaligned(ptr as *mut DROPFILES, header);
                let payload_ptr = (ptr as *mut u8).add(header_len) as *mut u16;
                std::ptr::copy_nonoverlapping(wide.as_ptr(), payload_ptr, wide.len());
                let _ = GlobalUnlock(hmem);
                Ok(())
            }
        };
        if let Err(e) = write_result {
            // SAFETY: hmem 是本函数独占持有、尚未交给剪贴板的句柄，写入失败后由本函数负责释放。
            unsafe {
                let _ = windows::Win32::Foundation::GlobalFree(Some(hmem));
            }
            return Err(e);
        }

        // SAFETY: OpenClipboard(None) 不携带窗口所有权语义上的额外要求；三步（打开/清空/设置）
        // 全部在本函数同步栈内完成并配对 CloseClipboard，不会跨函数持有剪贴板锁。
        let open_result: Result<(), String> = unsafe {
            OpenClipboard(None).map_err(|e| format!("打开剪贴板失败: {e}"))?;
            let r = (|| -> Result<(), String> {
                EmptyClipboard().map_err(|e| format!("清空剪贴板失败: {e}"))?;
                SetClipboardData(CF_HDROP, Some(HANDLE(hmem.0)))
                    .map_err(|e| format!("写入剪贴板文件列表失败: {e}"))?;
                Ok(())
            })();
            let _ = CloseClipboard();
            r
        };
        if open_result.is_err() {
            // SetClipboardData 失败时所有权未转移给系统，需要自行释放；成功时系统接管 hmem，
            // 绝不能在成功路径上再释放一次（会导致剪贴板内容立即失效/UAF）。
            unsafe {
                let _ = windows::Win32::Foundation::GlobalFree(Some(hmem));
            }
        }
        open_result
    }
}

// ==================== 剪贴板变化监听（轮询序列号） ====================

#[cfg(windows)]
fn clipboard_sequence() -> u32 {
    use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
    // SAFETY: 无参数的只读系统查询，无内存安全问题。
    unsafe { GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn clipboard_sequence() -> u32 {
    0
}

/// 一次剪贴板监听会话：归属会话 + 停止标志 + 后台线程句柄。
struct WatchSession {
    owner: ActiveSession,
    stop: std::sync::Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// 剪贴板监听会话表（进程内唯一一份）。不用 Tauri managed state 是因为本文件是新增文件，
/// 按边界约束不允许去改 `lib.rs` 里的 `.manage(...)` 注册列表；用 `OnceLock` 存进程级单例，
/// 效果与 managed state 等价。
///
/// 回收有两条路：插件自己调 stop 命令，以及宿主在会话被换下/插件被禁用时调
/// [`stop_all_for_session`]（接线点见 `commands.rs` 里与 `serve_api` / `camera` /
/// `record_av` 并列的那两处收尾）。**两条都必须有**：只留第一条的话，插件被禁用后
/// 轮询线程会一直跑下去，而它此刻已经没有任何窗口可推——纯粹的资源泄漏。
static WATCH: OnceLock<Mutex<Option<WatchSession>>> = OnceLock::new();

fn watch_slot() -> &'static Mutex<Option<WatchSession>> {
    WATCH.get_or_init(|| Mutex::new(None))
}

/// `serde` payload：剪贴板变化通知内容目前只带一个变化序号，具体内容由插件自行按需
/// 再调用 `plugin.readText()` / `readImage()` 读取——这里不预先把剪贴板内容塞进事件，
/// 避免大图片/大文本被迫无谓地经事件总线搬运一遍。
#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ClipboardChanged {
    sequence: u32,
}

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(300);

/// 启动剪贴板变化监听：变化时经 [`EVT_CLIPBOARD_CHANGED`] 事件推给发起调用的插件窗口。
/// 同一会话重复调用是幂等的（先停旧的再起新的）。
///
/// # Errors
/// 未授权 `clipboard-watch` 时返回提示文案。
#[tauri::command]
pub fn plugin_clipboard_watch_start(
    app: AppHandle,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = require_clipboard_watch(&webview, &registry, &settings)?;
    stop_watch_locked(&mut lock(watch_slot()), None);

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();
    // 事件必须推回**发起调用的那个窗口**，不能按会话推导成固定的 `plugin` / `plugin-dev`。
    //
    // 后台常驻插件跑在 `plugin-bg-<id>` 窗口里，独立窗口在 `plugin-win-<id>-<...>` 里
    //（见 commands.rs 的 BG_WINDOW_PREFIX）。推给固定 label 的结果是：watchStart 返回成功、
    // 监听线程也真的在跑，但事件发给了另一个窗口，插件的 onChange **永远不会触发**——
    // 一个返回成功却什么都收不到的 API，比直接报错更难排查。而「常驻后台盯剪贴板」
    // 恰恰是这个能力最典型的用法。
    let label = webview.label().to_string();
    let handle = std::thread::spawn(move || {
        let mut last = clipboard_sequence();
        while !stop_for_thread.load(Ordering::Relaxed) {
            std::thread::sleep(WATCH_POLL_INTERVAL);
            if stop_for_thread.load(Ordering::Relaxed) {
                break;
            }
            let cur = clipboard_sequence();
            if cur != last {
                last = cur;
                emit_to_plugin(&app, &label, EVT_CLIPBOARD_CHANGED, &ClipboardChanged { sequence: cur });
            }
        }
    });

    *lock(watch_slot()) = Some(WatchSession { owner: session, stop, handle });
    Ok(())
}

/// 停止剪贴板变化监听（仅能停自己会话发起的监听，同 `audio.rs` 的**完整会话身份**校验：
/// 插件 id + `dev` 标志缺一不可，调试会话与同名正式会话不能互相停对方的监听）。
///
/// # Errors
/// 未授权 `clipboard-watch`；或当前没有本会话在跑的监听（此时静默返回成功——「停止一个
/// 本来就没在跑的监听」不算错误，避免插件卸载/重复调用时报一堆无意义的错）。
#[tauri::command]
pub fn plugin_clipboard_watch_stop(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> Result<(), String> {
    let session = require_clipboard_watch(&webview, &registry, &settings)?;
    stop_watch_locked(&mut lock(watch_slot()), Some(&session));
    Ok(())
}

/// 若当前有监听会话，且（`only_owner` 为 `None`，或该会话与 `only_owner` 是同一会话）就停掉它。
/// 宿主侧强制收掉某会话名下的剪贴板监听——插件页被换下 / 插件被禁用或卸载 /
/// `clipboard-watch` 授权被收回时调用，不等插件自己调 stop（它此刻可能已经没机会调了）。
///
/// 与 `serve_api::stop_all_for_session` 等同族函数对齐：按**完整会话身份**（id + dev）匹配，
/// 不是本会话的监听一律不动。
pub fn stop_all_for_session(session: &ActiveSession) {
    stop_watch_locked(&mut lock(watch_slot()), Some(session));
}

fn stop_watch_locked(slot: &mut Option<WatchSession>, only_owner: Option<&ActiveSession>) {
    let should_stop = match (slot.as_ref(), only_owner) {
        (Some(_), None) => true,
        (Some(s), Some(owner)) => s.owner.same_as(owner),
        (None, _) => false,
    };
    if !should_stop {
        return;
    }
    if let Some(session) = slot.take() {
        session.stop.store(true, Ordering::Relaxed);
        // 加入等待：轮询线程最长 sleep WATCH_POLL_INTERVAL 就会看到停止标志并退出，
        // 这里同步等待保证「stop 命令返回时线程已真正结束」，不留悬空线程。
        let _ = session.handle.join();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}
