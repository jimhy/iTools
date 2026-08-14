//! 插件提审的**大模型审核**：把包里的源码交给模型，拿回结构化裁决。
//!
//! # 它承担的是机械校验做不到的那部分
//!
//! [`crate::pkg`] 已经挡掉了「格式不对 / 带可执行文件 / 路径穿越 / 装不上」这类**机器能判定**的问题。
//! 剩下的是必须读代码才能看出来的：
//!
//! - 恶意行为：窃取数据、隐蔽外联、执行任意命令；
//! - 权限名不副实：申请了 `network` 但代码里根本没用，或用了却没申请；
//! - 描述与实际功能不符；
//! - `eval` / `new Function` / 动态 `import()` 这类「审核时无害、运行时拉远程代码」的手法。
//!
//! # 诚信约束（doc/开发准则.md）
//!
//! - **模型没配 / 调用失败 / 返回不可解析 → 一律不放行**，提审单停在「需人工处理」并写明真实原因。
//!   绝不把「没审」当成「审过了」——那会让市场页上「已审核」的字样变成假话。
//! - 裁决原文（含 findings）**原样落库**并回显给作者，不做美化、不吞细节。
//! - 模型看不全的部分（截断的源码、二进制文件）在 prompt 里如实告知模型，
//!   并在裁决里保留下来，避免「模型没看到 → 没报问题 → 被当成没问题」。
//!
//! # 凭据
//!
//! API key 只从环境变量读（`ITOOLS_LLM_API_KEY`），源码与镜像零明文。key 不会进日志、
//! 不会随任何错误串回到客户端。

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;
use crate::pkg::StagedPackage;

/// 审核裁决（模型输出的结构，落库并原样回显给作者）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Verdict {
    /// `approve` = 可上线；`reject` = 拒绝。其它值一律按拒绝处理。
    pub verdict: String,
    /// 风险等级：`none` / `low` / `medium` / `high`。
    #[serde(default)]
    pub risk_level: String,
    /// 一句话结论（给作者看的中文）。
    #[serde(default)]
    pub summary: String,
    /// 逐条问题。
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// 权限声明核对结论。
    #[serde(default)]
    pub permission_check: Vec<PermissionCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    /// `blocker` / `major` / `minor` / `info`。
    #[serde(default)]
    pub severity: String,
    /// 出问题的文件相对路径（拿不准时为空）。
    #[serde(default)]
    pub file: String,
    /// 问题描述（中文）。
    #[serde(default)]
    pub issue: String,
    /// 代码依据（原文片段）。
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionCheck {
    #[serde(default)]
    pub permission: String,
    /// `declared-and-used` / `declared-not-used` / `used-not-declared`。
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub note: String,
}

impl Verdict {
    pub fn approved(&self) -> bool {
        self.verdict.eq_ignore_ascii_case("approve")
    }

    /// 给作者看的驳回原因（拼 summary + 阻断级 findings）。
    pub fn reject_reason(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.summary.trim().is_empty() {
            parts.push(self.summary.trim().to_string());
        }
        for f in &self.findings {
            if matches!(f.severity.as_str(), "blocker" | "major") {
                let where_ = if f.file.is_empty() {
                    String::new()
                } else {
                    format!("{}：", f.file)
                };
                parts.push(format!("· {where_}{}", f.issue));
            }
        }
        if parts.is_empty() {
            "模型判定不予通过，但没有给出具体原因".to_string()
        } else {
            parts.join("\n")
        }
    }
}

const SYSTEM_PROMPT: &str = r#"你是 iTools 插件市场的安全审核员。iTools 插件是纯前端页面（HTML/CSS/JS），
运行在桌面客户端的独立 WebView 窗口里，通过 window.itools.* 门面调用宿主能力。

宿主提供的高危能力必须在 plugin.json 的 permissions 里声明，用户安装后还要逐项授权：
- runCommand    执行本机程序
- network       联网（itools.fetch）
- screen-capture 屏幕截图/录屏
- audio-capture 录音
- hotkey        注册全局热键

你的任务是判断这个插件能否上架。重点看这些机器查不出来的问题：
1. 恶意行为：窃取用户数据并外传、隐蔽外联、执行任意命令、读写不该碰的文件、挖矿、后门。
2. 权限名不副实：声明了却完全没用到的权限（过度申请），或用到了却没声明的能力。
3. 描述与实际功能不符（plugin.json 的 description/features 与代码行为对不上）。
4. 运行时拉取并执行远程代码：eval、new Function、动态 import()、document.write 注入脚本、
   innerHTML 拼接远程内容等——这类手法在审核时无害，上线后可随时变成任意代码。
5. 明显的用户数据风险：把 itools.db/data 里的内容发到第三方地址、把剪贴板内容外传等。

判定尺度：
- 只要存在上述 1、4、5 类问题中的任何一条实锤，一律 reject。
- 权限过度申请、描述不符，若不涉及数据外传，可记为 major/minor，由你判断是否严重到 reject；
  倾向于：过度申请高危权限（runCommand / network / screen-capture / audio-capture）→ reject，
  只是描述略有出入 → approve 但记 minor。
- 代码质量差、UI 简陋、功能少，都不是拒绝理由。
- 拿不准且没有实际证据时，不要臆造问题；把疑点写进 findings 的 info 级，verdict 仍可 approve。

必须只输出一个 JSON 对象，不要 markdown 代码块，结构如下：
{
  "verdict": "approve" | "reject",
  "riskLevel": "none" | "low" | "medium" | "high",
  "summary": "一句话中文结论",
  "findings": [
    {"severity":"blocker|major|minor|info","file":"相对路径","issue":"中文描述","evidence":"代码原文片段"}
  ],
  "permissionCheck": [
    {"permission":"network","status":"declared-and-used|declared-not-used|used-not-declared","note":"中文说明"}
  ]
}
findings 与 permissionCheck 可以为空数组。所有面向作者的文字用中文。"#;

/// 组装用户消息：清单 + 权限 + 源码清单 + 源码正文。
fn build_user_prompt(pkg: &StagedPackage, omitted: &[String]) -> String {
    let m = &pkg.manifest;
    let mut s = String::new();
    s.push_str("# 待审插件\n\n");
    s.push_str(&format!("name: {}\n", m.name));
    if !m.display_name.is_empty() {
        s.push_str(&format!("显示名: {}\n", m.display_name));
    }
    s.push_str(&format!("version: {}\n", m.version));
    s.push_str(&format!("author: {}\n", if m.author.is_empty() { "（未填）" } else { &m.author }));
    s.push_str(&format!(
        "description: {}\n",
        if m.description.is_empty() { "（未填）" } else { &m.description }
    ));
    s.push_str(&format!(
        "permissions（作者声明的高危能力）: {}\n",
        if m.permissions.is_empty() { "（未声明任何高危能力）".to_string() } else { m.permissions.join(", ") }
    ));
    s.push_str("features:\n");
    for f in &m.features {
        let cmds: Vec<String> = f.cmds.iter().map(|c| c.to_string()).collect();
        s.push_str(&format!("  - code={} explain={} cmds={}\n", f.code, f.explain, cmds.join(" ")));
    }
    s.push_str(&format!(
        "\n包统计: {} 个文件，{} 字节，内容哈希 {}\n",
        pkg.file_count, pkg.total_bytes, pkg.content_hash
    ));

    if !pkg.readme.is_empty() {
        s.push_str("\n# README\n\n");
        s.push_str(&pkg.readme);
        s.push('\n');
    }

    // 诚实告知模型「你没看到什么」：二进制文件与超预算被丢下的文本文件。
    // 不写这一段，模型会默认「给我的就是全部」，从而对没看到的部分给出无根据的结论。
    if !omitted.is_empty() {
        s.push_str("\n# 未提供源码的文件（你没看到它们的内容，不要对其内容下结论）\n\n");
        for o in omitted {
            s.push_str(&format!("- {o}\n"));
        }
    }

    s.push_str("\n# 源码\n");
    for (path, text) in &pkg.sources {
        s.push_str(&format!("\n---- FILE: {path} ----\n"));
        s.push_str(text);
        s.push('\n');
    }
    s.push_str("\n---- END ----\n\n请按 system 指定的 JSON 结构输出裁决。");
    s
}

// ---------------- OpenAI 兼容协议的最小请求/响应 ----------------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    response_format: ResponseFormat,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: ChoiceMessage,
}

#[derive(Deserialize, Default)]
struct ChoiceMessage {
    #[serde(default)]
    content: String,
}

/// 一次审核的完整结果：裁决 + 模型原始输出（原样落库，便于事后复核模型是否胡说）。
pub struct ReviewOutcome {
    pub verdict: Verdict,
    pub raw: String,
}

/// 调模型审一个包。
///
/// 返回 `Err` 表示**这次审核没能完成**（未配置 / 网络失败 / 返回不可解析），
/// 调用方必须把提审单置为「需人工处理」，**绝不能当作通过**。
pub async fn review(cfg: &LlmConfig, pkg: &StagedPackage) -> Result<ReviewOutcome, String> {
    if !cfg.enabled() {
        return Err(
            "服务端未配置审核模型（缺 ITOOLS_LLM_API_KEY），本次提审需要维护者人工处理".to_string(),
        );
    }

    // 哪些文件模型没看到——二进制、非 UTF-8、以及超出预算被丢下的
    let omitted = omitted_files(pkg);
    let user = build_user_prompt(pkg, &omitted);

    let body = ChatRequest {
        model: &cfg.model,
        messages: vec![
            ChatMessage { role: "system", content: SYSTEM_PROMPT },
            ChatMessage { role: "user", content: &user },
        ],
        response_format: ResponseFormat { kind: "json_object" },
        max_tokens: cfg.max_tokens,
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.timeout_sec))
        .build()
        .map_err(|e| format!("构造 HTTP 客户端失败: {e}"))?;

    let payload = serde_json::to_vec(&body).map_err(|e| format!("序列化审核请求失败: {e}"))?;
    let url = format!("{}/v1/chat/completions", cfg.base_url.trim_end_matches('/'));
    let res = client
        .post(&url)
        .bearer_auth(&cfg.api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload)
        .send()
        .await
        .map_err(|e| format!("调用审核模型失败: {}", scrub(&e.to_string(), &cfg.api_key)))?;

    let status = res.status();
    let text = res
        .text()
        .await
        .map_err(|e| format!("读取模型响应失败: {}", scrub(&e.to_string(), &cfg.api_key)))?;
    if !status.is_success() {
        // 响应体可能含 key 回显，脱敏后再落日志/回显
        let brief: String = scrub(&text, &cfg.api_key).chars().take(500).collect();
        return Err(format!("审核模型返回 HTTP {status}：{brief}"));
    }

    let parsed: ChatResponse = serde_json::from_str(&text)
        .map_err(|e| format!("模型响应不是预期的 JSON: {e}"))?;
    if let Some(err) = parsed.error {
        return Err(format!("审核模型报错：{err}"));
    }
    let content = parsed
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();
    if content.trim().is_empty() {
        return Err("审核模型返回了空结论".to_string());
    }

    let verdict: Verdict = parse_verdict(&content)?;
    Ok(ReviewOutcome { verdict, raw: content })
}

/// 解析模型输出的裁决。模型偶尔会把 JSON 包在 ```json 代码块里，这里容忍这一种形态；
/// 其余解析不了的一律报错（**不做「解析不了就放行」的兜底**）。
fn parse_verdict(content: &str) -> Result<Verdict, String> {
    let t = content.trim();
    let body = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|s| s.trim_start_matches('\n').trim_end_matches('`').trim())
        .unwrap_or(t);
    serde_json::from_str::<Verdict>(body)
        .map_err(|e| format!("模型裁决无法解析为约定结构: {e}；原文前 300 字：{}", body.chars().take(300).collect::<String>()))
}

/// 列出模型没拿到内容的文件（如实写进 prompt）。
fn omitted_files(pkg: &StagedPackage) -> Vec<String> {
    // sources 里没有、但包里存在的文件——这里只能从统计差推断数量，
    // 精确列表由调用方（pkg::stage）保留的文件清单给出；当前用 sources 的补集近似：
    // pkg 没有保存全量文件名，因此只在数量不一致时给出一条总体说明。
    let shown = pkg.sources.len();
    if pkg.file_count > shown {
        vec![format!(
            "另有 {} 个文件（二进制资源，或超出审阅预算）未提供内容",
            pkg.file_count - shown
        )]
    } else {
        Vec::new()
    }
}

/// 从任意字符串里抹掉 API key（错误串可能带上请求 URL / 头部回显）。
fn scrub(s: &str, key: &str) -> String {
    if key.is_empty() {
        return s.to_string();
    }
    s.replace(key, "***")
}

/// 供路由层展示：把裁决压成一行摘要。
pub fn brief(v: &Verdict) -> String {
    let n = v.findings.len();
    let head = if v.approved() { "通过" } else { "驳回" };
    if v.summary.trim().is_empty() {
        format!("{head}（{n} 条问题）")
    } else {
        format!("{head}：{}", v.summary.trim())
    }
}

/// 权限声明与实际使用的对照表（给作者看的可读文本）。
pub fn permission_table(v: &Verdict) -> BTreeMap<String, String> {
    v.permission_check
        .iter()
        .map(|p| {
            let label = match p.status.as_str() {
                "declared-and-used" => "已声明且确实用到",
                "declared-not-used" => "已声明但代码里没用到",
                "used-not-declared" => "代码里用到了但没声明",
                other => other,
            };
            let note = if p.note.trim().is_empty() {
                String::new()
            } else {
                format!("（{}）", p.note.trim())
            };
            (p.permission.clone(), format!("{label}{note}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let v = parse_verdict(r#"{"verdict":"approve","riskLevel":"none","summary":"ok"}"#).unwrap();
        assert!(v.approved());
        assert_eq!(v.risk_level, "none");
    }

    #[test]
    fn parses_fenced_json() {
        let v = parse_verdict("```json\n{\"verdict\":\"reject\",\"summary\":\"外传数据\"}\n```").unwrap();
        assert!(!v.approved());
        assert_eq!(v.summary, "外传数据");
    }

    #[test]
    fn unparseable_is_error_not_approval() {
        // 这条是诚信红线：解析不了绝不能变成「通过」
        assert!(parse_verdict("我觉得这个插件挺好的").is_err());
    }

    #[test]
    fn unknown_verdict_is_not_approved() {
        let v = parse_verdict(r#"{"verdict":"maybe"}"#).unwrap();
        assert!(!v.approved());
    }

    #[test]
    fn reject_reason_collects_blockers() {
        let v: Verdict = serde_json::from_str(
            r#"{"verdict":"reject","summary":"存在数据外传","findings":[
                {"severity":"blocker","file":"x.js","issue":"把 db 内容 POST 到第三方"},
                {"severity":"info","file":"y.js","issue":"无关紧要"}]}"#,
        )
        .unwrap();
        let r = v.reject_reason();
        assert!(r.contains("存在数据外传"));
        assert!(r.contains("x.js"));
        assert!(!r.contains("无关紧要"), "info 级不该进驳回原因");
    }

    #[test]
    fn scrub_removes_key() {
        assert_eq!(scrub("Bearer sk-abc failed", "sk-abc"), "Bearer *** failed");
    }
}
