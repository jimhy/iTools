//! 插件全局热键：插件注册后，按下即唤起插件窗并向其派发 `hotkey` 事件（前端 `itools.onHotkey`）。
//! 需 **hotkey** 授权。复用宿主已装的 `tauri-plugin-global-shortcut`。
//!
//! Rust→插件 的推送统一走「`webview.eval` 调 `window.__itoolsEmit(channel, payload)`」的事件总线
//! （见 bridge.js）——插件页严格 CSP 下 Tauri 事件系统不便用，eval 注入最简单可靠。

use std::collections::HashMap;
use std::sync::Mutex;

use tauri::{AppHandle, Manager, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use super::commands::{current_plugin, plugin_granted};
use super::PluginRegistry;
use crate::settings::SettingsStore;

#[derive(Clone)]
pub struct HotkeyBinding {
    pub plugin_id: String,
    pub code: Option<String>,
    pub accelerator: String,
}

/// 已注册的插件热键：shortcut.id() → 绑定。managed state。
#[derive(Default)]
pub struct PluginHotkeys {
    pub map: Mutex<HashMap<u32, HotkeyBinding>>,
}

/// 全局热键按下的分发：命中插件热键则唤起「归属插件」并派发 onHotkey，返回 true；
/// 未命中或无法定向处理返回 false（调用方回退到「切换主窗口」的默认行为）。
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
    // 当前插件窗里加载的是哪个插件
    let current = app
        .try_state::<PluginRegistry>()
        .and_then(|r| r.current.lock().ok().and_then(|g| g.clone()));
    let win = app.get_webview_window("plugin");
    // 只有「窗口存在且当前就是热键归属插件」时才直接派发事件——避免把 A 的热键误投给正在显示的 B
    if let (Some(win), Some(cur)) = (win.as_ref(), current.as_ref()) {
        if cur == &binding.plugin_id {
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
        let target = format!("{}#{}", binding.plugin_id, code);
        let app2 = app.clone();
        tauri::async_runtime::spawn(async move {
            // hidden=true：热键截图时不弹面板（隐藏 webview 照常 onEnter→startCapture→原生覆盖层）
            let _ =
                crate::plugin::commands::open_plugin(app2, target, "__hotkey__".to_string(), true)
                    .await;
        });
        return true;
    }
    false
}

/// 注册一个全局热键，绑定到当前插件（可带 feature code）。需 hotkey 授权。
#[tauri::command]
pub fn plugin_register_hotkey(
    accelerator: String,
    code: Option<String>,
    app: AppHandle,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hotkeys: State<'_, PluginHotkeys>,
) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    if !plugin_granted(&settings, &id, "hotkey") {
        return Err("插件未获授权注册全局热键（请在「插件管理」里授权 hotkey）".to_string());
    }
    let shortcut = crate::hotkey::parse_hotkey(&accelerator)
        .ok_or_else(|| format!("无效快捷键（需至少一个修饰键）：{accelerator}"))?;
    let sid = shortcut.id();
    // 同键先撤销（换绑/去重）
    {
        let mut map = hotkeys.map.lock().unwrap();
        if map.remove(&sid).is_some() {
            let _ = app.global_shortcut().unregister(shortcut);
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
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    hotkeys: State<'_, PluginHotkeys>,
) -> Result<(), String> {
    let id = current_plugin(&registry)?;
    if !plugin_granted(&settings, &id, "hotkey") {
        return Err("插件未获授权全局热键（请在「插件管理」里授权 hotkey）".to_string());
    }
    let shortcut =
        crate::hotkey::parse_hotkey(&accelerator).ok_or_else(|| "无效快捷键".to_string())?;
    let sid = shortcut.id();
    let mut map = hotkeys.map.lock().unwrap();
    match map.get(&sid) {
        Some(b) if b.plugin_id == id => {
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
