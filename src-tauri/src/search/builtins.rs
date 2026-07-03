//! 内置即时小工具：输入即得（计算 / 进制 / 时间戳 / 颜色 / URL 直达）。
//! 每个匹配器独立判断是否命中，命中则产出一条 `kind = "command"` 的结果。

use super::SearchItem;

/// 依次尝试各内置命令，返回全部命中的结果（置顶于搜索列表）
pub fn match_commands(query: &str) -> Vec<SearchItem> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    if let Some(item) = try_calc(q) {
        out.push(item);
    }
    if let Some(item) = try_radix(q) {
        out.push(item);
    }
    if let Some(item) = try_timestamp(q) {
        out.push(item);
    }
    if let Some(item) = try_color(q) {
        out.push(item);
    }
    if let Some(item) = try_url(q) {
        out.push(item);
    }
    out
}

/// 构造一条命令结果；`action` 为 "copy"（复制 target）或 "open"（打开 target）
fn command_item(
    title: impl Into<String>,
    subtitle: impl Into<String>,
    target: impl Into<String>,
    action: &str,
) -> SearchItem {
    let title = title.into();
    SearchItem {
        id: format!("cmd::{title}"),
        title,
        subtitle: subtitle.into(),
        kind: "command".to_string(),
        target: target.into(),
        icon: None,
        action: action.to_string(),
    }
}

// ---------- 计算器 ----------

/// 含运算符的算式求值；避免把纯数字/文字误判为算式
fn try_calc(q: &str) -> Option<SearchItem> {
    let has_operator = q.chars().any(|c| "+-*/^%".contains(c));
    if !has_operator {
        return None;
    }
    let only_math = q
        .chars()
        .all(|c| c.is_ascii_digit() || "+-*/^%().eE ".contains(c));
    if !only_math {
        return None;
    }
    match fasteval::ez_eval(q, &mut fasteval::EmptyNamespace) {
        Ok(v) if v.is_finite() => {
            let s = format_num(v);
            Some(command_item(
                format!("= {s}"),
                "计算结果 · 回车复制",
                s,
                "copy",
            ))
        }
        _ => None,
    }
}

/// 整数不带小数点，浮点去掉尾随 0
fn format_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.10}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

// ---------- 进制转换 ----------

/// 十进制整数或 0x/0b/0o 前缀 → 展示四种进制
fn try_radix(q: &str) -> Option<SearchItem> {
    let val = parse_uint(q)?;
    let text = format!("DEC {val}   HEX 0x{val:X}   OCT 0o{val:o}   BIN 0b{val:b}");
    let copy = format!("{val} 0x{val:X} 0o{val:o} 0b{val:b}");
    Some(command_item(text, "进制转换 · 回车复制", copy, "copy"))
}

fn parse_uint(q: &str) -> Option<u64> {
    let q = q.trim();
    if let Some(rest) = q.strip_prefix("0x").or_else(|| q.strip_prefix("0X")) {
        return u64::from_str_radix(rest, 16).ok();
    }
    if let Some(rest) = q.strip_prefix("0b").or_else(|| q.strip_prefix("0B")) {
        return u64::from_str_radix(rest, 2).ok();
    }
    if let Some(rest) = q.strip_prefix("0o").or_else(|| q.strip_prefix("0O")) {
        return u64::from_str_radix(rest, 8).ok();
    }
    if !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        return q.parse::<u64>().ok();
    }
    None
}

// ---------- 时间戳 ----------

/// now/ts → 当前时间戳；10 位秒 / 13 位毫秒数字 → 转本地时间
fn try_timestamp(q: &str) -> Option<SearchItem> {
    use chrono::{Local, TimeZone, Utc};

    let lower = q.to_ascii_lowercase();
    if lower == "now" || lower == "ts" || q == "时间戳" {
        let now = Utc::now();
        let secs = now.timestamp();
        let local = now.with_timezone(&Local);
        return Some(command_item(
            secs.to_string(),
            format!("当前时间戳 · {} · 回车复制", local.format("%Y-%m-%d %H:%M:%S")),
            secs.to_string(),
            "copy",
        ));
    }

    if q.chars().all(|c| c.is_ascii_digit()) {
        match q.len() {
            10 => {
                let secs: i64 = q.parse().ok()?;
                let dt = Local.timestamp_opt(secs, 0).single()?;
                let s = dt.format("%Y-%m-%d %H:%M:%S").to_string();
                return Some(command_item(s.clone(), "时间戳转本地时间 · 回车复制", s, "copy"));
            }
            13 => {
                let ms: i64 = q.parse().ok()?;
                let dt = Local.timestamp_millis_opt(ms).single()?;
                let s = dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
                return Some(command_item(
                    s.clone(),
                    "毫秒时间戳转本地时间 · 回车复制",
                    s,
                    "copy",
                ));
            }
            _ => {}
        }
    }
    None
}

// ---------- 颜色转换 ----------

/// #RGB / #RRGGBB / rgb(r,g,b) → 展示 HEX / RGB / HSL
fn try_color(q: &str) -> Option<SearchItem> {
    let (r, g, b) = parse_color(q)?;
    let hex = format!("#{r:02X}{g:02X}{b:02X}");
    let rgb = format!("rgb({r}, {g}, {b})");
    let (h, s, l) = rgb_to_hsl(r, g, b);
    let hsl = format!("hsl({h}, {s}%, {l}%)");
    let text = format!("{hex}   {rgb}   {hsl}");
    Some(command_item(text, "颜色转换 · 回车复制 HEX", hex, "copy"))
}

fn parse_color(q: &str) -> Option<(u8, u8, u8)> {
    let q = q.trim();
    if let Some(hex) = q.strip_prefix('#') {
        return match hex.len() {
            3 => {
                let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
                let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
                let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
                Some((r, g, b))
            }
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some((r, g, b))
            }
            _ => None,
        };
    }
    let lower = q.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<_> = inner.split(',').map(|p| p.trim().parse::<u16>()).collect();
        if parts.len() == 3 {
            let r = (*parts[0].as_ref().ok()?).min(255) as u8;
            let g = (*parts[1].as_ref().ok()?).min(255) as u8;
            let b = (*parts[2].as_ref().ok()?).min(255) as u8;
            return Some((r, g, b));
        }
    }
    None
}

/// 标准 RGB→HSL，返回 (色相 0-360, 饱和度 %, 亮度 %)
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (u32, u32, u32) {
    let rf = r as f64 / 255.0;
    let gf = g as f64 / 255.0;
    let bf = b as f64 / 255.0;
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let l = (max + min) / 2.0;
    let d = max - min;

    let (h, s) = if d == 0.0 {
        (0.0, 0.0)
    } else {
        let s = d / (1.0 - (2.0 * l - 1.0).abs());
        let h = if max == rf {
            ((gf - bf) / d).rem_euclid(6.0)
        } else if max == gf {
            (bf - rf) / d + 2.0
        } else {
            (rf - gf) / d + 4.0
        };
        (h * 60.0, s)
    };

    (
        h.round() as u32 % 360,
        (s * 100.0).round() as u32,
        (l * 100.0).round() as u32,
    )
}

// ---------- URL 直达 ----------

/// 看起来像域名/URL（无空格、含点、末段是字母 TLD）→ 打开
fn try_url(q: &str) -> Option<SearchItem> {
    if q.contains(char::is_whitespace) || !q.contains('.') {
        return None;
    }
    let is_scheme = q.starts_with("http://") || q.starts_with("https://");
    let looks_domain = q
        .rsplit('.')
        .next()
        .map(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or(false);
    if !is_scheme && !looks_domain {
        return None;
    }
    let url = if is_scheme {
        q.to_string()
    } else {
        format!("https://{q}")
    };
    Some(command_item(
        format!("打开 {url}"),
        "在浏览器中打开",
        url,
        "open",
    ))
}
