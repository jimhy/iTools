//! 插件全局热键：插件注册后，按下即唤起插件窗并向其派发 `hotkey` 事件（前端 `itools.onHotkey`）。
//! 需 **hotkey** 授权。复用宿主已装的 `tauri-plugin-global-shortcut`。
//!
//! Rust→插件 的推送统一走「`webview.eval` 调 `window.__itoolsEmit(channel, payload)`」的事件总线
//! （见 bridge.js）——插件页严格 CSP 下 Tauri 事件系统不便用，eval 注入最简单可靠。

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use super::commands::{caller_session, plugin_granted};
use super::{ActiveSession, PluginRegistry};
use crate::logging::ilog;
use crate::settings::SettingsStore;

#[derive(Clone)]
pub struct HotkeyBinding {
    pub plugin_id: String,
    pub code: Option<String>,
    pub accelerator: String,
    /// 该热键是**调试会话**注册的：按下时要唤起调试窗口而不是正式插件窗口。
    pub dev: bool,
}

impl HotkeyBinding {
    /// 该绑定是否属于这个会话。**注册（[`arbitrate`]）与注销（[`plugin_unregister_hotkey`]）
    /// 共用同一口径**：id 与 dev 标志都相等才算自己的，同名的调试插件与正式插件互不相认。
    fn owned_by(&self, s: &ActiveSession) -> bool {
        self.plugin_id == s.id && self.dev == s.dev
    }
}

/// 已注册的插件热键：shortcut.id() → 绑定。managed state。
#[derive(Default)]
pub struct PluginHotkeys {
    pub map: Mutex<HashMap<u32, HotkeyBinding>>,
}

/// 全局热键按下的分发：命中插件热键则唤起「归属插件」并派发 onHotkey，返回 true；
/// 未命中或无法定向处理返回 false（调用方回退到「切换主窗口」的默认行为）。
///
/// # 调试插件的热键**不受**「调试插件进入主搜索」开关约束（有意为之，别照着"限制"去改）
///
/// `dev_search_visible` 管的只是「调试插件出不出现在主搜索 / 最近使用里」
/// （见 `crate::dev::refresh_search` 与 `crate::commands::plugin_entry_state`）。
/// 本函数**刻意不看**这个开关：`binding.dev == true` 就照常把调试插件拉起来。
///
/// 而且热键绑定只活在进程内的 [`PluginHotkeys::map`] 里——关开关、甚至关掉调试窗口，
/// 都不会注销上次注册的热键；只有退出 iTools 才会真正消失。
///
/// 这是**明知且接受**的行为：热键是调试插件自己通过 `itools.registerHotkey` 注册的，
/// 按下它属于「这次调试会话的延续」。开关一关就静默吞掉，用户只会看到"热键按了没反应"，
/// 排查成本比现在高得多。因此 UI / 文档**不得**写成「关掉开关后调试插件只能从『运行』tab 启动」
/// ——那是一句做不到的限制。要写就写准：**已注册的热键仍然有效，直到退出 iTools**。
pub fn dispatch(app: &AppHandle, shortcut_id: u32) -> bool {
    let binding = {
        let hk = match app.try_state::<PluginHotkeys>() {
            Some(s) => s,
            None => return false,
        };
        let map = hk.map.lock().unwrap();
        match map.get(&shortcut_id) {
            Some(b) => b.clone(),
            None => return false,
        }
    };
    // 归属窗口：调试会话注册的热键唤起调试窗，正式的唤起正式窗
    let label = if binding.dev {
        crate::dev::DEV_WINDOW_LABEL
    } else {
        crate::plugin::commands::PLUGIN_WINDOW_LABEL
    };
    // 该窗口里当前加载的是哪个插件
    let current = app
        .try_state::<PluginRegistry>()
        .and_then(|r| r.session_for(label));
    let win = app.get_webview_window(label);
    // 只有「窗口存在且当前就是热键归属插件」时才直接派发事件——避免把 A 的热键误投给正在显示的 B
    if let (Some(win), Some(cur)) = (win.as_ref(), current.as_ref()) {
        if cur.id == binding.plugin_id && cur.dev == binding.dev {
            // 不要在这里 show/focus 面板——热键动作可能是「不需要面板」的截图，
            // 先弹面板再被截图流程藏起来会闪一下。是否显示交给插件的 onHotkey 决定。
            // eval 在隐藏窗口上照常执行。
            let acc = serde_json::to_string(&binding.accelerator).unwrap_or_else(|_| "\"\"".into());
            let code = serde_json::to_string(&binding.code).unwrap_or_else(|_| "null".into());
            let js = format!(
                "window.__itoolsEmit && window.__itoolsEmit('hotkey', {{ accelerator: {acc}, code: {code} }})"
            );
            let _ = win.eval(&js);
            return true;
        }
    }
    // 否则（窗口不存在 / 当前是别的插件）：切换/重开到归属插件。传特殊 query "__hotkey__"，
    // 让插件 onEnter 能区分「热键唤起」与「关键词唤起」，从而无视 auto 设置也执行热键动作（如立即截图）。
    if let Some(code) = binding.code.clone() {
        let app2 = app.clone();
        let id = binding.plugin_id.clone();
        let dev = binding.dev;
        tauri::async_runtime::spawn(async move {
            if dev {
                // 调试会话：走调试窗口（调试插件不在正式注册表里，open_plugin 找不到它）
                let _ = crate::dev::commands::open_dev_window(
                    app2,
                    id,
                    code,
                    "__hotkey__".to_string(),
                    "keyword".to_string(),
                )
                .await;
            } else {
                // hidden=true：热键截图时不弹面板（隐藏 webview 照常 onEnter→startCapture→原生覆盖层）
                let _ = crate::plugin::commands::open_plugin(
                    app2,
                    format!("{id}#{code}"),
                    "__hotkey__".to_string(),
                    true,
                )
                .await;
            }
        });
        return true;
    }
    false
}

/// 同一个 accelerator 上「原绑定 vs 本次注册者」的仲裁结果。
#[derive(Debug, PartialEq, Eq)]
enum Takeover {
    /// 该键当前没有插件绑定：直接注册。
    Free,
    /// 允许覆盖：先撤掉原绑定再登记为本次注册者。
    Replace,
    /// 拒绝，附带给用户看的中文原因（跨 dev/prod 边界）。
    Deny(String),
}

/// 判定本次注册能不能动这个键上的原绑定。**注册与注销必须用同一套归属口径**——
/// 注销路径（[`plugin_unregister_hotkey`]）早就带上了 dev 标志，注册路径若不带，
/// 就等于留了一条「注销不了、注册却能顶掉」的静默夺权路径。
///
/// 规则：
/// 1. **跨 dev/prod 边界一律拒绝**。调试会话不得夺走正式插件的热键（反之亦然）：
///    正式插件那边毫无察觉——它仍以为自己注册着，dispatch 却再也不会派发给它。
///    这是「调试环境不影响正式环境」这条隔离承诺的一部分，必须硬挡并给出可读原因。
/// 2. **同侧（都正式 / 都调试）保持既有的「后注册者胜出」**，理由有三：
///    ① 插件之间互抢是本轮之前就存在的行为，不在本次修复范围，改它会波及已装插件；
///    ② 目前没有任何「热键冲突」的解决 UI——若改成拒绝，后装的插件会彻底注册不上，用户既看不到是谁占着、也无从释放，比让后者胜出更糟；
///    ③ 与 OS 全局热键「后注册者胜出」的语义一致。
///    但**换绑到另一个插件时记一条日志**，让「热键怎么突然不灵了」在日志里有迹可循。
/// 3. 同一个插件重复注册同一个键（改 code / 页面重载后重注册）：照常覆盖，不打日志。
fn arbitrate(prev: Option<&HotkeyBinding>, me: &ActiveSession, accelerator: &str) -> Takeover {
    let Some(b) = prev else {
        return Takeover::Free;
    };
    if b.dev != me.dev {
        // ⚠ 提示里只写**真的管用**的做法。实测：关掉调试窗口 / 在「插件管理」里停用插件
        // 都**不会**注销已注册的热键——绑定表是进程内的，只有「插件自己调 unregisterHotkey」
        // 或「重启 iTools」才会清掉（重启后各插件要等下次被打开才会重新注册）。
        // 写一句做了也没用的指引，就是又一个骗人的文案。
        let (who, hint) = if b.dev {
            (
                "调试会话中的插件",
                "调试环境与正式环境互不抢占。请换一个组合键；若那个调试插件已经不用了，重启 iTools 即可释放\
                 （插件热键只登记在进程内，关掉调试窗口并不会自动注销）",
            )
        } else {
            (
                "正式插件",
                "调试环境不会抢走正式插件的热键。请给调试插件换一个组合键；或重启 iTools 释放\
                 （插件热键只登记在进程内，重启后正式插件要等下次被打开才会重新注册）",
            )
        };
        return Takeover::Deny(format!(
            "快捷键 {accelerator} 已被{who}「{}」占用：{hint}",
            b.plugin_id
        ));
    }
    if !b.owned_by(me) {
        ilog!(
            "[iTools] 插件热键 {accelerator} 由「{}」换绑到「{}」（同侧后注册者胜出）",
            b.plugin_id,
            me.id
        );
    }
    Takeover::Replace
}

/// 注册一个全局热键，绑定到当前插件（可带 feature code）。需 hotkey 授权。
///
/// 换绑前先按 [`arbitrate`] 做归属判定：**跨 dev/prod 边界的抢占会被拒绝**。
#[tauri::command]
pub fn plugin_register_hotkey(
    accelerator: String,
    code: Option<String>,
    app: AppHandle,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hotkeys: State<'_, PluginHotkeys>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    let ActiveSession { id, dev } = session.clone();
    if !plugin_granted(&settings, &registry, &session, "hotkey") {
        return Err("插件未获授权注册全局热键（请在「插件管理」里授权 hotkey）".to_string());
    }
    let shortcut = crate::hotkey::parse_hotkey(&accelerator)
        .ok_or_else(|| format!("无效快捷键（需至少一个修饰键）：{accelerator}"))?;
    let sid = shortcut.id();
    // 同键换绑：**先看原绑定属于谁**，跨 dev/prod 边界的抢占直接拒绝（见 arbitrate）。
    // 临界区里只做「读原绑定 + 摘掉它」，不在持锁期间调 global_shortcut（那会与热键线程
    // 上正在跑的 dispatch 抢同一把锁）。
    {
        let mut map = hotkeys.map.lock().unwrap();
        // 先把仲裁结论算成**不再借 map 的值**，随后才能在同一临界区里改 map
        let decision = arbitrate(map.get(&sid), &session, &accelerator);
        match decision {
            Takeover::Deny(why) => return Err(why),
            Takeover::Replace => {
                map.remove(&sid);
                drop(map);
                let _ = app.global_shortcut().unregister(shortcut);
            }
            Takeover::Free => {}
        }
    }
    app.global_shortcut()
        .register(shortcut)
        .map_err(|e| format!("注册热键失败（可能被系统或其它程序占用）：{e}"))?;
    hotkeys.map.lock().unwrap().insert(
        sid,
        HotkeyBinding {
            plugin_id: id,
            code,
            accelerator,
            dev,
        },
    );
    Ok(())
}

/// 注销一个已注册的全局热键。**只允许注销本插件注册的键**——
/// 绝不能凭 accelerator 去 unregister 宿主自身的唤起热键或别的插件的热键（否则 = DoS / 越权）。
#[tauri::command]
pub fn plugin_unregister_hotkey(
    accelerator: String,
    app: AppHandle,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hotkeys: State<'_, PluginHotkeys>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if !plugin_granted(&settings, &registry, &session, "hotkey") {
        return Err("插件未获授权全局热键（请在「插件管理」里授权 hotkey）".to_string());
    }
    let shortcut =
        crate::hotkey::parse_hotkey(&accelerator).ok_or_else(|| "无效快捷键".to_string())?;
    let sid = shortcut.id();
    let mut map = hotkeys.map.lock().unwrap();
    match map.get(&sid) {
        // 归属校验带上 dev 标志（与注册路径同一口径，见 HotkeyBinding::owned_by）：
        // 同名的调试插件不能注销正式插件注册的键，反之亦然
        Some(b) if b.owned_by(&session) => {
            // 仅注销「确在插件热键表中、且归属本插件」的键
            let _ = app.global_shortcut().unregister(shortcut);
            map.remove(&sid);
            Ok(())
        }
        Some(_) => Err("该快捷键不属于本插件，拒绝注销".to_string()),
        None => Ok(()), // 本就未注册：幂等成功，且不碰宿主/其它注册
    }
}

/// 重注册所有已登记的插件热键。主唤起热键改动时 `save_settings` 会 `unregister_all()`
/// 撤掉一切键（含插件热键），须调用本函数补回，否则插件全局键会在 OS 层静默失效
/// （PluginHotkeys.map 仍留绑定，dispatch 却永远收不到）。id 由 accelerator 确定性生成，
/// 重注册后 dispatch 仍能按原 id 匹配。
pub fn reregister_all(app: &AppHandle) {
    if let Some(hk) = app.try_state::<PluginHotkeys>() {
        if let Ok(map) = hk.map.lock() {
            for binding in map.values() {
                if let Some(sc) = crate::hotkey::parse_hotkey(&binding.accelerator) {
                    let _ = app.global_shortcut().register(sc);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(id: &str, dev: bool) -> HotkeyBinding {
        HotkeyBinding {
            plugin_id: id.to_string(),
            code: Some("main".to_string()),
            accelerator: "Alt+Q".to_string(),
            dev,
        }
    }

    fn session(id: &str, dev: bool) -> ActiveSession {
        ActiveSession {
            id: id.to_string(),
            dev,
        }
    }

    /// 空位直接注册；同一插件重复注册自己的键照常覆盖（改 code / 页面重载都会走到这）。
    #[test]
    fn free_slot_and_self_rebind_are_allowed() {
        assert_eq!(arbitrate(None, &session("demo", false), "Alt+Q"), Takeover::Free);
        assert_eq!(
            arbitrate(Some(&binding("demo", false)), &session("demo", false), "Alt+Q"),
            Takeover::Replace
        );
        assert_eq!(
            arbitrate(Some(&binding("demo", true)), &session("demo", true), "Alt+Q"),
            Takeover::Replace
        );
    }

    /// **本轮修的就是这条**：调试会话不得夺走正式插件的热键，反之亦然——
    /// 即便两者同名（「正在开发的就是已装的那个」）。错误必须点明是被哪一侧的谁占着。
    #[test]
    fn cross_dev_prod_takeover_is_denied() {
        // 调试会话想抢正式插件的键
        let deny = arbitrate(Some(&binding("demo", false)), &session("demo", true), "Alt+Q");
        let Takeover::Deny(msg) = deny else {
            panic!("调试会话抢正式插件的热键必须被拒绝，实际: {deny:?}");
        };
        assert!(msg.contains("正式插件"), "要说清被谁占着: {msg}");
        assert!(msg.contains("demo") && msg.contains("Alt+Q"), "{msg}");
        assert!(msg.contains("换一个组合键"), "要给出真的管用的出路: {msg}");

        // 正式插件想抢调试会话的键
        let deny = arbitrate(Some(&binding("demo", true)), &session("demo", false), "Alt+Q");
        let Takeover::Deny(msg) = deny else {
            panic!("正式插件抢调试会话的热键必须被拒绝，实际: {deny:?}");
        };
        assert!(msg.contains("调试"), "{msg}");
        // 提示里不许出现「关掉调试窗口 / 停用插件」这类**做了也不生效**的指引
        assert!(!msg.contains("关掉调试窗口就"), "{msg}");
        assert!(msg.contains("重启"), "真正能释放热键的只有重启（或插件自己注销）: {msg}");
    }

    /// 同侧插件之间保持既有的「后注册者胜出」（理由见 `arbitrate` 文档）。
    #[test]
    fn same_side_preemption_keeps_last_writer_wins() {
        assert_eq!(
            arbitrate(Some(&binding("other", false)), &session("demo", false), "Alt+Q"),
            Takeover::Replace
        );
        assert_eq!(
            arbitrate(Some(&binding("other", true)), &session("demo", true), "Alt+Q"),
            Takeover::Replace
        );
    }

    /// 注册与注销必须同一口径：`owned_by` 是两条路径共用的唯一判定。
    #[test]
    fn ownership_is_symmetric_between_register_and_unregister() {
        let prod = binding("demo", false);
        assert!(prod.owned_by(&session("demo", false)));
        assert!(!prod.owned_by(&session("demo", true)), "同名调试会话不是同一个归属者");
        assert!(!prod.owned_by(&session("other", false)));
    }
}
