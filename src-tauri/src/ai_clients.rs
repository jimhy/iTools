//! **AI 助手接入**：一键把 iTools 的 MCP 端点与插件开发 skill 装进 AI 客户端。
//!
//! # 为什么把两件事合成一个动作
//!
//! 用户要的从来不是「装 skill」或「配 MCP」，而是**「让我的 AI 会写 iTools 插件」**。
//! 这两样缺一不可：skill 让它知道插件该怎么写，MCP 让它能跑模拟器、读日志、提审。
//! 分成两个卡片、两组按钮，等于把内部实现摊给用户去理解和拼装。
//!
//! 所以这一层按**客户端**组织：一行一个 AI 助手，一个按钮同时办完两件事，
//! 两个小标签（MCP / SKILL）如实反映各自的状态。
//!
//! # 边界
//!
//! - MCP 写入优先走各家官方 CLI（见 [`crate::mcp_config`]），只有 Cursor 没有 CLI 才改文件；
//! - skill 只碰 `<客户端目录>/skills/<本构建的命名空间>/` 这一个子目录
//!   （release `itools-plugin-dev` / debug `itools-plugin-dev-dev`，见
//!   [`crate::skills::SKILL_INSTALL_NAME`]）；
//! - **两件事分别报告成败**：一个成一个败时如实说明哪个成了、哪个没成，
//!   绝不因为「装了一半」就笼统报成功或失败。

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::logging::ilog;
use crate::mcp_config::{self, Approach, ConfigKind};
use crate::skills;

/// 一个受支持的 AI 客户端。
#[derive(Debug)]
struct ClientDef {
    id: &'static str,
    label: &'static str,
    /// 家目录下的配置目录名（探测客户端装没装、以及 skill 落点都用它）
    home_dir: &'static str,
    approach: Approach,
    kind: ConfigKind,
}

/// 支持的客户端。三家吃的是同一份 `SKILL.md`（开放标准），skill 源只有一份、装到三处。
const CLIENTS: &[ClientDef] = &[
    ClientDef {
        id: "claude",
        label: "Claude Code",
        home_dir: ".claude",
        approach: Approach::ClaudeCli,
        kind: ConfigKind::Json,
    },
    ClientDef {
        id: "codex",
        label: "Codex CLI",
        home_dir: ".codex",
        approach: Approach::CodexCli,
        kind: ConfigKind::Toml,
    },
    ClientDef {
        id: "cursor",
        label: "Cursor",
        home_dir: ".cursor",
        approach: Approach::JsonFile,
        kind: ConfigKind::Json,
    },
];

impl ClientDef {
    /// MCP 配置文件路径。
    ///
    /// ⚠ Claude Code 那份在**家目录根上**（`~/.claude.json`），不在 `~/.claude/` 里面。
    /// 弄错会读到一个不存在的文件，于是明明配好了却一直显示「未接入」。
    fn config_path(&self, home: &Path) -> PathBuf {
        match self.id {
            "claude" => home.join(".claude.json"),
            "codex" => home.join(".codex").join("config.toml"),
            _ => home.join(self.home_dir).join("mcp.json"),
        }
    }

    /// skill 安装目录。
    ///
    /// 末段是 [`skills::SKILL_INSTALL_NAME`]，**按构建类型分开**（release `itools-plugin-dev` /
    /// debug `itools-plugin-dev-dev`）。安装、状态检测、卸载全走这一个函数，所以
    /// **debug 从头到尾都够不着用户正式版那份**——尤其是卸载：以前两边同名时，
    /// 在 debug 面板点一次「断开接入」就会把 release 装的那份 `remove_dir_all` 掉。
    fn skill_dir(&self, home: &Path) -> PathBuf {
        home.join(self.home_dir).join("skills").join(skills::SKILL_INSTALL_NAME)
    }
}

/// 一个客户端的接入状态（前端一行渲染所需的全部信息）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiClient {
    pub id: String,
    pub label: String,
    /// 家目录下有没有它的配置目录。没有多半是没装这个客户端——**仍允许安装**
    /// （装完再装客户端也认），但界面要如实标注，免得用户以为装错了。
    pub detected: bool,
    /// MCP 是否已接入（读配置文件得来，不依赖 CLI）。
    pub mcp_ready: bool,
    /// 配着的地址与**当前实际监听地址**不一致（换过端口就会这样）——要重装才能修。
    pub mcp_stale: bool,
    /// skill 是否已装且是 iTools 装的。
    pub skill_ready: bool,
    /// skill 版本落后于随包版本。
    pub skill_outdated: bool,
    /// 已装的 skill 版本。
    pub skill_version: Option<String>,
    /// 接入 MCP 依赖的命令行工具在不在。**为假时一键按钮要禁用**，
    /// 而不是让用户点了才发现 `claude` 不在 PATH。
    pub cli_ready: bool,
    /// MCP 配置文件的完整路径（写别人的文件，得让用户看得见写到哪）。
    pub config_path: String,
    /// skill 安装目录的完整路径。
    pub skill_path: String,
    /// 需要额外讲清楚的情况（skill 目录不是 iTools 装的、CLI 缺失等）。
    pub note: Option<String>,
    /// **两样都齐且都不过期**才算就绪。由后端算好给前端，
    /// 免得前端再拼一遍判断逻辑、两边对不上。
    pub ready: bool,
}

/// 整页状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiClientsStatus {
    /// MCP 服务器在不在跑。**没跑就没法接入**（地址都没有），一键按钮要禁用并说明原因。
    pub mcp_running: bool,
    /// 当前实际监听地址（未运行为空串）。
    pub mcp_url: String,
    /// 随包 skill 源可用吗（打包漏了 resources 就会是 false）。
    pub skill_source_available: bool,
    pub skill_source_error: Option<String>,
    /// 随包 skill 版本（= 当前 iTools 版本）。
    pub bundled_version: String,
    /// 检测到几个客户端（图里那个「已检测到 N 个客户端」）。
    pub detected_count: usize,
    pub clients: Vec<AiClient>,
}

fn home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "找不到当前用户的家目录".to_string())
}

fn find(id: &str) -> Result<&'static ClientDef, String> {
    CLIENTS.iter().find(|c| c.id == id).ok_or_else(|| {
        format!(
            "不认识的 AI 客户端「{id}」。支持的是：{}",
            CLIENTS.iter().map(|c| c.id).collect::<Vec<_>>().join(" / ")
        )
    })
}

fn scan_one(def: &ClientDef, home: &Path, mcp_url: &str, bundled: &str) -> AiClient {
    let config_path = def.config_path(home);
    let skill_dir = def.skill_dir(home);

    // 读的是 mcp_config::SERVER_KEY 那一项，而它按构建类型分开（release `itools` /
    // debug `itools-dev`）。所以下面这一整套判定说的都是**本构建自己**那一项：
    // debug 面板上的「已接入」绝不会是拿 release 那项的状态冒充的，反之亦然。
    let configured = mcp_config::current_url(&config_path, def.kind);
    let mcp_ready = configured.is_some();
    // 地址对不上：多半是 MCP 端口换过（ITOOLS_MCP_PORT 或被占用后重选），
    // 这种「配了但连不上」比没配更迷惑人，所以单独标出来。
    // 注意这里比的是「自己那一项 vs 自己的监听地址」——另一种构建配在别的端口上
    // 是**正常**的，不能因为它而报 stale（否则用户一点「重新安装」就把对方抢了）
    let mcp_stale = match (&configured, mcp_url.is_empty()) {
        (Some(u), false) => u != mcp_url,
        _ => false,
    };

    // 和上面 MCP 那套同理：`skill_dir` 已经按构建分开，下面这几个判定说的都是**本构建自己**
    // 那一份。debug 面板上的「SKILL 已接入 / 需更新」既不会拿 release 那份的状态冒充，
    // 也不会因为 release 装的是别的版本就把自己报成 outdated（那正是过去两边抢 marker 的病根）。
    let skill_marker_version = skills::installed_version(&skill_dir);
    let skill_exists = skill_dir.is_dir();
    let skill_ready = skill_marker_version.is_some();
    let skill_outdated = skill_ready && skill_marker_version.as_deref() != Some(bundled);

    let cli_ready = mcp_config::cli_available(def.approach);

    let mut notes: Vec<String> = Vec::new();
    if skill_exists && !skill_ready {
        notes.push(format!(
            "{} 已存在但不是 iTools 装的，为免覆盖你自己的文件，安装与卸载都不会碰它。",
            skill_dir.display()
        ));
    }
    if !cli_ready {
        if let Some(bin) = match def.approach {
            Approach::ClaudeCli => Some("claude"),
            Approach::CodexCli => Some("codex"),
            Approach::JsonFile => None,
        } {
            notes.push(format!(
                "没有找到 `{bin}` 命令，无法自动接入 MCP。装好它并确保在 PATH 里之后再试。"
            ));
        }
    }
    if mcp_stale {
        notes.push(format!(
            "已接入的地址是 {}，与当前监听的 {mcp_url} 不一致，点「重新安装」可修正。",
            configured.as_deref().unwrap_or("?")
        ));
    }

    AiClient {
        id: def.id.to_string(),
        label: def.label.to_string(),
        detected: home.join(def.home_dir).is_dir(),
        mcp_ready,
        mcp_stale,
        skill_ready,
        skill_outdated,
        skill_version: skill_marker_version,
        cli_ready,
        config_path: config_path.display().to_string(),
        skill_path: skill_dir.display().to_string(),
        note: (!notes.is_empty()).then(|| notes.join(" ")),
        ready: mcp_ready && !mcp_stale && skill_ready && !skill_outdated,
    }
}

fn status_of(app: &AppHandle) -> Result<AiClientsStatus, String> {
    let home = home()?;
    let bundled = env!("CARGO_PKG_VERSION");
    let mcp = crate::mcp::status();
    let (skill_source_available, skill_source_error) = match skills::source_dir(app) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e)),
    };
    let clients: Vec<AiClient> = CLIENTS
        .iter()
        .map(|d| scan_one(d, &home, &mcp.url, bundled))
        .collect();
    Ok(AiClientsStatus {
        mcp_running: mcp.running,
        mcp_url: mcp.url,
        skill_source_available,
        skill_source_error,
        bundled_version: bundled.to_string(),
        detected_count: clients.iter().filter(|c| c.detected).count(),
        clients,
    })
}

// ==================== 命令 ====================

/// 命令：读全部 AI 客户端的接入状态。
#[tauri::command]
pub fn ai_clients_status(app: AppHandle) -> Result<AiClientsStatus, String> {
    status_of(&app)
}

/// 命令：一键接入（装 skill + 配 MCP）。
///
/// 两件事**分别报告**：一件成一件败时，如实说清哪个成了、哪个没成，
/// 并把已经成的那件保留下来（不做"要么全成要么全撤"的回滚——
/// 装好的 skill 本身是有用的，为了 MCP 失败就把它删掉反而更糟）。
#[tauri::command]
pub fn ai_client_install(app: AppHandle, target: String) -> Result<AiClientsStatus, String> {
    let def = find(&target)?;
    let home = home()?;
    let mcp = crate::mcp::status();
    if !mcp.running {
        return Err(format!(
            "MCP 服务器没有在运行，拿不到接入地址，无法接入。{}",
            mcp.error.unwrap_or_else(|| "请检查开发者中心里的服务器状态。".into())
        ));
    }

    let mut failures: Vec<String> = Vec::new();

    // ① skill
    match skills::source_dir(&app) {
        Ok(src) => {
            let dst = def.skill_dir(&home);
            if let Err(e) = skills::install_to(&src, &dst, env!("CARGO_PKG_VERSION")) {
                failures.push(format!("插件开发 skill 安装失败：{e}"));
            }
        }
        Err(e) => failures.push(format!("找不到随包的 skill 源：{e}")),
    }

    // ② MCP
    let config = def.config_path(&home);
    if let Err(e) = mcp_config::install(def.approach, &config, &mcp.url) {
        failures.push(format!("MCP 接入失败：{e}"));
    }

    if failures.is_empty() {
        ilog!("[iTools] 已为 {} 接入 MCP 与插件开发 skill", def.label);
        return status_of(&app);
    }
    Err(format!("{} 接入未完全成功 —— {}", def.label, failures.join("；")))
}

/// 断开时「删 skill」那一步。
///
/// 单独抽出来是为了**能被测试直接跑**：命令层要 `AppHandle`，测试造不出来，
/// 而这一步恰恰是全模块最危险的一处（`remove_dir_all` 别人配置目录里的东西）。
/// 抽出来之后，「debug 断开不会碰 release 那份」就能钉在真实调用路径上，
/// 而不是在测试里另抄一遍逻辑（那种测试只能证明抄得对，证明不了线上代码对）。
fn uninstall_skill(def: &ClientDef, home: &Path) -> Result<(), String> {
    let dst = def.skill_dir(home);
    // 没装过就不用报错（幂等）；装过但不是我们的，uninstall_at 会拒绝并说明
    if !dst.is_dir() {
        return Ok(());
    }
    skills::uninstall_at(&dst)
}

/// 命令：断开接入（删 skill + 移除 MCP 配置）。
#[tauri::command]
pub fn ai_client_uninstall(app: AppHandle, target: String) -> Result<AiClientsStatus, String> {
    let def = find(&target)?;
    let home = home()?;
    let mut failures: Vec<String> = Vec::new();

    if let Err(e) = uninstall_skill(def, &home) {
        failures.push(format!("skill 卸载失败：{e}"));
    }
    if let Err(e) = mcp_config::uninstall(def.approach, &def.config_path(&home)) {
        failures.push(format!("MCP 配置移除失败：{e}"));
    }

    if failures.is_empty() {
        ilog!("[iTools] 已为 {} 断开 MCP 与 skill", def.label);
        return status_of(&app);
    }
    Err(format!("{} 断开未完全成功 —— {}", def.label, failures.join("；")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code 的 MCP 配置在**家目录根上**，不在 `~/.claude/` 里。
    /// 这条写错会导致「明明配好了却一直显示未接入」，而且很难查。
    #[test]
    fn claude_config_lives_at_home_root_not_inside_dot_claude() {
        let home = PathBuf::from("C:\\Users\\someone");
        let claude = find("claude").expect("应当支持 claude");
        assert_eq!(claude.config_path(&home), home.join(".claude.json"));
        assert_ne!(
            claude.config_path(&home),
            home.join(".claude").join(".claude.json"),
            "别把它放进 .claude 目录里"
        );

        let codex = find("codex").expect("应当支持 codex");
        assert_eq!(codex.config_path(&home), home.join(".codex").join("config.toml"));

        let cursor = find("cursor").expect("应当支持 cursor");
        assert_eq!(cursor.config_path(&home), home.join(".cursor").join("mcp.json"));
    }

    /// skill 一律落在 `<客户端目录>/skills/<本构建的命名空间>`，不许越出这个命名空间。
    #[test]
    fn skill_dirs_stay_in_their_namespace() {
        let home = PathBuf::from("C:\\Users\\someone");
        for c in CLIENTS {
            let p = c.skill_dir(&home);
            assert!(p.starts_with(&home));
            assert!(
                p.ends_with(skills::SKILL_INSTALL_NAME),
                "必须以命名空间目录结尾：{}",
                p.display()
            );
            assert_eq!(p.parent().and_then(|x| x.file_name()), Some("skills".as_ref()));
        }
    }

    /// debug 构建的 skill 落点，必须**不是** release 那个冻结目录。
    ///
    /// 这条只在 debug 下跑：`cargo test --release` 时落点本就该是 `itools-plugin-dev`。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_skill_dir_is_not_the_release_one() {
        let home = PathBuf::from("C:\\Users\\someone");
        for c in CLIENTS {
            assert_ne!(
                c.skill_dir(&home),
                home.join(c.home_dir).join("skills").join("itools-plugin-dev"),
                "debug 不许指向用户正式版那份 skill（{}）",
                c.id
            );
        }
    }

    /// **本模块最危险的一条**：开发者在 debug 面板点「断开接入」，
    /// 绝不能把用户日常在用的正式版那份 skill 删掉。
    ///
    /// 这不是假想的风险——两边同名时它是必然发生的：debug 与 release 的
    /// `skill_dir()` 会算出同一个路径，`uninstall_at` 看到自己写的标记，删得理直气壮，
    /// 用户那边只会发现「我的 AI 突然不会写 iTools 插件了」，且没有任何提示。
    ///
    /// 顺带钉住状态读取：面板上的版本必须是 debug 自己那份的，不能串到 release 那份。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_uninstall_never_touches_the_release_copy() {
        let home = std::env::temp_dir().join("itools-aic-skill-isolation");
        let _ = std::fs::remove_dir_all(&home);
        let claude = find("claude").expect("应当支持 claude");

        // ① 造一份「用户正式版装的」：目录名是冻结的 itools-plugin-dev，带我们的标记
        let release_copy = home.join(".claude").join("skills").join("itools-plugin-dev");
        std::fs::create_dir_all(&release_copy).expect("建 release 那份");
        std::fs::write(release_copy.join("SKILL.md"), "release 装的，别动").expect("写 SKILL.md");
        std::fs::write(
            release_copy.join(skills::MARKER),
            r#"{"installedBy":"iTools","version":"1.0.0"}"#,
        )
        .expect("写 release 的标记");

        // ② debug 自己装一份：落点必须是另一个目录
        let src = home.join("src");
        std::fs::create_dir_all(&src).expect("建源目录");
        std::fs::write(src.join("SKILL.md"), "---\nname: itools-plugin-dev\n---\ndebug 装的").expect("写源");
        let dst = claude.skill_dir(&home);
        assert_ne!(dst, release_copy, "debug 的落点不能和 release 撞");
        skills::install_to(&src, &dst, "9.9.9").expect("debug 安装应当成功");
        assert_eq!(
            std::fs::read_to_string(release_copy.join("SKILL.md")).unwrap(),
            "release 装的，别动",
            "安装也不许碰 release 那份"
        );

        // ③ 状态只反映 debug 自己那份：版本是 9.9.9 而不是 release 的 1.0.0，
        //    因而也不会被随包版本 9.9.9 判成「需更新」
        let c = scan_one(claude, &home, "", "9.9.9");
        assert!(c.skill_ready, "自己那份装好了就该显示已接入");
        assert_eq!(c.skill_version.as_deref(), Some("9.9.9"), "读到的必须是自己那份的版本");
        assert!(!c.skill_outdated, "自己那份就是随包版本，不该报需更新");
        assert!(c.skill_path.ends_with(skills::SKILL_INSTALL_NAME), "路径要如实指向自己那份");

        // ④ 断开：只删自己那份，release 那份一个字节都不动
        uninstall_skill(claude, &home).expect("断开应当成功");
        assert!(!dst.exists(), "debug 自己那份要删掉");
        assert!(release_copy.is_dir(), "release 那份必须还在");
        assert_eq!(
            std::fs::read_to_string(release_copy.join("SKILL.md")).unwrap(),
            "release 装的，别动",
            "release 那份的内容不许被改动"
        );
        assert!(release_copy.join(skills::MARKER).is_file(), "release 那份的标记也不许动");

        // ⑤ 断开之后再看状态：显示未安装（而不是把 release 那份认成自己的）
        let c2 = scan_one(claude, &home, "", "9.9.9");
        assert!(!c2.skill_ready, "自己那份删了就该显示未安装，不许拿 release 那份冒充");
        assert!(c2.skill_version.is_none(), "不许读到 release 那份的版本");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// 只有 Cursor 走「改配置文件」，另外两家必须走官方 CLI。
    /// 这条锁住「不碰 .claude.json / config.toml」的承诺。
    #[test]
    fn only_cursor_writes_config_files() {
        for c in CLIENTS {
            match c.id {
                "cursor" => assert_eq!(c.approach, Approach::JsonFile),
                "claude" => assert_eq!(c.approach, Approach::ClaudeCli, "不许直接写 .claude.json"),
                "codex" => assert_eq!(c.approach, Approach::CodexCli, "不许直接写 config.toml"),
                other => panic!("新增了客户端 {other} 却没决定它的接入方式"),
            }
        }
    }

    #[test]
    fn unknown_client_is_rejected_with_the_list() {
        let e = find("notepad").expect_err("不支持的客户端要报错");
        assert!(e.contains("claude") && e.contains("codex") && e.contains("cursor"), "{e}");
    }

    /// 地址变了要能识别成 stale —— 「配了但连不上」比没配更迷惑人。
    #[test]
    fn stale_url_is_detected() {
        let dir = std::env::temp_dir().join("itools-aic-stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cursor")).expect("建目录");
        // 键名随构建走：debug 是 itools-dev、release 是 itools（见 mcp_config::SERVER_KEY）
        std::fs::write(
            dir.join(".cursor").join("mcp.json"),
            format!(
                r#"{{"mcpServers":{{"{}":{{"type":"http","url":"http://127.0.0.1:1111/mcp"}}}}}}"#,
                mcp_config::SERVER_KEY
            ),
        )
        .expect("写配置");

        let cursor = find("cursor").unwrap();
        let c = scan_one(cursor, &dir, "http://127.0.0.1:7345/mcp", "1.0.0");
        assert!(c.mcp_ready, "配过就算 ready");
        assert!(c.mcp_stale, "地址与当前监听不一致要标成 stale");
        assert!(c.note.is_some(), "要告诉用户怎么修");

        // 地址一致时不该误报
        let c2 = scan_one(cursor, &dir, "http://127.0.0.1:1111/mcp", "1.0.0");
        assert!(!c2.mcp_stale, "地址一致不该报 stale");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// debug 与 release 是两个不同端口的 MCP 服务器，AI 客户端的配置却只有一份。
    /// 各自**只认自己那一项**：配置里只有对方那一项时必须如实显示「未接入」，
    /// 既不能冒充成已接入（点开一看连的是另一个端口），也不能把对方判成自己的 stale
    /// ——一判 stale，用户就会去点「重新安装」，那一点就把对方的配置抢过来了。
    #[test]
    fn the_other_builds_entry_is_neither_ours_nor_stale() {
        let other = if cfg!(debug_assertions) { "itools" } else { "itools-dev" };
        assert_ne!(other, mcp_config::SERVER_KEY, "两种构建的 server 名必须不同");

        let dir = std::env::temp_dir().join("itools-aic-otherbuild");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cursor")).expect("建目录");
        std::fs::write(
            dir.join(".cursor").join("mcp.json"),
            format!(r#"{{"mcpServers":{{"{other}":{{"type":"http","url":"http://127.0.0.1:9999/mcp"}}}}}}"#),
        )
        .expect("写配置");

        let cursor = find("cursor").unwrap();
        let c = scan_one(cursor, &dir, "http://127.0.0.1:7345/mcp", "1.0.0");
        assert!(!c.mcp_ready, "只有对方构建那一项时，必须如实显示未接入");
        assert!(!c.mcp_stale, "对方的条目不该被当成我们的「过期」条目");
        assert!(!c.ready, "MCP 没接入就不能算就绪");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
