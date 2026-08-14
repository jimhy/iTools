//! 插件的 **Git 安装 / 更新**（URL 形态对齐 Unity Package Manager）。
//!
//! **插件生态以 GitHub 为准**：默认 host、示例与错误提示一律用 `github.com`。
//! Gitee 的支持仍然完整保留（已实现且工作正常，删掉只是白白减少用户选择），
//! 但不再是默认、也不再在文档里主推。GitHub 在国内可达性差的问题由
//! [`super::mirror`]（镜像源竞速 + 服务端健康探测）解决，**不自建反代**。
//!
//! 为什么用「下载 zip 归档」而不是调 git CLI：
//! - 用户机器不一定装 git；插件是纯前端小目录，不需要历史、分支与工作区；
//! - GitHub / Gitee 都提供 zip 归档直链与 raw 单文件直链——**检查更新只拉一个 plugin.json**
//!   （通常 < 1KB），比 `git ls-remote` + clone 轻得多，也不受本机 git 凭据管理器干扰。
//!
//! 关键不变量（改动本文件前务必理解）：
//! - **插件的用户数据不在插件目录里**（在 SQLite `plugin_data` 与 `plugin-data/<id>/files/`），
//!   所以「整目录替换」式更新不会丢用户数据，这正是敢做原子替换的前提；
//! - 落地目录名一律取自 `plugin.json` 的 `name`，**不取仓库名**——`scan_plugins` 强校验
//!   `name == 目录名`，取错就会「装上却扫不到」；
//! - 安装 / 更新全程先落到 `<plugins_root>/.staging/`（与插件根同盘，rename 才能原子），
//!   校验通过才 rename 到位，失败不留半成品。因此 `scan_plugins` 与 `watch` 必须跳过
//!   `.` 开头的目录，否则暂存过程会污染扫描结果与热重载指纹；
//! - 「替换失败且回滚也失败」时旧插件被寄存到 `<plugins_root>/.recover-<name>/`，
//!   **不在 `.staging/` 下**——`.staging` 每次启动被整体删除，把用户仅存的旧插件放进去
//!   等于「提示能抢救、重启就没了」。启动时 [`recover_orphans`] 会把它自动搬回原位。
//!
//! 安全：域名白名单、Zip Slip 防护、体积/条目上限、可执行文件拒收、拒绝覆盖内置插件、
//! 安装后权限一律未授权（换来源的覆盖安装还会**清空**同名插件的历史授权）。
//! 令牌只从环境变量读，源码零明文，且所有错误出口经 [`redact_token`] 脱敏后才回到 UI；
//! **带令牌的请求一律直连官方域名，绝不走镜像**（否则等于把凭据交给第三方反代）。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::logging::ilog;
use crate::search::SearchIndex;
use crate::settings::SettingsStore;

use super::mirror;
use super::PluginRegistry;

// ==================== 常量 ====================

/// 拉取远端 plugin.json 的超时（秒）——只有几百字节，短超时避免检查更新卡住 UI。
const META_TIMEOUT_SECS: u64 = 10;
/// 下载归档的超时（秒）。
const ARCHIVE_TIMEOUT_SECS: u64 = 120;
/// 归档下载体积上限（字节）。插件是纯前端资源，超过这个量级基本可判定为「装错了仓库」。
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024;
/// 解压后总体积上限（字节）——防 zip 炸弹。
const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024;
/// 单文件解压体积上限（字节）。
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// 归档条目数上限——防「百万小文件」型消耗。
const MAX_ENTRIES: usize = 4000;
/// 远端 plugin.json 读取上限（字节）：正常清单几 KB，超了说明拿到的不是清单（如 404 HTML 页）。
const MAX_META_BYTES: u64 = 512 * 1024;
/// 预览返回的 README 最大字符数（超出截断，避免一次 IPC 传几 MB 文本）。
const MAX_README_CHARS: usize = 20_000;
/// 预览暂存的存活时长：用户打开预览后迟迟不确认（或前端崩了），到点自动清理。
const STAGING_TTL: Duration = Duration::from_secs(30 * 60);
/// 暂存目录名（`.` 开头，被扫描与热重载跳过）。
const STAGING_DIR: &str = ".staging";
/// 「回滚失败的旧插件」寄存目录前缀（`.` 开头，同样被扫描与热重载跳过）。
/// 刻意**不放在 [`STAGING_DIR`] 下**：`.staging` 每次启动被无条件清空，
/// 把用户唯一的旧插件副本放进去会在下次启动时被一起删掉（见 [`recover_orphans`]）。
const RECOVER_PREFIX: &str = ".recover-";
/// 安装来源锁文件名（`.` 开头，同上）。
const LOCK_FILE: &str = ".installed.json";
/// 锁文件格式版本。
const LOCK_VERSION: u32 = 1;

/// 可选的 GitHub 访问令牌环境变量（私有仓库 / 提升限流）。源码与二进制零明文。
///
/// 走 `Authorization: Bearer <PAT>` **请求头**（GitHub 官方文档化的方式）：
/// 头部不会像 query 参数那样出现在 ureq 的 `Transport` Display 里，天然没有
/// 「一次超时就把令牌回显到 UI」的泄漏面（那是 Gitee query 形态才需要 [`redact_token`] 兜的）。
const GITHUB_TOKEN_ENV: &str = "ITOOLS_GITHUB_TOKEN";
/// 可选的 Gitee 访问令牌环境变量（私有仓库 / 防限流）。源码与二进制零明文。
const GITEE_TOKEN_ENV: &str = "ITOOLS_GITEE_TOKEN";
/// 追加放行的托管站环境变量（逗号分隔，形如 `git.corp.com=github` 或 `git.corp.com`）。
const EXTRA_HOSTS_ENV: &str = "ITOOLS_PLUGIN_HOSTS";

/// 拒收的可执行/脚本扩展名：插件是**纯前端**（HTML/CSS/JS/图片），
/// 归档里出现这些一律整包拒绝——一个前端插件没有任何理由携带 PE/脚本载荷。
///
/// 收录标准是「资源管理器里双击即执行 / 即改系统状态」：除 PE 与脚本外，
/// `lnk`（快捷方式可指向任意命令行）、`url` / `scf`（可触发本地程序）、`hta`（本地权限 HTML 应用）、
/// `reg`（双击即写注册表）、`pif` / `msc` 同属此类，一并拒收。
const DENIED_EXTS: &[&str] = &[
    "exe", "dll", "bat", "cmd", "com", "scr", "ps1", "psm1", "msi", "vbs", "vbe", "wsf", "jar",
    "sys", "drv", "cpl", "lnk", "url", "hta", "reg", "scf", "pif", "msc",
];
/// 插件 logo 的读取上限（字节）——见 [`super::read_logo`]，防「远端指定一个几 GB 的本地文件」撑爆内存。
pub(crate) const MAX_LOGO_BYTES: u64 = 2 * 1024 * 1024;

// ==================== 数据结构 ====================

/// 一个插件的 Git 安装来源（前端按 camelCase 对齐）。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GitSource {
    /// 托管站**风格**：`"gitee"` | `"github"`，决定归档 / raw 直链怎么拼，**不是域名**。
    /// 自建镜像走 `ITOOLS_PLUGIN_HOSTS` 放行时（如 `git.corp.com=github`），风格是 `github`
    /// 而域名是 `git.corp.com`——UI 若拿 `host` 展示就会把自建站说成 github.com（虚假展示）。
    pub host: String,
    /// **真实域名**（如 `gitee.com` / `git.corp.com`），供 UI 展示与来源核对。
    ///
    /// `#[serde(default)]`：本字段是后加的，老锁文件里没有它；缺省为空串时
    /// [`GitSource::real_host`] 会回退到从 [`GitSource::page_url`] 还原，保证老记录仍可用。
    #[serde(default)]
    pub domain: String,
    pub owner: String,
    pub repo: String,
    /// 仓库内子目录，`""` = 仓库根。
    pub sub_path: String,
    /// 分支 / tag / 完整 commit sha；`""` = 跟随默认分支（请求时用字面量 `HEAD`）。
    pub revision: String,
    /// 规范化后的完整安装 URL（可原样再次安装）。
    pub url: String,
    /// 仓库网页地址，供 UI「查看仓库」。
    pub page_url: String,
}

/// 安装预览：**尚未落地**，只是把远端包解到暂存区后解析出来的信息，供用户确认。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPreview {
    /// 暂存句柄，确认 / 取消时回传。
    pub token: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub permissions: Vec<String>,
    pub feature_count: usize,
    /// 关键字预览。
    pub cmds: Vec<String>,
    /// logo 的 data URL。
    pub logo: Option<String>,
    /// README.md 文本（过长已截断）。
    pub readme: Option<String>,
    pub source: GitSource,
    pub already_installed: bool,
    pub installed_version: Option<String>,
    /// **本机原本就装着来自同一个 `source.url` 的同名插件**（= 同源覆盖更新）。
    ///
    /// 为什么必须告诉前端：[`plugin_install_confirm`] 只在 `!same_source` 时清空
    /// `plugin_permissions`，同源覆盖**保留**历史授权。而「覆盖更新到 vX」恰恰是最常见的用法，
    /// 装完 runCommand / network 立刻可用——弹窗若一律显示「安装后默认不授权，需逐项开启」
    /// 就是一句谎话。前端据此分支渲染文案。全新安装 / 换来源时为 false。
    pub same_source: bool,
    /// 同名的是随包内置插件（内置插件每次启动会被播种回来，禁止覆盖）。
    pub is_builtin: bool,
    pub file_count: usize,
    pub total_bytes: u64,
    /// 本次归档实际是从哪个源下载的（如 `github.com（官方直连）` / `gh-proxy.com`）。
    pub download_source: String,
    /// 是否经由第三方镜像下载（镜像**可以篡改内容**，UI 必须如实提示）。
    pub via_mirror: bool,
    /// 本次下载是否做过 sha256 校验。手动粘 URL 安装没有可信哈希来源 → false，
    /// 此时 UI 必须显示「本次下载未经哈希校验」，不许含糊过去。
    pub hash_verified: bool,
}

/// 一个插件的更新检查结果。
///
/// `checked` 存在的意义：**不许把「没检查」伪装成「已是最新」**。
/// 手工放入 / 内置的插件没有 Git 来源，锁定到 commit 的插件不发请求，检查失败的有 `error`，
/// 这三种情况 `checked` 都是 false，UI 必须据此区分展示。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdate {
    pub name: String,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub has_update: bool,
    /// revision 是 40 位 commit sha → 锁定版本，不跟随更新。
    pub pinned: bool,
    /// 是否真的完成了一次远端检查。
    pub checked: bool,
    /// 检查失败的真实原因。
    pub error: Option<String>,
    /// 本次请求实际用的下载源（如 `github.com（官方直连）` / `gh-proxy.com`）。没发请求时为 None。
    pub download_source: Option<String>,
    /// 是否经由第三方镜像（镜像可篡改内容，UI 需如实提示）。
    pub via_mirror: bool,
}

/// 锁文件里的一条安装记录（GitSource 字段被 flatten 平铺）。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct InstalledEntry {
    #[serde(flatten)]
    pub source: GitSource,
    /// 安装时落地的插件版本号（来自当时的 plugin.json）。
    pub resolved_version: String,
    /// 安装时间（RFC3339）。
    pub installed_at: String,
}

/// `<plugins_root>/.installed.json` 的整体结构。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LockFile {
    /// 缺失时按 0 读入、下次写入时补成 [`LOCK_VERSION`]——手工编辑过的锁文件不该导致整表作废。
    #[serde(default)]
    pub version: u32,
    pub plugins: BTreeMap<String, InstalledEntry>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            plugins: BTreeMap::new(),
        }
    }
}

// ==================== 托管站 ====================

/// 托管站风格：决定归档 / raw 直链怎么拼。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Forge {
    Gitee,
    GitHub,
    /// 插件市场（自建服务端）。**不拼任何 Git 直链**——包从市场端点整包下载，
    /// 更新检查也走市场索引，永远不进 [`download_capped`]。
    Market,
}

impl Forge {
    fn as_str(self) -> &'static str {
        match self {
            Forge::Gitee => "gitee",
            Forge::GitHub => "github",
            Forge::Market => MARKET_HOST,
        }
    }
}

/// 市场来源在锁文件里的 `host` 值。
pub(crate) const MARKET_HOST: &str = "market";

/// 市场来源的 URL 前缀。
///
/// 它**不是可访问的地址**，只是一个稳定的来源身份：锁文件按 `source.url` 判「同源覆盖」
/// （决定升级时保不保留用户授权），更新检查按它路由到市场分支。
/// 之所以用一个 scheme 而不是真实 URL：真实市场地址会随用户改服务器而变，
/// 一变就成了「换了来源」，用户的授权会被无缘无故清空。
pub(crate) const MARKET_SCHEME: &str = "itools-market://";

/// 放行的托管站清单：**`github.com` 为主**（插件生态的默认与推荐），`gitee.com` 继续保留，
/// 可用 `ITOOLS_PLUGIN_HOSTS` 追加自建镜像。
///
/// 为什么保留 Gitee：这套代码已实现且工作正常，删掉只是白白减少用户选择；
/// 但默认值、示例与错误提示一律以 github.com 为准（见 [`parse_git_url_with`]）。
///
/// 追加项形如 `git.corp.com=github`（按 GitHub 风格拼 URL）或 `git.corp.com`（按 Gitee 风格）。
/// 之所以要显式风格：两家的归档 / raw 路径完全不同，猜错就是 404。
fn allowed_hosts() -> Vec<(String, Forge)> {
    let mut out = vec![
        ("github.com".to_string(), Forge::GitHub),
        ("gitee.com".to_string(), Forge::Gitee),
    ];
    if let Ok(extra) = std::env::var(EXTRA_HOSTS_ENV) {
        for item in extra.split(',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (host, forge) = match item.split_once('=') {
                Some((h, f)) if f.trim().eq_ignore_ascii_case("github") => (h.trim(), Forge::GitHub),
                Some((h, _)) => (h.trim(), Forge::Gitee),
                None => (item, Forge::Gitee),
            };
            let host = host.trim_start_matches("www.").to_ascii_lowercase();
            if !host.is_empty() && !out.iter().any(|(h, _)| h == &host) {
                out.push((host, forge));
            }
        }
    }
    out
}

// ==================== URL 解析 ====================

/// 解析 Unity UPM 风格的插件仓库地址。
///
/// 支持形态（`.git` 后缀可有可无、结尾斜杠容忍、host 大小写归一）：
/// ```text
/// https://github.com/user/repo.git
/// https://github.com/user/repo.git#v1.2.3                   # 号后是分支/tag/完整 sha
/// https://github.com/user/repo.git?path=/plugins/foo        ?path= 是仓库内子目录
/// https://github.com/user/repo.git?path=/plugins/foo#dev    组合：path 在前、#revision 在最后
/// ```
pub fn parse_git_url(input: &str) -> Result<GitSource, String> {
    parse_git_url_with(input, &allowed_hosts())
}

/// 构造一条「来自插件市场」的来源记录。
///
/// 字段语义与 Git 来源刻意错开，免得别处误当仓库用：`owner` 空、`repo` = 插件名、
/// `revision` 空（市场插件跟随索引里的最新版本，不锁 commit）、`page_url` 空（没有仓库网页）。
fn market_source(name: &str) -> Result<GitSource, String> {
    let name = name.trim().trim_end_matches('/');
    if !is_valid_plugin_name(name) {
        return Err(format!("市场来源里的插件名「{name}」不合法"));
    }
    Ok(GitSource {
        host: MARKET_HOST.to_string(),
        domain: String::new(),
        owner: String::new(),
        repo: name.to_string(),
        sub_path: String::new(),
        revision: String::new(),
        url: format!("{MARKET_SCHEME}{name}"),
        page_url: String::new(),
    })
}

/// [`parse_git_url`] 的可注入版本：把「放行清单」显式传入，便于单测不依赖环境变量。
fn parse_git_url_with(input: &str, allowed: &[(String, Forge)]) -> Result<GitSource, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("请输入插件仓库地址".to_string());
    }
    // 市场来源：锁文件里记的是 `itools-market://<name>`。
    // 必须在这里认出来，否则 read_lock 的「重新解析 url，解析不过就丢弃」会把
    // 所有从市场装的插件的安装记录整批丢掉——表现为它们突然全部「无来源、不检查更新」。
    if let Some(name) = raw.strip_prefix(MARKET_SCHEME) {
        return market_source(name);
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("git@") || lower.starts_with("ssh://") {
        return Err("暂不支持 SSH 形式（git@… / ssh://…）：通过 https 下载归档安装，不依赖本机 git。请改用 https:// 开头的仓库地址（如 https://github.com/用户名/仓库名.git）".to_string());
    }
    if lower.starts_with("file://") {
        return Err("不支持 file:// 本地路径安装，请使用 https:// 的仓库地址".to_string());
    }
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| "地址应以 https:// 开头，例如 https://github.com/用户名/仓库名.git".to_string())?;
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(format!("仅支持 https 协议（收到 {scheme}），http 明文与其它协议一律拒绝"));
    }

    // 先切 fragment（#revision 永远在最后），再切 query（?path=）
    let (rest, revision) = match rest.split_once('#') {
        Some((a, b)) => (a, b.trim()),
        None => (rest, ""),
    };
    let (path_part, query) = match rest.split_once('?') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };

    // ---- host / owner / repo ----
    let mut segs = path_part.split('/').filter(|s| !s.is_empty());
    let host_raw = segs
        .next()
        .ok_or_else(|| "地址缺少域名".to_string())?
        .to_ascii_lowercase();
    let host = host_raw.trim_start_matches("www.").to_string();
    if host.contains(':') {
        return Err("仓库地址不支持指定端口".to_string());
    }
    let forge = allowed
        .iter()
        .find(|(h, _)| h == &host)
        .map(|(_, f)| *f)
        .ok_or_else(|| {
            let list = allowed
                .iter()
                .map(|(h, _)| h.as_str())
                .collect::<Vec<_>>()
                .join("、");
            format!("不允许的仓库来源：{host}。当前只放行 {list}（可用环境变量 {EXTRA_HOSTS_ENV} 追加）")
        })?;

    let owner = segs.next().unwrap_or("").to_string();
    let repo_raw = segs.next().unwrap_or("");
    let repo = repo_raw
        .strip_suffix(".git")
        .unwrap_or(repo_raw)
        .to_string();
    if owner.is_empty() || repo.is_empty() {
        return Err("仓库地址应形如 https://github.com/用户名/仓库名.git".to_string());
    }
    if segs.next().is_some() {
        return Err("仓库地址只应包含「用户名/仓库名」两段；仓库内子目录请用 ?path=/子/目录 指定".to_string());
    }
    check_ident(&owner, "用户名")?;
    check_ident(&repo, "仓库名")?;

    // ---- ?path= ----
    let mut sub_path = String::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k != "path" {
            return Err(format!("不认识的参数 {k}，仅支持 ?path=/仓库内/子目录"));
        }
        sub_path = normalize_sub_path(v)?;
    }

    // ---- #revision ----
    check_revision(revision)?;

    let mut url = format!("https://{host}/{owner}/{repo}.git");
    if !sub_path.is_empty() {
        url.push_str("?path=/");
        url.push_str(&sub_path);
    }
    if !revision.is_empty() {
        url.push('#');
        url.push_str(revision);
    }

    Ok(GitSource {
        host: forge.as_str().to_string(),
        domain: host.clone(),
        owner: owner.clone(),
        repo: repo.clone(),
        sub_path,
        revision: revision.to_string(),
        url,
        page_url: format!("https://{host}/{owner}/{repo}"),
    })
}

/// 校验 owner / repo 段：只允许字母数字与 `.` `_` `-`，且不得是纯点（`.` / `..`）。
fn check_ident(s: &str, what: &str) -> Result<(), String> {
    if s.chars().all(|c| c == '.') {
        return Err(format!("非法的{what}：{s}"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!("{what}含非法字符：{s}"));
    }
    Ok(())
}

/// 归一化 `?path=` 值：前后斜杠容忍、去重复斜杠，禁止 `..` 段与反斜杠。
/// 返回不带首尾斜杠的相对路径（`""` = 仓库根）。
///
/// 每段**必须**匹配 `^[A-Za-z0-9._-]+$`，理由有二（都不是洁癖）：
/// 1. Windows 上 `PathBuf::push("C:")` 的语义是「带前缀无根 → 整个替换 self」（std 文档明载），
///    所以 `?path=/C:/foo` 会让 [`stage_from_source`] 里逐段 push 出来的落点**跳出暂存沙盒**，
///    后续的 `rename` 会把 `C:` 盘当前目录下的同名目录整个搬走；
/// 2. `sub_path` 会被直接拼进 raw / archive URL，段内出现 `?` `#` 会凭空多出 query/fragment
///    分隔符，既改变请求目标，也影响 [`with_token`] 对 `?`/`&` 的判断。
fn normalize_sub_path(v: &str) -> Result<String, String> {
    let v = v.trim();
    if v.contains('\\') {
        return Err("?path= 请用正斜杠 / 分隔".to_string());
    }
    let mut segs = Vec::new();
    for seg in v.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg.chars().all(|c| c == '.') {
            return Err("?path= 不允许包含上跳路径（..）".to_string());
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!(
                "?path= 的子目录名「{seg}」含非法字符：只允许字母、数字与 . _ -（盘符、? # 与空白一律拒绝）"
            ));
        }
        segs.push(seg);
    }
    Ok(segs.join("/"))
}

/// 校验 `#revision`：分支名可含 `/`，但不许空白、`..`、以及会破坏 URL 的字符。
fn check_revision(rev: &str) -> Result<(), String> {
    if rev.is_empty() {
        return Ok(());
    }
    if rev.contains("..") {
        return Err("revision 不允许包含 ..".to_string());
    }
    if rev.starts_with('/') || rev.ends_with('/') || rev.starts_with('-') {
        return Err(format!("非法的 revision：{rev}"));
    }
    if !rev
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return Err(format!("revision 含非法字符：{rev}"));
    }
    Ok(())
}

/// revision 是否为 40 位十六进制 commit sha（→ 锁定版本，不跟随更新）。
fn is_pinned(revision: &str) -> bool {
    revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit())
}

impl GitSource {
    fn forge(&self) -> Forge {
        match self.host.as_str() {
            "github" => Forge::GitHub,
            MARKET_HOST => Forge::Market,
            _ => Forge::Gitee,
        }
    }

    /// 这条记录是不是「从插件市场装的」。
    ///
    /// 市场来源没有归档 / raw 直链，更新检查与下载都必须走市场分支；
    /// 误让它进 [`download_capped`] 会拼出一个 `https:///` 之类的废 URL 并报一个无从理解的错。
    pub(crate) fn is_market(&self) -> bool {
        self.forge() == Forge::Market
    }

    /// 真实域名——自建镜像时与 [`GitSource::host`]（风格名）不同。
    ///
    /// 优先取 [`GitSource::domain`]；为空说明是**加该字段之前写下的老锁文件**，
    /// 回退到从 `page_url` 还原（保持老记录仍能检查更新 / 更新，不因升级而失效）。
    pub(crate) fn real_host(&self) -> String {
        if !self.domain.is_empty() {
            return self.domain.clone();
        }
        self.page_url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string()
    }

    /// 现场重建仓库网页地址，并核对域名仍在放行清单内。
    ///
    /// 为什么不直接用存盘的 `page_url`：锁文件是**普通用户权限即可写**的普通文件，
    /// 任何本地进程都能把它改成 `file:///…/payload.exe` 或 `https://evil.tld/u/r`；
    /// 「查看仓库」若原样交给 `opener::open`，就等于给了任意 URI/可执行文件一个启动入口。
    /// 这里只用经校验的 host/owner/repo 拼回 https URL，存盘字符串一概不信。
    fn verified_page_url(&self) -> Result<String, String> {
        if self.is_market() {
            // 市场插件没有仓库网页可开。诚实报出来，不要拼一个能打开但指向别处的地址。
            return Err("这个插件来自插件市场，没有对应的代码仓库页面".to_string());
        }
        let host = self.real_host();
        let allowed = allowed_hosts();
        if !allowed.iter().any(|(h, _)| h == &host) {
            return Err(format!(
                "该插件的安装记录指向未放行的域名 {host}，已拒绝打开（安装记录可能被篡改）"
            ));
        }
        check_ident(&self.owner, "用户名")?;
        check_ident(&self.repo, "仓库名")?;
        Ok(format!("https://{host}/{}/{}", self.owner, self.repo))
    }

    /// 请求用的 ref：revision 为空时用字面量 `HEAD`——GitHub 与 Gitee 的归档与 raw 都认它
    /// （镜像站同样透传），表示默认分支，因此**不需要**先查默认分支名，也不写 master/main 的猜测回退。
    fn git_ref(&self) -> &str {
        if self.revision.is_empty() {
            "HEAD"
        } else {
            &self.revision
        }
    }

    /// 本来源在 [`super::mirror`] 里的坐标；**只有真正的 github.com 才返回 `Some`**。
    ///
    /// Gitee 与自建 GitHub 风格站（Gitea 等）的路径形态跟 GitHub 公共镜像所代理的完全不同，
    /// 把它们塞进镜像模板只会得到 404；自建站更不该把内网地址交给公共反代。
    fn gh_coord(&self, rel: &str) -> Option<mirror::GhCoord> {
        if self.forge() != Forge::GitHub || self.real_host() != "github.com" {
            return None;
        }
        debug_assert!(!self.is_market(), "市场来源不该走 GitHub 镜像坐标");
        Some(mirror::GhCoord {
            owner: self.owner.clone(),
            repo: self.repo.clone(),
            git_ref: self.git_ref().to_string(),
            path: rel.to_string(),
        })
    }

    /// 直连官方时要带的鉴权头：GitHub PAT 走 `Authorization: Bearer <token>`。
    ///
    /// **只发给 github.com**：把 PAT 发给自建站或第三方镜像等于泄露凭据。
    /// 令牌只从环境变量 [`GITHUB_TOKEN_ENV`] 读，源码与二进制零明文。
    /// 返回 `Some` 时 [`mirror::fetch`] 会强制只走官方源（安全铁律）。
    fn auth_header(&self) -> Option<(String, String)> {
        if self.forge() != Forge::GitHub || self.real_host() != "github.com" {
            return None;
        }
        let tok = std::env::var(GITHUB_TOKEN_ENV).ok()?;
        let tok = tok.trim();
        if tok.is_empty() {
            return None;
        }
        Some(("Authorization".to_string(), format!("Bearer {tok}")))
    }

    /// 官方源展示名（回落到镜像时 UI 要能区分「用的是官方还是第三方」）。
    /// 拼串规则只有 [`mirror::official_label`] 一份——同一个源在不同面板里必须同名。
    fn official_label(&self) -> String {
        mirror::official_label(&self.real_host())
    }

    /// zip 归档直链（官方源）。
    fn archive_url(&self) -> String {
        let (host, r) = (self.real_host(), self.git_ref());
        let (owner, repo) = (&self.owner, &self.repo);
        match self.forge() {
            Forge::Gitee => format!("https://{host}/{owner}/{repo}/repository/archive/{r}.zip"),
            // github.com：与镜像模板共用同一套占位符渲染（编码规则只有一份，见 mirror::render）
            Forge::GitHub if host == "github.com" => mirror::render(
                mirror::OFFICIAL_ARCHIVE_TPL,
                &mirror::GhCoord {
                    owner: owner.clone(),
                    repo: repo.clone(),
                    git_ref: r.to_string(),
                    path: String::new(),
                },
            ),
            // 自建 GitHub 风格镜像（Gitea 等）：用网页同款归档路径
            Forge::GitHub => format!("https://{host}/{owner}/{repo}/archive/{r}.zip"),
            // 市场来源没有归档直链；[`download_capped`] 会先一步拦下，走不到这里。
            Forge::Market => String::new(),
        }
    }

    /// 仓库内某个文件的 raw 直链（`rel` 是相对仓库根的路径；官方源）。
    fn raw_url(&self, rel: &str) -> String {
        let (host, r) = (self.real_host(), self.git_ref());
        let (owner, repo) = (&self.owner, &self.repo);
        match self.forge() {
            Forge::Gitee => format!("https://{host}/{owner}/{repo}/raw/{r}/{rel}"),
            Forge::GitHub if host == "github.com" => mirror::render(
                mirror::OFFICIAL_RAW_TPL,
                &mirror::GhCoord {
                    owner: owner.clone(),
                    repo: repo.clone(),
                    git_ref: r.to_string(),
                    path: rel.to_string(),
                },
            ),
            Forge::GitHub => format!("https://{host}/{owner}/{repo}/raw/{r}/{rel}"),
            Forge::Market => String::new(),
        }
    }

    /// 仓库内 plugin.json 的相对路径（考虑 `?path=` 子目录）。
    fn manifest_rel(&self) -> String {
        if self.sub_path.is_empty() {
            "plugin.json".to_string()
        } else {
            format!("{}/plugin.json", self.sub_path)
        }
    }
}

// ==================== HTTP ====================

/// 把任意字符串里的 `access_token=<值>` 抹成 `access_token=***`。
///
/// **这是凭据不外泄的最后一道闸，任何会回到 UI / 日志的字符串都要过一遍。**
/// 起因：ureq 2.12.1 的错误 Display **会带上完整请求 URL**——
/// `error.rs` 里 `impl Display for Transport` 第一件事就是 `write!(f, "{}: ", url)`，
/// `Error::Status` 也写 `response.get_url()`；而 `request.rs` 在连接失败时执行
/// `.map_err(|e| e.url(url))` 把 URL 塞进错误。于是一次超时/断网，
/// `?access_token=<明文>` 就会随错误串一路回显到安装弹窗与更新徽标，用户截图报错即泄露。
///
/// 只按 `access_token=` 这一个键名匹配（大小写不敏感）。令牌值截止于 URL 参数分隔符
/// （`&` `#`）或错误串里常见的标点 / 空白——ureq 的 Transport Display 形如 `"{url}: {kind}"`，
/// 所以 `:` 也算终止符；Gitee 令牌是纯字母数字串，不含这些字符，不会出现「只抹了一半」。
pub(crate) fn redact_token(s: String) -> String {
    const KEY: &str = "access_token=";
    // to_ascii_lowercase 只改 ASCII 字母，字节长度与 char 边界与原串完全一致 → 下标可通用
    let lower = s.to_ascii_lowercase();
    if !lower.contains(KEY) {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let (mut rest, mut lrest): (&str, &str) = (&s, &lower);
    while let Some(pos) = lrest.find(KEY) {
        let after = pos + KEY.len();
        out.push_str(&rest[..after]);
        out.push_str("***");
        let tail = &rest[after..];
        let cut = tail
            .find(|c: char| {
                matches!(c, '&' | '#' | ':' | ')' | ',' | ';' | '"' | '\'') || c.is_whitespace()
            })
            .unwrap_or(tail.len());
        rest = &tail[cut..];
        lrest = &lrest[after + cut..];
    }
    out.push_str(rest);
    out
}

/// 给 **Gitee** 请求附加访问令牌（**只从环境变量读**，源码零明文）。未设置则匿名请求。
/// GitHub 侧不走这里——它用 `Authorization: Bearer` 请求头（见 [`GitSource::auth_header`]）。
///
/// 为什么仍用 query 参数而不是 `Authorization` 请求头：本模块访问的是 Gitee 的**网页侧**
/// 归档 / raw 直链（`/repository/archive/<ref>.zip`、`/raw/<ref>/<file>`），它们不属于 v5 OpenAPI，
/// 官方只文档化了 `?access_token=` 这一种带令牌方式。改成请求头在本环境无法验证
/// （公开仓库匿名也能过，验不出头是否真被识别；私有仓库又没有可用令牌可测），
/// 一旦不被识别就会让「私有仓库安装」静默退化成 401/404——那正是准则里禁止的
/// 「看着能用、点了不生效」。因此维持 query，泄漏面改由 [`redact_token`] 在**所有错误出口**封死。
fn with_token(url: String, forge: Forge) -> String {
    if forge != Forge::Gitee {
        return url;
    }
    match std::env::var(GITEE_TOKEN_ENV) {
        Ok(tok) if !tok.trim().is_empty() => {
            let sep = if url.contains('?') { '&' } else { '?' };
            format!("{url}{sep}access_token={}", tok.trim())
        }
        _ => url,
    }
}

/// 带上限的下载：**经 [`super::mirror`] 选源**（官方直连 + 镜像竞速），超过 `max` 判失败。
///
/// 铁律：`source.auth_header()` 非空（带令牌）时 [`mirror::fetch`] 只走官方源，绝不发给镜像。
/// `expect_sha256` 是**给未来插件市场准备的真实入口**：`Some` 时下载后必须校验、不匹配即拒绝；
/// 手动粘 URL 安装没有可信哈希来源，传 `None` 并由 [`mirror::Fetched::hash_verified`]
/// 如实回传「本次未经校验」，UI 据此提示用户。
fn download_capped(
    source: &GitSource,
    kind: mirror::Kind,
    rel: &str,
    timeout: Duration,
    max: u64,
    what: &str,
    expect_sha256: Option<String>,
) -> Result<mirror::Fetched, String> {
    // 市场来源没有 Git 直链可拼。走到这里说明某条路由漏了分流——
    // 与其拼出一个 `https:///…` 的废 URL 让用户看到莫名其妙的网络错误，不如直说。
    if source.is_market() {
        return Err(format!(
            "{what}失败：「{}」来自插件市场，没有代码仓库地址。请在「插件市场」页更新它。",
            source.repo
        ));
    }
    let auth = source.auth_header();
    let req = mirror::Request {
        kind,
        official_url: with_token(
            match kind {
                mirror::Kind::Raw => source.raw_url(rel),
                mirror::Kind::Archive => source.archive_url(),
            },
            source.forge(),
        ),
        official_label: source.official_label(),
        // 带令牌 ⇒ 不给镜像坐标（mirror::fetch 内部还会再挡一道，纵深防御）
        github: if auth.is_some() {
            None
        } else {
            source.gh_coord(rel)
        },
        auth,
        timeout,
        max_bytes: max,
        what: what.to_string(),
        expect_sha256,
    };
    mirror::fetch(&req)
}

// ==================== 解压（安全） ====================

/// 校验并规范化一个 zip 条目名，返回可安全 join 的相对路径。
///
/// 这是 Zip Slip 的第一道闸：拒绝绝对路径、盘符、`..` 段、NUL，
/// 并把反斜杠一并当分隔符处理（Windows 上 `a\..\b` 同样能穿越）。
fn safe_entry_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("归档中存在空路径条目".to_string());
    }
    if name.contains('\0') {
        return Err(format!("归档条目路径非法：{name}"));
    }
    let unified = name.replace('\\', "/");
    if unified.starts_with('/') {
        return Err(format!("归档含绝对路径条目（不安全）：{name}"));
    }
    let mut out = PathBuf::new();
    let mut count = 0usize;
    for seg in unified.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg.chars().all(|c| c == '.') {
            return Err(format!("归档条目含上跳路径（..）：{name}"));
        }
        let b = seg.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            return Err(format!("归档条目含盘符（不安全）：{name}"));
        }
        if seg.ends_with(' ') || seg.ends_with('.') {
            // Windows 会静默剥掉结尾的空格与点（"payload.exe." 落地后就是 "payload.exe"），
            // 落点与校验不一致 → 直接拒绝，避免用结尾点绕过可执行文件拒收清单
            return Err(format!("归档条目名以空格或点结尾（不安全）：{name}"));
        }
        out.push(seg);
        count += 1;
    }
    if count == 0 {
        return Err(format!("归档条目路径为空：{name}"));
    }
    Ok(out)
}

/// 命中拒收扩展名则返回该扩展名。
///
/// 不用 `Path::extension()`：对 `"payload.exe."` 它返回 `Some("")`（结尾点被当成空扩展名），
/// 小写后是空串，永远不命中 [`DENIED_EXTS`] → 整包放行，而 Windows 落盘时会把结尾点剥掉，
/// 结果磁盘上躺着一个真正的 `payload.exe`。这里改为对**文件名自身**判断：
/// 先按 Windows 的规则剥掉结尾的点与空格，再取最后一个 `.` 之后的部分比对。
fn denied_ext(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?.trim_end_matches(['.', ' ']);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    DENIED_EXTS.iter().copied().find(|d| *d == ext)
}

/// 把 zip 字节流安全解压到 `dest`（`dest` 必须已是规范化路径）。
fn extract_zip(bytes: Vec<u8>, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("创建解压目录失败: {e}"))?;
    let canon_dest = dest
        .canonicalize()
        .map_err(|e| format!("解析解压目录失败: {e}"))?;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("归档不是有效的 zip（可能是 404 页面）: {e}"))?;
    if zip.len() > MAX_ENTRIES {
        return Err(format!(
            "归档条目数 {} 超过上限 {MAX_ENTRIES}，已中止",
            zip.len()
        ));
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .map_err(|e| format!("读取归档条目失败: {e}"))?;
        let raw_name = f.name().to_string();
        // 符号链接条目：解出来在 Windows 上可能变成指向任意位置的重解析点，一律拒绝
        if let Some(mode) = f.unix_mode() {
            if mode & 0xF000 == 0xA000 {
                return Err(format!("归档含符号链接条目（不安全）：{raw_name}"));
            }
        }
        let rel = safe_entry_path(&raw_name)?;
        let out = canon_dest.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| format!("创建目录失败 {}: {e}", out.display()))?;
            continue;
        }
        if let Some(ext) = denied_ext(&rel) {
            return Err(format!(
                "归档含可执行/脚本文件（.{ext}）：{raw_name}。插件应为纯前端资源，已整包拒绝"
            ));
        }
        if f.size() > MAX_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 声明大小超过单文件上限 {} MB，已中止",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
            // 落点复核（Zip Slip 第二道闸）：父目录 canonicalize 后必须仍在暂存根内，
            // 挡住「预先存在的软链接/重解析点」把写入引向根外的情况。
            let cp = parent
                .canonicalize()
                .map_err(|e| format!("解析落点失败: {e}"))?;
            if !cp.starts_with(&canon_dest) {
                return Err(format!("归档条目落点越界（不安全）：{raw_name}"));
            }
        }
        let mut w =
            std::fs::File::create(&out).map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        let written = std::io::copy(&mut f.by_ref().take(MAX_FILE_BYTES + 1), &mut w)
            .map_err(|e| format!("写入文件失败 {}: {e}", out.display()))?;
        if written > MAX_FILE_BYTES {
            return Err(format!(
                "文件 {raw_name} 实际大小超过单文件上限 {} MB，已中止",
                MAX_FILE_BYTES / 1024 / 1024
            ));
        }
        total += written;
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "解压总体积超过上限 {} MB，已中止（疑似 zip 炸弹）",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
    }
    Ok(())
}

/// 若目录内容恰好只有**一个顶层目录**，返回它的名字（归档惯例：`{repo}-{ref}/…`）。
/// 不硬编码目录名——不同托管站/不同 ref 的命名并不一致。
fn single_root(entries: &[(String, bool)]) -> Option<String> {
    match entries {
        [(name, true)] => Some(name.clone()),
        _ => None,
    }
}

/// 剥掉归档的单一顶层目录；不满足「恰好一个顶层目录」时原样返回。
fn strip_single_root(dir: &Path) -> Result<PathBuf, String> {
    let mut entries = Vec::new();
    for e in std::fs::read_dir(dir).map_err(|e| format!("读取解压结果失败: {e}"))? {
        let e = e.map_err(|e| format!("读取解压结果失败: {e}"))?;
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push((e.file_name().to_string_lossy().into_owned(), is_dir));
    }
    Ok(match single_root(&entries) {
        Some(name) => dir.join(name),
        None => dir.to_path_buf(),
    })
}

// ==================== 插件名 / 版本 ====================

/// 目录改名，对 Windows 上的**瞬时占用**做有限重试。
///
/// 为什么需要：刚解压出来的文件带 Mark-of-the-Web（来源是网络下载的 zip），
/// Defender 会立刻实时扫描它们；扫描期间文件被独占打开，此时 `rename` 整个目录会得到
/// `拒绝访问 (os error 5)`。索引器、同步盘客户端也会造成同样的现象。
/// 这类占用只持续几十到几百毫秒，退避重试几次就过去了。
///
/// 只对**确实可能是瞬时占用**的错误码重试（5 拒绝访问 / 32 共享冲突 / 33 锁定冲突）；
/// 其它错误立即返回——真正的权限问题不该被重试掩盖成「慢但成功」，更不该被无限重试拖住。
fn rename_with_retry(src: &Path, dst: &Path) -> std::io::Result<()> {
    const TRANSIENT: [i32; 3] = [5, 32, 33];
    let mut delay = std::time::Duration::from_millis(40);
    let mut last = match std::fs::rename(src, dst) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    for attempt in 1..=5 {
        if !last.raw_os_error().is_some_and(|c| TRANSIENT.contains(&c)) {
            break;
        }
        std::thread::sleep(delay);
        delay *= 2; // 40 / 80 / 160 / 320 / 640ms，最坏累计约 1.2s
        match std::fs::rename(src, dst) {
            Ok(()) => {
                ilog!("[iTools] 插件包改名在第 {} 次重试后成功（此前被占用）", attempt);
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// 递归复制目录（`move_dir` 的兜底路径用）。
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 把目录搬到新位置：先试 `rename`（快，同卷是原子的），不行就退回「复制 + 删源」。
///
/// 为什么需要兜底：Windows 上对**刚解压出来的目录**做 rename 会间歇性地返回
/// `拒绝访问 (os error 5)`——安全软件扫描、索引器、同步盘都可能短暂持有句柄，
/// 而 `rename` 要求对整个目录独占。退避重试能挡住其中一部分，但实测存在**重试上千毫秒
/// 仍不放手**的情况（本轮就是：5 次退避全失败，而同一目录用复制则毫无问题）。
///
/// 复制比 rename 慢，但插件包只有几百 KB，代价可以忽略；换来的是「装不上」变成「装得上」。
/// 源目录删不掉也不算失败：它在本次安装的暂存目录里，随后会被整体清理。
fn move_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    let err = match rename_with_retry(src, dst) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    // 诊断信息留档：下次再遇到才有据可查，不必从头猜一遍
    let state = std::fs::metadata(src)
        .map(|m| format!("只读={} 目录={}", m.permissions().readonly(), m.is_dir()))
        .unwrap_or_else(|e| format!("取属性失败: {e}"));
    ilog!(
        "[iTools] 改名失败（{err}；源 {} {state}），改用复制落地",
        src.display()
    );
    copy_dir_all(src, dst)?;
    if let Err(e) = std::fs::remove_dir_all(src) {
        ilog!("[iTools] 复制成功，但清理源目录失败（不影响安装，暂存区随后统一清理）: {e}");
    }
    Ok(())
}

/// 插件名合法性：`^[a-z0-9][a-z0-9._-]{0,63}$`。
///
/// 之所以严格：这个名字**同时**是落地目录名、`itplugin://` 的路径段、
/// 以及 SQLite 命名空间 `plugin:<id>`，任何路径分隔符或点目录都可能被当成穿越。
pub(crate) fn is_valid_plugin_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 64 {
        return false;
    }
    if !(b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) {
        return false;
    }
    b.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// 语义化版本比较：`a > b` 返回 true。缺失段按 0 补齐，非数字段按 0 兜底。
/// （与 `updater::version_gt` 同思路，但**不跨模块引用私有函数**，各自独立可测。）
pub(crate) fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|x| x.trim().parse().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

// ==================== 锁文件 ====================

/// 读安装来源锁文件。**读失败/损坏一律当空表**（只告警不崩）——
/// 丢锁文件只是「这些插件变成无来源、不能自动更新」，不该让插件系统整体不可用。
///
/// 读回来的每条记录都要再过两道闸，**磁盘上的内容一律不信**
/// （`%LOCALAPPDATA%\itools\plugins\.installed.json` 是普通用户权限即可写的普通文件）：
/// 1. **重新解析 `source.url`**：用 [`parse_git_url`] 的结果整体覆盖 `source`，
///    使域名白名单在「读回时」同样生效。否则任何本地进程把 `pageUrl` 改成 `https://evil.tld/u/r`，
///    下一次「检查更新 / 更新」就会从 evil.tld 下载并原子落地——白名单形同虚设。
///    解析不过的条目直接丢弃并告警（宁可退化成「无来源、不能自动更新」）。
/// 2. **目录已不存在的条目不返回**：插件被删/被改名后，陈旧记录会让后来手工放入的同名插件
///    被冒认成「来自那个仓库」，进而显示「查看仓库 / 有新版」，用户一点更新就把手写代码整目录换掉。
///    这里做惰性对账，下一次 [`write_lock`] 会把对账结果落盘。
pub fn read_lock(root: &Path) -> LockFile {
    let path = root.join(LOCK_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return LockFile::default();
    };
    let mut lock = match serde_json::from_str::<LockFile>(&text) {
        Ok(lock) => lock,
        Err(e) => {
            ilog!("[iTools] 插件安装记录 {} 解析失败（按空表处理）: {e}", path.display());
            return LockFile::default();
        }
    };
    let mut kept = BTreeMap::new();
    for (name, mut entry) in std::mem::take(&mut lock.plugins) {
        if !root.join(&name).is_dir() {
            ilog!("[iTools] 插件 {name} 的目录已不存在，忽略其陈旧安装记录");
            continue;
        }
        match parse_git_url(&entry.source.url) {
            Ok(source) => {
                entry.source = source;
                kept.insert(name, entry);
            }
            Err(e) => ilog!("[iTools] 插件 {name} 的安装来源不合法（已忽略该记录）: {e}"),
        }
    }
    lock.plugins = kept;
    lock
}

/// 写安装来源锁文件：**先写临时文件再 rename**，避免进程中断留下半截 JSON。
///
/// 临时文件名带唯一后缀（不是固定的 `.installed.json.tmp`）：固定名意味着两个并发写入者
/// 会写同一个临时文件、再各自 rename，后者的 rename 会撞上「已被搬走的 tmp」而失败。
/// 进程内的并发已由 [`update_lock`] 的互斥挡住，唯一名再挡住**进程外**并发（多开 / 外部工具）。
/// `.` 开头 → 被 `scan_plugins` 与热重载跳过，不会污染插件扫描。
fn write_lock(root: &Path, lock: &LockFile) -> Result<(), String> {
    let path = root.join(LOCK_FILE);
    let tmp = root.join(format!("{LOCK_FILE}.{}.tmp", new_token()));
    let text = serde_json::to_string_pretty(lock).map_err(|e| format!("序列化安装记录失败: {e}"))?;
    std::fs::write(&tmp, text).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("写入安装记录失败: {e}")
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("保存安装记录失败: {e}")
    })
}

/// 锁文件「读 → 改 → 写」的进程内互斥。
///
/// 为什么必须有：`plugin_update` / `plugin_install_confirm` 都是异步命令，由 tokio 的
/// multi_thread 运行时并发执行（同步命令时代它们串行在 UI 线程上，这个竞态**当时不存在**）。
/// 插件列表每张卡片都有独立的「更新」按钮，用户先后点 A、B 两个插件就能让两次更新重叠：
/// 两边各自 `read_lock` 拿到同一份表、各自改自己那条、再各自整表写回 ——
/// 后写者会把先写者的 `resolvedVersion` 覆盖掉。丢一条记录的后果是该插件退化成
/// 「本地安装、无来源」，列表不再显示来源与更新入口（而写失败只 ilog，用户看不见）。
static LOCK_GUARD: Mutex<()> = Mutex::new(());

/// 在互斥保护下对锁文件做一次「读 → 改 → 写」。**所有改锁文件的地方都必须走这里。**
///
/// 锁只在这个同步函数内持有，不跨 `await`（调用方都在 `spawn_blocking` 的同步闭包里），
/// 因此不会与异步运行时组合出死锁。互斥被毒化（别处 panic）时直接接管内部值继续用：
/// 一次 panic 不该让「插件安装记录」永久写不进去。
fn update_lock<F: FnOnce(&mut LockFile)>(root: &Path, f: F) -> Result<(), String> {
    let _guard = LOCK_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    // read_lock 已顺带对账（目录不存在的条目本就不会返回），回写即把对账结果落盘
    let mut lock = read_lock(root);
    lock.version = LOCK_VERSION;
    f(&mut lock);
    write_lock(root, &lock)
}

/// 从安装记录里抹掉某个插件（**删除插件时必须调用**）。
///
/// 不抹会留下「陈旧来源」：用户删掉 Git 装的 foo 之后，自己手写一个同名 foo 放回 plugins/，
/// 列表就会把这个手写插件标成来自那个仓库、显示「查看仓库 / 有新版」，
/// 一点更新就用远端包把手写代码整目录替换掉（不可恢复地丢数据），且全程 UI 都在虚假归属。
///
/// 只在锁文件确实存在时改写；写失败只告警——安装记录是元数据，不该反过来阻断「删除插件」。
pub(crate) fn forget(root: &Path, name: &str) {
    if !root.join(LOCK_FILE).is_file() {
        return;
    }
    if let Err(e) = update_lock(root, |lock| {
        lock.plugins.remove(name);
    }) {
        ilog!("[iTools] 删除插件 {name} 后清理安装记录失败（不影响删除本身）: {e}");
    }
}

// ==================== 内置插件判定 ====================

static BUILTIN_NAMES: OnceLock<HashSet<String>> = OnceLock::new();

/// 启动时登记「随安装包分发的内置插件」名单（`resource_dir/plugins` 下的目录名）。
///
/// 为什么必须单独记：内置插件每次启动都会从资源目录「缺啥补啥」播种回来，
/// 允许从 Git 覆盖同名内置插件只会造成「装完又被播种文件混回去」的诡异状态，因此一律拒绝覆盖。
///
/// **登记的前提是「本次运行的插件根确实会被资源目录播种」**（[`super::is_seeded_root`]）。
/// 只看「资源目录里有没有 plugins/」是错的：`tauri.conf.json` 配了
/// `"resources": { "../plugins": "plugins" }`，**dev 下 tauri 同样会把它铺到 `target/debug/plugins`**
/// （实测那里确有 base64/deskbox/json-format/password/pixshot 五个目录），
/// 而 dev 的插件根是项目 `plugins/`——两者是两份互不相干的拷贝。按旧写法，
/// 开发机上这 5 个插件会被全部判成内置：列表挂「内置」徽标、更新与覆盖安装一律被拒，
/// 与「dev 下本就该随便改」完全相反。
/// 注意也**不能**简单地用「seed 目录 != 插件根」来判定：dev 下这两个路径本来就不同，
/// 那样判等于没判。真正的区别是「有没有播种关系」，即插件根是不是打包分支那个可写目录。
pub fn init_builtins(app: &AppHandle, plugins_root: &Path) {
    use tauri::Manager;
    let mut set = HashSet::new();
    if super::is_seeded_root(plugins_root) {
        if let Ok(res) = app.path().resource_dir() {
            if let Ok(rd) = std::fs::read_dir(res.join("plugins")) {
                for e in rd.flatten() {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        set.insert(e.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    if set.is_empty() {
        ilog!(
            "[iTools] 插件根 {} 不由随包资源播种（dev / 自定义目录），内置插件名单为空",
            plugins_root.display()
        );
    } else {
        ilog!("[iTools] 内置插件（不可被 Git 安装覆盖）：{:?}", set);
    }
    let _ = BUILTIN_NAMES.set(set);
}

/// 某插件名是否为随包内置插件。
pub fn is_builtin(name: &str) -> bool {
    BUILTIN_NAMES.get().is_some_and(|s| s.contains(name))
}

// ==================== 暂存区 ====================

/// 一次待确认的安装。
struct Pending {
    /// `<plugins_root>/.staging/<token>`
    dir: PathBuf,
    /// 校验通过的包目录 `<dir>/pkg`（确认时整体 rename 到位）。
    pkg: PathBuf,
    name: String,
    version: String,
    source: GitSource,
    created: SystemTime,
}

/// 安装暂存区（managed state）：预览与确认之间的中转。
#[derive(Default)]
pub struct InstallStaging {
    pending: Mutex<HashMap<String, Pending>>,
}

impl InstallStaging {
    fn put(&self, token: String, p: Pending) {
        if let Ok(mut g) = self.pending.lock() {
            g.insert(token, p);
        }
    }

    fn take(&self, token: &str) -> Option<Pending> {
        self.pending.lock().ok()?.remove(token)
    }

    /// 清掉超时未确认的暂存（用户开了预览就走了 / 前端崩了）。
    fn sweep(&self) {
        let Ok(mut g) = self.pending.lock() else {
            return;
        };
        let now = SystemTime::now();
        let expired: Vec<String> = g
            .iter()
            .filter(|(_, p)| {
                now.duration_since(p.created)
                    .map(|d| d > STAGING_TTL)
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            if let Some(p) = g.remove(&k) {
                let _ = std::fs::remove_dir_all(&p.dir);
            }
        }
    }
}

/// 生成暂存句柄：时间戳 + 进程随机哈希（`RandomState` 每进程密钥随机），文件名安全。
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(nanos);
    h.write_u32(std::process::id());
    format!("{nanos:016x}{:016x}", h.finish())
}

/// 清理遗留暂存目录：进程崩溃时 `.staging/` 里可能留下半截包，启动时整个删掉。
///
/// **只清 `.staging/`**：回滚失败寄存的旧插件在 `.recover-<name>/`（见 [`RECOVER_PREFIX`]），
/// 那是用户插件的唯一副本，绝不能被这里连坐删掉——交给 [`recover_orphans`] 处理。
pub fn cleanup_staging(root: &Path) {
    let dir = root.join(STAGING_DIR);
    if dir.is_dir() {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => ilog!("[iTools] 已清理遗留的插件安装暂存目录"),
            Err(e) => ilog!("[iTools] 清理插件安装暂存目录失败（不影响使用）: {e}"),
        }
    }
}

/// 启动恢复：把「更新失败且回滚也失败」寄存下来的旧插件搬回原位。
///
/// 触发场景：更新时杀软 / 资源管理器锁住目录，两次 `rename` 都失败，
/// [`atomic_place`] 把旧插件留在 `<root>/.recover-<name>`。此时插件目录是空的，
/// 用户最自然的反应就是重启 iTools——重启这一刻正好是文件锁已释放的时机，直接自动搬回。
///
/// 若 `<root>/<name>` 已存在（用户自己抢救过、或后来重装了），**不覆盖也不删除**，
/// 只把残留路径打进日志由用户处置：这份副本可能是用户手写插件的唯一存档。
pub fn recover_orphans(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir_name = e.file_name().to_string_lossy().into_owned();
        let Some(name) = dir_name.strip_prefix(RECOVER_PREFIX) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let target = root.join(name);
        if target.exists() {
            ilog!(
                "[iTools] 插件 {name} 存在待恢复副本 {}，但目标目录已存在，未自动覆盖（请自行处置）",
                e.path().display()
            );
            continue;
        }
        match std::fs::rename(e.path(), &target) {
            Ok(()) => ilog!("[iTools] 已把上次更新失败寄存的插件 {name} 恢复回原位"),
            Err(err) => ilog!(
                "[iTools] 恢复插件 {name} 失败（副本仍在 {}）: {err}",
                e.path().display()
            ),
        }
    }
}

// ==================== 下载 + 校验（预览/更新共用） ====================

/// 一个已下载、已解压、已校验通过的待落地包。
struct StagedPackage {
    dir: PathBuf,
    pkg: PathBuf,
    plugin: super::LoadedPlugin,
    /// 本次归档实际来自哪个源（官方直连 / 某镜像），供 UI 如实展示。
    download_source: String,
    /// 是否经由第三方镜像（镜像可篡改内容）。
    via_mirror: bool,
    /// 本次下载是否做过 sha256 校验（无可信哈希时为 false，UI 必须如实提示）。
    hash_verified: bool,
}

/// 下载归档 → 安全解压到暂存 → 剥单一顶层目录 → 定位 `?path=` 子目录 → 解析校验 plugin.json。
///
/// `expect_sha256`：调用方若有**可信**的归档哈希（未来插件市场场景）就传进来，
/// 不匹配即整包拒绝；手动粘 URL 安装没有可信来源，传 `None`，
/// 并由 [`StagedPackage::hash_verified`] 如实回传「未校验」。
///
/// 失败时自动清掉本次暂存目录（不留垃圾）。**不碰** `<plugins_root>/<name>`。
fn stage_from_source(
    root: &Path,
    source: &GitSource,
    token: &str,
    expect_sha256: Option<String>,
    expect_content_hash: Option<&str>,
) -> Result<StagedPackage, String> {
    let staging_root = root.join(STAGING_DIR);
    std::fs::create_dir_all(&staging_root).map_err(|e| format!("创建暂存目录失败: {e}"))?;
    let dir = staging_root.join(token);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建暂存目录失败: {e}"))?;

    let result = (|| -> Result<StagedPackage, String> {
        let got = download_capped(
            source,
            mirror::Kind::Archive,
            "",
            Duration::from_secs(ARCHIVE_TIMEOUT_SECS),
            MAX_ARCHIVE_BYTES,
            "下载插件归档",
            expect_sha256,
        )?;
        let (download_source, via_mirror, hash_verified) =
            (got.source_label, got.via_mirror, got.hash_verified);
        let raw = dir.join("raw");
        extract_zip(got.bytes, &raw)?;
        let stripped = strip_single_root(&raw)?;
        // 定位仓库内子目录（?path=）。sub_path 已在解析期禁止 .. 段与盘符/问号等字符。
        let mut src = stripped;
        for seg in source.sub_path.split('/').filter(|s| !s.is_empty()) {
            src.push(seg);
        }
        if !src.is_dir() {
            return Err(if source.sub_path.is_empty() {
                "归档内容异常：没有找到插件目录".to_string()
            } else {
                format!("仓库中不存在子目录 {}", source.sub_path)
            });
        }
        // 落点复核（纵深防御）：拼出来的 src 必须仍在本次暂存目录内。
        // 单靠 normalize_sub_path 的字符白名单已足够，但 `PathBuf::push` 在 Windows 上遇到
        // 带前缀的段会**整体替换 self**（std 文档明载），一旦白名单将来被放宽，
        // 下面那句 `rename(&src, &pkg)` 就会把沙盒外的目录整个搬走——这里断言死这条底线。
        let (canon_dir, canon_src) = (
            dir.canonicalize().map_err(|e| format!("解析暂存目录失败: {e}"))?,
            src.canonicalize().map_err(|e| format!("解析插件子目录失败: {e}"))?,
        );
        if !canon_src.starts_with(&canon_dir) {
            return Err(format!("子目录越界（不安全）：{}", source.sub_path));
        }
        let src = canon_src;
        let manifest_path = src.join("plugin.json");
        if !manifest_path.exists() {
            return Err(format!(
                "{} 下没有 plugin.json（若插件在仓库子目录里，请用 ?path=/子/目录 指定）",
                if source.sub_path.is_empty() {
                    "仓库根目录".to_string()
                } else {
                    source.sub_path.clone()
                }
            ));
        }
        // 复用运行期同一套校验（name 非空 / 有 index.html / features 非空 / code 不重复）
        let plugin = super::load_one(&src, &manifest_path)?;
        if !is_valid_plugin_name(&plugin.manifest.name) {
            return Err(format!(
                "plugin.json 的 name「{}」不合法：只允许小写字母数字与 . _ -，须以字母或数字开头，长度 ≤ 64",
                plugin.manifest.name
            ));
        }
        // 落地前把包移到固定位置 `<dir>/pkg`，确认时一次 rename 即到位
        let pkg = dir.join("pkg");
        move_dir(&src, &pkg).map_err(|e| {
            format!(
                "整理插件包失败: {e}\n\
                 （若持续出现「拒绝访问」，多为安全软件正在扫描刚解压的文件，\
                 可稍后重试，或把 {} 加入杀软排除目录）",
                dir.display()
            )
        })?;

        // **市场安装的收口**：校验的是解压后的**内容**而不是 zip 本身。
        // GitHub 的归档不是字节确定的（压缩实现一变，同一 commit 的 zip 哈希就变），
        // 所以可信哈希记的是内容哈希，见 `market::content_hash` 的说明。
        // 校验在这里做（包已就位、尚未落地）：不匹配直接 Err，外层会连暂存目录一起删掉。
        let hash_verified = match expect_content_hash {
            Some(expect) => {
                super::market::verify_content_hash(&pkg, expect)?;
                true
            }
            // 没有可信哈希来源（手工粘 URL 安装）时，沿用归档层的校验结论——
            // 那一层通常也是 false，并会被如实回传给 UI，绝不假装校验过。
            None => hash_verified,
        };

        let plugin = super::LoadedPlugin {
            manifest: plugin.manifest,
            dir: pkg.clone(),
        };
        Ok(StagedPackage {
            dir: dir.clone(),
            pkg,
            plugin,
            download_source,
            via_mirror,
            hash_verified,
        })
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// 从**已下载好的 zip 字节**准备暂存包（插件市场用）。
///
/// 与 [`stage_from_source`] 的差别只有两点：包不是这里下载的，且没有 `?path=` 子目录
/// （市场包的根就是插件目录）。**其余校验完全共用同一批函数**——
/// [`extract_zip`]（路径穿越 / 可执行文件 / 体积 / 条目数）、[`super::load_one`]（清单）、
/// [`is_valid_plugin_name`]、内容哈希。分成两套实现早晚会有一边漏掉某个检查。
fn stage_from_bytes(
    root: &Path,
    bytes: Vec<u8>,
    token: &str,
    expect_content_hash: Option<&str>,
    download_source: &str,
) -> Result<StagedPackage, String> {
    let staging_root = root.join(STAGING_DIR);
    std::fs::create_dir_all(&staging_root).map_err(|e| format!("创建暂存目录失败: {e}"))?;
    let dir = staging_root.join(token);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建暂存目录失败: {e}"))?;

    let result = (|| -> Result<StagedPackage, String> {
        let raw = dir.join("raw");
        extract_zip(bytes, &raw)?;
        // 市场包通常直接压的是插件目录内容（plugin.json 在 zip 根），
        // 但作者用资源管理器压缩会多套一层——服务端两种都收，这里也两种都认。
        let src = strip_single_root(&raw)?;
        let canon_dir = dir.canonicalize().map_err(|e| format!("解析暂存目录失败: {e}"))?;
        let src = src.canonicalize().map_err(|e| format!("解析插件目录失败: {e}"))?;
        if !src.starts_with(&canon_dir) {
            return Err("插件包内容越界（不安全），已拒绝".to_string());
        }
        let manifest_path = src.join("plugin.json");
        if !manifest_path.exists() {
            return Err("插件包里没有 plugin.json（包可能已损坏）".to_string());
        }
        let plugin = super::load_one(&src, &manifest_path)?;
        if !is_valid_plugin_name(&plugin.manifest.name) {
            return Err(format!(
                "plugin.json 的 name「{}」不合法：只允许小写字母数字与 . _ -，须以字母或数字开头，长度 ≤ 64",
                plugin.manifest.name
            ));
        }
        let pkg = dir.join("pkg");
        move_dir(&src, &pkg).map_err(|e| format!("整理插件包失败: {e}"))?;

        // 与 Git 安装同一条收口：有可信哈希就必须逐字节对上，没有就如实标注「未校验」。
        let hash_verified = match expect_content_hash {
            Some(expect) => {
                super::market::verify_content_hash(&pkg, expect)?;
                true
            }
            None => false,
        };

        let plugin = super::LoadedPlugin {
            manifest: plugin.manifest,
            dir: pkg.clone(),
        };
        Ok(StagedPackage {
            dir: dir.clone(),
            pkg,
            plugin,
            download_source: download_source.to_string(),
            // 市场包直接从配置的服务器下载，中间没有第三方镜像
            via_mirror: false,
            hash_verified,
        })
    })();

    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

/// 统计目录内文件数与总字节（给预览展示「这包有多大」）。
fn dir_stats(dir: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if e.file_type().is_file() {
            files += 1;
            if let Ok(m) = e.metadata() {
                bytes += m.len();
            }
        }
    }
    (files, bytes)
}

/// 读 README.md（超长截断，避免一次 IPC 传几 MB 文本）。
/// 截断提示计入配额，保证返回值**始终** ≤ [`MAX_README_CHARS`] 字符。
fn read_readme(dir: &Path) -> Option<String> {
    const NOTICE: &str = "\n\n…（README 过长，已截断）";
    let text = std::fs::read_to_string(dir.join("README.md")).ok()?;
    if text.chars().count() <= MAX_README_CHARS {
        return Some(text);
    }
    let keep = MAX_README_CHARS - NOTICE.chars().count();
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(NOTICE);
    Some(out)
}

/// 收集清单里的关键字（供预览/列表展示）。
fn keyword_cmds(manifest: &super::PluginManifest) -> Vec<String> {
    let mut out = Vec::new();
    for f in &manifest.features {
        for c in &f.cmds {
            if let super::Cmd::Keyword(k) = c {
                out.push(k.clone());
            }
        }
    }
    out
}

/// 「回滚失败时旧插件的寄存目录」路径：`<root>/.recover-<name>`。见 [`RECOVER_PREFIX`]。
///
/// `pub(crate)`：`commands::delete_plugin` 必须能一并删掉它——否则用户显式删除的插件
/// 会在下次启动被 [`recover_orphans`] 搬回来「复活」（详见那里的注释）。
pub(crate) fn recover_dir(root: &Path, name: &str) -> PathBuf {
    root.join(format!("{RECOVER_PREFIX}{name}"))
}

/// 原子替换：把 `pkg` 落到 `target`。
///
/// 步骤：目标已存在 → 先 rename 到 `old_dir`（同盘，瞬时）→ 再 rename `pkg` 到 `target`；
/// 第二步失败必须把 old 搬回原位（否则用户会「更新失败顺带丢了原插件」），成功后删除 old。
///
/// `old_dir` 必须由 [`recover_dir`] 给出（`<root>/.recover-<name>`）：两次 rename 都失败时
/// 它是用户旧插件的**唯一副本**，放在 `.staging/` 下会被下次启动的 [`cleanup_staging`] 删光。
fn atomic_place(pkg: &Path, target: &Path, old_dir: &Path) -> Result<(), String> {
    let had_old = target.exists();
    if had_old {
        let _ = std::fs::remove_dir_all(old_dir);
        move_dir(target, old_dir)
            .map_err(|e| format!("移开旧插件目录失败（可能有程序正占用该目录）: {e}"))?;
    }
    // 与暂存阶段同理：Windows 上刚解压/复制出来的目录常被安全软件短暂独占，
    // 直接 rename 会得到「拒绝访问」。move_dir 会先重试、再退回复制，保证装得上。
    if let Err(e) = move_dir(pkg, target) {
        if had_old {
            // 回滚：把旧目录搬回去，保证失败后现场不变
            if let Err(e2) = std::fs::rename(old_dir, target) {
                ilog!("[iTools] 严重：安装失败且旧插件目录回滚失败，残留于 {}: {e2}", old_dir.display());
                return Err(format!(
                    "安装失败且回滚失败：{e}；旧插件已寄存于 {}，重启 iTools 会自动搬回原位",
                    old_dir.display()
                ));
            }
        }
        return Err(format!("落地插件目录失败: {e}"));
    }
    if had_old {
        let _ = std::fs::remove_dir_all(old_dir);
    }
    Ok(())
}

/// 插件集合变动后的收尾：重扫插件、刷新搜索索引，并广播 `plugins-changed` 事件。
///
/// 事件的订阅方是**管理中心的插件页**（`src/admin/plugins.ts` 里 `listen("plugins-changed")`），
/// 收到后重新拉列表，使变动结果不必手动刷新即可见。
/// `plugin::watch`（插件目录热更新）发的是**同名事件**，两条路径共用同一个订阅者。
/// 没有订阅者时 `emit` 是无副作用的空发，不影响本函数其余步骤。
///
/// **调用方必须是插件集合的每一处变动**：安装（[`plugin_install_confirm`]）、
/// 更新（[`plugin_update`]）、删除（`commands::delete_plugin`，`pub(super)` 就是为了它）。
/// 少接一处，那条路径的结果就要等用户手动刷新才可见——上一版的注释写了「删除」、
/// 实现却没接，属于注释与实现不符，别再退回去。
pub(super) fn refresh_after_change(app: &AppHandle, registry: &PluginRegistry, settings: &SettingsStore, index: &SearchIndex) {
    let _ = index; // 索引由 dev::apply_plugin_search 统一写（它还要并入调试插件命令）
    let cmds = registry.reload(&settings.get().disabled_plugins);
    crate::dev::apply_plugin_search(app, cmds);
    let _ = app.emit("plugins-changed", ());
}

/// 当前时间的 RFC3339 字符串。`pub(crate)`：镜像配置缓存也要记「上次拉取时间」，共用同一种格式。
pub(crate) fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

// ==================== 命令 ====================

/// 命令：解析地址并**预览**要安装的插件（下载 + 解压 + 校验，但**不落地**）。
///
/// 预览成功后包已在暂存区待命，前端应在用户确认后调 [`plugin_install_confirm`]、
/// 放弃时调 [`plugin_install_cancel`]；都没调也会在 30 分钟后被自动清理。
///
/// **线程模型（写错过两次，务必别再退回去）**：
/// 1. 不带 `async` 的命令走 `ExecutionContext::Blocking`——tauri-macros 的 `body_blocking`
///    把函数体直接内联进 IPC handler，而 Windows 上 IPC handler 由 WebView2 controller 所属的
///    **主 UI 线程**调用。本命令最长阻塞 120 秒下载归档，那 120 秒里消息泵被占死。
/// 2. 只加 `(async)` 但函数体仍是同步的**同样不够**：`body_async` 生成的
///    `async move { $path(args) }` 里，整段阻塞是在 tauri `async_runtime` 的 tokio worker 上跑的
///    （`Runtime::new()` → multi_thread，worker 数 = CPU 核数）。1~2 vCPU 的机器上，
///    一次 120 秒安装就能把可用 worker 占满，让 `plugin_fetch` / `load_icons` / `sync_now` /
///    `data_usage` 一起排队——问题只是从「卡 UI 线程」搬到了「卡异步运行时」。
/// 3. 因此正确写法是 `pub async fn` + [`tauri::async_runtime::spawn_blocking`]，
///    把阻塞段挪到专用阻塞线程池（与 `updater::download_update` 一致）。
///
/// `State<'_, T>` 不能跨 `spawn_blocking`（闭包要求 `'static`），所以进闭包前先把
/// `registry.root` 等按值克隆出来；`staging` 的读写留在闭包外（都是瞬时的内存操作）。
#[tauri::command(async)]
pub async fn plugin_install_preview(
    url: String,
    registry: State<'_, PluginRegistry>,
    staging: State<'_, InstallStaging>,
) -> Result<InstallPreview, String> {
    // 手动粘 URL 安装没有可信哈希来源 → None（下载结果会如实标注「未经校验」）
    preview_impl(&url, None, &registry, &staging).await
}

/// 供 `market` 模块复用预览链路：包已经下载好，带着索引给的可信内容哈希。
///
/// `source_url` 是 `itools-market://<name>`（来源身份，见 [`MARKET_SCHEME`]），
/// `server_label` 是实际下载它的服务器地址，只用于在预览里如实展示「这包从哪来的」。
pub(super) async fn preview_from_market_package(
    bytes: Vec<u8>,
    source_url: &str,
    version_hint: &str,
    server_label: &str,
    expect_content: Option<String>,
    registry: &PluginRegistry,
    staging: &InstallStaging,
) -> Result<InstallPreview, String> {
    let source = parse_git_url(source_url)?;
    let root = registry.root.clone();
    staging.sweep();

    let installed: Vec<(String, String)> = registry
        .plugins
        .read()
        .map(|g| {
            g.iter()
                .map(|p| (p.manifest.name.clone(), p.manifest.version.clone()))
                .collect()
        })
        .unwrap_or_default();

    let token = new_token();
    let tk = token.clone();
    let label = if server_label.is_empty() {
        "插件市场".to_string()
    } else {
        format!("插件市场（{server_label}）")
    };
    let staged = tauri::async_runtime::spawn_blocking(move || {
        let staged = stage_from_bytes(&root, bytes, &tk, expect_content.as_deref(), &label)?;
        let stats = dir_stats(&staged.pkg);
        let logo = super::read_logo(&staged.pkg, &staged.plugin.manifest.icon);
        let readme = read_readme(&staged.pkg);
        let prior = read_lock(&root)
            .plugins
            .get(&staged.plugin.manifest.name)
            .map(|e| e.source.url.clone());
        Ok::<_, String>((staged, stats, logo, readme, prior))
    })
    .await
    .map_err(|e| format!("插件预览任务异常终止: {e}"))?;
    let (staged, (file_count, total_bytes), logo, readme, prior_url) = staged?;

    let manifest = staged.plugin.manifest.clone();
    let name = manifest.name.clone();
    // 包里的版本才算数：索引里的 version 只是个提示，两者不一致说明服务端数据有问题
    if !version_hint.is_empty() && version_hint != manifest.version {
        ilog!(
            "[iTools] 市场条目 {name} 声称 v{version_hint}，包里的 plugin.json 写的是 v{}",
            manifest.version
        );
    }
    let installed_version = installed.into_iter().find(|(n, _)| n == &name).map(|(_, v)| v);

    let preview = InstallPreview {
        token: token.clone(),
        name: name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        author: manifest.author.clone(),
        permissions: manifest.permissions.clone(),
        feature_count: manifest.features.len(),
        cmds: keyword_cmds(&manifest),
        logo,
        readme,
        source: source.clone(),
        already_installed: installed_version.is_some(),
        installed_version,
        same_source: prior_url.as_deref() == Some(source.url.as_str()),
        is_builtin: is_builtin(&name),
        file_count,
        total_bytes,
        download_source: staged.download_source.clone(),
        via_mirror: staged.via_mirror,
        hash_verified: staged.hash_verified,
    };
    staging.put(
        token,
        Pending {
            dir: staged.dir,
            pkg: staged.pkg,
            name,
            version: manifest.version.clone(),
            source,
            created: SystemTime::now(),
        },
    );
    Ok(preview)
}

/// 预览的公共实现：手动粘 URL 与从市场安装共用同一条链路。
///
/// 唯一的差别是 `expect_content`：市场条目带着审核时算出的内容哈希，
/// 手动安装没有可信来源。抽成一个函数是为了让两条路的**安全校验完全一致**——
/// 分成两份实现，早晚会有一边漏掉某个检查。
async fn preview_impl(
    url: &str,
    expect_content: Option<String>,
    registry: &PluginRegistry,
    staging: &InstallStaging,
) -> Result<InstallPreview, String> {
    let source = parse_git_url(url)?;
    let root = registry.root.clone();
    staging.sweep();

    // 已装同名插件的现状（供 UI 展示「将从 x.y.z 覆盖为 a.b.c」）。
    // 注意：这里还不知道包里的 name，先整表取出，拿到 manifest 后再比对。
    let installed: Vec<(String, String)> = registry
        .plugins
        .read()
        .map(|g| {
            g.iter()
                .map(|p| (p.manifest.name.clone(), p.manifest.version.clone()))
                .collect()
        })
        .unwrap_or_default();

    let token = new_token();
    let tk = token.clone();
    let src = source.clone();
    let staged = tauri::async_runtime::spawn_blocking(move || {
        let staged = stage_from_source(&root, &src, &tk, None, expect_content.as_deref())?;
        let stats = dir_stats(&staged.pkg);
        let logo = super::read_logo(&staged.pkg, &staged.plugin.manifest.icon);
        let readme = read_readme(&staged.pkg);
        // 锁文件里原本记的来源（判「同源覆盖」用）——同样是磁盘 I/O，一并放进阻塞线程
        let prior = read_lock(&root)
            .plugins
            .get(&staged.plugin.manifest.name)
            .map(|e| e.source.url.clone());
        Ok::<_, String>((staged, stats, logo, readme, prior))
    })
    .await
    .map_err(|e| format!("插件预览任务异常终止: {e}"))?;
    let (staged, (file_count, total_bytes), logo, readme, prior_url) = staged?;

    let manifest = staged.plugin.manifest.clone();
    let name = manifest.name.clone();
    let installed_version = installed
        .into_iter()
        .find(|(n, _)| n == &name)
        .map(|(_, v)| v);

    let preview = InstallPreview {
        token: token.clone(),
        name: name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        author: manifest.author.clone(),
        permissions: manifest.permissions.clone(),
        feature_count: manifest.features.len(),
        cmds: keyword_cmds(&manifest),
        logo,
        readme,
        source: source.clone(),
        already_installed: installed_version.is_some(),
        installed_version,
        // 与 plugin_install_confirm 的清授权条件严格一致（那里也是比 prior_url == source.url）
        same_source: prior_url.as_deref() == Some(source.url.as_str()),
        is_builtin: is_builtin(&name),
        file_count,
        total_bytes,
        download_source: staged.download_source.clone(),
        via_mirror: staged.via_mirror,
        hash_verified: staged.hash_verified,
    };
    staging.put(
        token,
        Pending {
            dir: staged.dir,
            pkg: staged.pkg,
            name,
            version: manifest.version.clone(),
            source,
            created: SystemTime::now(),
        },
    );
    Ok(preview)
}

/// 命令：确认安装——把暂存包原子落地到 `<plugins_root>/<name>`，写安装记录并热重载。
///
/// 安装后该插件的高危权限**一律未授权**：本命令不写 `plugin_permissions`，
/// 并且在「全新安装 / 换了来源的覆盖安装」时**主动清空**同名插件的历史授权。
///
/// 为什么必须清：授权表是按**插件名**存的。用户给 `gitee.com/alice/notes` 授过 runCommand 后，
/// 只要粘贴 `gitee.com/mallory/notes.git` 覆盖安装（或先卸载再装同名插件），
/// mallory 的代码落地即刻继承 runCommand——既是权限提升，也让安装弹窗那句
/// 「安装后默认不授权，需到插件详情逐项开启」变成谎话。
/// 同源升级（同一个 `source.url`）保留授权，那才是用户真正授权过的那个插件。
///
/// 线程模型同 [`plugin_install_preview`]：`pub async fn` + `spawn_blocking`。本命令的
/// `read_lock` + `atomic_place`（两次 rename）+ 暂存树 `remove_dir_all`（上限 32MB / 4000 文件）
/// + `update_lock` 全是磁盘 I/O，慢盘或杀软实时扫描下足以卡住好几秒；
/// 写成不带 `async` 的命令就等于把这几秒直接压在 WebView2 的主 UI 线程上。
///
/// `LOCK_GUARD` 在 [`update_lock`] 内部持有，而 `update_lock` 整个跑在下面的**同步闭包**里，
/// 锁不跨 `await`（这是它能用 `std::sync::Mutex` 的前提）。
#[tauri::command(async)]
pub async fn plugin_install_confirm(
    token: String,
    app: AppHandle,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    index: State<'_, SearchIndex>,
    staging: State<'_, InstallStaging>,
) -> Result<(), String> {
    ilog!("[iTools] 收到安装确认请求 token={}", &token[..8.min(token.len())]);
    let pending = staging.take(&token).ok_or_else(|| {
        ilog!("[iTools] 安装确认失败：token={} 在暂存区里找不到", &token[..8.min(token.len())]);
        "安装会话已失效（可能已超时或已被处理），请重新解析仓库地址".to_string()
    })?;
    if is_builtin(&pending.name) {
        // 暂存树可能有几十 MB，这一下删除同样丢给阻塞线程池
        let dir = pending.dir.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        return Err(format!(
            "「{}」是随安装包分发的内置插件，不能被 Git 安装覆盖（每次启动都会被内置版本播种回来）",
            pending.name
        ));
    }
    let root = registry.root.clone();
    // State 不能跨 spawn_blocking（闭包要求 `'static`）：闭包后面还要用的几项先克隆出来
    let name = pending.name.clone();
    let version = pending.version.clone();
    let source_url = pending.source.url.clone();

    // 落地 + 写安装记录全在阻塞线程池上跑
    let same_source = tauri::async_runtime::spawn_blocking(move || -> Result<bool, String> {
        // 落地前先记下「本机原本装的是不是同一个来源」——落地后锁文件会被本次安装覆盖，就问不出来了
        let prior_url = read_lock(&root)
            .plugins
            .get(&pending.name)
            .map(|e| e.source.url.clone());
        let same_source = prior_url.as_deref() == Some(pending.source.url.as_str());

        let target = root.join(&pending.name);
        let old_dir = recover_dir(&root, &pending.name);
        let placed = atomic_place(&pending.pkg, &target, &old_dir);
        let _ = std::fs::remove_dir_all(&pending.dir);
        // 失败必须留痕：这一步是「插件装没装上」的分水岭，UI 上那行红字留不住，
        // 没有日志就只能靠复现来猜（本轮为此绕了很久）。
        if let Err(e) = &placed {
            ilog!("[iTools] 插件 {} 落地失败: {e}", pending.name);
        }
        placed?;

        // 「读 → 改 → 写」必须整段互斥：并发的另一次安装/更新会拿到同一份旧表并整表写回（见 update_lock）
        let entry = InstalledEntry {
            source: pending.source.clone(),
            resolved_version: pending.version.clone(),
            installed_at: now_rfc3339(),
        };
        if let Err(e) = update_lock(&root, |lock| {
            lock.plugins.insert(pending.name.clone(), entry);
        }) {
            // 插件已装好，只是「来源」没记住——如实告警（它将表现为「无来源、不能自动更新」）
            ilog!("[iTools] 插件 {} 安装成功但安装记录写入失败: {e}", pending.name);
        }
        Ok(same_source)
    })
    .await
    .map_err(|e| format!("插件安装任务异常终止: {e}"))??;

    // 换来源（或本机原本没有这个插件的安装记录）→ 清空同名插件的历史授权，见函数头注释。
    //
    // 这一步从「落地之后立刻」挪到了「阻塞任务返回之后」（settings 是 State，进不了闭包）。
    // 为什么不构成提权窗口：授权是在**每次调用时**由 `commands::plugin_granted` 现查
    // `plugin_permissions` 的，不是装载时快照；而新代码要等用户主动打开插件窗口才会执行。
    // 但仍必须排在 refresh_after_change **之前**——那一步才把新插件放进 registry 与搜索索引。
    if !same_source {
        let mut s = settings.get();
        if s.plugin_permissions.remove(&name).is_some() {
            settings.set(s);
            ilog!("[iTools] 插件 {name} 的安装来源已变化，已清空其历史授权（需重新逐项授权）");
        }
    }
    refresh_after_change(&app, &registry, &settings, &index);
    ilog!("[iTools] 已从 {source_url} 安装插件 {name} v{version}");
    Ok(())
}

/// 命令：放弃安装——丢弃暂存目录。
///
/// 同样是 `pub async fn` + `spawn_blocking`：整棵暂存树的 `remove_dir_all`
/// （上限 32MB / 4000 文件）不该压在主 UI 线程上。
#[tauri::command(async)]
pub async fn plugin_install_cancel(
    token: String,
    staging: State<'_, InstallStaging>,
) -> Result<(), String> {
    // 已被超时清理或重复取消：目标状态已达成，不当作错误
    let Some(p) = staging.take(&token) else {
        return Ok(());
    };
    tauri::async_runtime::spawn_blocking(move || std::fs::remove_dir_all(&p.dir))
        .await
        .map_err(|e| format!("清理暂存目录任务异常终止: {e}"))?
        .map_err(|e| format!("清理暂存目录失败: {e}"))
}

/// 检查一个插件的远端版本（只拉 raw 的 plugin.json，通常 < 1KB）。
fn check_one(name: String, current: String, entry: Option<InstalledEntry>) -> PluginUpdate {
    let base = PluginUpdate {
        name,
        current_version: current.clone(),
        latest_version: None,
        has_update: false,
        pinned: false,
        checked: false,
        error: None,
        download_source: None,
        via_mirror: false,
    };
    // 没有来源（手工放入 / 内置）：如实返回「未检查」，绝不伪装成「已是最新」
    let Some(entry) = entry else {
        return base;
    };
    // 市场来源：比索引里的版本，不去请求任何 Git 托管站
    if entry.source.is_market() {
        return match super::market::latest_version(&entry.source.repo) {
            Ok(Some(latest)) => PluginUpdate {
                has_update: version_gt(&latest, &current),
                latest_version: Some(latest),
                checked: true,
                ..base
            },
            // 条目从市场消失了（被下架 / 换名）——这不是「已是最新」，如实说清楚
            Ok(None) => PluginUpdate {
                error: Some("市场里已经没有这个插件了（可能已被下架）".to_string()),
                ..base
            },
            Err(e) => PluginUpdate {
                error: Some(e),
                ..base
            },
        };
    }
    if is_pinned(&entry.source.revision) {
        return PluginUpdate {
            pinned: true,
            ..base
        };
    }
    let got = match download_capped(
        &entry.source,
        mirror::Kind::Raw,
        &entry.source.manifest_rel(),
        Duration::from_secs(META_TIMEOUT_SECS),
        MAX_META_BYTES,
        "检查插件更新",
        None,
    ) {
        Ok(b) => b,
        Err(e) => {
            return PluginUpdate {
                error: Some(e),
                ..base
            }
        }
    };
    let base = PluginUpdate {
        download_source: Some(got.source_label),
        via_mirror: got.via_mirror,
        ..base
    };
    #[derive(Deserialize)]
    struct Probe {
        #[serde(default)]
        version: String,
    }
    let latest = match serde_json::from_slice::<Probe>(&got.bytes) {
        Ok(p) if !p.version.trim().is_empty() => p.version.trim().to_string(),
        Ok(_) => {
            return PluginUpdate {
                error: Some("远端 plugin.json 没有 version 字段".to_string()),
                ..base
            }
        }
        Err(e) => {
            return PluginUpdate {
                error: Some(format!("远端 plugin.json 解析失败: {e}")),
                ..base
            }
        }
    };
    PluginUpdate {
        has_update: version_gt(&latest, &current),
        latest_version: Some(latest),
        checked: true,
        ..base
    }
}

/// 命令：检查所有插件的更新。
///
/// 轻量策略：**只拉远端 raw 的 plugin.json 比版本号**，不下载归档。
/// 逐插件并发（插件数量个位数，线程开销远小于串行等待网络）。
/// 返回的每一项都如实标注 `checked` / `pinned` / `error`——UI 必须区分
/// 「已是最新」「未检查」「检查失败」，不许一律显示成最新。
///
/// 线程模型同 [`plugin_install_preview`]：`pub async fn` + `spawn_blocking`。
/// 前端每次进插件页都会自动跑一次本命令；同步版会锁死主 UI 线程最多 10 秒，
/// 而「同步函数体 + `(async)`」只是把它挪去占满异步运行时的 worker（安装期尤其致命）。
/// `thread::scope` 仍在闭包内部使用——它只是把各插件的网络请求并发起来，
/// 现在整段跑在专用阻塞线程上，既不占 UI 线程也不占 tokio worker。
///
/// 取 registry 用的是 `AppHandle` 而非 `State<'_, _>`：本命令的返回值不是 `Result`，
/// 而 tauri 对「非 Result 的 async 命令」要求整个 future 是 `'static`（`ipc/command.rs` 里
/// `F: Future<Output = T> + Send + 'static`），带生命周期的 `State<'_, _>` 参数过不了这条界。
/// 为不改动前端契约（原本就返回数组、失败时是空数组），这里改用自持的 `AppHandle`。
#[tauri::command(async)]
pub async fn plugin_check_updates(app: AppHandle) -> Vec<PluginUpdate> {
    use tauri::Manager;
    // borrow 不跨 await：先在同步块里把需要的值克隆出来
    let Some((root, loaded)) = ({
        let registry = app.state::<PluginRegistry>();
        let root = registry.root.clone();
        registry.plugins.read().ok().map(|g| {
            let loaded: Vec<(String, String)> = g
                .iter()
                .map(|p| (p.manifest.name.clone(), p.manifest.version.clone()))
                .collect();
            (root, loaded)
        })
    }) else {
        return Vec::new();
    };
    tauri::async_runtime::spawn_blocking(move || {
        let lock = read_lock(&root);
        let targets: Vec<(String, String, Option<InstalledEntry>)> = loaded
            .into_iter()
            .map(|(name, ver)| {
                let entry = lock.plugins.get(&name).cloned();
                (name, ver, entry)
            })
            .collect();
        std::thread::scope(|s| {
            let handles: Vec<_> = targets
                .into_iter()
                .map(|(name, ver, entry)| {
                    let fallback = name.clone();
                    let fallback_ver = ver.clone();
                    (
                        s.spawn(move || check_one(name, ver, entry)),
                        fallback,
                        fallback_ver,
                    )
                })
                .collect();
            handles
                .into_iter()
                .map(|(h, name, ver)| {
                    h.join().unwrap_or_else(|_| PluginUpdate {
                        name,
                        current_version: ver,
                        latest_version: None,
                        has_update: false,
                        pinned: false,
                        checked: false,
                        error: Some("检查线程异常退出".to_string()),
                        download_source: None,
                        via_mirror: false,
                    })
                })
                .collect()
        })
    })
    .await
    // 任务被取消 / panic：返回空表（前端本就把「没有条目」当作未检查，不会伪装成「已是最新」）
    .unwrap_or_else(|e| {
        ilog!("[iTools] 检查插件更新任务异常终止: {e}");
        Vec::new()
    })
}

/// 命令：把某个插件更新到其来源的最新版本。
///
/// 按锁文件里的来源重新下载 → 校验（**新包的 name 必须与原插件一致**，否则拒绝，
/// 避免「更新」把 A 插件换成 B 插件）→ 原子替换目录 → 更新锁文件 → 热重载。
/// 用户数据不在插件目录（在 SQLite 与 plugin-data/），因此替换不丢数据。
///
/// 线程模型同 [`plugin_install_preview`]：`pub async fn` + `spawn_blocking`（本命令会下载整个归档）。
///
/// 授权在这里**保留**：来源就是锁文件里那条（已被 [`read_lock`] 用白名单重新校验过），
/// 是用户当初授权的同一个仓库，属同源升级。换来源的覆盖安装走 [`plugin_install_confirm`]，那里会清空授权。
#[tauri::command(async)]
pub async fn plugin_update(
    name: String,
    app: AppHandle,
    registry: State<'_, PluginRegistry>,
    settings: State<'_, SettingsStore>,
    index: State<'_, SearchIndex>,
) -> Result<PluginUpdate, String> {
    if is_builtin(&name) {
        return Err(format!("「{name}」是随安装包分发的内置插件，请通过应用整体更新升级"));
    }
    // name 来自前端，会被用于拼 `<root>/<name>` 与 `.recover-<name>`；此处收口成与安装期同一套白名单
    if !is_valid_plugin_name(&name) {
        return Err(format!("非法的插件名：{name}"));
    }
    let root = registry.root.clone();
    let current = registry
        .plugins
        .read()
        .ok()
        .and_then(|g| {
            g.iter()
                .find(|p| p.manifest.name == name)
                .map(|p| p.manifest.version.clone())
        })
        .ok_or_else(|| format!("插件 {name} 未加载，无法更新"))?;

    // 下载 + 原子替换 + 改锁文件全在阻塞线程池上跑；State 不跨 spawn_blocking，只带走 root/name
    let n = name.clone();
    let updated = tauri::async_runtime::spawn_blocking(move || -> Result<PluginUpdate, String> {
        let entry = read_lock(&root)
            .plugins
            .get(&n)
            .cloned()
            .ok_or_else(|| format!("「{n}」不是从插件市场或 Git 仓库安装的，没有可用的更新来源"))?;

        let token = new_token();
        // 两条来源，两种下载方式，但**校验、落地、锁文件更新完全共用下面这一段**。
        let staged = if entry.source.is_market() {
            // 市场来源：拿索引里那条的内容哈希做校验（与从市场页安装同一条收口）
            let mk = super::market::find_entry(&entry.source.repo)?;
            let expect = (!mk.content_hash.is_empty()).then(|| mk.content_hash.clone());
            let label = crate::account::cloud_endpoint().unwrap_or_default();
            let bytes = super::market::fetch_package(&mk)?;
            stage_from_bytes(
                &root,
                bytes,
                &token,
                expect.as_deref(),
                &if label.is_empty() {
                    "插件市场".to_string()
                } else {
                    format!("插件市场（{label}）")
                },
            )?
        } else {
            // Git 来源：这条路径**没有**可信哈希，两个都传 None 并如实回传「未校验」——
            // 锁文件里不记内容哈希（可信来源只有市场索引，存一份旧的等于自欺）。
            stage_from_source(&root, &entry.source, &token, None, None)?
        };
        let new_name = staged.plugin.manifest.name.clone();
        let new_version = staged.plugin.manifest.version.clone();
        if new_name != n {
            let _ = std::fs::remove_dir_all(&staged.dir);
            return Err(format!(
                "远端包的插件名是「{new_name}」，与本地「{n}」不一致，已拒绝更新（避免把插件替换成另一个插件）"
            ));
        }
        let target = root.join(&n);
        let old_dir = recover_dir(&root, &n);
        let place = atomic_place(&staged.pkg, &target, &old_dir);
        let _ = std::fs::remove_dir_all(&staged.dir);
        place?;

        // 「读 → 改 → 写」整段互斥：并发更新另一个插件时不会互相覆盖记录（见 update_lock）
        let record = InstalledEntry {
            source: entry.source.clone(),
            resolved_version: new_version.clone(),
            installed_at: now_rfc3339(),
        };
        if let Err(e) = update_lock(&root, |lock| {
            lock.plugins.insert(n.clone(), record);
        }) {
            ilog!("[iTools] 插件 {n} 更新成功但安装记录写入失败: {e}");
        }
        Ok(PluginUpdate {
            name: n,
            current_version: new_version.clone(),
            latest_version: Some(new_version),
            has_update: false,
            pinned: is_pinned(&entry.source.revision),
            checked: true,
            error: None,
            download_source: Some(staged.download_source),
            via_mirror: staged.via_mirror,
        })
    })
    .await
    .map_err(|e| format!("插件更新任务异常终止: {e}"))??;

    refresh_after_change(&app, &registry, &settings, &index);
    ilog!(
        "[iTools] 插件 {name} 已更新：{current} → {}（来源：{}）",
        updated.current_version,
        updated.download_source.as_deref().unwrap_or("未知")
    );
    Ok(updated)
}

/// 命令：在系统默认浏览器打开该插件的仓库网页。无 Git 来源则返回错误（UI 应据此隐藏入口）。
///
/// **不信任存盘的 `pageUrl`**：交给 `opener::open` 的地址由 [`GitSource::verified_page_url`]
/// 用经白名单核对的 host/owner/repo **现场重建**。锁文件是普通用户权限即可写的文件，
/// 原样打开就意味着任何本地进程都能把「查看仓库」变成「启动任意 URI / 可执行文件」
/// （而按钮 title 上显示的还是原来那个 gitee.com/u/r，与真实目标不一致）。
#[tauri::command]
pub fn plugin_open_source_page(
    name: String,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let lock = read_lock(&registry.root);
    let entry = lock
        .plugins
        .get(&name)
        .ok_or_else(|| format!("「{name}」不是从 Git 仓库安装的，没有仓库页面"))?;
    let url = entry.source.verified_page_url()?;
    opener::open(&url).map_err(|e| format!("打开仓库页面失败: {e}"))
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<(String, Forge)> {
        vec![
            ("github.com".to_string(), Forge::GitHub),
            ("gitee.com".to_string(), Forge::Gitee),
        ]
    }

    /// 造一个「插件根」临时目录，并在里面建好若干插件目录（read_lock 会按目录存在与否对账）。
    fn temp_root(tag: &str, plugins: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("itools-{tag}-{}", new_token()));
        for p in plugins {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ---------- 市场来源（itools-market://<name>）----------

    #[test]
    fn market_source_parses_and_is_recognized() {
        let s = parse_git_url("itools-market://deskbox").unwrap();
        assert!(s.is_market());
        assert_eq!(s.host, MARKET_HOST);
        assert_eq!(s.repo, "deskbox");
        assert_eq!(s.url, "itools-market://deskbox");
        // 市场插件跟随索引里的最新版本，不锁 commit —— 别被当成「已锁定」而排除出更新检查
        assert!(!is_pinned(&s.revision));
    }

    #[test]
    fn market_source_survives_lock_roundtrip() {
        // read_lock 会拿 source.url 重新解析、解析不过就**丢弃整条记录**。
        // 这一条挂了的表现是：所有从市场装的插件突然集体变成「无来源、不检查更新」。
        let root = temp_root("lock-market", &["deskbox"]);
        let src = parse_git_url("itools-market://deskbox").unwrap();
        update_lock(&root, |lock| {
            lock.plugins.insert(
                "deskbox".to_string(),
                InstalledEntry {
                    source: src,
                    resolved_version: "1.0.0".into(),
                    installed_at: now_rfc3339(),
                },
            );
        })
        .unwrap();
        let back = read_lock(&root);
        let e = back.plugins.get("deskbox").expect("市场来源的记录必须能读回来");
        assert!(e.source.is_market());
        assert_eq!(e.source.repo, "deskbox");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn market_source_rejected_by_git_download_path() {
        // 市场来源没有 Git 直链可拼。万一某条路由漏了分流，必须给一句能懂的话，
        // 而不是拼出 `https:///…` 让用户看到莫名其妙的网络错误。
        let s = parse_git_url("itools-market://deskbox").unwrap();
        let err = download_capped(
            &s,
            mirror::Kind::Archive,
            "",
            Duration::from_secs(1),
            1024,
            "下载插件归档",
            None,
        )
        .err()
        .expect("市场来源必须被 Git 下载路径拒绝");
        assert!(err.contains("插件市场"), "{err}");
        assert!(err.contains("deskbox"), "{err}");
    }

    #[test]
    fn market_source_has_no_repo_page() {
        let s = parse_git_url("itools-market://deskbox").unwrap();
        // 「查看仓库」对市场插件应诚实报错，不能拼一个能打开但指向别处的地址
        assert!(s.verified_page_url().is_err());
    }

    #[test]
    fn market_source_name_is_validated() {
        assert!(parse_git_url("itools-market://Bad Name").is_err());
        assert!(parse_git_url("itools-market://../etc").is_err());
        assert!(parse_git_url("itools-market://").is_err());
    }

    // ---------- 从 zip 字节 stage（市场安装链路的客户端半段）----------

    /// 造一个最小可用的插件 zip。
    fn plugin_zip(name: &str, version: &str, extra: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("plugin.json", opts).unwrap();
            w.write_all(
                format!(
                    r#"{{"name":"{name}","version":"{version}","description":"d","features":[{{"code":"main","cmds":["{name}"]}}]}}"#
                )
                .as_bytes(),
            )
            .unwrap();
            w.start_file("index.html", opts).unwrap();
            w.write_all(b"<html></html>").unwrap();
            for (n, c) in extra {
                w.start_file(*n, opts).unwrap();
                w.write_all(c).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    #[test]
    fn stage_from_bytes_extracts_and_parses() {
        let root = temp_root("stage-bytes", &[]);
        let zip = plugin_zip("demo", "1.2.3", &[("assets/app.js", b"console.log(1)")]);
        let staged = stage_from_bytes(&root, zip, "tok1", None, "插件市场（测试）").unwrap();
        assert_eq!(staged.plugin.manifest.name, "demo");
        assert_eq!(staged.plugin.manifest.version, "1.2.3");
        assert!(staged.pkg.join("index.html").is_file());
        assert!(staged.pkg.join("assets/app.js").is_file());
        // 没给可信哈希时必须如实标注「未校验」，绝不假装校验过
        assert!(!staged.hash_verified);
        // 市场包直接从配置的服务器下载，中间没有第三方镜像
        assert!(!staged.via_mirror);
        assert_eq!(staged.download_source, "插件市场（测试）");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stage_from_bytes_verifies_content_hash() {
        let root = temp_root("stage-hash", &[]);
        let zip = plugin_zip("demo", "1.0.0", &[]);

        // 先算出这份包的真实内容哈希
        let probe = stage_from_bytes(&root, zip.clone(), "tokA", None, "市场").unwrap();
        let real = super::super::market::content_hash(&probe.pkg).unwrap();
        let _ = std::fs::remove_dir_all(&probe.dir);

        // 哈希对得上 → 通过并标记已校验
        let ok = stage_from_bytes(&root, zip.clone(), "tokB", Some(&real), "市场").unwrap();
        assert!(ok.hash_verified);
        let _ = std::fs::remove_dir_all(&ok.dir);

        // 哈希对不上 → 必须拒绝（这是「传输途中被改过」的收口）
        let bad = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let err = stage_from_bytes(&root, zip, "tokC", Some(bad), "市场")
            .err()
            .expect("内容哈希对不上必须拒绝");
        assert!(err.contains("内容校验失败"), "{err}");
        // 失败时暂存目录要被清掉，不留垃圾
        assert!(!root.join(STAGING_DIR).join("tokC").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stage_from_bytes_rejects_executables() {
        // 与 Git 安装共用同一套 extract_zip，这里钉死「市场包也过同一道闸」
        let root = temp_root("stage-exe", &[]);
        let zip = plugin_zip("demo", "1.0.0", &[("payload.exe", b"MZ")]);
        let err = stage_from_bytes(&root, zip, "tok2", None, "市场")
            .err()
            .expect("包内含可执行文件必须整包拒绝");
        assert!(err.contains(".exe"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stage_from_bytes_accepts_wrapped_single_root() {
        // 作者用资源管理器压缩会多套一层目录 —— 服务端两种都收，客户端也必须两种都认
        use std::io::Write as _;
        let root = temp_root("stage-wrap", &[]);
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            w.start_file("demo/plugin.json", opts).unwrap();
            w.write_all(
                br#"{"name":"demo","version":"1.0.0","features":[{"code":"m","cmds":["d"]}]}"#,
            )
            .unwrap();
            w.start_file("demo/index.html", opts).unwrap();
            w.write_all(b"<html></html>").unwrap();
            w.finish().unwrap();
        }
        let staged = stage_from_bytes(&root, buf, "tok3", None, "市场").unwrap();
        assert_eq!(staged.plugin.manifest.name, "demo");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_basic() {
        let s = parse_git_url_with("https://gitee.com/user/repo.git", &hosts()).unwrap();
        assert_eq!(s.host, "gitee");
        assert_eq!(s.domain, "gitee.com");
        assert_eq!(s.real_host(), "gitee.com");
        assert_eq!(s.owner, "user");
        assert_eq!(s.repo, "repo");
        assert_eq!(s.sub_path, "");
        assert_eq!(s.revision, "");
        assert_eq!(s.url, "https://gitee.com/user/repo.git");
        assert_eq!(s.page_url, "https://gitee.com/user/repo");
        assert_eq!(s.git_ref(), "HEAD");
        assert_eq!(
            s.archive_url(),
            "https://gitee.com/user/repo/repository/archive/HEAD.zip"
        );
        assert_eq!(
            s.raw_url(&s.manifest_rel()),
            "https://gitee.com/user/repo/raw/HEAD/plugin.json"
        );
    }

    #[test]
    fn parse_without_git_suffix_and_trailing_slash() {
        let s = parse_git_url_with("https://GitHub.com/User/Repo/", &hosts()).unwrap();
        assert_eq!(s.host, "github");
        assert_eq!(s.owner, "User"); // owner/repo 保留大小写（远端大小写敏感）
        assert_eq!(s.repo, "Repo");
        assert_eq!(s.url, "https://github.com/User/Repo.git");
        assert_eq!(
            s.archive_url(),
            "https://codeload.github.com/User/Repo/zip/HEAD"
        );
        assert_eq!(
            s.raw_url("plugin.json"),
            "https://raw.githubusercontent.com/User/Repo/HEAD/plugin.json"
        );
    }

    #[test]
    fn parse_www_prefix_is_equivalent() {
        let s = parse_git_url_with("https://www.gitee.com/u/r.git", &hosts()).unwrap();
        assert_eq!(s.page_url, "https://gitee.com/u/r");
    }

    #[test]
    fn parse_revision_and_path() {
        let s = parse_git_url_with("https://gitee.com/user/repo.git#v1.2.3", &hosts()).unwrap();
        assert_eq!(s.revision, "v1.2.3");
        assert_eq!(s.git_ref(), "v1.2.3");

        let s = parse_git_url_with(
            "https://github.com/user/repo.git?path=/plugins/foo",
            &hosts(),
        )
        .unwrap();
        assert_eq!(s.sub_path, "plugins/foo");
        assert_eq!(s.url, "https://github.com/user/repo.git?path=/plugins/foo");
        assert_eq!(s.manifest_rel(), "plugins/foo/plugin.json");

        let s = parse_git_url_with(
            "https://github.com/user/repo.git?path=/plugins/foo#dev",
            &hosts(),
        )
        .unwrap();
        assert_eq!(s.sub_path, "plugins/foo");
        assert_eq!(s.revision, "dev");
        assert_eq!(
            s.url,
            "https://github.com/user/repo.git?path=/plugins/foo#dev"
        );
        // 规范化后的 URL 可原样再次解析（幂等）
        let again = parse_git_url_with(&s.url, &hosts()).unwrap();
        assert_eq!(again.url, s.url);

        // path 前后斜杠都容忍
        let s = parse_git_url_with("https://gitee.com/u/r.git?path=plugins/foo/", &hosts()).unwrap();
        assert_eq!(s.sub_path, "plugins/foo");
    }

    #[test]
    fn parse_rejects_bad_input() {
        let bad = [
            "",
            "git@gitee.com:user/repo.git",
            "ssh://git@gitee.com/user/repo.git",
            "file:///C:/tmp/repo",
            "http://gitee.com/user/repo.git",
            "https://evil.com/user/repo.git",
            "https://gitee.com/user",
            "https://gitee.com/user/repo/extra",
            "https://gitee.com/user/repo.git?path=/a/../../etc",
            "https://gitee.com/user/repo.git?path=..",
            "https://gitee.com/user/repo.git?ref=main",
            "https://gitee.com/user/repo.git#bad rev",
            "https://gitee.com/user/repo.git#a..b",
            "https://gitee.com:8443/user/repo.git",
            // ?path= 段必须是 [A-Za-z0-9._-]+：盘符段会让 PathBuf::push 丢弃整个暂存根（沙盒逃逸）
            "https://gitee.com/user/repo.git?path=/C:/foo",
            "https://gitee.com/user/repo.git?path=/c:",
            // 段内的 ? 会在拼 raw/archive URL 时凭空多出 query 分隔符（并影响 with_token 的 ?/& 判断）
            "https://gitee.com/user/repo.git?path=/a?b",
            "https://gitee.com/user/repo.git?path=/a b",
        ];
        for b in bad {
            assert!(
                parse_git_url_with(b, &hosts()).is_err(),
                "本应拒绝但通过了: {b}"
            );
        }
    }

    #[test]
    fn extra_hosts_are_honored() {
        let allowed = vec![
            ("gitee.com".to_string(), Forge::Gitee),
            ("git.corp.com".to_string(), Forge::GitHub),
        ];
        let s = parse_git_url_with("https://git.corp.com/u/r.git", &allowed).unwrap();
        assert_eq!(s.host, "github");
        assert_eq!(s.page_url, "https://git.corp.com/u/r");
        // 镜像站用网页同款归档路径（codeload 只属于 github.com）
        assert_eq!(s.archive_url(), "https://git.corp.com/u/r/archive/HEAD.zip");
        assert!(parse_git_url_with("https://github.com/u/r.git", &allowed).is_err());
    }

    #[test]
    fn zip_slip_entries_rejected() {
        for bad in [
            "../evil.js",
            "a/../../evil.js",
            "/etc/passwd",
            "C:/Windows/evil.js",
            "a\\..\\..\\evil.js",
            "..",
            "",
            "a/ /b",
            // Windows 会剥掉结尾的点与空格，落点与校验不一致 → 拒绝（也堵死 denied_ext 的结尾点绕过）
            "payload.exe.",
            "a/b./c.js",
            "trailing ",
        ] {
            assert!(safe_entry_path(bad).is_err(), "本应拒绝但通过了: {bad:?}");
        }
        assert_eq!(
            safe_entry_path("repo-main/index.html").unwrap(),
            PathBuf::from("repo-main").join("index.html")
        );
        // 结尾斜杠（目录条目）与重复斜杠都应被折叠
        assert_eq!(
            safe_entry_path("repo-main//assets/").unwrap(),
            PathBuf::from("repo-main").join("assets")
        );
        // 反斜杠当分隔符处理（Windows 归档偶见）
        assert_eq!(
            safe_entry_path("repo\\a.js").unwrap(),
            PathBuf::from("repo").join("a.js")
        );
    }

    #[test]
    fn executable_extensions_rejected() {
        for bad in [
            "a.exe",
            "x/y.DLL",
            "s.Ps1",
            "m.msi",
            "j.jar",
            "c.cpl",
            // 「双击即执行 / 即改系统状态」的同类
            "s.lnk",
            "u.URL",
            "h.hta",
            "r.reg",
            "s.scf",
            "p.pif",
            "m.msc",
            // 结尾点绕过：Path::extension() 对它返回 Some("")，旧实现整包放行，
            // 而 Windows 落盘时会把点剥掉，磁盘上就是一个真正的 payload.exe
            "payload.exe.",
            "x/y.dll. ",
        ] {
            assert!(
                denied_ext(Path::new(bad)).is_some(),
                "本应拒收但放行了: {bad}"
            );
        }
        for ok in [
            "index.html",
            "a.js",
            "logo.png",
            "README.md",
            "s.json",
            // 无扩展名 / 以点开头的元数据文件不该被误伤
            "LICENSE",
            ".gitignore",
        ] {
            assert!(denied_ext(Path::new(ok)).is_none(), "本应放行但拒收了: {ok}");
        }
    }

    #[test]
    fn redact_masks_access_token() {
        // ureq 的 Transport Display 形态：完整 URL + ": " + 错误原因
        let leaked = "下载插件归档失败：https://gitee.com/u/r/repository/archive/HEAD.zip?access_token=abc123SECRET: io: timed out".to_string();
        let safe = redact_token(leaked);
        assert!(!safe.contains("abc123SECRET"), "令牌仍在: {safe}");
        assert!(safe.contains("access_token=***"), "脱敏形态不对: {safe}");
        assert!(safe.contains(": io: timed out"), "错误原因被误删: {safe}");

        // & 前截断、多次出现、大小写不敏感
        let s = redact_token("a?x=1&ACCESS_TOKEN=tok1&y=2 和 access_token=tok2".to_string());
        assert_eq!(s, "a?x=1&ACCESS_TOKEN=***&y=2 和 access_token=***");

        // 不含令牌时原样返回（含多字节字符也不能 panic）
        let plain = "检查插件更新失败：仓库不存在（中文）".to_string();
        assert_eq!(redact_token(plain.clone()), plain);
        assert_eq!(
            redact_token("access_token=中文令牌&next=1".to_string()),
            "access_token=***&next=1"
        );
    }

    #[test]
    fn single_root_detection() {
        assert_eq!(
            single_root(&[("repo-main".to_string(), true)]),
            Some("repo-main".to_string())
        );
        // 只有一个顶层文件 → 不剥
        assert_eq!(single_root(&[("plugin.json".to_string(), false)]), None);
        // 多个顶层项 → 不剥
        assert_eq!(
            single_root(&[("a".to_string(), true), ("b".to_string(), true)]),
            None
        );
        assert_eq!(single_root(&[]), None);
    }

    #[test]
    fn version_compare() {
        assert!(version_gt("1.2.3", "1.2.2"));
        assert!(version_gt("v1.3.0", "1.2.9"));
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(version_gt("1.2.1", "1.2"));
        assert!(!version_gt("1.2.3", "1.2.3"));
        assert!(!version_gt("1.2.2", "1.2.3"));
        assert!(!version_gt("1.2", "1.2.0"));
    }

    #[test]
    fn plugin_name_validation() {
        for ok in ["base64", "a", "0abc", "my.plugin_v2-x"] {
            assert!(is_valid_plugin_name(ok), "本应合法: {ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-lead",
            "UpperCase",
            "has space",
            "a/b",
            "a\\b",
            "中文名",
            &"x".repeat(65),
        ] {
            assert!(!is_valid_plugin_name(bad), "本应非法: {bad}");
        }
    }

    #[test]
    fn pinned_detection() {
        assert!(is_pinned(&"a".repeat(40)));
        assert!(is_pinned("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_pinned("main"));
        assert!(!is_pinned(""));
        assert!(!is_pinned(&"z".repeat(40)));
        assert!(!is_pinned(&"a".repeat(39)));
    }

    #[test]
    fn lock_file_roundtrip() {
        let dir = temp_root("lock-test", &["demo"]);

        // 不存在 → 空表（不报错）
        assert!(read_lock(&dir).plugins.is_empty());

        let source = parse_git_url_with(
            "https://gitee.com/u/r.git?path=/plugins/demo#v1.0.0",
            &hosts(),
        )
        .unwrap();
        let mut lock = LockFile::default();
        lock.plugins.insert(
            "demo".to_string(),
            InstalledEntry {
                source,
                resolved_version: "1.0.2".to_string(),
                installed_at: "2026-08-12T10:00:00+08:00".to_string(),
            },
        );
        write_lock(&dir, &lock).unwrap();

        let back = read_lock(&dir);
        assert_eq!(back.version, LOCK_VERSION);
        let e = back.plugins.get("demo").expect("应存在 demo 记录");
        assert_eq!(e.resolved_version, "1.0.2");
        assert_eq!(e.installed_at, "2026-08-12T10:00:00+08:00");
        assert_eq!(e.source.host, "gitee");
        assert_eq!(e.source.domain, "gitee.com");
        assert_eq!(e.source.sub_path, "plugins/demo");
        assert_eq!(e.source.revision, "v1.0.0");

        // 磁盘上确实是 camelCase 扁平结构
        let text = std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap();
        assert!(text.contains("\"subPath\""));
        assert!(text.contains("\"pageUrl\""));
        assert!(text.contains("\"domain\""));
        assert!(text.contains("\"resolvedVersion\""));
        assert!(text.contains("\"installedAt\""));

        // 损坏 → 当空表，不 panic
        std::fs::write(dir.join(LOCK_FILE), "{ not json").unwrap();
        assert!(read_lock(&dir).plugins.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老锁文件（加 `domain` 字段之前写下的）必须仍能反序列化，且真实域名回退到从 pageUrl 还原。
    #[test]
    fn legacy_lock_without_domain_is_compatible() {
        let dir = temp_root("lock-legacy", &["demo"]);
        // 注意：这段 JSON 里**没有** domain 字段，模拟升级前写下的记录
        let legacy = r#"{
          "version": 1,
          "plugins": {
            "demo": {
              "host": "github",
              "owner": "u",
              "repo": "r",
              "subPath": "",
              "revision": "",
              "url": "https://git.corp.com/u/r.git",
              "pageUrl": "https://git.corp.com/u/r",
              "resolvedVersion": "1.0.0",
              "installedAt": "2026-01-01T00:00:00+08:00"
            }
          }
        }"#;
        // 先直接验证结构体层面的兼容（不经 read_lock 的白名单校验）
        let lock: LockFile = serde_json::from_str(legacy).unwrap();
        let e = lock.plugins.get("demo").unwrap();
        assert_eq!(e.source.domain, "", "老记录没有 domain，应落到 serde default");
        assert_eq!(
            e.source.real_host(),
            "git.corp.com",
            "domain 为空时必须回退到从 pageUrl 还原，否则老记录会拼错下载地址"
        );
        assert_eq!(
            e.source.raw_url("plugin.json"),
            "https://git.corp.com/u/r/raw/HEAD/plugin.json"
        );

        // 再验证经 read_lock 时：自建站没在放行清单里 → 该条被丢弃（白名单读回时同样生效）
        std::fs::write(dir.join(LOCK_FILE), legacy).unwrap();
        assert!(
            read_lock(&dir).plugins.is_empty(),
            "未放行域名的记录必须被丢弃"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 锁文件是普通用户权限即可写的普通文件：被篡改的来源必须在**读回时**就失效。
    #[test]
    fn read_lock_revalidates_tampered_entries() {
        let dir = temp_root("lock-tamper", &["demo", "evil", "gone"]);
        let tampered = r#"{
          "version": 1,
          "plugins": {
            "demo": {
              "host": "gitee", "domain": "evil.tld", "owner": "u", "repo": "r",
              "subPath": "", "revision": "",
              "url": "https://gitee.com/u/r.git",
              "pageUrl": "file:///C:/tmp/payload.exe",
              "resolvedVersion": "1.0.0", "installedAt": "x"
            },
            "evil": {
              "host": "gitee", "domain": "evil.tld", "owner": "u", "repo": "r",
              "subPath": "", "revision": "",
              "url": "https://evil.tld/u/r.git",
              "pageUrl": "https://evil.tld/u/r",
              "resolvedVersion": "1.0.0", "installedAt": "x"
            }
          }
        }"#;
        std::fs::write(dir.join(LOCK_FILE), tampered).unwrap();
        let back = read_lock(&dir);

        // url 合法 → 整个 source 用解析结果覆盖，被改过的 domain / pageUrl 一律作废
        let demo = back.plugins.get("demo").expect("合法来源应保留");
        assert_eq!(demo.source.domain, "gitee.com");
        assert_eq!(demo.source.page_url, "https://gitee.com/u/r");
        assert_eq!(
            demo.source.verified_page_url().unwrap(),
            "https://gitee.com/u/r",
            "「查看仓库」必须用重建的 https 地址，而不是存盘的 file:// 字符串"
        );
        // url 指向未放行域名 → 整条丢弃（否则「检查更新/更新」会从 evil.tld 下载并原子落地）
        assert!(!back.plugins.contains_key("evil"), "未放行域名的记录必须丢弃");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录已不存在的条目不返回（惰性对账），且 `forget` 能把记录真正抹掉。
    #[test]
    fn stale_entries_are_reconciled_and_forgettable() {
        let dir = temp_root("lock-stale", &["alive"]);
        let mut lock = LockFile::default();
        for name in ["alive", "deleted"] {
            lock.plugins.insert(
                name.to_string(),
                InstalledEntry {
                    source: parse_git_url_with("https://gitee.com/u/r.git", &hosts()).unwrap(),
                    resolved_version: "1.0.0".to_string(),
                    installed_at: "x".to_string(),
                },
            );
        }
        write_lock(&dir, &lock).unwrap();

        let back = read_lock(&dir);
        assert!(back.plugins.contains_key("alive"));
        assert!(
            !back.plugins.contains_key("deleted"),
            "目录不存在的插件不应再被认作有 Git 来源（否则手写的同名插件会被冒认并被更新覆盖）"
        );

        // forget：删掉 alive 的目录与记录后，磁盘上的锁文件也不该再有它
        std::fs::remove_dir_all(dir.join("alive")).unwrap();
        forget(&dir, "alive");
        let text = std::fs::read_to_string(dir.join(LOCK_FILE)).unwrap();
        assert!(!text.contains("alive"), "记录未被抹掉: {text}");
        assert!(!text.contains("deleted"), "对账结果应一并落盘: {text}");

        // 锁文件不存在时 forget 不该凭空造一个
        let empty = temp_root("lock-none", &[]);
        forget(&empty, "whatever");
        assert!(!empty.join(LOCK_FILE).exists());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    /// 「查看仓库」只认现场重建的 https 地址：域名不在放行清单里就拒绝打开。
    #[test]
    fn verified_page_url_rejects_unlisted_host() {
        let mut s = parse_git_url_with("https://gitee.com/u/r.git", &hosts()).unwrap();
        assert_eq!(s.verified_page_url().unwrap(), "https://gitee.com/u/r");
        // 直接篡改结构体（等价于篡改锁文件后绕过 read_lock 的场景）
        s.domain = "evil.tld".to_string();
        s.page_url = "file:///C:/tmp/payload.exe".to_string();
        assert!(s.verified_page_url().is_err(), "未放行域名必须拒绝");
    }

    /// 回滚失败寄存的旧插件放在 `.recover-<name>`（不在 `.staging` 下），
    /// 且 `cleanup_staging` 不得连坐删掉它，下次启动应被自动搬回原位。
    #[test]
    fn recover_dir_survives_cleanup_and_is_restored() {
        let root = temp_root("recover", &[]);
        let staging = root.join(STAGING_DIR);
        std::fs::create_dir_all(staging.join("leftover")).unwrap();
        let rec = recover_dir(&root, "demo");
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(rec.join("plugin.json"), "old").unwrap();

        cleanup_staging(&root);
        assert!(!staging.exists(), ".staging 应被清掉");
        assert!(rec.exists(), ".recover-demo 是旧插件唯一副本，不能被连坐删除");

        recover_orphans(&root);
        assert!(!rec.exists(), "恢复后寄存目录应消失");
        assert_eq!(
            std::fs::read_to_string(root.join("demo").join("plugin.json")).unwrap(),
            "old",
            "旧插件应被搬回 <root>/demo"
        );

        // 目标已存在时不覆盖、也不删除（那份副本可能是用户唯一的存档）
        std::fs::create_dir_all(&rec).unwrap();
        std::fs::write(rec.join("plugin.json"), "stale").unwrap();
        recover_orphans(&root);
        assert!(rec.exists(), "目标已存在时应保留副本待用户处置");
        assert_eq!(
            std::fs::read_to_string(root.join("demo").join("plugin.json")).unwrap(),
            "old",
            "已存在的插件目录不能被覆盖"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn strip_single_root_on_disk() {
        let base = std::env::temp_dir().join(format!("itools-strip-test-{}", new_token()));
        let inner = base.join("repo-main");
        std::fs::create_dir_all(inner.join("assets")).unwrap();
        std::fs::write(inner.join("plugin.json"), "{}").unwrap();
        assert_eq!(strip_single_root(&base).unwrap(), inner);

        // 再加一个顶层项 → 不剥
        std::fs::write(base.join("README.md"), "x").unwrap();
        assert_eq!(strip_single_root(&base).unwrap(), base);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 令牌铁律：GitHub PAT 走请求头、只发给 github.com，且**绝不**出现在任何 URL 里。
    ///
    /// 本用例独占操作进程级环境变量 `ITOOLS_GITHUB_TOKEN`（[`GitSource::auth_header`] 读它），
    /// 因此把相关断言合并在一个用例里，避免与其它用例互相污染。
    #[test]
    fn github_token_is_header_only_and_never_reaches_mirrors() {
        let gh = parse_git_url_with("https://github.com/u/r.git", &hosts()).unwrap();
        let gitee = parse_git_url_with("https://gitee.com/u/r.git", &hosts()).unwrap();
        let corp = parse_git_url_with(
            "https://git.corp.com/u/r.git",
            &[("git.corp.com".to_string(), Forge::GitHub)],
        )
        .unwrap();

        std::env::remove_var(GITHUB_TOKEN_ENV);
        assert!(gh.auth_header().is_none(), "未设置令牌时应匿名请求");

        std::env::set_var(GITHUB_TOKEN_ENV, "ghp_TESTTOKEN");
        let (k, v) = gh.auth_header().expect("github.com 应带上鉴权头");
        assert_eq!(k, "Authorization");
        assert_eq!(v, "Bearer ghp_TESTTOKEN");
        // 令牌只在请求头里：URL 上零出现（Gitee 那种 query 形态才需要 redact_token 兜泄漏面）
        assert!(!gh.archive_url().contains("ghp_TESTTOKEN"));
        assert!(!gh.raw_url("plugin.json").contains("ghp_TESTTOKEN"));
        // 绝不发给 Gitee / 自建站（把 GitHub PAT 交给第三方等于泄露凭据）
        assert!(gitee.auth_header().is_none());
        assert!(corp.auth_header().is_none());
        std::env::remove_var(GITHUB_TOKEN_ENV);

        // 只有真正的 github.com 才有镜像坐标：Gitee / 自建站的路径形态与公共镜像所代理的不同
        assert!(gh.gh_coord("plugin.json").is_some());
        assert!(gitee.gh_coord("plugin.json").is_none());
        assert!(corp.gh_coord("plugin.json").is_none());

        // 官方 URL 与镜像共用同一套占位符渲染（编码规则只有一份）
        assert_eq!(
            gh.archive_url(),
            "https://codeload.github.com/u/r/zip/HEAD"
        );
        assert_eq!(
            gh.raw_url("plugins/foo/plugin.json"),
            "https://raw.githubusercontent.com/u/r/HEAD/plugins/foo/plugin.json"
        );
    }

    /// R1 回归：并发的「读锁文件 → 改 → 写回」不得互相覆盖。
    ///
    /// `plugin_update` / `plugin_install_confirm` 异步化后由 tokio multi_thread 并发执行
    /// （同步命令时代它们串行在 UI 线程上，这个竞态当时不存在）。闭包里刻意睡 2ms 放大窗口：
    /// 没有 [`LOCK_GUARD`] 时，后写者会拿着旧快照整表覆盖，先写者那条记录直接消失。
    #[test]
    fn concurrent_lock_updates_keep_all_records() {
        let names = ["a", "b", "c", "d"];
        let dir = temp_root("lock-race", &names);
        write_lock(&dir, &LockFile::default()).unwrap();
        let src = parse_git_url_with("https://github.com/u/r.git", &hosts()).unwrap();

        std::thread::scope(|s| {
            for name in names {
                let dir = &dir;
                let src = src.clone();
                s.spawn(move || {
                    for _ in 0..5 {
                        let entry = InstalledEntry {
                            source: src.clone(),
                            resolved_version: "1.0.0".to_string(),
                            installed_at: "x".to_string(),
                        };
                        let _ = update_lock(dir, |lock| {
                            std::thread::sleep(Duration::from_millis(2));
                            lock.plugins.insert(name.to_string(), entry);
                        });
                    }
                });
            }
        });

        let back = read_lock(&dir);
        for name in names {
            assert!(
                back.plugins.contains_key(name),
                "{name} 的安装记录被并发写入覆盖丢失了（该插件会退化成「本地安装、无来源」）"
            );
        }
        // 临时文件名唯一 + 写完即 rename：不该留下任何 .tmp 残渣
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `update_lock` 是改锁文件的唯一入口：`forget` 也走它，且互斥被毒化后仍能继续工作
    /// （一次 panic 不该让「安装记录」永久写不进去）。
    #[test]
    fn update_lock_survives_poisoned_guard() {
        let dir = temp_root("lock-poison", &["demo"]);
        let src = parse_git_url_with("https://github.com/u/r.git", &hosts()).unwrap();
        // 在持有互斥时 panic，令其毒化
        let poisoned = std::thread::spawn(|| {
            let _g = LOCK_GUARD.lock().unwrap();
            panic!("模拟持锁 panic");
        })
        .join();
        assert!(poisoned.is_err());
        assert!(LOCK_GUARD.is_poisoned(), "互斥应已被毒化");

        update_lock(&dir, |lock| {
            lock.plugins.insert(
                "demo".to_string(),
                InstalledEntry {
                    source: src,
                    resolved_version: "1.0.0".to_string(),
                    installed_at: "x".to_string(),
                },
            );
        })
        .expect("毒化后仍应能写入");
        assert!(read_lock(&dir).plugins.contains_key("demo"));

        forget(&dir, "demo");
        assert!(!read_lock(&dir).plugins.contains_key("demo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_place_replaces_and_rolls_back() {
        let base = std::env::temp_dir().join(format!("itools-place-test-{}", new_token()));
        let pkg = base.join("pkg");
        let target = base.join("demo");
        let old = base.join("old");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("plugin.json"), "new").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("plugin.json"), "old").unwrap();

        atomic_place(&pkg, &target, &old).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("plugin.json")).unwrap(),
            "new"
        );
        assert!(!old.exists(), "落地成功后 old 应被删除");
        assert!(!pkg.exists(), "pkg 已被 rename 走");

        // pkg 不存在 → 落地失败，且原目录必须被回滚回来
        let err = atomic_place(&pkg, &target, &old);
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(target.join("plugin.json")).unwrap(),
            "new",
            "失败后原插件必须原样还在"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // 注：GitSource / InstallPreview / PluginUpdate 的**前后端契约快照**（字段名钉死 +
    // 导出机器可读清单）统一放在 `src/contract.rs`，不再各模块一份，免得两处期望值分叉。
}
