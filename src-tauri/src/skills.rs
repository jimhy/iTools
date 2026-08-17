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

/// skill 目录名。既是源目录名，也是我们在用户 skills 目录里的命名空间。
pub(crate) const SKILL_NAME: &str = "itools-plugin-dev";

/// 安装标记文件名。**覆盖与卸载的唯一凭据**，没有它我们就当那个目录不是自己的。
pub(crate) const MARKER: &str = ".installed-by-itools.json";

/// 安装标记的内容。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    /// 固定为 `iTools`。给人看，也免得把别的工具写的同名文件误认成自己的。
    installed_by: String,
    /// 安装时的 iTools 版本 —— 用它判断已装的 skill 有没有过期。
    version: String,
}

/// 随包分发的 skill 源目录。
///
/// 与 [`crate::plugin::resolve_plugins_root`] 同一套双分支：dev 下用项目根的 `skills/`
/// （改完立刻能装，不用重新打包），打包后用 `resource_dir/skills`。
pub(crate) fn source_dir(app: &AppHandle) -> Result<PathBuf, String> {
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
pub(crate) fn dev_source_dir() -> Option<PathBuf> {
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

/// 某个 skill 目录里装的是哪个版本（**只认带我们标记的**，别人放的返回 None）。
///
/// 给 [`crate::ai_clients`] 用：它按客户端组织状态，不需要整个 `SkillTarget`。
pub(crate) fn installed_version(dir: &Path) -> Option<String> {
    read_marker(dir).map(|m| m.version)
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
pub(crate) fn install_to(src: &Path, dst: &Path, version: &str) -> Result<(), String> {
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
pub(crate) fn uninstall_at(dst: &Path) -> Result<(), String> {
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
}
