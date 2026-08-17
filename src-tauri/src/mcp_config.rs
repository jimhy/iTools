//! **把 iTools 的 MCP 端点接进 AI 客户端**（Claude Code / Codex / Cursor）。
//!
//! # 读用文件，写用官方 CLI
//!
//! 这是本模块最重要的一条设计：
//!
//! - **读状态**直接解析配置文件。只读没有任何风险，还不依赖 CLI 在不在 PATH，
//!   面板因此总能如实显示"配没配"。
//! - **写入**优先调各家自己的 CLI（`claude mcp add` / `codex mcp add`）。
//!   它们自己处理文件锁、格式与 schema 变化——比我们去改别人正在用的配置文件安全一个数量级。
//!   `~/.claude.json` 有 100KB+ 的运行状态且 Claude Code 运行时还在写它，
//!   `~/.codex/config.toml` 里有用户手写的注释，这两个文件我们**一个字节都不碰**。
//!
//! Cursor 没有可用的命令行入口，只能改文件。好在它的 `~/.cursor/mcp.json` 是个
//! 只装 MCP 配置的小文件、格式标准，改动面比前两个小得多；即便如此仍然守着：
//! 只增改 `itools` 一个键、写前备份、解析不了就放弃、写完读回来校验。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// 我们在各家配置里占用的 server 名。**卸载只认它**。
pub const SERVER_KEY: &str = "itools";

/// 备份后缀。带 `itools` 字样，让用户一眼看出这文件是谁生成的。
const BAK_SUFFIX: &str = ".itools-bak";

/// 配置文件的格式（仅用于**读**状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigKind {
    /// 顶层 `mcpServers` 对象（Claude Code、Cursor）
    Json,
    /// `[mcp_servers.<name>]` 表（Codex）
    Toml,
}

/// 接入方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approach {
    /// `claude mcp add --transport http <name> <url>`
    ClaudeCli,
    /// `codex mcp add <name> --url <url>`
    CodexCli,
    /// 没有 CLI，直接改 JSON 配置文件（Cursor）
    JsonFile,
}

impl Approach {
    /// 这个方式依赖的命令行程序（走文件的没有）。
    fn cli_bin(self) -> Option<&'static str> {
        match self {
            Approach::ClaudeCli => Some("claude"),
            Approach::CodexCli => Some("codex"),
            Approach::JsonFile => None,
        }
    }
}

/// 读出配置文件里 `itools` 当前指向的 url（没配则 None）。
///
/// 解析失败一律当作"没配"：这个函数只用于渲染状态，读不懂别人的文件不该让整页报错。
pub fn current_url(path: &Path, kind: ConfigKind) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    match kind {
        ConfigKind::Json => {
            let v: Value = serde_json::from_str(&raw).ok()?;
            v.get("mcpServers")?.get(SERVER_KEY)?.get("url")?.as_str().map(str::to_string)
        }
        ConfigKind::Toml => {
            let doc: toml_edit::DocumentMut = raw.parse().ok()?;
            doc.get("mcp_servers")?.get(SERVER_KEY)?.get("url")?.as_str().map(str::to_string)
        }
    }
}

/// 接入（已配置则更新 url）。
pub fn install(approach: Approach, config: &Path, url: &str) -> Result<(), String> {
    match approach {
        Approach::ClaudeCli => run_cli(
            "claude",
            &["mcp", "add", "--transport", "http", SERVER_KEY, url],
            // 已存在时 add 会报错，先删再加，保证幂等
            Some(&["mcp", "remove", SERVER_KEY]),
        ),
        Approach::CodexCli => run_cli(
            "codex",
            &["mcp", "add", SERVER_KEY, "--url", url],
            Some(&["mcp", "remove", SERVER_KEY]),
        ),
        Approach::JsonFile => json_file_install(config, url),
    }
}

/// 断开（幂等：本来就没配也算成功）。
pub fn uninstall(approach: Approach, config: &Path) -> Result<(), String> {
    match approach {
        Approach::ClaudeCli => run_cli("claude", &["mcp", "remove", SERVER_KEY], None),
        Approach::CodexCli => run_cli("codex", &["mcp", "remove", SERVER_KEY], None),
        Approach::JsonFile => json_file_uninstall(config),
    }
}

/// CLI 在不在（决定面板要不要把按钮点亮）。
pub fn cli_available(approach: Approach) -> bool {
    match approach.cli_bin() {
        None => true, // 走文件的不需要 CLI
        Some(bin) => run_raw(bin, &["--version"]).is_ok(),
    }
}

/// 跑一条 CLI 命令；`cleanup` 是执行前的幂等清理（失败无所谓）。
fn run_cli(bin: &str, args: &[&str], cleanup: Option<&[&str]>) -> Result<(), String> {
    if let Some(c) = cleanup {
        let _ = run_raw(bin, c); // 本来就没配时会失败，属正常
    }
    run_raw(bin, args).map(|_| ())
}

/// 实际起进程。
///
/// 走 `cmd /C` 是因为 Windows 上这些工具是 npm 装的 `.cmd` 包装脚本，
/// 直接 spawn 程序名会 `NotFound`（PATHEXT 由 shell 解析，不是 CreateProcess）。
/// 参数都是我们自己拼的固定串与本机回环 URL，不含 `& | < > ^` 这类 cmd 元字符。
fn run_raw(bin: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd;
    #[cfg(windows)]
    {
        cmd = Command::new("cmd");
        cmd.arg("/C").arg(bin).args(args);
        // 不弹黑窗：这是 GUI 应用，起子进程不该闪一个控制台出来
        #[allow(clippy::unnecessary_cast)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
    }
    #[cfg(not(windows))]
    {
        cmd = Command::new(bin);
        cmd.args(args);
    }

    let out = cmd
        .output()
        .map_err(|e| format!("没能执行 `{bin}`：{e}。请确认它已安装并在 PATH 里。"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).to_string());
    }
    // 把 CLI 自己的错误原文回给用户——它比我们能给的任何转述都准确
    let err = String::from_utf8_lossy(&out.stderr);
    let err = if err.trim().is_empty() {
        String::from_utf8_lossy(&out.stdout).to_string()
    } else {
        err.to_string()
    };
    Err(format!("`{bin} {}` 执行失败：{}", args.join(" "), err.trim()))
}

// ==================== 只对 Cursor 用的文件改写 ====================

fn backup(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let bak = PathBuf::from(format!("{}{BAK_SUFFIX}", path.display()));
    std::fs::copy(path, &bak)
        .map(|_| ())
        .map_err(|e| format!("备份 {} 失败：{e}（已放弃修改，你的配置没被动过）", path.display()))
}

fn rollback(path: &Path) {
    let bak = PathBuf::from(format!("{}{BAK_SUFFIX}", path.display()));
    if bak.is_file() {
        let _ = std::fs::copy(&bak, path);
    }
}

fn json_file_install(path: &Path, url: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建配置目录 {} 失败：{e}", parent.display()))?;
    }
    backup(path)?;

    let mut root: Value = if path.is_file() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            // 解析不了就**放弃**，不去猜结构——这是别人的配置文件
            serde_json::from_str(&raw).map_err(|e| {
                format!(
                    "{} 不是合法 JSON（{e}）。没有改动它，请先修好这个文件或手动配置。",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };

    let obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} 的顶层不是 JSON 对象，已放弃改动", path.display()))?;
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    servers
        .as_object_mut()
        .ok_or_else(|| format!("{} 里的 mcpServers 不是对象，已放弃改动", path.display()))?
        .insert(SERVER_KEY.to_string(), json!({ "type": "http", "url": url }));

    let text = serde_json::to_string_pretty(&root).map_err(|e| format!("序列化失败：{e}"))?;
    std::fs::write(path, &text).map_err(|e| {
        rollback(path);
        format!("写入 {} 失败：{e}（已从备份回滚）", path.display())
    })?;

    // 写完读回来校验，确认没把文件写成读不懂的东西
    if current_url(path, ConfigKind::Json).as_deref() != Some(url) {
        rollback(path);
        return Err(format!("写入 {} 后校验未通过，已从备份回滚", path.display()));
    }
    Ok(())
}

fn json_file_uninstall(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let mut v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("{} 不是合法 JSON（{e}），没有改动它", path.display()))?;
    let removed = match v.get_mut("mcpServers").and_then(Value::as_object_mut) {
        Some(map) => map.remove(SERVER_KEY).is_some(),
        None => false,
    };
    if !removed {
        return Ok(()); // 本来就没配，幂等返回
    }
    backup(path)?;
    let text = serde_json::to_string_pretty(&v).map_err(|e| format!("序列化失败：{e}"))?;
    std::fs::write(path, text).map_err(|e| {
        rollback(path);
        format!("写入 {} 失败：{e}（已从备份回滚）", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str, name: &str, content: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("itools-mcpcfg-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let p = dir.join(name);
        if let Some(c) = content {
            std::fs::write(&p, c).expect("写初始内容");
        }
        p
    }

    /// 只改 `itools` 一个键：用户其余的顶层键、别的 server、**键顺序**都要原样保住。
    #[test]
    fn json_keeps_everything_else_including_order() {
        let p = tmp(
            "keep",
            "mcp.json",
            Some(r#"{"numStartups":42,"mcpServers":{"dart":{"type":"stdio","command":"dart"}},"tips":{"a":1}}"#),
        );
        json_file_install(&p, "http://127.0.0.1:7345/mcp").expect("应当写入成功");

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 42, "用户的其它顶层键不能丢");
        assert_eq!(v["tips"]["a"], 1, "嵌套内容不能丢");
        assert_eq!(v["mcpServers"]["dart"]["command"], "dart", "用户已有的 server 不能丢");
        assert_eq!(v["mcpServers"][SERVER_KEY]["url"], "http://127.0.0.1:7345/mcp");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["numStartups", "mcpServers", "tips"], "键顺序必须原样保留");

        json_file_uninstall(&p).expect("卸载成功");
        let v2: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v2["mcpServers"][SERVER_KEY].is_null(), "itools 应当被删掉");
        assert_eq!(v2["mcpServers"]["dart"]["command"], "dart", "别人的 server 必须还在");
        assert_eq!(v2["numStartups"], 42);
    }

    /// 文件损坏时放弃改动，且原文件一个字节都不许变。
    #[test]
    fn broken_file_is_left_untouched() {
        let broken = "{ 这不是合法 JSON";
        let p = tmp("broken", "mcp.json", Some(broken));
        let err = json_file_install(&p, "http://127.0.0.1:7345/mcp").expect_err("应当拒绝");
        assert!(err.contains("不是合法 JSON"), "要说清楚为什么放弃：{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), broken, "原文件必须原封不动");
    }

    #[test]
    fn creates_file_when_absent_and_reinstall_updates_in_place() {
        let p = tmp("absent", "mcp.json", None);
        json_file_install(&p, "http://127.0.0.1:7345/mcp").expect("首次应当创建");
        json_file_install(&p, "http://127.0.0.1:9999/mcp").expect("再次应当更新");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"][SERVER_KEY]["url"], "http://127.0.0.1:9999/mcp");
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 1, "不该留下重复条目");
    }

    #[test]
    fn backup_is_created_before_writing() {
        let orig = r#"{"mcpServers":{}}"#;
        let p = tmp("backup", "mcp.json", Some(orig));
        json_file_install(&p, "http://127.0.0.1:7345/mcp").expect("写入");
        let bak = PathBuf::from(format!("{}{BAK_SUFFIX}", p.display()));
        assert!(bak.is_file(), "必须留下备份");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), orig, "备份是改动前的内容");
    }

    /// TOML 只读：能从 Codex 的配置里认出我们那一项，且不受其它 server 干扰。
    #[test]
    fn reads_url_from_codex_toml() {
        let p = tmp(
            "toml",
            "config.toml",
            Some("# 用户注释\n[mcp_servers.pencil]\ncommand = 'p.exe'\n\n[mcp_servers.itools]\nurl = \"http://127.0.0.1:7345/mcp\"\n"),
        );
        assert_eq!(
            current_url(&p, ConfigKind::Toml).as_deref(),
            Some("http://127.0.0.1:7345/mcp")
        );
        let p2 = tmp("toml-none", "config.toml", Some("[mcp_servers.pencil]\ncommand = 'p.exe'\n"));
        assert!(current_url(&p2, ConfigKind::Toml).is_none(), "没配就该是 None");
    }

    /// 走 CLI 的方式不需要我们碰配置文件——这条锁住"高风险文件一个字节都不写"的约定。
    #[test]
    fn cli_approaches_declare_their_binary() {
        assert_eq!(Approach::ClaudeCli.cli_bin(), Some("claude"));
        assert_eq!(Approach::CodexCli.cli_bin(), Some("codex"));
        assert_eq!(Approach::JsonFile.cli_bin(), None, "只有它才允许改文件");
    }
}
