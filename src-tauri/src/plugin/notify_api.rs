//! 插件通知增强 + 插件专属托盘图标。
//!
//! ## 背景
//! 旧能力 `itools.notify(body)`（见 `commands.rs::plugin_notify`）只有一行正文，没有标题、
//! 没有点击回调、没有进度。后台常驻类插件（隐藏窗口，见 `commands.rs::BG_WINDOW_PREFIX`）
//! 几乎必然需要托盘图标作为唯一可见入口——没有它，一个「常驻后台」的插件对用户来说
//! 就是「装了但看不见、不知道还在不在跑」。
//!
//! ## 通知点击回调——已绕开 `tauri-plugin-notification`，直接用 `notify-rust`
//! 实查 `tauri-plugin-notification 2.3.3` 源码发现它的桌面端 `show()` 把
//! `notify_rust::Notification::show()` 返回的 `NotificationHandle` 直接丢弃了，点击/关闭事件
//! 因此在那层封装里彻底收不到（详见历史记录）。经主控评估授权，`Cargo.toml` 已把 `notify-rust`
//! （原本只是间接依赖）升级为直接依赖，本模块改为**直接用 `notify_rust::Notification`**，自己
//! 拿住 `NotificationHandle`：
//!
//! - **点击通知本体**（`NotificationResponse::Default`）：给了 `featureCode` 就调
//!   [`super::commands::open_plugin`] 唤起该 feature；没给就推 `plugin-notify-click` 事件
//!   （带 `notifyId`）给插件页。
//! - **点击 action 按钮**（`NotificationResponse::Action(key)`）：Windows 后端（实查
//!   `tauri-winrt-notification 0.7.3::Toast::add_button` + `notify-rust` 的 `windows.rs`）
//!   确实支持——`key` 就是 [`plugin_tray_set`] 同款设计里插件自己传入的按钮 id，原样通过
//!   `plugin-notify-action` 事件回传，不需要额外校验/转换。
//! - **静音**（`silent`）：实查 `tauri-winrt-notification 0.7.3::Toast::sound`——`sound(None)`
//!   直接产出 `<audio silent="true" />`，`sound(Some(Sound::Default))` 产出空 `<audio>`（走系统
//!   默认提示音）。所以 `silent=true` 时不设置 `sound_name`（notify-rust 在没设置时会以 `None`
//!   调 `.sound()`，天然静音）；`silent=false`/缺省时显式 `sound_name("Default")`。**已生效**，
//!   不再是「收下不生效」的占位字段。
//! - **关闭/超时**（`NotificationResponse::Closed`）与 **Reply**（仅 macOS `preview-macos-un`
//!   特性会产生，本项目 Windows-only 用不到）：不推事件给插件页——没人订阅这两类结果。
//!
//! `NotificationHandle::wait_for_response` 是**阻塞调用**（内部 `mpsc::Receiver::recv()`），
//! 因此放进独立的 `std::thread::spawn` 里等，不占 tokio 工作线程；里面唤起插件走
//! `tauri::async_runtime::spawn` 把异步调用转进 tauri 的异步运行时（该线程本身不在 tokio
//! 上下文里，不能直接 `.await`）。这条线程不需要额外 `CoInitializeEx`——它只做一次
//! `Receiver::recv()`（纯标准库调用）和 Tauri 自己的线程安全 API（`webview.eval` /
//! `async_runtime::spawn`），没有任何直接 WinRT/COM 调用；真正触发 WinRT 弹窗的
//! `notification.show()` 仍在命令自身的调用线程上同步执行，与现有 `plugin_notify` 命令的既有
//! 调用方式一致。
//!
//! ## 插件托盘图标为什么必须与宿主自己的托盘完全隔离
//! 宿主自己的托盘（`lib.rs::setup_tray`，菜单项 `show`/`admin`/`reload_plugins`/`quit`）是
//! 唯一的、承载应用退出等关键操作的入口，绝不能被插件写坏。本模块做到隔离靠三层：
//!
//! 1. **完全独立的 [`tauri::tray::TrayIconBuilder`] 实例**：每个插件会话的托盘图标是单独
//!    `build()` 出来的 [`tauri::tray::TrayIcon`] 对象，物理上就不是宿主那一个，`set_icon`/
//!    `set_menu` 这类调用只会作用于插件自己持有的这个实例，不可能改到宿主的。
//! 2. **`TrayIconId` 与菜单项 id 都带随机 nonce 前缀**（见 [`new_unique_id`]）：Windows 下
//!    `TrayIconId` 全局唯一，避免与宿主或另一个插件的托盘 id 撞车。
//! 3. **菜单点击事件的“串听”防护**：tauri 的 `on_menu_event` 监听器登记在**全应用共享**的
//!    一个 `Vec` 里（源码注释原话：「called for any menu event, whether it is coming from this
//!    window, another window or from the tray icon menu」），也就是说无论宿主菜单还是任意一个
//!    插件菜单被点击，**所有**注册过的闭包都会被逐一调用一遍；且这个 Vec 只增不减（tauri 未
//!    暴露反注册 API），`plugin_tray_remove` 之后再 `plugin_tray_set` 会让旧闭包继续留在表里。
//!    本模块用「菜单项 id = `{本次构建的 nonce}::{插件传入的原始 id}`」而不是「id = 插件会话
//!    标识」来防串扰：每次真正新建（而非更新）托盘都换一个新 nonce，闭包里按**自己构建时捕获
//!    的 nonce**做前缀匹配——宿主的菜单事件 id（`"show"`/`"admin"`/…）、别的插件的 nonce
//!    前缀，以及本插件*上一次*已被移除的托盘所留下的旧闭包，都不会匹配通过，天然只有
//!    “当前这一个”托盘的闭包会真正执行转发。
//!
//! ## `plugin_tray_set` 是更新语义，不是每次新建
//! 「一个插件最多一个托盘图标，重复 set 是更新」——本模块严格做到：同一会话第二次调用
//! `plugin_tray_set` 时，**复用已有的 [`tauri::tray::TrayIcon`]**（`set_icon`/`set_tooltip`/
//! `set_menu`），不会再跑一次 `TrayIconBuilder::build()`。除了语义要求如此，这也是避免上面第 3
//! 点里“重复 build 攒一堆闭包”问题的关键——同一个会话只要没被 `remove` 过，nonce 从头到尾
//! 不变，压根不会产生新的重复闭包。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, State,
};

use super::commands::{caller_session, plugin_granted};
use super::{read_logo, ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

// ==================== 公共小工具 ====================

/// 容忍锁中毒（持锁线程 panic）：拿到内部值继续用。做法与 `exec.rs::lock` 一致。
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 某会话在 [`TrayState`] 里的唯一 key：`dev` 标志 + 插件 id，与调试/正式库隔离同一套规则
/// （见 `plugin::ActiveSession` 文档：调试插件与正式插件可能同名，只比 id 会互相踩）。
fn session_key(session: &ActiveSession) -> String {
    if session.dev {
        format!("dev:{}", session.id)
    } else {
        session.id.clone()
    }
}

/// 日志里只记「谁」，不记通知正文 / 托盘菜单文案——这些都可能是插件在处理的用户数据
/// （账号、路径、剪贴板内容……），道理同 `commands.rs::plugin_notify` 与 `exec.rs::who`。
/// key 的形态（`dev:` 前缀）恰好也是日志里想要的「区分调试/正式」样式，直接复用。
fn who(session: &ActiveSession) -> String {
    session_key(session)
}

/// 生成一个全局唯一 id：时间戳 + 单调计数器，足够本模块「托盘 nonce / 通知 id」这类
/// 只需要「进程内不重复」的场景，不追求跨进程/跨重启唯一。写法与 `exec.rs::new_stream_id`
/// 一致（未合并为公共函数：`exec.rs` 是私有实现，不对外暴露）。
fn new_unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now_ns:x}-{n:x}")
}

/// 把一条事件推给**插件页**。**必须**走 `webview.eval` → `window.__itoolsEmit`，不能用
/// `app.emit_to` 原生事件——插件页拿不到 `window.__TAURI__`，原生事件推过去没有接收方。
/// 照抄 `exec.rs::emit_to_plugin`（该函数是私有的，模块间不能直接复用，逻辑与注释保持一致）。
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

/// 某插件当前清单是否声明了 `code` 这个 feature——用于校验 `featureCode` 不是插件传错的
/// 死值（即便当前打不通点击回调，也不该悄悄放过一个压根不存在的 code，见模块头注释）。
fn feature_exists(registry: &PluginRegistry, session: &ActiveSession, code: &str) -> bool {
    if session.dev {
        registry
            .dev
            .list(&HashMap::new())
            .into_iter()
            .find(|p| p.id == session.id)
            .is_some_and(|p| p.features.iter().any(|f| f.code == code))
    } else {
        registry.plugins.read().is_ok_and(|g| {
            g.iter().any(|p| {
                p.manifest.name == session.id && p.manifest.features.iter().any(|f| f.code == code)
            })
        })
    }
}

/// 某插件的 logo（base64 data URL），用作托盘图标缺省值。调试会话查调试注册表，
/// 正式会话查正式注册表——与其它一切「按会话分流」的命令同一套规则。
fn plugin_logo_data_url(registry: &PluginRegistry, session: &ActiveSession) -> Option<String> {
    if session.dev {
        registry
            .dev
            .list(&HashMap::new())
            .into_iter()
            .find(|p| p.id == session.id)
            .and_then(|p| p.logo)
    } else {
        let plugins = registry.plugins.read().ok()?;
        let p = plugins.iter().find(|p| p.manifest.name == session.id)?;
        read_logo(&p.dir, &p.manifest.icon)
    }
}

// ==================== 通知增强 ====================

const EVT_NOTIFY_CLICK: &str = "plugin-notify-click";
const EVT_NOTIFY_ACTION: &str = "plugin-notify-action";

/// 点击通知本体（非按钮）且未给 `featureCode` 时，推给插件页的载荷。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NotifyClickEvent {
    notify_id: String,
}

/// 点击某个 action 按钮时，推给插件页的载荷：带插件自己传入的按钮 id。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct NotifyActionEvent {
    notify_id: String,
    action_id: String,
}

/// [`plugin_notify_show`] 里单个 action 按钮的描述（Windows 上真实支持，见模块头注释）。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyActionOpt {
    /// 按钮 id，点击后原样通过 `plugin-notify-action` 事件回传。
    pub id: String,
    /// 按钮上显示的文字。
    pub label: String,
}

/// [`plugin_notify_show`] 的参数。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyShowOptions {
    /// 通知标题；不传则只有正文（与旧的 `itools.notify` 行为一致，标题栏由系统用应用名兜底）。
    pub title: Option<String>,
    /// 通知正文。
    pub body: String,
    /// 点击通知**本体**（不是某个 action 按钮）后应唤起本插件的哪个 feature；必须是插件清单
    /// 里真实声明的 feature code，否则本命令直接报错拒绝（不允许传一个死值静默放过）。
    /// 不传则点击本体时改为推 `plugin-notify-click` 事件（带 `notifyId`）给插件页。
    pub feature_code: Option<String>,
    /// 静音展示。**已生效**（见模块头注释：Windows 后端 `sound(None)` 天然产出
    /// `<audio silent="true" />`）。缺省 `false`（正常带系统默认提示音）。
    pub silent: Option<bool>,
    /// 通知上的 action 按钮（Windows 真实支持，见模块头注释）；点击后原样通过
    /// `plugin-notify-action` 事件回传按钮 id，与 `featureCode`/默认点击互不影响——按钮永远
    /// 走 `plugin-notify-action`，点击通知本体才走 `featureCode`/`plugin-notify-click`。
    pub actions: Option<Vec<NotifyActionOpt>>,
}

/// [`plugin_notify_show`] 的返回。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyShowResult {
    /// 本次通知的 id（进程内唯一），点击/按钮事件的载荷会带上它，供插件页关联是哪条通知。
    pub notify_id: String,
}

/// 弹一条系统通知，返回 `notifyId`。是 `itools.notify(body)`（`commands.rs::plugin_notify`）的
/// 加强版：标题、`featureCode` 点击唤起、`silent` 静音、action 按钮均已打通，见模块头注释逐条
/// 说明各自的真实实现依据。
///
/// # Errors
/// - 会话不存在（插件未在运行）
/// - `body` 为空
/// - `featureCode` 给了值，但不是插件清单里真实声明的 feature code
/// - `actions` 里出现空 id 或重复 id（重复 id 会导致点击事件分不清是哪个按钮）
/// - 底层 `notify_rust::Notification::show()` 调用失败
#[tauri::command]
pub fn plugin_notify_show(
    opts: NotifyShowOptions,
    app: AppHandle,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<NotifyShowResult, String> {
    let session = caller_session(&webview, &registry)?;
    if opts.body.trim().is_empty() {
        return Err("body 为空".to_string());
    }
    if let Some(code) = &opts.feature_code {
        if !feature_exists(&registry, &session, code) {
            return Err(format!("featureCode 不存在于插件清单: {code}"));
        }
    }
    if let Some(actions) = &opts.actions {
        let mut seen = std::collections::HashSet::new();
        for a in actions {
            if a.id.trim().is_empty() {
                return Err("actions 里有空 id".to_string());
            }
            if !seen.insert(a.id.as_str()) {
                return Err(format!("actions 里 id 重复: {}", a.id));
            }
        }
    }

    let title_chars = opts.title.as_deref().map(|s| s.chars().count()).unwrap_or(0);
    let body_chars = opts.body.chars().count();
    let action_count = opts.actions.as_ref().map(Vec::len).unwrap_or(0);

    let mut notification = notify_rust::Notification::new();
    if let Some(title) = opts.title.as_deref() {
        notification.summary(title);
    }
    notification.body(&opts.body);
    if !opts.silent.unwrap_or(false) {
        // 显式给个具体声音（而不是让它保持"没设置"），因为"没设置"在这个后端等价于静音，
        // 见模块头注释对 `Toast::sound(None)` 的实查结论。
        notification.sound_name("Default");
    }
    if let Some(actions) = &opts.actions {
        for a in actions {
            notification.action(&a.id, &a.label);
        }
    }
    // 打包安装态下把通知归到本应用的 AppUserModelID，而不是回落到 PowerShell 的默认 id
    // （否则通知在操作中心会显示成"Windows PowerShell"发的，对用户是明显的品牌错乱）。
    // 写法照抄 `tauri-plugin-notification 2.3.3::desktop.rs` 里已经验证过的判断逻辑：只有
    // exe 目录不是 `target/debug`/`target/release`（即已安装打包）时才设置。
    #[cfg(windows)]
    {
        use std::path::MAIN_SEPARATOR as SEP;
        if let Ok(exe) = tauri::utils::platform::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let curr_dir = exe_dir.display().to_string();
                let is_dev_target = curr_dir.ends_with(&format!("{SEP}target{SEP}debug"))
                    || curr_dir.ends_with(&format!("{SEP}target{SEP}release"));
                if !is_dev_target {
                    notification.app_id(&app.config().identifier);
                }
            }
        }
    }

    let notify_id = new_unique_id();
    let handle = notification.show().map_err(|e| format!("弹通知失败: {e}"))?;

    // wait_for_response 是阻塞调用，放独立线程等；具体理由见模块头注释。
    let feature_code = opts.feature_code.clone();
    let owner = session.clone();
    let evt_app = app.clone();
    let evt_label = webview.label().to_string();
    let evt_notify_id = notify_id.clone();
    std::thread::spawn(move || {
        let _ = handle.wait_for_response(move |response: &notify_rust::NotificationResponse| {
            match response {
                notify_rust::NotificationResponse::Default => {
                    if let Some(code) = &feature_code {
                        let target = if owner.dev {
                            format!("{}{}#{code}", crate::dev::DEV_ID_PREFIX, owner.id)
                        } else {
                            format!("{}#{code}", owner.id)
                        };
                        let app2 = evt_app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = super::commands::open_plugin(app2, target, String::new(), false).await;
                        });
                    } else {
                        emit_to_plugin(
                            &evt_app,
                            &evt_label,
                            EVT_NOTIFY_CLICK,
                            &NotifyClickEvent {
                                notify_id: evt_notify_id.clone(),
                            },
                        );
                    }
                }
                notify_rust::NotificationResponse::Action(key) => {
                    emit_to_plugin(
                        &evt_app,
                        &evt_label,
                        EVT_NOTIFY_ACTION,
                        &NotifyActionEvent {
                            notify_id: evt_notify_id.clone(),
                            action_id: key.clone(),
                        },
                    );
                }
                // 被划掉/超时关闭，以及 Reply（仅 macOS preview-macos-un 特性会产生，本项目
                // Windows-only 用不到）：都没有订阅方，不推事件。
                notify_rust::NotificationResponse::Closed(_) | notify_rust::NotificationResponse::Reply(_) => {}
            }
        });
    });

    ilog!(
        "[iTools][plugin] notify_show: 来自 {}，标题 {title_chars} 字，正文 {body_chars} 字，\
         actions={action_count}（内容含用户数据，不入日志）",
        who(&session)
    );

    Ok(NotifyShowResult { notify_id })
}

// ==================== 插件托盘图标 ====================

const EVT_TRAY_CLICK: &str = "plugin-tray-click";
const EVT_TRAY_MENU: &str = "plugin-tray-menu";

/// 托盘图标点击事件推给插件页时的载荷。目前只是一个空对象（占位，供以后扩展按键/位置等
/// 信息），先把事件通不通打通——载荷从空对象扩展字段是非破坏性变更，先做窄接口更安全。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrayClickEvent {}

/// 托盘菜单项被点击时推给插件页的载荷：带插件自己传入的原始菜单项 id（nonce 前缀已在
/// 后端剥离，插件页拿到的就是它建菜单时传的那个 id，原样往返）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct TrayMenuClickEvent {
    id: String,
}

/// [`plugin_tray_set`] 里单个菜单项的描述。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayMenuItemOpt {
    /// 菜单项 id，点击后原样回传给插件页（见 [`TrayMenuClickEvent`]）。
    pub id: String,
    pub label: String,
    /// 缺省可点击（true）。
    pub enabled: Option<bool>,
    /// 为 true 时忽略 `id`/`label`，渲染成一条分隔线。
    pub separator: Option<bool>,
}

/// [`plugin_tray_set`] 的参数。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraySetOptions {
    /// 图标，base64 PNG（可带 `data:image/png;base64,` 前缀，也接受不带前缀的纯 base64）。
    /// 缺省用插件自己的 logo（`plugin.json` 的 `icon` 字段）；两者都没有则报错——没有图标的
    /// 托盘图标在系统里会显示成一个空白占位方块，等同于「装了看不见」，不能悄悄这么弹给用户。
    pub icon: Option<String>,
    /// 悬浮提示文字。创建时缺省用插件 id；更新时不传则保留上一次设置的值。
    pub tooltip: Option<String>,
    /// 右键菜单。创建时不传 = 不带菜单；更新时不传 = 保留上一次的菜单，传 `[]` = 清空菜单。
    pub menu: Option<Vec<TrayMenuItemOpt>>,
}

/// 一个插件会话持有的托盘图标：图标对象本身 + 归属信息 + 当前这次构建用的 nonce
/// （菜单点击事件靠它做「只有最新这一次构建的托盘」才会转发，见模块头注释第 3 点）。
struct PluginTraySession {
    // 会话归属由 `sessions` 这张 map 的 key（`session_key`）编码，事件推送用的是各闭包
    // 捕获的 label 副本——所以这里不再冗余存一份 owner / label。存了不读的字段迟早
    // 与真正生效的那份走歪，而且不会有任何报错提醒。
    nonce: String,
    icon: TrayIcon,
}

/// `plugin_tray_set` / `plugin_tray_remove` 的运行期状态（managed）。**每个会话最多一条记录**
/// ——重复 `set` 是更新而不是新增，见模块头注释。
#[derive(Default)]
pub struct TrayState {
    sessions: Mutex<HashMap<String, PluginTraySession>>,
}

/// 把 base64（可带 `data:image/xxx;base64,` 前缀）解码、限尺寸、转成托盘图标可用的 RGBA 图像。
///
/// 不经 `tauri::image::Image::from_bytes`：那个方法需要 tauri 的 `image-png`/`image-ico`
/// feature（本项目未开，`Cargo.toml` 属越界改动范围），改用已直接依赖的 `image` crate 解码
/// （`ocr.rs` 已有同样用法），再用零 feature 要求的 [`tauri::image::Image::new_owned`] 构造。
fn decode_tray_icon(b64: &str) -> Result<tauri::image::Image<'static>, String> {
    use base64::Engine as _;
    // 兼容 `data:image/png;base64,XXXX` 与不带前缀的纯 base64。
    let raw = b64.rsplit(',').next().unwrap_or(b64).trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| format!("图标 base64 解码失败: {e}"))?;
    let mut img = image::load_from_memory(&bytes).map_err(|e| format!("图标图片解码失败: {e}"))?;
    // 托盘图标本来就很小（Windows 常见 16~32px，高 DPI 也很少超过 256px）；限一下尺寸，
    // 防止插件传一张巨图把内存 / 首次渲染耗时拖垮（做法与 `ocr.rs` 的 CAP 同一思路）。
    const CAP: u32 = 256;
    let (w, h) = (img.width(), img.height());
    if w > CAP || h > CAP {
        let k = CAP as f64 / w.max(h) as f64;
        img = img.resize(
            ((w as f64 * k) as u32).max(1),
            ((h as f64 * k) as u32).max(1),
            image::imageops::FilterType::Lanczos3,
        );
    }
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(tauri::image::Image::new_owned(rgba.into_raw(), w, h))
}

/// 建一份右键菜单：每一项的真实 id 前面拼上 `{nonce}::`，点击时再原样剥掉——理由见模块头
/// 注释第 3 点（`on_menu_event` 是全应用共享监听表，必须靠 nonce 前缀避免串听旧闭包/别的插件）。
fn build_menu(app: &AppHandle, nonce: &str, items: &[TrayMenuItemOpt]) -> Result<Menu<tauri::Wry>, String> {
    let mut built: Vec<Box<dyn tauri::menu::IsMenuItem<tauri::Wry>>> = Vec::with_capacity(items.len());
    for it in items {
        if it.separator.unwrap_or(false) {
            let sep = PredefinedMenuItem::separator(app).map_err(|e| e.to_string())?;
            built.push(Box::new(sep));
        } else {
            let full_id = format!("{nonce}::{}", it.id);
            let mi = MenuItem::with_id(app, full_id, &it.label, it.enabled.unwrap_or(true), None::<&str>)
                .map_err(|e| e.to_string())?;
            built.push(Box::new(mi));
        }
    }
    let refs: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = built.iter().map(|b| b.as_ref()).collect();
    Menu::with_items(app, &refs).map_err(|e| e.to_string())
}

/// 创建或更新调用者会话的托盘图标。需已获 `tray` 授权。
///
/// # Errors
/// - 会话不存在（插件未在运行）
/// - 未获 `tray` 授权
/// - `opts.icon` 未传且插件没有 logo
/// - 图标 base64 解码 / 图片解码失败
/// - 底层 tauri 菜单 / 托盘 API 调用失败（如主线程调度异常）
#[tauri::command]
pub fn plugin_tray_set(
    opts: TraySetOptions,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    state: State<'_, TrayState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "tray") {
        return Err("插件未获授权托盘图标（请在「插件管理」里授权 tray）".to_string());
    }

    let icon_src = opts.icon.clone().or_else(|| plugin_logo_data_url(&registry, &session));
    let Some(icon_src) = icon_src else {
        return Err("缺少图标：既未传 icon，插件也没有 logo".to_string());
    };
    let icon = decode_tray_icon(&icon_src)?;

    let app = webview.app_handle().clone();
    let label = webview.label().to_string();
    let key = session_key(&session);

    let menu_item_count = opts.menu.as_ref().map(Vec::len).unwrap_or(0);
    let sessions = lock(&state.sessions);

    if let Some(existing) = sessions.get(&key) {
        // 更新路径：复用已建好的 TrayIcon，绝不重新 build（重新 build 会在共享的 on_menu_event
        // 监听表里再攒一个闭包，见模块头注释第 3 点）。
        existing
            .icon
            .set_icon(Some(icon))
            .map_err(|e| format!("更新托盘图标失败: {e}"))?;
        if let Some(tooltip) = opts.tooltip.as_deref() {
            existing
                .icon
                .set_tooltip(Some(tooltip))
                .map_err(|e| format!("更新托盘提示文字失败: {e}"))?;
        }
        if let Some(items) = &opts.menu {
            let menu = build_menu(&app, &existing.nonce, items)?;
            existing
                .icon
                .set_menu(Some(menu))
                .map_err(|e| format!("更新托盘菜单失败: {e}"))?;
        }
        ilog!(
            "[iTools][plugin] tray_set(更新): 来自 {}，menuItems={menu_item_count}",
            who(&session)
        );
        return Ok(());
    }
    drop(sessions);

    // 创建路径。
    let nonce = new_unique_id();
    let tooltip = opts.tooltip.clone().unwrap_or_else(|| session.id.clone());
    let menu = match &opts.menu {
        Some(items) => Some(build_menu(&app, &nonce, items)?),
        None => None,
    };

    let mut builder = TrayIconBuilder::with_id(format!("plugin-tray:{nonce}"))
        .icon(icon)
        .tooltip(tooltip)
        // 左键点击不弹菜单，改成推事件给插件页；右键仍走系统默认（弹菜单）——与宿主自己的
        // 托盘（`lib.rs::setup_tray`）同一约定，避免插件托盘表现得和宿主托盘不一致。
        .show_menu_on_left_click(false);
    if let Some(menu) = menu {
        builder = builder.menu(&menu);
    }

    let evt_nonce = nonce.clone();
    let evt_app = app.clone();
    let evt_label = label.clone();
    let evt_prefix = format!("{evt_nonce}::");
    builder = builder.on_menu_event(move |_app, event| {
        let raw = event.id.as_ref();
        let Some(item_id) = raw.strip_prefix(evt_prefix.as_str()) else {
            return; // 不是本托盘这次构建产生的菜单项：宿主菜单 / 别的插件 / 本插件旧闭包，忽略
        };
        emit_to_plugin(
            &evt_app,
            &evt_label,
            EVT_TRAY_MENU,
            &TrayMenuClickEvent { id: item_id.to_string() },
        );
    });

    let click_app = app.clone();
    let click_label = label.clone();
    builder = builder.on_tray_icon_event(move |_tray, event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            emit_to_plugin(&click_app, &click_label, EVT_TRAY_CLICK, &TrayClickEvent {});
        }
    });

    let tray_icon = builder.build(&app).map_err(|e| format!("创建托盘图标失败: {e}"))?;

    let mut sessions = lock(&state.sessions);
    sessions.insert(
        key,
        PluginTraySession {
            nonce,
            icon: tray_icon,
        },
    );
    ilog!(
        "[iTools][plugin] tray_set(新建): 来自 {}，menuItems={menu_item_count}",
        who(&session)
    );
    Ok(())
}

/// 移除调用者会话的托盘图标（若存在）。幂等：没有图标也返回成功，不当错误处理——
/// 调用方大概率是在退出流程里兜底调用，不该因为“本来就没有”而额外报错中断退出逻辑。
///
/// # Errors
/// - 会话不存在（插件未在运行）——这是唯一的错误路径；「本来就没有托盘图标」不算错误。
#[tauri::command]
pub fn plugin_tray_remove(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    state: State<'_, TrayState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    let removed = lock(&state.sessions).remove(&session_key(&session)).is_some();
    if removed {
        ilog!("[iTools][plugin] tray_remove: 来自 {}", who(&session));
    }
    Ok(())
}

/// 供主控在「插件页关闭 / 插件被禁用或卸载」时调用，清掉该会话可能留下的托盘图标。
///
/// 托盘图标寄存在 [`TrayState`]（managed state），不会随插件窗口关闭自动消失——不主动清理，
/// 用户会看到一个点了没反应的僵尸图标（这正是本项目红线明确禁止的「看着能用、点了不生效」，
/// 只不过肇因不是代码写错，是生命周期没人收口）。调用时机建议：
/// - 正式/调试插件窗口即将关闭或被替换（`open_plugin` 复用/关闭旧窗口之前）；
/// - 后台常驻插件被用户禁用、卸载，或后台进程被停止（`open_plugin_background` 的反向操作）。
///
/// 找不到该会话的记录（本来就没建过 / 已经清过）时什么也不做，不是错误。
pub fn remove_for_session(app: &AppHandle, session: &ActiveSession) {
    let Some(state) = app.try_state::<TrayState>() else {
        return;
    };
    lock(&state.sessions).remove(&session_key(session));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_separates_dev_and_prod() {
        let prod = ActiveSession {
            id: "demo".to_string(),
            dev: false,
        };
        let dev = ActiveSession {
            id: "demo".to_string(),
            dev: true,
        };
        assert_eq!(session_key(&prod), "demo");
        assert_eq!(session_key(&dev), "dev:demo");
        assert_ne!(session_key(&prod), session_key(&dev));
    }

    #[test]
    fn new_unique_id_is_actually_unique() {
        let a = new_unique_id();
        let b = new_unique_id();
        assert_ne!(a, b);
    }

    /// 1x1 透明像素 PNG 的 data URL（业界常用的最小合法 PNG 测试样本）。
    const TINY_PNG_DATA_URL: &str =
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

    #[test]
    fn decode_tray_icon_accepts_data_url_prefix() {
        let img = decode_tray_icon(TINY_PNG_DATA_URL).expect("应能成功解码合法的 1x1 PNG");
        assert_eq!((img.width(), img.height()), (1, 1));
        assert_eq!(img.rgba().len(), 4); // 1 像素 = 4 字节 RGBA
    }

    #[test]
    fn decode_tray_icon_accepts_bare_base64_without_prefix() {
        let bare = TINY_PNG_DATA_URL.rsplit(',').next().unwrap_or_default();
        let img = decode_tray_icon(bare).expect("不带 data: 前缀的纯 base64 也应能解码");
        assert_eq!((img.width(), img.height()), (1, 1));
    }

    #[test]
    fn decode_tray_icon_rejects_garbage() {
        assert!(decode_tray_icon("not-a-valid-base64!!").is_err());
    }
}
