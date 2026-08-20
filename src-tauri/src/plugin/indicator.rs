//! **敏感能力运行时指示器**：摄像头 / 麦克风 / 屏幕正在被谁用，用户随时看得见、能掐断。
//!
//! # 为什么这是硬约束而不是加分项
//!
//! 授权开关只在**授权那一刻**征求过用户同意。之后插件什么时候真的开了摄像头、录了多久，
//! 界面上没有任何痕迹——用户唯一能做的是「撤销授权」这种一刀切操作，而且他根本不知道
//! 什么时候该撤。macOS 的绿灯、iOS 的橙点解决的就是这件事：**能力在被使用的当下，
//! 用户必须能看见**。
//!
//! 能力开放路线文档把它列为四条硬约束之一，且写明「不接受先上线后补」。
//!
//! # 实现取舍
//!
//! - **主动查询而非各模块上报**：各能力模块（`camera` / `record_av` / `audio` / `record`）
//!   各自维护着自己的活跃会话表，这里去问它们，而不是要求它们反过来调本模块登记。
//!   前者加一个新能力时只要在这里加一行，后者要求每个模块都记得两处配对调用——
//!   漏一次就是一个「用了但不显示」的隐私漏洞，而这种漏洞不会有任何报错提醒你。
//! - **指示器由宿主渲染，插件碰不到**：这里只提供查询，托盘文案与菜单项由 `lib.rs` 渲染。
//!   插件没有任何接口能改写或隐藏它。

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::logging::ilog;

/// 一条「某插件正在使用某敏感能力」的记录。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveUse {
    /// 插件 id。
    pub plugin_id: String,
    /// 能力标识，与权限标识一致（`camera` / `audio-capture` / `screen-capture`）。
    pub capability: String,
    /// 给用户看的中文说明。
    pub label: String,
}

/// 一条使用记录的**内部**形态：带完整会话身份（id + dev）。
///
/// 为什么不直接用 [`ActiveUse`]：那个结构是给前端/托盘看的，只有 `plugin_id` 没有 `dev` 标志，
/// 而 [`super::ActiveSession::same_as`] 要求两者都对得上。拿它去掐断，调试会话会一个都停不掉。
struct Entry {
    session: super::ActiveSession,
    capability: &'static str,
    label: &'static str,
}

/// 汇总当前所有正在进行的敏感能力使用（内部形态，带完整会话身份）。
///
/// 新增敏感能力时**必须**在这里加一行——否则那个能力就成了「用了也不显示」的盲区。
/// 而且**加了这里就必须同时在 [`stop_all_sensitive`] 里给它一条停止通路**：
/// 只登记不掐断，等于托盘上摆一个「点击停止」却点了不生效的按钮，比不显示更糟。
fn active_entries(app: &AppHandle) -> Vec<Entry> {
    let mut out = Vec::new();
    // 麦克风录音与 GIF 录屏：既有能力，同样是敏感设备，不能漏
    if let Some(session) = app.state::<super::audio::AudioState>().active_owner() {
        out.push(Entry { session, capability: "audio-capture", label: "麦克风" });
    }
    if let Some(session) = app.state::<super::record::RecordState>().active_owner() {
        out.push(Entry { session, capability: "screen-capture", label: "屏幕录制" });
    }
    for session in super::camera::active_sessions() {
        out.push(Entry { session, capability: "camera", label: "摄像头" });
    }
    for (session, label) in super::record_av::active_sessions() {
        out.push(Entry { session, capability: capability_of(label), label });
    }
    out
}

/// 汇总当前所有正在进行的敏感能力使用（对外形态）。
pub fn active_uses(app: &AppHandle) -> Vec<ActiveUse> {
    active_entries(app)
        .into_iter()
        .map(|e| ActiveUse {
            plugin_id: e.session.id,
            capability: e.capability.to_string(),
            label: e.label.to_string(),
        })
        .collect()
}

/// `record_av` 的一条使用记录该算在哪项授权名下。
///
/// 内录占的是声卡/麦克风那条授权（`audio-capture`），mp4 录屏占的是屏幕那条（`screen-capture`）。
/// 笼统标成一个，用户在「谁在用什么」面板上就会看到与他实际授权出去的能力对不上的名字——
/// 而这个面板存在的全部意义就是让他核对「我授权的和它在用的是不是一回事」。
fn capability_of(label: &str) -> &'static str {
    if label.contains("内录") {
        "audio-capture"
    } else {
        "screen-capture"
    }
}

/// 是否有任何敏感能力正在被使用。托盘每次刷新都会问一次，做成便宜的短路判断。
pub fn any_active(app: &AppHandle) -> bool {
    super::camera::is_active()
        || super::record_av::is_active()
        || app.state::<super::audio::AudioState>().active_owner().is_some()
        || app.state::<super::record::RecordState>().active_owner().is_some()
}

/// 供管理中心查询（「谁在用摄像头」面板）。
#[tauri::command]
pub fn plugin_active_uses(app: AppHandle) -> Vec<ActiveUse> {
    active_uses(&app)
}

/// 一键停掉所有正在进行的敏感能力使用。
///
/// 这是指示器旁边那个「停止」的落点：用户看见有插件在用摄像头，必须能立刻掐掉，
/// 而不是只能去插件管理里翻授权开关。
#[tauri::command]
pub fn plugin_stop_all_sensitive(app: AppHandle) {
    stop_all_sensitive(&app);
}

/// 托盘提示文案：没有使用中返回 None（此时托盘保持默认文案）。
///
/// 文案里点名到插件：只说「摄像头使用中」而不说是谁，用户还是得自己一个个排查。
pub fn tray_tooltip(app: &AppHandle) -> Option<String> {
    let uses = active_uses(app);
    if uses.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = uses
        .iter()
        .map(|u| format!("{} 正在使用{}", u.plugin_id, u.label))
        .collect();
    parts.dedup();
    Some(format!("iTools — {}", parts.join("；")))
}

/// 掐断所有正在进行的敏感能力使用（托盘菜单项的落点，也供命令复用）。
///
/// # 这里必须覆盖 [`active_entries`] 列出的**每一类**
///
/// 曾经漏过两类：麦克风录音（`audio`）与 GIF 录屏（`record`）只登记、没有停止通路，
/// 于是托盘上明明写着「⚠ XX 正在使用麦克风（点击停止）」，点下去什么都不会发生。
/// 同期 `record_av` 那一类也停不掉——它当时给出的「会话身份」其实是一句中文描述，
/// 拿去 `same_as` 匹配永远不成立。两者叠加的结果是这个按钮**对一半以上的场景是假的**。
///
/// 所以规则写死在这里：**[`active_entries`] 里能出现的能力，这里都要有对应的掐断动作**。
/// 加新能力时两处一起改，漏一处就是又一个「看着能用、点了不生效」的控件。
pub fn stop_all_sensitive(app: &AppHandle) {
    // 单例型能力（同一时刻只允许一个会话）：直接注销，不需要按会话匹配。
    let audio_stopped = app.state::<super::audio::AudioState>().force_stop();
    let record_stopped = app.state::<super::record::RecordState>().force_stop();

    // 多会话型能力：按**完整会话身份**逐个掐断（id + dev 都要对得上）。
    let mut sessions: Vec<super::ActiveSession> = Vec::new();
    for e in active_entries(app) {
        if !sessions.iter().any(|s| s.same_as(&e.session)) {
            sessions.push(e.session);
        }
    }
    for session in &sessions {
        super::camera::stop_all_for_session(session);
        super::record_av::stop_all_for_session(session);
    }

    ilog!(
        "[iTools][plugin] 掐断敏感能力：麦克风={audio_stopped}，GIF 录屏={record_stopped}，         按会话掐断 {} 个（摄像头 / 录屏录音）",
        sessions.len()
    );
}

/// 起一个后台循环，把托盘 tooltip 与「敏感能力」菜单项跟着实际使用状态刷新。
///
/// 为什么用轮询而不是让各能力模块主动通知：通知要求每个模块在开始/结束两处都记得调，
/// 漏一次就是一个「用着却不显示」的隐私漏洞，而这种漏洞不会有任何报错提醒你。
/// 一秒一次的轮询开销可以忽略（只是读几个 Mutex），换来的是**不可能漏报**。
pub fn spawn_indicator(
    app: AppHandle,
    tray: tauri::tray::TrayIcon,
    item: tauri::menu::MenuItem<tauri::Wry>,
) {
    tauri::async_runtime::spawn(async move {
        let mut last: Option<String> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            // 绝大多数时刻什么都没在用：先做一次便宜的布尔短路，
            // 避免每秒都去拼一遍字符串。
            let tip = if any_active(&app) {
                tray_tooltip(&app)
            } else {
                None
            };
            // 状态没变就什么都不做——每秒无谓地重设一遍托盘会让某些 Windows 版本的图标闪烁
            if tip == last {
                continue;
            }
            match &tip {
                Some(text) => {
                    let _ = tray.set_tooltip(Some(text.as_str()));
                    let _ = item.set_text(format!("⚠ {text}（点击停止）"));
                    let _ = item.set_enabled(true);
                }
                None => {
                    let _ = tray.set_tooltip(Some("iTools"));
                    let _ = item.set_text("敏感能力：当前无使用");
                    let _ = item.set_enabled(false);
                }
            }
            last = tip;
        }
    });
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 内录与录屏必须落到**各自**那条授权名下。
    ///
    /// 这两个字符串是 `record_av.rs` 里写死的会话 label，改那边就要改这边——
    /// 所以这里直接用它们的原文断言，改动会立刻在这条测试上现形。
    #[test]
    fn capability_follows_actual_purpose() {
        assert_eq!(capability_of("系统内录（回环声卡）"), "audio-capture");
        assert_eq!(capability_of("屏幕录制（mp4）"), "screen-capture");
    }
}
