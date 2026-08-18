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
//! 只增改 [`SERVER_KEY`] 一个键、写前备份、解析不了就放弃、写完读回来校验。

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

/// 我们在各家配置里占用的 server 名。**读状态、写入、卸载都只认它**。
///
/// # 为什么 debug 与 release 必须是两个不同的名字
///
/// 两种构建是**两个不同端口**的 MCP 服务器（默认 7345，被占用则自动避让到
/// 7346/7347…，见 [`crate::mcp`]），而 AI 客户端的配置文件全机器只有一份。
/// 名字一样的话，配置里就只能存下一个端口——在哪个实例点「重新安装」，配置就指向哪个，
/// 另一个实例立刻被 [`crate::ai_clients`] 判成 `mcpStale`，两个实例来回抢同一个键。
/// 分成两个名字之后，两份配置各占各的，谁也不碰谁：
/// **debug 卸载只会删掉 `itools-dev`，用户正式版的 `itools` 一个字节都不会动。**
///
/// # release 的值锁死为 `"itools"`
///
/// 已装用户的 `~/.claude.json` / `~/.codex/config.toml` / `~/.cursor/mcp.json` 里
/// 存的就是这个名字。改掉它等于让他们已有的配置全部失效：面板显示「未接入」，
/// 而 AI 那边还留着一条我们再也不会去清理的旧条目。所以这半边**一个字符都不能改**。
///
/// # 为什么用 `const` + `cfg!` 而不是拆成两条 `#[cfg]` 常量、或改成函数
///
/// - 两个取值写在同一行，「release 是哪个、debug 是哪个」一眼可见，不用跨 cfg 分支对照；
/// - `cfg!` 展开成 bool 字面量，`if` 在常量上下文里可求值，仍是编译期定死的常量；
/// - 类型还是 `&'static str`，十来处调用点（拼 CLI 参数、读写 JSON/TOML 的键）一处都不用改。
pub const SERVER_KEY: &str = if cfg!(debug_assertions) { "itools-dev" } else { "itools" };

/// 备份后缀。带 `itools` 字样，让用户一眼看出这文件是谁生成的。
///
/// 这个**刻意不分构建**：它只在单次写入内用于回滚（写前备份 → 写失败就拷回来），
/// 生命周期短到两种构建撞不上，而文件名保持稳定反倒方便用户认出来。
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

/// 读出配置文件里 [`SERVER_KEY`] 当前指向的 url（没配则 None）。
///
/// 只认自己那一个键：debug 读不到 release 的 `itools`，反之亦然——
/// 面板上的「已接入 / 未接入」因此永远说的是**本构建自己**的真实状态。
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
            // ⚠ `--scope user` 不能省（省了会写成「装了但不生效」，见下方长注释）
            &["mcp", "add", "--scope", "user", "--transport", "http", SERVER_KEY, url],
            // 已存在时 add 会报错，先删再加，保证幂等。
            // 两个 scope 都清：`user` 是我们自己写的那份；`local` 是补这个 bug 之前的
            // 老版本漏了 `--scope` 时留下的错位残留，不清掉的话面板断开了它还在。
            CLAUDE_CLEANUPS,
        ),
        Approach::CodexCli => run_cli(
            "codex",
            &["mcp", "add", SERVER_KEY, "--url", url],
            // Codex 没有 scope 概念，`~/.codex/config.toml` 全局唯一一份，读写同址。
            &[&["mcp", "remove", SERVER_KEY]],
        ),
        Approach::JsonFile => json_file_install(config, url),
    }
}

/// 断开（幂等：本来就没配也算成功）。
///
/// **只删 [`SERVER_KEY`] 这一项**——别人的 server 不动，另一种构建的那一项也不动
/// （debug 在这里删的是 `itools-dev`，用户正式版的 `itools` 碰都不会碰）。
///
/// Claude 那条同样两个 scope 都删：只删 `user` 的话，老版本写进 `local` 的错位残留
/// 会永远留在 `~/.claude.json` 里，用户点了「断开」也甩不掉。
pub fn uninstall(approach: Approach, config: &Path) -> Result<(), String> {
    match approach {
        Approach::ClaudeCli => {
            // 两条**都要跑完**，不能删成一条就提前返回：`user` 是当前写的那份，
            // `local` 是老版本漏 `--scope` 留下的错位残留，两份都得清掉。
            let mut removed_any = false;
            let mut errs: Vec<String> = Vec::new();
            for args in CLAUDE_CLEANUPS {
                match run_raw("claude", args) {
                    Ok(_) => removed_any = true,
                    Err(e) => errs.push(e),
                }
            }
            // `claude mcp remove` 删一个不存在的 server 会以**退出码 1** 结束
            // （实测，不是猜的），所以「本来就没配」在这里长得和真失败一样。
            // 靠 CLI 的原文区分：两个 scope 都说「没有这一项」才算幂等成功；
            // 其它原因（claude 不在 PATH、配置文件损坏等）如实报错，绝不假装断开成功。
            if removed_any || errs.iter().all(|e| e.contains("No MCP server named")) {
                Ok(())
            } else {
                Err(errs.join("；"))
            }
        }
        Approach::CodexCli => run_cli("codex", &["mcp", "remove", SERVER_KEY], &[]),
        Approach::JsonFile => json_file_uninstall(config),
    }
}

/// Claude Code 侧要清理的两个 scope。
///
/// # 为什么必须显式带 `--scope`（这是个真踩过的 bug，别改回去）
///
/// `claude mcp add` 的默认 scope 是 **local**，写进的是
/// `~/.claude.json` → `projects.<当前工作目录>.mcpServers`，**不是顶层 `mcpServers`**。
/// 而这条命令是 iTools 进程调起的，子进程继承的是 iTools 自己的工作目录
/// （正式版下就是安装目录，如 `D:\Program Files\iTools`）。于是：
///
/// - 配置落进了 `projects["D:/Program Files/iTools"].mcpServers` 这个谁也不会 cd 进去的"项目"，
///   用户在自己的代码目录里开 Claude Code **根本加载不到这个 MCP**——真的不生效，
///   不只是显示问题（表现为 skill 里记的那条「连不上开发者中心 MCP」）；
/// - [`current_url`] 读的是顶层 `mcpServers`，那里永远没有我们这一项，于是面板恒显
///   「MCP 未接入」，整行状态卡在「未完成」，用户点多少次「补全接入」都不会变绿。
///
/// 两头一起错，正好撞进项目诚信红线的「看着装好了、点了不生效」。`--scope user` 写的
/// 才是顶层 `mcpServers`，与 [`current_url`] 读的位置一致，且对所有目录生效。
const CLAUDE_CLEANUPS: &[&[&str]] = &[
    &["mcp", "remove", "--scope", "user", SERVER_KEY],
    &["mcp", "remove", "--scope", "local", SERVER_KEY],
];

/// CLI 在不在（决定面板要不要把按钮点亮）。
pub fn cli_available(approach: Approach) -> bool {
    match approach.cli_bin() {
        None => true, // 走文件的不需要 CLI
        Some(bin) => run_raw(bin, &["--version"]).is_ok(),
    }
}

/// 跑一条 CLI 命令；`cleanups` 是执行前逐条跑的幂等清理（每条失败都无所谓）。
fn run_cli(bin: &str, args: &[&str], cleanups: &[&[&str]]) -> Result<(), String> {
    for c in cleanups {
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

    /// 另一种构建占用的 server 名（用 debug 跑测试时，它就是 release 的 `itools`）。
    fn other_build_key() -> &'static str {
        if cfg!(debug_assertions) { "itools" } else { "itools-dev" }
    }

    /// server 名必须按构建类型分开，且 **release 那半边锁死在 `itools`**。
    ///
    /// 名字相同 → 两种构建抢同一个配置键，互相把对方顶成 `mcpStale`；
    /// release 名字改了 → 已装用户的配置全部失效。两条都是发版级事故，所以钉在测试里。
    #[test]
    fn server_key_is_per_build_and_release_name_is_frozen() {
        if cfg!(debug_assertions) {
            assert_eq!(SERVER_KEY, "itools-dev", "debug 必须用独立的 server 名");
        } else {
            assert_eq!(SERVER_KEY, "itools", "release 的名字是已装用户配置里的既有值，不许改");
        }
        assert_ne!(SERVER_KEY, other_build_key(), "两种构建的 server 名不能相同");
    }

    /// 只改 [`SERVER_KEY`] 一个键：用户其余的顶层键、别的 server、**键顺序**都要原样保住。
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
        assert!(v2["mcpServers"][SERVER_KEY].is_null(), "我们那一项应当被删掉");
        assert_eq!(v2["mcpServers"]["dart"]["command"], "dart", "别人的 server 必须还在");
        assert_eq!(v2["numStartups"], 42);
    }

    /// **本任务最容易出错的地方**：debug 版接入 / 断开时，绝不能碰到 release 版的那一项。
    ///
    /// 现实场景就是「一边跑 release 正常用，一边跑 debug 调」：debug 里点一次「断开」，
    /// 如果把 `itools` 也删了，用户的正式版接入就被开发调试悄悄干掉了。
    #[test]
    fn install_and_uninstall_never_touch_the_other_builds_entry() {
        let other = other_build_key();
        let other_url = "http://127.0.0.1:9999/mcp";
        let p = tmp(
            "otherbuild",
            "mcp.json",
            Some(&format!(
                r#"{{"mcpServers":{{"{other}":{{"type":"http","url":"{other_url}"}}}}}}"#
            )),
        );

        json_file_install(&p, "http://127.0.0.1:7345/mcp").expect("应当写入成功");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["mcpServers"][other]["url"], other_url, "另一种构建的条目不能被覆盖");
        assert_eq!(v["mcpServers"][SERVER_KEY]["url"], "http://127.0.0.1:7345/mcp");
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 2, "两项各占各的");

        json_file_uninstall(&p).expect("卸载成功");
        let v2: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v2["mcpServers"][SERVER_KEY].is_null(), "只该删掉自己那一项");
        assert_eq!(v2["mcpServers"][other]["url"], other_url, "另一种构建的条目必须原样还在");
    }

    /// 只有对方构建那一项时，`current_url` 必须读成「没配」——
    /// 拿对方的状态冒充自己「已接入」，用户会以为能用、实际连的是另一个端口。
    #[test]
    fn other_builds_entry_reads_as_not_configured() {
        let other = other_build_key();
        let p = tmp(
            "otheronly",
            "mcp.json",
            Some(&format!(
                r#"{{"mcpServers":{{"{other}":{{"type":"http","url":"http://127.0.0.1:9999/mcp"}}}}}}"#
            )),
        );
        assert!(current_url(&p, ConfigKind::Json).is_none(), "不许把对方那一项当成自己的");

        let t = tmp(
            "otheronly-toml",
            "config.toml",
            Some(&format!("[mcp_servers.{other}]\nurl = \"http://127.0.0.1:9999/mcp\"\n")),
        );
        assert!(current_url(&t, ConfigKind::Toml).is_none(), "TOML 侧同样不许认错");
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
        // 键名随构建走（`itools` / `itools-dev`）——两者都是合法的 TOML 裸键
        let p = tmp(
            "toml",
            "config.toml",
            Some(&format!(
                "# 用户注释\n[mcp_servers.pencil]\ncommand = 'p.exe'\n\n[mcp_servers.{SERVER_KEY}]\nurl = \"http://127.0.0.1:7345/mcp\"\n"
            )),
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
