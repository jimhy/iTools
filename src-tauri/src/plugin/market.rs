//! 插件市场：索引拉取与**内容哈希**校验。
//!
//! # 内容哈希是这条信任链的收口
//!
//! 前几轮做了「第三方镜像竞速」来解决 GitHub 国内可达性问题，但镜像由第三方运营，
//! 技术上有能力篡改传输内容——这条风险此前只能靠**披露**，没法消除。
//!
//! 市场收录的插件带上了审核时记录的内容哈希之后才真正闭环：不管插件包经由官方源还是
//! 任何镜像下载，解压后算一遍哈希，对不上一律拒绝安装。镜像即使被控制，也只能让你**装不上**，
//! 不能让你**装到被改过的代码**。
//!
//! # 为什么校验的是「内容」而不是 zip 本身
//!
//! GitHub 的归档**不是字节级确定的**：同一个 commit 在不同时间下载，zip 可能因压缩实现变化
//! 而哈希不同（2023 年 GitHub 更换压缩库时，全网依赖归档 checksum 的工具集体失效过一次）。
//! 用 zip 的 sha256 当依据，早晚会变成「市场里所有插件突然全部校验失败」。
//!
//! 所以只对**解压后的文件内容**算哈希，与压缩方式、时间戳、条目顺序全都无关。
//!
//! # 算法（与 `scripts/registry/hash.mjs` 必须逐字一致）
//!
//! ```text
//! 对插件目录下每个【文件】（目录本身不算）：
//!     rel  = 相对插件目录的路径，分隔符固定 '/'
//!     line = rel + "\t" + hex(sha256(文件字节))
//! 所有 line 按 rel 的 UTF-8 字节序升序排序
//! content_hash = "sha256:" + hex(sha256(utf8(lines.join("\n"))))
//! ```
//!
//! 排序用**字节序**是为了跨语言一致：Rust 的 `String` 排序天然按 UTF-8 字节比较，
//! 而 JS 的 `Array.sort()` 默认按 UTF-16 code unit 比较——纯 ASCII 路径下两者一致，
//! 但插件里只要有一个中文文件名就会分道扬镳，导致该插件永远校验失败。
//! `hash.mjs` 那边显式用 `Buffer.compare` 对齐了本实现，两边有同一组基准用例钉死
//! （见本文件测试 `content_hash_matches_node_golden`）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::logging::ilog;

use super::mirror;

/// 索引坐标的环境变量覆盖，形如 `owner/repo@ref:path`。
///
/// 按项目红线「服务端地址可配置、不写死」：默认值指向本仓库的 `registry/`，
/// 但任何人都能把客户端指向自己的索引（自建市场 / 内部分发 / 联调）。
const REGISTRY_ENV: &str = "ITOOLS_REGISTRY";
const DEFAULT_OWNER: &str = "jimhy";
const DEFAULT_REPO: &str = "iTools";
const DEFAULT_REF: &str = "main";
const DEFAULT_PATH: &str = "registry/index.json";

/// 索引缓存文件名（放在 `%LOCALAPPDATA%\itools\` 下，与镜像配置缓存同级）。
const CACHE_FILE: &str = "market-index.json";
/// 索引拉取超时与体积上限。索引是纯文本 JSON，2MB 足够放几千个条目。
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;

// ==================== 索引 ====================

/// 市场索引里的一个插件条目，与 `registry/schema/entry.schema.json` 对应。
///
/// 除 `contentHash` / `fileCount` 由 CI 计算回填外，其余字段由收录条目原样带过来。
/// **未知字段一律保留**（`#[serde(default)]` + 宽松解析）：索引是服务端演进的，
/// 老客户端遇到新字段不该崩，遇到缺字段也不该崩。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketEntry {
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    /// 安装源仓库，形如 `https://github.com/owner/repo`。
    pub repo: String,
    /// 仓库内子目录，空串 = 仓库根。
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub version: String,
    /// 审核时的完整 40 位 commit sha —— 客户端**只装这个确切 commit**。
    pub revision: String,
    /// 审核时算出的内容哈希（`sha256:…`）。空串表示索引里没给，
    /// 此时安装会**退化为不校验**并如实告知用户，绝不假装校验过。
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub permission_reasons: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub min_app_version: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub source_repo: String,
    #[serde(default)]
    pub screenshots: Vec<String>,
    /// 已吊销：客户端应警告并禁用，`revokedReason` 会原样展示给用户。
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub revoked_reason: String,
    #[serde(default)]
    pub added_at: String,
    #[serde(default)]
    pub audit_report: String,
    #[serde(default)]
    pub file_count: usize,
}

impl MarketEntry {
    /// 拼出可直接交给现有安装流程的 URL（与手工粘贴的形态完全一致）。
    ///
    /// 固定带 `#<40 位 sha>`，因此客户端装到的**必然**是审核过的那个 commit；
    /// 这也让它在插件列表里天然显示为「已锁定」，不会被误当成可自动跟随分支更新。
    pub fn install_url(&self) -> String {
        let mut u = self.repo.clone();
        if !self.path.is_empty() {
            u.push_str("?path=");
            u.push_str(&self.path);
        }
        u.push('#');
        u.push_str(&self.revision);
        u
    }
}

/// 索引文件的顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketIndex {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub plugins: Vec<MarketEntry>,
}

/// 索引的来源坐标（可被环境变量整体覆盖）。
struct Coord {
    owner: String,
    repo: String,
    git_ref: String,
    path: String,
}

/// 解析 `ITOOLS_REGISTRY`（`owner/repo@ref:path`），任一段缺失或格式不对就整体回落默认值。
///
/// 刻意**不做部分覆盖**：半个自定义坐标比默认值更难排查（用户以为指向了自己的索引，
/// 实际只改了 owner）。要么整条生效，要么完全不生效并写日志。
fn coord() -> Coord {
    let fallback = || Coord {
        owner: DEFAULT_OWNER.into(),
        repo: DEFAULT_REPO.into(),
        git_ref: DEFAULT_REF.into(),
        path: DEFAULT_PATH.into(),
    };
    let Ok(raw) = std::env::var(REGISTRY_ENV) else {
        return fallback();
    };
    let raw = raw.trim();
    let parsed = (|| {
        let (repo_part, path) = raw.split_once(':')?;
        let (owner_repo, git_ref) = repo_part.split_once('@')?;
        let (owner, repo) = owner_repo.split_once('/')?;
        let ok = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
        (ok(owner) && ok(repo) && ok(git_ref) && ok(path)).then(|| Coord {
            owner: owner.into(),
            repo: repo.into(),
            git_ref: git_ref.into(),
            path: path.into(),
        })
    })();
    match parsed {
        Some(c) => c,
        None => {
            ilog!("[iTools] {REGISTRY_ENV} 格式不对（应为 owner/repo@ref:path），已回落默认索引");
            fallback()
        }
    }
}

fn cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("itools").join(CACHE_FILE))
}

/// 从远端拉取索引。走 [`mirror::fetch`]，因此自动享受「官方抢跑 → 镜像兜底」与代理设置：
/// 拉索引本身在国内同样会被 GitHub 可达性拖住，没理由让它比装插件更难。
///
/// **索引不做哈希校验**——它就是信任的起点（哈希从它那里来），
/// 拿它自己校验自己没有意义。它的可信度由 HTTPS 与仓库归属保证。
pub fn fetch_index() -> Result<MarketIndex, String> {
    let c = coord();
    let official_url = format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        c.owner, c.repo, c.git_ref, c.path
    );
    let req = mirror::Request {
        kind: mirror::Kind::Raw,
        official_url,
        official_label: mirror::official_label(mirror::OFFICIAL_HOST),
        github: Some(mirror::GhCoord {
            owner: c.owner,
            repo: c.repo,
            git_ref: c.git_ref,
            path: c.path,
        }),
        auth: None,
        timeout: FETCH_TIMEOUT,
        max_bytes: MAX_INDEX_BYTES,
        what: "拉取插件市场索引".to_string(),
        expect_sha256: None,
    };
    let got = mirror::fetch(&req)?;
    let index: MarketIndex = serde_json::from_slice(&got.bytes)
        .map_err(|e| format!("插件市场索引解析失败: {e}（索引文件可能损坏或不是预期格式）"))?;
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 缓存写失败不影响本次结果：内存里已经有索引了，下次重新拉一遍即可。
        if let Err(e) = std::fs::write(&p, &got.bytes) {
            ilog!("[iTools] 市场索引缓存写入失败（不影响本次使用）: {e}");
        }
    }
    Ok(index)
}

/// 读本地缓存的索引（拉取失败时的兜底）。没有缓存或解析失败都返回 None。
pub fn cached_index() -> Option<MarketIndex> {
    let p = cache_path()?;
    let bytes = std::fs::read(p).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ==================== 命令 ====================

/// 「插件市场」页的一次拉取结果。
///
/// `origin` 如实说明这批条目是**刚从远端拉的**还是**本地缓存**，`error` 是拉取失败的真实原因。
/// 两者都要给 UI ——「拿着三天前的缓存」和「刚更新过」对用户是不同的信息，
/// 把失败悄悄吞掉、只展示缓存，就是在假装一切正常。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketView {
    pub plugins: Vec<MarketEntry>,
    /// "remote" = 本次成功拉到；"cache" = 拉取失败，用的本地缓存；"none" = 两者都没有。
    pub origin: String,
    /// 拉取失败的真实原因（成功时为 None）。
    pub error: Option<String>,
    /// 当前已安装的插件名 → 版本，供 UI 标注「已安装 / 可更新」。
    pub installed: std::collections::HashMap<String, String>,
    /// 索引来源的可读描述（如 `jimhy/iTools@main:registry/index.json`），便于用户确认自己连的是哪个市场。
    pub source: String,
}

/// 命令：拉取市场列表。
///
/// 失败时**不返回 Err**，而是带着 `error` + 本地缓存返回——市场拉不到不是致命错误，
/// 用户仍然可以看缓存里的插件、也仍然可以手动粘 URL 安装。但失败原因必须原样呈现。
#[tauri::command(async)]
pub async fn market_list(
    registry: tauri::State<'_, super::PluginRegistry>,
) -> Result<MarketView, String> {
    let installed: std::collections::HashMap<String, String> = registry
        .plugins
        .read()
        .map(|g| {
            g.iter()
                .map(|p| (p.manifest.name.clone(), p.manifest.version.clone()))
                .collect()
        })
        .unwrap_or_default();
    let c = coord();
    let source = format!("{}/{}@{}:{}", c.owner, c.repo, c.git_ref, c.path);

    let fetched = tauri::async_runtime::spawn_blocking(fetch_index)
        .await
        .map_err(|e| format!("市场索引任务异常终止: {e}"))?;

    Ok(match fetched {
        Ok(index) => MarketView {
            plugins: index.plugins,
            origin: "remote".into(),
            error: None,
            installed,
            source,
        },
        Err(e) => match cached_index() {
            Some(index) => MarketView {
                plugins: index.plugins,
                origin: "cache".into(),
                error: Some(e),
                installed,
                source,
            },
            None => MarketView {
                plugins: Vec::new(),
                origin: "none".into(),
                error: Some(e),
                installed,
                source,
            },
        },
    })
}

/// 命令：从市场安装某个插件（预览阶段）。
///
/// 与手动粘 URL 安装走**同一条**链路（同一套解压、路径、清单、可执行文件校验），
/// 唯一的加强是带上索引给的内容哈希：装到的必须逐字节等于收录时审核过的那份，
/// 否则拒绝。这也是「第三方镜像可篡改传输内容」这条风险的收口——
/// 镜像即便被控制，也只能让你装不上，不能让你装到被改过的代码。
///
/// 确认安装仍走 `plugin_install_confirm`（token 相同），不另开一条落地路径。
#[tauri::command(async)]
pub async fn market_install_preview(
    name: String,
    registry: tauri::State<'_, super::PluginRegistry>,
    staging: tauri::State<'_, super::install::InstallStaging>,
) -> Result<super::install::InstallPreview, String> {
    let index = tauri::async_runtime::spawn_blocking(fetch_index)
        .await
        .map_err(|e| format!("市场索引任务异常终止: {e}"))?
        .map_err(|e| format!("{e}\n无法确认要安装的版本，已中止（不使用可能过期的缓存索引）"))?;

    let entry = index
        .plugins
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("市场里没有名为「{name}」的插件（索引可能已更新）"))?;

    if entry.revoked {
        let why = if entry.revoked_reason.is_empty() {
            "未给出原因".to_string()
        } else {
            entry.revoked_reason.clone()
        };
        return Err(format!("「{name}」已被市场下架，不能安装。下架原因：{why}"));
    }

    // 没有哈希就如实降级为「不校验」，绝不假装校验过。
    // 正常情况下 CI 一定会写入 contentHash，走到这里说明索引是手工改过或版本过旧。
    let expect = (!entry.content_hash.is_empty()).then(|| entry.content_hash.clone());
    if expect.is_none() {
        ilog!("[iTools] 市场条目 {name} 没有 contentHash，本次安装将不做内容校验");
    }

    let preview = super::install::preview_from_market(&entry.install_url(), expect, &registry, &staging).await?;

    // 纵深防御：索引条目声称的 name 必须与包里 plugin.json 的 name 一致。
    // 有 contentHash 时这条其实是冗余的（哈希对上就说明整包内容一致），
    // 但没有哈希时它是唯一能挡住「索引条目指向了别的插件」的检查。
    if preview.name != entry.name {
        // 暂存目录留给 sweep 清理；这里只拒绝，不动用户已装的任何东西
        return Err(format!(
            "市场条目与实际内容不符：条目名「{}」，包里的 plugin.json 写的是「{}」。已拒绝安装。",
            entry.name, preview.name
        ));
    }
    Ok(preview)
}

// ==================== 内容哈希 ====================

/// 计算目录的内容哈希，返回形如 `sha256:9f86d0…`。
///
/// 跳过 `.git`（正常的插件包里不会有，但手工放进 dev 目录的可能带着，
/// 算进去会让同一份插件在「从 zip 装」与「从本地目录」两种来源下得到不同的哈希）。
pub fn content_hash(dir: &Path) -> Result<String, String> {
    let mut lines: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = entry.map_err(|e| format!("遍历插件目录失败: {e}"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dir)
            .map_err(|e| format!("计算相对路径失败: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(entry.path()).map_err(|e| format!("读 {rel} 失败: {e}"))?;
        lines.push(line_of(&rel, &bytes));
    }
    Ok(hash_lines(lines))
}

/// 单个文件在哈希输入里对应的那一行：`相对路径 \t 内容的 sha256`。
fn line_of(rel: &str, content: &[u8]) -> String {
    format!("{rel}\t{}", hex(&Sha256::digest(content)))
}

/// 纯算法部分（不碰文件系统，便于与 Node 侧做等价性测试）。
///
/// `Vec<String>` 的排序即 UTF-8 字节序 —— 这正是与 JS 侧对齐的那个口径。
/// 排序的是整行而非单独的 rel：行内分隔符是 `\t`(0x09)，小于任何能出现在路径里的可见字符，
/// 因此「按整行排」与「按 rel 排」结果必然一致（rel 是另一个 rel 的前缀时，
/// 短的那个下一位是 0x09，一定更小，与只比 rel 的结论相同）。
fn hash_lines(mut lines: Vec<String>) -> String {
    lines.sort();
    let body = lines.join("\n");
    format!("sha256:{}", hex(&Sha256::digest(body.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // 写入 String 不会失败
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 校验目录内容是否与期望哈希一致。
///
/// 期望值来自市场索引（审核时算出并记录）。不匹配时给出**双方的值**，
/// 便于分辨「装到了别的版本」还是「传输被改动」——只说一句「校验失败」排查不下去。
pub fn verify_content_hash(dir: &Path, expect: &str) -> Result<(), String> {
    let got = content_hash(dir)?;
    if got.eq_ignore_ascii_case(expect) {
        return Ok(());
    }
    Err(format!(
        "内容校验失败：期望 {expect}，实际 {got}。\
         这份插件包与市场收录时审核过的内容不一致——可能是下载源被篡改，也可能是市场索引已过期。\
         已拒绝安装。"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &[u8]) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("itools-market-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 由内存条目算哈希（绕开文件系统，专测算法本身）。
    fn hash_of(entries: &[(&str, &[u8])]) -> String {
        hash_lines(entries.iter().map(|(rel, c)| line_of(rel, c)).collect())
    }

    /// **跨语言一致性基准**：期望值由 `scripts/registry/hash.mjs` 生成（Node 侧实现）。
    ///
    /// 两端算法只要有一处不同（尤其是排序口径），市场里每个插件都会在安装时报「内容校验失败」，
    /// 而且 `cargo test` 与 CI 各自都是绿的、谁都发现不了。所以必须有这组用例把两边钉死。
    ///
    /// 刻意**不经过文件系统**：Windows 的 NTFS 大小写不敏感（`z.txt` 与 `Z.txt` 是同一个文件），
    /// 用真实目录构造用例会静默丢条目，测出来的是文件系统行为而不是算法。
    /// 目录遍历那一半由 `walks_dir_into_relative_paths` 单独覆盖。
    ///
    /// 改动任一侧算法后，重跑 Node 侧生成新基准，两边同时更新。
    #[test]
    fn content_hash_matches_node_golden() {
        assert_eq!(
            hash_of(&[("index.html", b"<h1>hi</h1>")]),
            "sha256:19211284cbca24a70a77c34606aa41d0750538c8ffee9fd7ed3916f90e575a59",
            "单文件"
        );

        assert_eq!(
            hash_of(&[
                ("index.html", b"<h1>hi</h1>".as_slice()),
                ("plugin.json", br#"{"name":"demo"}"#.as_slice()),
                ("assets/logo.png", [0x89u8, 0x50, 0x4e, 0x47].as_slice()),
            ]),
            "sha256:5298b33fa66beb06374e0164dc939880e1edf77df11258d30a8221b2205af38f",
            "典型插件（含子目录）"
        );

        // **排序口径**：这组用例的全部意义在于卡住「按 UTF-16 排序」的实现。
        //   U+FF21 全角Ａ : UTF-8 = EF BC A1 , UTF-16 = FF21
        //   U+1F600 😀    : UTF-8 = F0 9F 98 80 , UTF-16 = D83D DE00（代理对）
        // 字节序   : EF < F0     → 全角Ａ 在前
        // UTF-16序 : FF21 > D83D → 😀 在前
        // 两种排法结果相反，所以只要有一侧排错，这个断言必红。
        // （只用 ASCII + 中文是测不出来的：那种情况下两种排序恰好一致。）
        assert_eq!(
            hash_of(&[
                ("a.txt", "a".as_bytes()),
                ("a/b.txt", "ab".as_bytes()),
                ("中文.txt", "zh".as_bytes()),
                ("\u{FF21}.txt", "fullwidth-A".as_bytes()),
                ("\u{1F600}.txt", "emoji".as_bytes()),
            ]),
            "sha256:f5d12a366e1cddbcc2e85e8ba65b193e1dd6ef00e30f45994af25655f875d218",
            "排序口径（UTF-8 字节序，不是 UTF-16 序）"
        );

        assert_eq!(
            hash_of(&[("empty.txt", b"")]),
            "sha256:0bb554efdf07f6e39bbec3980cb56dce47a344a7c5574cddf74f05e715ccd8aa",
            "空文件也必须参与哈希"
        );
    }

    /// 目录遍历那一半：相对路径要以 `/` 分隔、递归覆盖子目录，且与内存算法结果一致。
    #[test]
    fn walks_dir_into_relative_paths() {
        let d = tmp("walk");
        write(&d, "index.html", b"<h1>hi</h1>");
        write(&d, "plugin.json", br#"{"name":"demo"}"#);
        write(&d, "assets/logo.png", &[0x89, 0x50, 0x4e, 0x47]);
        assert_eq!(
            content_hash(&d).unwrap(),
            hash_of(&[
                ("index.html", b"<h1>hi</h1>".as_slice()),
                ("plugin.json", br#"{"name":"demo"}"#.as_slice()),
                ("assets/logo.png", [0x89u8, 0x50, 0x4e, 0x47].as_slice()),
            ]),
            "遍历真实目录的结果必须与同内容的内存条目一致（子目录要用 / 而不是 \\）"
        );
    }

    #[test]
    fn verify_reports_both_sides() {
        let d = tmp("verify");
        write(&d, "a.txt", b"a");
        let real = content_hash(&d).unwrap();
        assert!(verify_content_hash(&d, &real).is_ok());

        let err = verify_content_hash(&d, "sha256:deadbeef").unwrap_err();
        assert!(err.contains("sha256:deadbeef"), "错误里要有期望值");
        assert!(err.contains(&real), "错误里要有实际值，否则无从排查");
        assert!(err.contains("已拒绝安装"), "必须明确说明结果是拒绝，而不是含糊带过");
    }

    #[test]
    fn git_dir_is_skipped() {
        let d = tmp("gitskip");
        write(&d, "a.txt", b"a");
        let without = content_hash(&d).unwrap();
        write(&d, ".git/HEAD", b"ref: refs/heads/main");
        write(&d, ".git/config", b"[core]");
        assert_eq!(content_hash(&d).unwrap(), without, ".git 不得参与内容哈希");
    }
}
