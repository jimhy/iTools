//! 快捷键字符串解析。
//!
//! `Shortcut`（即 global-hotkey 的 `HotKey`）原生实现 `FromStr`：`+` 分隔、
//! 大小写不敏感、主键同时接受 W3C code 名（KeyA/Digit5/Space/F5）与短别名（a/5/esc）。
//! 这里只做两件事：`win` → `super` 别名归一（官方别名表没有 win），
//! 以及限制无修饰键的全局热键——除功能键 F1-F12 外，其余键必须带至少一个修饰键
//! （单字母/数字/空格无修饰太易误触；F 键无修饰是 PixPin 式截图/贴图热键的常见键位）。

use tauri_plugin_global_shortcut::Shortcut;

/// 解析 "alt+shift+space" / "ctrl+alt+KeyA" / "win+f2" 形式；无效或无修饰键返回 None
pub fn parse_hotkey(s: &str) -> Option<Shortcut> {
    let normalized: Vec<String> = s
        .split('+')
        .map(|p| {
            let p = p.trim();
            if p.eq_ignore_ascii_case("win") || p.eq_ignore_ascii_case("meta") {
                "super".to_string()
            } else {
                p.to_string()
            }
        })
        .collect();
    let shortcut: Shortcut = normalized.join("+").parse().ok()?;
    // 无修饰键时只放行功能键 F1-F12：单字母/数字/空格无修饰会严重干扰输入，仍拒绝；
    // F 键无修饰是 PixPin 式截图/贴图热键的常见键位（如 F1 截图、F3 贴图），误触风险低。
    if shortcut.mods.is_empty() && !is_standalone_key(shortcut.key) {
        return None;
    }
    Some(shortcut)
}

/// 允许「无修饰键」单独作全局热键的键：仅功能键 F1-F12（其余单键无修饰太易误触，拒绝）。
fn is_standalone_key(code: tauri_plugin_global_shortcut::Code) -> bool {
    use tauri_plugin_global_shortcut::Code;
    matches!(
        code,
        Code::F1
            | Code::F2
            | Code::F3
            | Code::F4
            | Code::F5
            | Code::F6
            | Code::F7
            | Code::F8
            | Code::F9
            | Code::F10
            | Code::F11
            | Code::F12
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_smoke() {
        assert!(parse_hotkey("alt+space").is_some());
        assert!(parse_hotkey("ctrl+shift+k").is_some());
        assert!(parse_hotkey("win+f2").is_some(), "win 应归一为 super");
        assert!(parse_hotkey("Alt + Space").is_some(), "大小写/空格宽容");
        assert!(parse_hotkey("alt+shift+Space").is_some(), "e.code 形式");
        assert!(parse_hotkey("ctrl+alt+KeyA").is_some(), "KeyA 形式");
        assert!(parse_hotkey("space").is_none(), "无修饰普通键应拒绝");
        assert!(parse_hotkey("f3").is_some(), "无修饰功能键 F3 应放行（PixPin 式单键热键）");
        assert!(parse_hotkey("F3").is_some(), "功能键大小写宽容");
        assert!(parse_hotkey("a").is_none(), "无修饰字母键仍拒绝");
        assert!(parse_hotkey("alt+").is_none(), "缺主键应拒绝");
        assert!(parse_hotkey("alt+foo").is_none(), "未知键应拒绝");
        assert!(parse_hotkey("shift+KeyQ+alt").is_none(), "修饰键在主键后应拒绝");

        // 字符串形式与显式构造等价
        use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut};
        let parsed = parse_hotkey("alt+shift+space").expect("应可解析");
        let built = Shortcut::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::Space);
        assert_eq!(parsed.id(), built.id(), "解析结果应与显式构造一致");
    }
}
