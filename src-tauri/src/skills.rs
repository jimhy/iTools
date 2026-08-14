//! **skill 一键安装**：把随包分发的 `itools-plugin-dev` skill 装进 AI 客户端的 skills 目录。
//!
//! # 它和 MCP 的分工
//!
//! MCP 解决「AI 能不能**驱动**开发者中心」（跑模拟器、读日志、提审），
//! skill 解决「AI 知不知道 iTools 插件**该怎么写**」。后者是纯知识，本来用户自己拷目录也行 ——
//! 但那要求他知道 `~/.claude/skills/` 这类约定，而且 iTools 升级后装过的 skill 会悄悄过期，
//! 没有任何地方会提醒他。做成一键安装，顺带就能显示「装的是哪个版本、要不要更新」。
//!
//! # 往别人的配置目录里写文件（诚信红线）
//!
//! 这是 iTools 唯一一处会写**其它程序配置目录**的功能，所以边界必须收得很死：
//!
//! - **只碰 `<客户端目录>/skills/itools-plugin-dev/` 这一个子目录**，绝不动同级的别的 skill；
//! - 安装时写一个 [`MARKER`] 标记文件，**它是覆盖与卸载的唯一凭据**：
//!   目录在、但没有标记 → 那是用户自己放的同名 skill，我们**既不覆盖也不删除**，
//!   如实告诉他「这个目录不是 iTools 装的，请自行处理」。宁可不干活，也不能删错东西；
//! - 状态里把**完整目标路径**给到前端显示，让用户点之前就知道会写到哪。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::logging::ilog;

/// skill 目录名。既是源目录名，也是我们在用户 skills 目录里的命名空间。
const SKILL_NAME: &str = "itools-plugin-dev";

/// 安装标记文件名。**覆盖与卸载的唯一凭据**，没有它我们就当那个目录不是自己的。
const MARKER: &str = ".installed-by-itools.json";

/// 支持的客户端：`(id, 展示名, 家目录下的配置目录名)`。
///
/// 两家吃的是同一份 `SKILL.md`（开放格式），所以源目录只有一份、装到两处。
const TARGETS: &[(&str, &str, &str)] = &[
    ("claude", "Claude Code", ".claude"),
    ("codex", "Codex CLI", ".codex"),
];

/// 安装标记的内容。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    /// 固定为 `iTools`。给人看，也免得把别的工具写的同名文件误认成自己的。
    installed_by: String,
    /// 安装时的 iTools 版本 —— 用它判断已装的 skill 有没有过期。
    version: String,
}

/// 一个目标客户端的安装状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillTarget {
    /// 客户端 id（`claude` / `codex`），调用安装/卸载时传它。
    pub id: String,
    /// 给用户看的名字。
    pub label: String,
    /// 完整目标路径。**面板上要显示出来**——写别人的目录，得让他先看见写到哪。
    pub dir: String,
    /// 家目录下有没有这个客户端的配置目录。没有多半是没装该客户端（仍允许安装）。
    pub client_detected: bool,
    /// 目标目录存不存在。
    pub installed: bool,
    /// 是不是 iTools 装的（有 [`MARKER`]）。`installed` 为真而这里为假 = 用户自己的同名 skill。
    pub managed: bool,
    /// 已装的版本（来自标记文件；非托管目录读不到，为 None）。
    pub installed_version: Option<String>,
    /// 已装版本与随包版本不一致 —— 该更新了。
    pub outdated: bool,
    /// 需要额外讲清楚的情况（如「目录已存在但不是 iTools 装的」）。
    pub note: Option<String>,
}

/// 全部目标的安装状态 + 源可用性。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsStatus {
    /// 随包的 skill 源目录在不在。**为假时一切安装按钮都该禁用**，而不是点了才报错。
    pub source_available: bool,
    /// 源不可用的真实原因。
    pub source_error: Option<String>,
    /// 随包 skill 的版本（= 当前 iTools 版本）。
    pub bundled_version: String,
    pub targets: Vec<SkillTarget>,
}

fn home() -> Result<PathBuf, String> {
    dirs::home_dir().ok_or_else(|| "找不到当前用户的家目录，无法定位 skills 目录".to_string())
}

/// 某客户端的 skill 安装目录：`~/<client>/skills/itools-plugin-dev`。
fn target_dir(home: &Path, client_dir: &str) -> PathBuf {
    home.join(client_dir).join("skills").join(SKILL_NAME)
}

/// 随包分发的 skill 源目录。
///
/// 与 [`crate::plugin::resolve_plugins_root`] 同一套双分支：dev 下用项目根的 `skills/`
/// （改完立刻能装，不用重新打包），打包后用 `resource_dir/skills`。
fn source_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(dev) = dev_source_dir() {
        return Ok(dev);
    }
    let res = app
        .path()
        .resource_dir()
        .map_err(|e| format!("拿不到程序资源目录：{e}"))?;
    let cand = res.join("skills").join(SKILL_NAME);
    if cand.is_dir() {
        return Ok(cand);
    }
    Err(format!(
        "安装包里没有找到 skill 源目录（{}）。这通常意味着构建时漏了 bundle.resources 配置。",
        cand.display()
    ))
}

/// dev 环境的源目录：从 exe 上溯找到含 `src-tauri` 的项目根，用它的 `skills/`。
///
/// 单独抽出来是为了能被测试直接跑——它是「面板上的安装按钮在 dev 下到底能不能用」的全部依据。
/// 打包后不会命中（安装目录里没有 `src-tauri`），那时走 `resource_dir`。
fn dev_source_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|anc| anc.join("src-tauri").is_dir())
        .map(|root| root.join("skills").join(SKILL_NAME))
        .filter(|p| p.is_dir())
}

/// 读安装标记。读不到 / 解析失败 / `installedBy` 不对，都当作「不是我们装的」——
/// 宁可少认一个自己的目录（顶多让用户手动删），也不能多认一个别人的（会删错东西）。
fn read_marker(dir: &Path) -> Option<Marker> {
    let raw = std::fs::read_to_string(dir.join(MARKER)).ok()?;
    let m: Marker = serde_json::from_str(&raw).ok()?;
    (m.installed_by == "iTools").then_some(m)
}

fn scan_target(home: &Path, id: &str, label: &str, client_dir: &str, bundled: &str) -> SkillTarget {
    let dir = target_dir(home, client_dir);
    let installed = dir.is_dir();
    let marker = if installed { read_marker(&dir) } else { None };
    let managed = marker.is_some();
    let installed_version = marker.map(|m| m.version);
    let outdated = managed && installed_version.as_deref() != Some(bundled);
    let note = if installed && !managed {
        Some(format!(
            "这个目录已经存在，但不是 iTools 装的（没有 {MARKER} 标记）。\
             为免覆盖你自己的文件，安装与卸载都不会碰它；确实要换成 iTools 版的话，请先手动删除或改名。"
        ))
    } else {
        None
    };
    SkillTarget {
        id: id.to_string(),
        label: label.to_string(),
        dir: dir.display().to_string(),
        client_detected: home.join(client_dir).is_dir(),
        installed,
        managed,
        installed_version,
        outdated,
        note,
    }
}

/// 当前状态（面板渲染 + 每次安装/卸载后回传）。
fn status_of(app: &AppHandle) -> Result<SkillsStatus, String> {
    let home = home()?;
    let bundled = env!("CARGO_PKG_VERSION");
    let (source_available, source_error) = match source_dir(app) {
        Ok(_) => (true, None),
        Err(e) => (false, Some(e)),
    };
    Ok(SkillsStatus {
        source_available,
        source_error,
        bundled_version: bundled.to_string(),
        targets: TARGETS
            .iter()
            .map(|(id, label, cdir)| scan_target(&home, id, label, cdir, bundled))
            .collect(),
    })
}

fn find_target(id: &str) -> Result<(&'static str, &'static str, &'static str), String> {
    TARGETS
        .iter()
        .find(|(tid, _, _)| *tid == id)
        .copied()
        .ok_or_else(|| {
            format!(
                "不认识的客户端「{id}」。支持的是：{}",
                TARGETS.iter().map(|(i, _, _)| *i).collect::<Vec<_>>().join(" / ")
            )
        })
}

/// 递归复制，**跳过源目录里可能存在的标记文件**（标记只应由本模块写）。
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == MARKER {
            continue;
        }
        let (from, to) = (entry.path(), dst.join(&name));
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 把 `src` 装到 `dst`（路径怎么来的不关心）。
///
/// 与命令层分开是为了**能被测试真正跑一遍**：命令层只做「id → 路径」的解析，
/// 所有会动文件的判断都在这里，测试因此可以覆盖完整的装 / 覆盖 / 拒绝流程。
fn install_to(src: &Path, dst: &Path, version: &str) -> Result<(), String> {
    if dst.is_dir() && read_marker(dst).is_none() {
        return Err(format!(
            "{} 已经存在，但不是 iTools 装的（没有 {MARKER} 标记）。\
             为免覆盖你自己的文件，这次没有做任何改动。要换成 iTools 版的，请先手动删除或改名。",
            dst.display()
        ));
    }
    // 先删再拷：只「缺啥补啥」的话，上个版本里已被删掉的 reference 文件会一直留着，
    // AI 就会读到早该消失的旧规范。此处要删的目录已确认带我们的标记。
    if dst.is_dir() {
        std::fs::remove_dir_all(dst).map_err(|e| format!("清理旧版本失败（{}）：{e}", dst.display()))?;
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建 skills 目录失败（{}）：{e}", parent.display()))?;
    }
    copy_tree(src, dst)
        .map_err(|e| format!("复制 skill 失败（{} → {}）：{e}", src.display(), dst.display()))?;

    let marker = Marker {
        installed_by: "iTools".to_string(),
        version: version.to_string(),
    };
    let raw = serde_json::to_string_pretty(&marker).map_err(|e| format!("生成安装标记失败：{e}"))?;
    // 标记写不进去就把刚拷的目录撤掉：留下一个「没有标记的 iTools 目录」，
    // 下次就会被自己判成「用户的目录」而再也装不上、也删不掉。
    if let Err(e) = std::fs::write(dst.join(MARKER), raw) {
        let _ = std::fs::remove_dir_all(dst);
        return Err(format!("写入安装标记失败，已撤销本次安装：{e}"));
    }
    Ok(())
}

/// 删掉 `dst`，但**只在它带我们的标记时**。
fn uninstall_at(dst: &Path) -> Result<(), String> {
    if !dst.is_dir() {
        return Err(format!("本来就没装（{}）。", dst.display()));
    }
    if read_marker(dst).is_none() {
        return Err(format!(
            "拒绝删除 {}：它没有 {MARKER} 标记，说明不是 iTools 装的。请自行确认后手动删除。",
            dst.display()
        ));
    }
    std::fs::remove_dir_all(dst).map_err(|e| format!("删除失败（{}）：{e}", dst.display()))
}

// ==================== 命令 ====================

/// 命令：读 skill 安装状态。
#[tauri::command]
pub fn skills_status(app: AppHandle) -> Result<SkillsStatus, String> {
    status_of(&app)
}

/// 命令：把 skill 装到指定客户端（已装则整目录替换成随包版本）。
///
/// 目标存在但**没有**我们的标记时**直接拒绝**，不覆盖：那是用户自己的同名 skill。
#[tauri::command]
pub fn skills_install(app: AppHandle, target: String) -> Result<SkillsStatus, String> {
    let (_, label, client_dir) = find_target(&target)?;
    let src = source_dir(&app)?;
    let home = home()?;
    let dst = target_dir(&home, client_dir);

    install_to(&src, &dst, env!("CARGO_PKG_VERSION")).map_err(|e| format!("{label}：{e}"))?;
    ilog!("[iTools] 已安装 skill 到 {}", dst.display());
    status_of(&app)
}

/// 命令：卸载（只删**带我们标记**的目录）。
#[tauri::command]
pub fn skills_uninstall(app: AppHandle, target: String) -> Result<SkillsStatus, String> {
    let (_, label, client_dir) = find_target(&target)?;
    let home = home()?;
    let dst = target_dir(&home, client_dir);

    uninstall_at(&dst).map_err(|e| format!("{label}：{e}"))?;
    ilog!("[iTools] 已卸载 skill：{}", dst.display());
    status_of(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 卸载的护栏：没有标记的目录**一律不认**，哪怕它长得再像我们装的。
    #[test]
    fn unmarked_dir_is_never_ours() {
        let tmp = std::env::temp_dir().join("itools-skill-test-unmarked");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("建临时目录");
        std::fs::write(tmp.join("SKILL.md"), "# 用户自己写的").expect("写文件");
        assert!(read_marker(&tmp).is_none(), "没有标记文件就不能认作 iTools 装的");

        // 标记内容不对（别的工具写了同名文件）同样不认
        std::fs::write(tmp.join(MARKER), r#"{"installedBy":"别的工具","version":"1.0.0"}"#).expect("写标记");
        assert!(read_marker(&tmp).is_none(), "installedBy 不是 iTools 就不能认");

        // 正确的标记才认
        std::fs::write(tmp.join(MARKER), r#"{"installedBy":"iTools","version":"9.9.9"}"#).expect("写标记");
        assert_eq!(read_marker(&tmp).map(|m| m.version).as_deref(), Some("9.9.9"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 坏掉的标记文件不能让整个状态读取崩掉。
    #[test]
    fn corrupt_marker_degrades_to_unmanaged() {
        let tmp = std::env::temp_dir().join("itools-skill-test-corrupt");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("建临时目录");
        std::fs::write(tmp.join(MARKER), "{ 这不是 json").expect("写坏标记");
        assert!(read_marker(&tmp).is_none(), "解析失败要退化成「不是我们的」，不能 panic");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// 目标路径必须落在 `<客户端>/skills/<命名空间>` 下，不能越出去。
    #[test]
    fn target_path_stays_in_its_namespace() {
        let home = PathBuf::from("C:\\Users\\someone");
        for (_, _, cdir) in TARGETS {
            let p = target_dir(&home, cdir);
            assert!(p.starts_with(&home), "目标必须在家目录内");
            assert!(p.ends_with(SKILL_NAME), "目标必须以命名空间目录结尾，不能是 skills 根");
            assert_eq!(p.parent().and_then(|x| x.file_name()), Some("skills".as_ref()));
        }
    }

    /// 复制时不能把源目录里混进来的标记文件也带过去（标记只能由安装流程写）。
    #[test]
    fn copy_skips_stray_marker() {
        let base = std::env::temp_dir().join("itools-skill-test-copy");
        let (src, dst) = (base.join("src"), base.join("dst"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("references")).expect("建源目录");
        std::fs::write(src.join("SKILL.md"), "# skill").expect("写 SKILL.md");
        std::fs::write(src.join("references").join("a.md"), "ref").expect("写 reference");
        std::fs::write(src.join(MARKER), r#"{"installedBy":"iTools","version":"0.0.1"}"#).expect("写混进来的标记");

        copy_tree(&src, &dst).expect("复制应当成功");
        assert!(dst.join("SKILL.md").is_file(), "SKILL.md 要拷过去");
        assert!(dst.join("references").join("a.md").is_file(), "子目录要递归拷过去");
        assert!(!dst.join(MARKER).exists(), "源目录里的标记文件不能被带过去");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// dev 环境必须能定位到仓库里的 skill 源目录 —— 否则开发机上那个「安装」按钮是废的。
    ///
    /// 顺带体检源目录本身：`SKILL.md` 是 skill 的入口，缺了它装过去也不会被任何客户端加载。
    #[test]
    fn dev_source_dir_finds_the_repo_skill() {
        let p = dev_source_dir().expect("开发环境应当能定位到仓库的 skills/itools-plugin-dev");
        assert!(p.join("SKILL.md").is_file(), "源目录里必须有 SKILL.md：{}", p.display());
        assert!(p.ends_with(SKILL_NAME), "定位到的必须是 skill 自己的目录：{}", p.display());
    }

    /// 端到端跑一遍真实的装 → 覆盖 → 卸载，全部落到真实文件系统上。
    #[test]
    fn install_overwrite_uninstall_roundtrip() {
        let base = std::env::temp_dir().join("itools-skill-test-roundtrip");
        let (src, dst) = (base.join("src"), base.join("home/.claude/skills/itools-plugin-dev"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("references")).expect("建源目录");
        std::fs::write(src.join("SKILL.md"), "v1").expect("写 SKILL.md");
        std::fs::write(src.join("references").join("old.md"), "旧文件").expect("写旧 reference");

        // ① 首次安装：父目录不存在也要能建出来
        install_to(&src, &dst, "1.0.0").expect("首次安装应当成功");
        assert_eq!(std::fs::read_to_string(dst.join("SKILL.md")).unwrap(), "v1");
        assert_eq!(read_marker(&dst).map(|m| m.version).as_deref(), Some("1.0.0"));

        // ② 覆盖安装：源里删掉的文件**不能**在目标里残留（否则 AI 会读到早该消失的旧规范）
        std::fs::remove_file(src.join("references").join("old.md")).expect("删源里的旧文件");
        std::fs::write(src.join("references").join("new.md"), "新文件").expect("写新 reference");
        std::fs::write(src.join("SKILL.md"), "v2").expect("更新 SKILL.md");
        install_to(&src, &dst, "2.0.0").expect("覆盖安装应当成功");
        assert_eq!(std::fs::read_to_string(dst.join("SKILL.md")).unwrap(), "v2");
        assert!(dst.join("references").join("new.md").is_file(), "新文件要装进去");
        assert!(!dst.join("references").join("old.md").exists(), "源里已删的文件不能残留");
        assert_eq!(read_marker(&dst).map(|m| m.version).as_deref(), Some("2.0.0"));

        // ③ 卸载
        uninstall_at(&dst).expect("卸载应当成功");
        assert!(!dst.exists(), "卸载后目录应当消失");
        assert!(uninstall_at(&dst).is_err(), "重复卸载要如实报错，不能假装成功");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 状态字段的四种组合必须自洽 —— 面板完全按它们决定摆不摆按钮、摆哪个，
    /// 这里错一个字段，界面上就会出现一个「点了必然失败」的控件。
    #[test]
    fn scan_reports_managed_outdated_and_note_consistently() {
        let base = std::env::temp_dir().join("itools-skill-test-scan");
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let dir = target_dir(&home, ".claude");
        let scan = || scan_target(&home, "claude", "Claude Code", ".claude", "1.2.0");

        // ① 没装
        let t = scan();
        assert!(!t.installed && !t.managed && !t.outdated, "没装时三个标志都该是 false");
        assert!(t.note.is_none(), "没装不需要额外说明");
        assert!(!t.client_detected, "家目录里没有 .claude 就该如实说没检测到");

        // ② 装了、版本一致
        std::fs::create_dir_all(&dir).expect("建目录");
        std::fs::write(dir.join(MARKER), r#"{"installedBy":"iTools","version":"1.2.0"}"#).expect("写标记");
        let t = scan();
        assert!(t.installed && t.managed, "带标记的目录是我们的");
        assert!(!t.outdated, "同版本不该报「可更新」");
        assert!(t.client_detected, "目录建出来了就该检测到");

        // ③ 装了、版本落后
        std::fs::write(dir.join(MARKER), r#"{"installedBy":"iTools","version":"1.0.0"}"#).expect("写旧标记");
        let t = scan();
        assert!(t.outdated, "版本不一致要报「可更新」");
        assert_eq!(t.installed_version.as_deref(), Some("1.0.0"));

        // ④ 目录在、但不是我们装的
        std::fs::remove_file(dir.join(MARKER)).expect("删标记");
        let t = scan();
        assert!(t.installed && !t.managed, "没标记就不是我们的");
        assert!(!t.outdated, "不是我们的就谈不上「可更新」");
        assert!(t.installed_version.is_none(), "读不到版本就别编一个");
        assert!(t.note.is_some(), "非托管必须给出说明——面板靠它决定不摆按钮");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 用户自己放的同名 skill：既不覆盖也不删除，且**原文件必须原封不动**。
    #[test]
    fn refuses_to_touch_a_dir_we_did_not_install() {
        let base = std::env::temp_dir().join("itools-skill-test-foreign");
        let (src, dst) = (base.join("src"), base.join("dst"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&src).expect("建源目录");
        std::fs::write(src.join("SKILL.md"), "iTools 版").expect("写源");
        std::fs::create_dir_all(&dst).expect("建目标目录");
        std::fs::write(dst.join("SKILL.md"), "用户自己写的，别动").expect("写用户文件");

        let e = install_to(&src, &dst, "1.0.0").expect_err("没有标记就不该覆盖");
        assert!(e.contains("不是 iTools 装的"), "要说清楚为什么拒绝：{e}");
        let e2 = uninstall_at(&dst).expect_err("没有标记就不该删除");
        assert!(e2.contains("拒绝删除"), "要说清楚为什么拒绝：{e2}");

        assert_eq!(
            std::fs::read_to_string(dst.join("SKILL.md")).unwrap(),
            "用户自己写的，别动",
            "被拒绝的两次操作都不许改动用户的文件"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unknown_target_is_rejected_with_the_valid_list() {
        let e = find_target("cursor").expect_err("不支持的客户端要报错");
        assert!(e.contains("claude") && e.contains("codex"), "错误里要给出支持的清单：{e}");
    }
}
