//! 快捷键字符串解析。
//!
//! `Shortcut`（即 global-hotkey 的 `HotKey`）原生实现 `FromStr`：`+` 分隔、
//! 大小写不敏感、主键同时接受 W3C code 名（KeyA/Digit5/Space/F5）与短别名（a/5/esc）。
//! 这里只做两件事：`win` → `super` 别名归一（官方别名表没有 win），
//! 以及强制至少一个修饰键（无修饰的全局热键太易误触）。

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
    if shortcut.mods.is_empty() {
        return None;
    }
    Some(shortcut)
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
        assert!(parse_hotkey("space").is_none(), "无修饰键应拒绝");
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
