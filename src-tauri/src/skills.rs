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
//! - **只碰 `<客户端目录>/skills/<本构建的命名空间>/` 这一个子目录**（release 是
//!   `itools-plugin-dev`，debug 是 `itools-plugin-dev-dev`，见 [`SKILL_INSTALL_NAME`]），
//!   绝不动同级的别的 skill——**包括另一种构建装的那份**；
//! - 安装时写一个 [`MARKER`] 标记文件，**它是覆盖与卸载的唯一凭据**：
//!   目录在、但没有标记 → 那是用户自己放的同名 skill，我们**既不覆盖也不删除**，
//!   如实告诉他「这个目录不是 iTools 装的，请自行处理」。宁可不干活，也不能删错东西；
//! - 状态里把**完整目标路径**给到前端显示，让用户点之前就知道会写到哪。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 随包 skill 的**源**目录名：仓库里 `skills/` 下那个目录，也是安装包 `resources` 里的那个目录
/// （`tauri.conf.json` 的 `"../skills": "skills"`）。
///
/// 它**不分构建**：两种构建吃的是同一份源。跟着仓库目录名走，改名要连仓库目录一起改。
pub(crate) const SKILL_SOURCE_NAME: &str = "itools-plugin-dev";

/// release 装进用户 skills 目录时占用的目录名。**冻结值，一个字符都不能改**。
///
/// 已装用户的 `~/.claude/skills/` 下就是这个目录，里面还有我们写的 [`MARKER`]。
/// 改掉它 = 旧那份再也不会被我们识别、更新或卸载：面板显示「未安装」，
/// 而用户的 AI 那边永远留着一份我们再也不会去动的、越来越旧的规范。
const RELEASE_INSTALL_NAME: &str = "itools-plugin-dev";

/// debug 装进用户 skills 目录时占用的目录名。
///
/// # 为什么必须和 release 分开（这是一次真事故的复盘）
///
/// 用户日常跑的是 release，开发者在同一台机器上编 debug。命名空间不分家的话，
/// 两个构建指着**同一个目录**，于是：
///
/// - **会删用户的数据**：debug 面板上点一次「断开接入」→ [`uninstall_at`] →
///   `remove_dir_all` 把 release 装的那份删了。用户的 AI 从此不再会写 iTools 插件，
///   而他毫不知情——面板上什么提示都不会有，因为在 debug 看来它删的是「自己那份」。
/// - **版本来回跳**：工作区版本号通常已经 bump 过，debug 一装就把 marker 写成新版本，
///   release 面板立刻报「需更新」；用户点一下装回旧版，debug 面板又变成「需更新」。
///   两个构建为一个 marker 来回拉锯，和 MCP 那边的 `mcpStale` 是同一个病
///   （见 [`crate::mcp_config::SERVER_KEY`]）。
/// - **未发布内容外溢**：debug 的源是**工作区**的 `skills/`（见 [`dev_source_dir`]），
///   点一次「接入」就把开发中、还没发布的规范覆盖到用户那份上，
///   用户的 AI 于是照着一份未发布的规范写插件。
///
/// # 为什么叫 `itools-plugin-dev-dev`（是的，读着别扭）
///
/// 全仓的 debug 命名统一是「release 名 + `-dev` 后缀」：数据根 `itools` → `itools-dev`
/// （[`crate::paths`]）、MCP server 名 `itools` → `itools-dev`（[`crate::mcp_config`]）。
/// 源名本身恰好以 `-dev` 结尾（那个 dev 指的是 *plugin **dev**elopment*，与构建类型无关），
/// 叠起来就成了双 `-dev`。宁可难看也不为它单开一条命名规则：
/// 换个好看的名字（`itools-plugin-dev-debug` 之类）就等于让后来人多记一条例外，
/// 而这个名字只出现在开发机上，看它的只有开发者自己。
const DEBUG_INSTALL_NAME: &str = "itools-plugin-dev-dev";

/// 本次构建在用户 skills 目录里占用的命名空间。**安装、覆盖、状态检测、卸载都只认它**。
///
/// 写法（`const` + `cfg!`）与 [`crate::mcp_config::SERVER_KEY`] 保持一致：两个取值同屏可见，
/// `cfg!` 展开成 bool 字面量、`if` 在常量上下文里可求值，仍是编译期定死的 `&'static str`。
pub(crate) const SKILL_INSTALL_NAME: &str = if cfg!(debug_assertions) {
    DEBUG_INSTALL_NAME
} else {
    RELEASE_INSTALL_NAME
};

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
    let cand = res.join("skills").join(SKILL_SOURCE_NAME);
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
        .map(|root| root.join("skills").join(SKILL_SOURCE_NAME))
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
    // 装到别名目录（debug）时，SKILL.md 里的 `name:` 得跟着改，否则客户端多半不加载它，
    // 面板却显示「已装」——那就是骗人。失败按整次安装失败处理（理由见函数内注释）。
    if let Err(e) = align_skill_name(dst) {
        let _ = std::fs::remove_dir_all(dst);
        return Err(format!("{e}（已撤销本次安装）"));
    }

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

/// 把装好的 `SKILL.md` 里 front matter 的 `name:` 对齐成**安装目录名**。
///
/// # 为什么需要这一步
///
/// skill 的约定是「目录名 = front matter 里的 `name`」，客户端按这个名字登记 skill。
/// release 装到 `itools-plugin-dev/`，两者天然一致，这个函数是**空操作**；
/// debug 装到 `itools-plugin-dev-dev/`，源里的 `name: itools-plugin-dev` 就对不上了：
/// 轻则客户端按目录名登记、front matter 名字沦为噪音，重则直接判成不合法而跳过加载，
/// 更糟的是它与 release 那份**重名**，两份 skill 抢同一个名字。
/// 而面板那边只看我们自己写的 [`MARKER`]，照样显示「SKILL 已接入」——
/// 于是「装了但 AI 根本没加载」这件事没有任何地方会说出来。所以拷完就把名字改准。
///
/// 只改 front matter 里那一行 `name:`，`description` 与正文一个字都不动。
/// 没有 front matter / 没有 `name:` / 名字本来就对，都直接返回 `Ok` 且不写盘。
fn align_skill_name(dst: &Path) -> Result<(), String> {
    let Some(want) = dst.file_name().and_then(|s| s.to_str()) else {
        return Ok(());
    };
    let path = dst.join("SKILL.md");
    // 读不出来（没有这个文件、不是 UTF-8）不在本函数的职责内：源目录长什么样由
    // source_dir 那边负责，这里只做「能改就改准」，读不到就当没有可改的。
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Some(patched) = rewrite_front_matter_name(&raw, want) else {
        return Ok(());
    };
    // 写失败才报错：那说明目标目录已经处于半成品状态（内容是源的、名字却没改过来），
    // 与其留一份可能不被加载的 skill，不如撤销重来。
    std::fs::write(&path, patched).map_err(|e| format!("改写 {} 的 name 失败：{e}", path.display()))
}

/// [`align_skill_name`] 的纯字符串部分（抽出来是为了能被测试直接钉住边界情况）。
///
/// 返回 `None` = 不需要改（没 front matter、没 `name:`、或名字已经是 `want`）。
/// 只认「文件第一行就是 `---`」的标准写法，且在遇到闭合的 `---` 时停下——
/// 绝不去碰正文里可能出现的 `name:`（`plugin.json` 的示例里就有一堆）。
fn rewrite_front_matter_name(raw: &str, want: &str) -> Option<String> {
    // split_inclusive 保留各行原本的换行符：CRLF 的文件不会被我们悄悄改成 LF。
    let lines: Vec<&str> = raw.split_inclusive('\n').collect();
    if lines.first()?.trim_end() != "---" {
        return None;
    }
    for (i, line) in lines.iter().enumerate().skip(1) {
        let trimmed = line.trim_end();
        if trimmed == "---" {
            return None; // front matter 结束了都没见到 name:
        }
        let Some(value) = trimmed.strip_prefix("name:") else {
            continue;
        };
        if value.trim() == want {
            return None; // 本来就对（release 走的就是这条）
        }
        let eol = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            "" // 文件正好在这一行结束（没有末尾换行）
        };
        let mut out = String::with_capacity(raw.len() + want.len());
        out.push_str(&lines[..i].concat());
        out.push_str("name: ");
        out.push_str(want);
        out.push_str(eol);
        out.push_str(&lines[i + 1..].concat());
        return Some(out);
    }
    None
}

/// 删掉 `dst`，但**只在它带我们的标记时**。
///
/// `dst` 由调用方按 [`SKILL_INSTALL_NAME`] 拼出来（见 [`crate::ai_clients`]），
/// 所以 debug 构建能删到的最多只有 `itools-plugin-dev-dev`——
/// **用户正式版装的 `itools-plugin-dev` 这个函数根本收不到那个路径**。
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
        assert!(
            p.ends_with(SKILL_SOURCE_NAME),
            "定位到的必须是 skill 源目录（源名不随构建变，debug 也读这一份）：{}",
            p.display()
        );
    }

    /// release 的安装目录名冻结：这条断言就是那道闸门。
    ///
    /// 谁把 `RELEASE_INSTALL_NAME` 改了，测试立刻红——因为那等于让已装用户那份
    /// skill 变成「不是 iTools 装的」：更新不到、卸载不掉，面板还显示未安装。
    /// 源目录名一并钉住：它同时是仓库目录名和 `bundle.resources` 里的路径，
    /// 改了会让打包后的 [`source_dir`] 直接找不到源。
    #[test]
    fn release_install_name_is_frozen() {
        assert_eq!(
            RELEASE_INSTALL_NAME, "itools-plugin-dev",
            "release 的 skill 安装目录名不可更改：改了 = 已装用户那份既更新不到也卸载不掉"
        );
        assert_eq!(
            SKILL_SOURCE_NAME, "itools-plugin-dev",
            "源目录名要和仓库里的 skills/itools-plugin-dev、以及 tauri.conf.json 的 resources 对得上"
        );
    }

    /// 两种构建**必须**装进不同的目录。撞在一起的后果见 [`DEBUG_INSTALL_NAME`] 的注释：
    /// debug 点一次「断开」就把用户正式版那份删了。
    #[test]
    fn debug_and_release_install_into_different_dirs() {
        assert_ne!(
            RELEASE_INSTALL_NAME, DEBUG_INSTALL_NAME,
            "debug 与 release 共用 skill 目录 = debug 的卸载会删掉用户正式版那份"
        );
        // 本次构建用的是它自己那个，没有第三种可能
        let expected = if cfg!(debug_assertions) { DEBUG_INSTALL_NAME } else { RELEASE_INSTALL_NAME };
        assert_eq!(SKILL_INSTALL_NAME, expected);
    }

    /// debug 构建的落点绝不能等于 release 的那个目录名。
    ///
    /// 加 `#[cfg(debug_assertions)]`：`cargo test --release` 下 `SKILL_INSTALL_NAME`
    /// 本就该是 release 名，那时这条断言不成立也不该跑（与 [`crate::paths`] 的测试同款处理）。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_install_name_is_isolated() {
        assert_eq!(SKILL_INSTALL_NAME, DEBUG_INSTALL_NAME);
        assert_ne!(SKILL_INSTALL_NAME, RELEASE_INSTALL_NAME);
    }

    /// 装到别名目录时，front matter 的 `name:` 要跟着改成目录名；
    /// release（目录名 = 源名）则必须**一个字节都不改**。
    #[test]
    fn front_matter_name_follows_the_install_dir() {
        let raw = "---\nname: itools-plugin-dev\ndescription: 写 iTools 插件\n---\n\n# 正文\n\n```json\n{ \"name\": \"my-plugin\" }\n```\n";

        // ① 目录名不同 → 只改那一行
        let out = rewrite_front_matter_name(raw, "itools-plugin-dev-dev").expect("名字不同就该改");
        assert!(out.starts_with("---\nname: itools-plugin-dev-dev\ndescription: 写 iTools 插件\n---\n"));
        assert!(out.contains("{ \"name\": \"my-plugin\" }"), "正文里的 name 不许动");
        assert_eq!(out.matches("itools-plugin-dev-dev").count(), 1);

        // ② 名字本来就对（release 走这条）→ 不改、不写盘
        assert!(rewrite_front_matter_name(raw, "itools-plugin-dev").is_none());

        // ③ 没有 front matter / front matter 里没有 name → 一律不动
        assert!(rewrite_front_matter_name("# 只是一篇 markdown\nname: 别动我\n", "x").is_none());
        assert!(rewrite_front_matter_name("---\ndescription: 没写 name\n---\nname: 正文里的\n", "x").is_none());

        // ④ CRLF 的文件不能被顺手改成 LF
        let crlf = rewrite_front_matter_name("---\r\nname: a\r\n---\r\n正文\r\n", "b").expect("该改");
        assert_eq!(crlf, "---\r\nname: b\r\n---\r\n正文\r\n");
    }

    /// 走真实文件系统验一遍：装完之后 `SKILL.md` 的 `name` 必须等于安装目录名。
    #[test]
    fn install_aligns_skill_name_with_dir() {
        let base = std::env::temp_dir().join("itools-skill-test-align");
        let (src, dst) = (base.join("src"), base.join("dst").join(SKILL_INSTALL_NAME));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&src).expect("建源目录");
        std::fs::write(src.join("SKILL.md"), "---\nname: itools-plugin-dev\n---\n正文\n").expect("写源");

        install_to(&src, &dst, "1.0.0").expect("安装应当成功");
        let installed = std::fs::read_to_string(dst.join("SKILL.md")).expect("读装好的 SKILL.md");
        assert!(
            installed.contains(&format!("name: {SKILL_INSTALL_NAME}\n")),
            "装好的 SKILL.md 的 name 必须等于安装目录名，实际：{installed}"
        );
        assert!(installed.contains("正文"), "正文不能丢");
        // 源文件本身一个字节都不许动（它是仓库里的文件）
        assert_eq!(
            std::fs::read_to_string(src.join("SKILL.md")).unwrap(),
            "---\nname: itools-plugin-dev\n---\n正文\n",
            "只改装到目标目录的那份，源目录是仓库文件，绝不能被改"
        );
        let _ = std::fs::remove_dir_all(&base);
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
