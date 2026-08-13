//! 插件下载源：**GitHub 官方直连 + 镜像源竞速**（解决国内直连 github.com 时通时不通的问题）。
//!
//! 为什么不自建反代：服务器在国内，它自己访问 GitHub 同样要翻墙，反代只是把问题换个地方。
//! 采用的方案是「服务端维护镜像源清单并定时探活 → 客户端定期拉最新清单 → 每次下载前竞速选源」，
//! 依据是**不可能所有公共镜像同时失效**。
//!
//! 关键设计（改本文件前务必理解）：
//! - **每个镜像各自声明 raw 与 archive 的 URL 模板**，不假设统一形态。实测依据（2026-08-12）：
//!   `https://gh-proxy.com/https://codeload.github.com/{o}/{r}/zip/{ref}` 返回 200，
//!   而同样的 codeload 形态在 `ghfast.top` 上是 **403**；改用
//!   `https://{mirror}/https://github.com/{o}/{r}/archive/{ref}.zip` 两家都 200。
//!   所以模板必须逐镜像可配，写死一种形态必然踩坑。
//! - **官方源不在镜像清单里**：它是编译进客户端的固定候选，任何模式下都在候选集合里
//!   （`mirror` 模式下排在最后当兜底），即便镜像配置全丢也能装（配置拉取失败绝不阻塞安装）。
//!   注意「在候选集合里」≠「每次都会被请求」：抢跑窗口内另一组已经胜出时，它不会被联系。
//! - **竞速用极小请求**（`Range: bytes=0-0`）决胜负，不并发下载完整归档浪费带宽；
//!   决胜后只用赢家完整下载，赢家结果缓存 30 分钟。
//! - **抢跑窗口**（见 [`HEAD_START`]）：竞速不是「谁先 2xx 谁赢」的裸竞速——
//!   `auto` 模式下**官方先单独发**，它在窗口内成功就根本不去联系任何镜像；
//!   `mirror` 模式是用户显式选择的反向偏好（镜像先发、官方兜底）。
//!   这条是**安全**设计而非性能设计，理由见常量注释。
//! - **服务端新引入的主机要可见**：镜像清单可由同步服务端下发，
//!   [`MirrorEntry::unlisted`] 标出「不在客户端内置列表里的主机」，UI 必须据此标注；
//!   `label` 是服务端给的任意文本、可以撒谎，展示主机名必须用 [`MirrorEntry::host`]
//!   （由客户端从 URL 模板现场解析）。
//! - **安全铁律 1**：带令牌的请求一律直连官方域名，绝不走镜像
//!   （把 PAT 交给第三方反代等于泄露凭据）。本模块在 [`fetch`] 入口处强制：
//!   `auth` 非空 ⇒ 候选集合只剩官方。
//! - **安全铁律 2**：镜像可篡改内容。调用方能给出可信 sha256 时（未来的插件市场场景）
//!   必须校验，不匹配即拒绝；手动粘 URL 安装没有可信哈希来源，此时
//!   [`Fetched::hash_verified`] 为 false，**UI 必须如实提示「本次下载未经哈希校验」**。
//!   测速（[`plugin_mirror_test`]）会拿内置探针（[`default_probe`]）做一次真实的内容比对，
//!   但那只证明**探针文件**没被改，**不能**替代「按包校验 SHA-256」——见 `default_probe` 的安全边界注释。
//! - 失败原因**可区分**（DNS / 连接 / 超时 / 403 限流 / 404 / 体积超限 / 校验失败），
//!   这直接决定用户能不能自查国内访问问题，不许笼统一句「检查失败」；
//!   并且**分场景取词**（[`Scene`]）：「下载插件包」的 404 与「拉镜像配置」的 404 成因毫不相干，
//!   共用一套措辞就是把用户往错误方向引（本轮修的就是这个）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::logging::ilog;
use crate::plugin::install::redact_token;

// ==================== 常量 ====================

/// 请求 User-Agent（GitHub 对无 UA 的请求直接 403）。
const USER_AGENT: &str = "itools-plugin-installer";
/// 服务端镜像配置接口路径（挂在用户配置的服务器地址下）。
const MIRRORS_API_PATH: &str = "/api/mirrors";
/// 拉取镜像配置的超时（秒）——配置只有几 KB，拉不到就用缓存/内置，不值得久等。
const CONFIG_TIMEOUT_SECS: u64 = 5;
/// 镜像配置有效期：30 分钟内不再打服务端接口（避免每次进插件页都发请求）。
const CONFIG_TTL: Duration = Duration::from_secs(30 * 60);
/// 镜像配置响应体上限（字节）。
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
/// 单次竞速探测超时（秒）。
const RACE_TIMEOUT_SECS: u64 = 5;
/// 竞速并发度上限。
const RACE_CONCURRENCY: usize = 3;
/// **抢跑窗口**：竞速时优先组先单独发，它在这个窗口内跑出成功，另一组就**根本不会被请求**。
///
/// 这是一条**安全**约束，不是性能优化：
/// - 镜像清单可以由同步服务端下发（用户的核心诉求就是「镜像失效了能换新的」），
///   而手动粘 URL 安装没有可信 sha256 可校验（[`Request::expect_sha256`] 为 None）。
///   在「谁先 2xx 谁赢」的裸竞速里，一个响应极快的恶意镜像能**稳定**压过官方源，
///   等于谁控制了同步服务端谁就能替换用户即将安装的插件代码。
/// - 有了抢跑窗口，`auto` 模式下只要官方直连可用，镜像连一个请求都收不到，快也没用。
///
/// 为什么取 400ms：直连 GitHub 可用的网络（境外 / 有代理）握手通常 100~300ms 内完成，
/// 400ms 足够它赢下抢跑；而直连被墙的网络表现是**连接超时 / 连不上**（秒级，见 [`explain`] 的分类），
/// 不是「慢一点」，所以国内用户最差只多等 0.4 秒就会转入镜像竞速，体验不受影响。
const HEAD_START: Duration = Duration::from_millis(400);
/// 赢家缓存有效期：TTL 内直接用上次的赢家，不重复竞速。
const WINNER_TTL: Duration = Duration::from_secs(30 * 60);
/// 本地缓存文件名，落在 `%LOCALAPPDATA%\itools\`（**不放插件根**，免得污染插件扫描）。
const CACHE_FILE: &str = "mirrors.json";
/// 镜像条目数上限（服务端返回超量时截断，防配置被塞爆）。
const MAX_MIRRORS: usize = 16;
/// 官方源的候选 id。
pub const OFFICIAL_ID: &str = "official";
/// 官方源的主机名（GitHub-only：可走镜像的坐标只可能是 github.com）。
pub const OFFICIAL_HOST: &str = "github.com";

/// 官方源展示名的**唯一来源**。
///
/// 面板里同一个源出现两种叫法是实打实的欺骗性展示，而它极易发生：
/// `MirrorConfigView.official_label`、`MirrorTestResult.label`、
/// `InstallPreview.download_source`、`PluginUpdate.download_source` 是四个**独立**字段，
/// 各自拼一次串就会分叉（本轮之前 `plugin_mirror_test` 传的就是裸 `"github.com"`，
/// 而配置视图给的是 `"github.com（官方直连）"`）。所有要展示官方源名字的地方都必须调它。
pub fn official_label(host: &str) -> String {
    format!("{host}（官方直连）")
}

/// GitHub 官方 raw 直链模板。
pub const OFFICIAL_RAW_TPL: &str =
    "https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}";
/// GitHub 官方归档直链模板。
pub const OFFICIAL_ARCHIVE_TPL: &str = "https://codeload.github.com/{owner}/{repo}/zip/{ref}";

/// 探测 raw 内容的读取上限（字节）——探测目标是个 13 字节的 README。
const MAX_PROBE_BYTES: u64 = 64 * 1024;

/// 字节数转人类可读单位。用于超限提示：本模块的上限跨度很大
/// （测速探针 64KB、配置 256KB、插件包 32MB），按 MB 整除会打出「超过 0 MB 上限」这种废话。
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if n >= MB && n % MB == 0 {
        format!("{} MB", n / MB)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{} KB", n / KB)
    } else {
        format!("{n} 字节")
    }
}

/// **默认探针**：内置配置用它，服务端下发的配置没给 `probe` 时也用它（唯一出处，两处别再各写一份）。
///
/// 坐标 `octocat/Hello-World@7fd1a60…/README`：GitHub 官方示例仓库，公开、极小（13 字节，
/// 内容为 `Hello World!\n`）、长期存在。
///
/// ### 关于这个 sha256
/// - **实测得来**，非臆造：`curl -sSL https://raw.githubusercontent.com/octocat/Hello-World/7fd1a60b01f91b314f59955a4e4d4e80d8edf11d/README | sha256sum`
///   → `03ba204e…`（2026-08-12 复核）。
/// - `ref` **pin 到固定 commit sha 而不是分支**：分支会动，内容一变哈希就失效，测速会把所有源
///   都误报成「内容与官方不一致」。pin 到 commit 后这份内容永远不会变。
///
/// ### 已知的临时选择（如实标注）
/// 它依赖**第三方仓库** `octocat/Hello-World`（GitHub 官方示例仓库，极稳定，但终究不归我们控制）。
/// 等 iTools 自己的插件索引仓库建起来后，应换成自己仓库里的探针文件并同步更新此处哈希。
///
/// ### 安全边界（别把这条当成完整防线）
/// 探针比对**只能证明**「该镜像在传输这个探针文件时没有改动内容」，**不能证明**它传输插件包时
/// 也不改：针对性投毒的镜像完全可以对探针原样放行、只在插件归档上做手脚。
/// 真正的闭环仍然是「按包校验 SHA-256」（插件市场索引给出可信哈希，见 [`Request::expect_sha256`]）。
/// 这里的价值在于：给用户一个**本地可自查**的手段，能揪出「无差别篡改 / 整站替换」这类粗糙劫持。
fn default_probe() -> ProbeSpec {
    ProbeSpec {
        owner: "octocat".to_string(),
        repo: "Hello-World".to_string(),
        // 固定 commit（不是分支）：内容不会随上游变动
        git_ref: "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d".to_string(),
        path: "README".to_string(),
        sha256: "03ba204e50d126e4674c005e04d82e84c21366780af1f43bd54a37816b6ab340".to_string(),
    }
}

/// 默认探针的**官方 raw 直链**（`https://raw.githubusercontent.com/octocat/Hello-World/7fd1a60…/README`）。
///
/// 除了测速，它还是「设置 → 网络 → 测试代理」的探测目标（见 `crate::http::probe_target`）：
/// 13 字节、pin 到固定 commit、匿名可读且不限流，是本仓库里唯一一个「小到可以随手请求」
/// 且长期稳定的公网 https 目标。单拎成函数是为了让两处共用同一份坐标——
/// 各写一遍字面量，改了探针而代理测试还在打旧地址，就是又一处会分叉的重复。
pub fn probe_raw_url() -> String {
    let p = default_probe();
    render(
        OFFICIAL_RAW_TPL,
        &GhCoord {
            owner: p.owner,
            repo: p.repo,
            git_ref: p.git_ref,
            path: p.path,
        },
    )
}

// ==================== 配置结构（客户端内置 / 本地缓存 / 服务端返回三处同一格式） ====================

/// 一次探活用的目标文件坐标。`sha256` 为空 = 服务端没给可信哈希，此时**不校验并如实标注**
/// （内置默认探针带实测哈希，见 [`default_probe`]）。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProbeSpec {
    pub owner: String,
    pub repo: String,
    /// 字面量 `HEAD` 表示默认分支（官方与镜像都支持）。
    #[serde(rename = "ref", default)]
    pub git_ref: String,
    pub path: String,
    /// 期望的 sha256（小写 hex）；空 = 未提供，探测时不做校验并如实标注。
    #[serde(default)]
    pub sha256: String,
}

/// 一个镜像源。**raw 与 archive 模板各自独立**——见模块头注释里的实测差异。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MirrorEntry {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// raw 单文件模板，占位符 `{owner}` `{repo}` `{ref}` `{path}`。
    pub raw: String,
    /// zip 归档模板，占位符 `{owner}` `{repo}` `{ref}`（**不含** `{path}`）。
    pub archive: String,
    /// 服务端探活结论：false 的镜像不参与候选。
    #[serde(default = "yes")]
    pub healthy: bool,
    /// 服务端探活测得的延迟（毫秒），用于候选排序；缺省排在有值者之后。
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// 上次探活成功时间（RFC3339），仅供 UI 展示。
    #[serde(default)]
    pub last_ok_at: Option<String>,
    /// **真实主机名**（由客户端从 [`MirrorEntry::raw`] 模板现场解析，见 [`tpl_host`]）。
    ///
    /// `#[serde(skip_deserializing)]`：**绝不接受外部输入**。`label` 是服务端下发的任意文本，
    /// 完全可以写着 `gh-proxy.com` 而模板指向 `evil.tld`；UI 展示「这个源到底是谁」必须用本字段。
    #[serde(skip_deserializing)]
    pub host: String,
    /// 该镜像的主机**不在客户端内置默认列表里**（= 由同步服务端下发引入的新主机）。
    ///
    /// 同样 `skip_deserializing`：由 [`sanitize`] 现场判定，服务端无法把自己标成「内置」。
    /// UI 必须据此标注（这是一条内置清单未背书的新代码分发路径），
    /// 但**不是**禁止——镜像失效后能换新的正是这套机制存在的理由。
    #[serde(skip_deserializing)]
    pub unlisted: bool,
}

fn yes() -> bool {
    true
}

/// 镜像配置整体。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct MirrorConfig {
    pub version: u32,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub probe: Option<ProbeSpec>,
    #[serde(default)]
    pub mirrors: Vec<MirrorEntry>,
}

/// 编译进客户端的内置默认列表（兜底链最后一层）。
///
/// 收录的三家均为 **2026-08-12 本机实测存活**（raw 与归档都能取到，速度不亚于直连；
/// 直连 raw 4.11s vs gh-proxy.com 0.92s）。**不写死 latencyMs / lastOkAt**：
/// 本机没有可信来源核对，编一个数就是制造假数据——这些字段由服务端探活后下发。
/// （`probe.sha256` 是**例外**：它有本机实测的可信值，见 [`default_probe`]，
/// 没有它测速就只能测「通不通」、永远测不出「内容对不对」。）
///
/// 归档一律用 `https://{mirror}/https://github.com/{o}/{r}/archive/{ref}.zip` 形态：
/// 这是实测里**通用**的那一种；`.../https://codeload.github.com/{o}/{r}/zip/{ref}` 在
/// gh-proxy.com 上是 200 但在 ghfast.top 上是 **403**，所以模板必须逐镜像可配、绝不能假设统一形态
/// （这也正是配置结构里 raw / archive 各自独立的原因）。
/// 某个镜像若不支持这种形态，竞速与串行兜底会自动绕开它，
/// `plugin_mirror_test` 也会把 raw / archive 分开测出来，用户能看到究竟哪一段不通。
pub fn builtin_config() -> MirrorConfig {
    let mk = |id: &str, host: &str| MirrorEntry {
        id: id.to_string(),
        label: host.to_string(),
        raw: format!("https://{host}/https://raw.githubusercontent.com/{{owner}}/{{repo}}/{{ref}}/{{path}}"),
        archive: format!("https://{host}/https://github.com/{{owner}}/{{repo}}/archive/{{ref}}.zip"),
        healthy: true,
        latency_ms: None,
        last_ok_at: None,
        // host / unlisted 由 sanitize 现场解析回填（这里写上只是让内置列表自解释；
        // 真正生效的值永远来自 sanitize，见 builtin_loaded）
        host: host.to_string(),
        unlisted: false,
    };
    MirrorConfig {
        version: 1,
        updated_at: String::new(),
        // 探针带实测哈希：测速时 raw 那行会真的取回内容并比对，能检出「镜像改了内容」。
        // 注意它证明不了「插件包也没被改」——安全边界见 default_probe 的注释。
        probe: Some(default_probe()),
        mirrors: vec![
            mk("gh-proxy", "gh-proxy.com"),
            mk("ghfast", "ghfast.top"),
            mk("ghproxy-net", "ghproxy.net"),
        ],
    }
}

/// 清洗外来配置（服务端返回 / 本地缓存文件都要过）。
///
/// 磁盘缓存与服务端响应都不是可信输入：模板会被直接拼成下载地址，
/// 允许 `http://` 就等于允许明文劫持，允许缺占位符就等于把所有插件指向同一个固定文件。
/// 不合规的条目**逐条丢弃**（不是整份作废），保证「一条坏配置不至于让所有源都用不了」。
pub fn sanitize(mut cfg: MirrorConfig) -> MirrorConfig {
    let mut seen = HashSet::new();
    let builtin = builtin_hosts();
    let mut kept: Vec<MirrorEntry> = Vec::new();
    for m in std::mem::take(&mut cfg.mirrors) {
        if kept.len() >= MAX_MIRRORS {
            break;
        }
        let id = m.id.trim().to_string();
        if id.is_empty() || id.len() > 64 || !seen.insert(id.clone()) {
            continue;
        }
        if !valid_tpl(&m.raw, true) || !valid_tpl(&m.archive, false) {
            ilog!("[iTools] 镜像 {id} 的 URL 模板不合规，已忽略该镜像");
            continue;
        }
        let label = if m.label.trim().is_empty() {
            id.clone()
        } else {
            m.label.trim().chars().take(64).collect()
        };
        // host / unlisted **一律现场重算**：外来 JSON 里的同名字段已被 skip_deserializing 丢弃，
        // 服务端既不能伪造主机名，也不能把自己冒充成「内置源」。
        // 两个模板的主机都在内置列表里才算「内置源」——只对 raw 放行的话，
        // 一份 raw 用 gh-proxy.com、archive 指向 evil.tld 的配置就能混成「内置」。
        let host = tpl_host(&m.raw);
        let unlisted = !builtin.contains(&host) || !builtin.contains(&tpl_host(&m.archive));
        if unlisted {
            ilog!("[iTools] 镜像 {id} 的主机 {host} 不在客户端内置列表内（由服务端下发），已标注为新增源");
        }
        kept.push(MirrorEntry {
            id,
            label,
            raw: m.raw.trim().to_string(),
            archive: m.archive.trim().to_string(),
            host,
            unlisted,
            ..m
        });
    }
    cfg.mirrors = kept;
    if cfg.version == 0 {
        cfg.version = 1;
    }
    cfg
}

/// 模板合规性：必须 https、无空白、含 `{owner}`/`{repo}`/`{ref}`；
/// raw 必须含 `{path}`，archive 必须**不含** `{path}`（含了说明模板写串了）。
fn valid_tpl(tpl: &str, need_path: bool) -> bool {
    let t = tpl.trim();
    if !t.starts_with("https://") || t.len() > 512 {
        return false;
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if !t.contains("{owner}") || !t.contains("{repo}") || !t.contains("{ref}") {
        return false;
    }
    t.contains("{path}") == need_path
}

/// 从 URL 模板里取出**真实主机名**（小写）。模板已由 [`valid_tpl`] 保证是 `https://` 开头、无空白，
/// 所以这里只做纯字符串切分，不引依赖。
///
/// `userinfo@host` 形态取 `@` **之后**那段：浏览器与 curl 都以此为真实主机，
/// 不这样取就会被 `https://gh-proxy.com@evil.tld/...` 这种模板骗过去（肉眼看着是内置源，实际连的是 evil.tld）。
/// 端口原样保留（`evil.tld:8443` 与 `evil.tld` 是不同的展示事实，不该被抹平）。
fn tpl_host(tpl: &str) -> String {
    let rest = tpl.trim().strip_prefix("https://").unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .to_ascii_lowercase()
}

/// 内置默认列表里出现过的主机集合（判定「这是不是服务端新引入的主机」用）。
///
/// 按**主机**而非 id 判定：服务端完全可以沿用 `gh-proxy` 这个 id 却把模板换成别的域名，
/// 按 id 比对等于没比。
fn builtin_hosts() -> HashSet<String> {
    builtin_config()
        .mirrors
        .into_iter()
        .flat_map(|m| [tpl_host(&m.raw), tpl_host(&m.archive)])
        .collect()
}

// ==================== 下载目标与模式 ====================

/// 下载类型：raw 单文件 / zip 归档（两者的模板与赢家缓存互相独立）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Raw,
    Archive,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Raw => "raw",
            Kind::Archive => "archive",
        }
    }
}

/// 可走镜像的 GitHub 坐标。只有**真正的 github.com** 才构造它
/// （Gitee / 自建站的路径形态与 GitHub 不同，镜像模板对它们无意义）。
#[derive(Clone, Debug)]
pub struct GhCoord {
    pub owner: String,
    pub repo: String,
    /// 字面量 `HEAD` = 默认分支。
    pub git_ref: String,
    /// 仓库内相对路径（archive 时用不到，传空串即可）。
    pub path: String,
}

/// 下载源偏好（用户可在设置里改，持久化在 `AppSettings.plugin_mirror_mode`）。
/// 三个取值必须**行为上真的不同**，否则就是「选了不生效」的假控件：
/// - [`Mode::Auto`]：官方抢跑（[`HEAD_START`]）→ 官方可用时镜像收不到任何请求；
/// - [`Mode::Official`]：只有官方一个候选，镜像永不参与；
/// - [`Mode::Mirror`]：反过来让镜像抢跑，官方兜底。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// **默认**。官方先抢跑 [`HEAD_START`]：官方在窗口内成功就直接用官方，
    /// **根本不去联系镜像**；官方失败或窗口内没响应，才放镜像入场竞速。
    Auto,
    /// 只走官方直连（不信任第三方镜像时选它）。
    Official,
    /// **用户显式选择「优先镜像」**（直连不通 / 极慢的网络）：镜像先抢跑，
    /// 窗口内有镜像成功就用镜像、官方不发请求；镜像都不行时**退回官方兜底**，
    /// 并在返回的来源标签里如实注明用的是谁，绝不假装走了镜像。
    ///
    /// 注意本模式**不享受官方抢跑保护**：这是用户主动选的「优先信任第三方镜像」，
    /// UI 必须如实提示（镜像可以篡改内容）。之所以不给它也加官方抢跑：
    /// 那会让本模式与 [`Mode::Auto`] 行为完全一致，设置项就成了「选了不生效」的假控件。
    Mirror,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "official" => Mode::Official,
            "mirror" => Mode::Mirror,
            _ => Mode::Auto,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Official => "official",
            Mode::Mirror => "mirror",
        }
    }
}

/// 运行期的下载源偏好（启动时由设置装入，`plugin_mirror_set_mode` 改它）。
static MODE: RwLock<Mode> = RwLock::new(Mode::Auto);

/// 由 `AppSettings.plugin_mirror_mode` 更新运行期偏好（启动装载设置后、以及命令改动时调用）。
pub fn set_mode(raw: &str) {
    let next = Mode::parse(raw);
    if let Ok(mut g) = MODE.write() {
        *g = next;
    }
    // 模式变了候选集合就变了，上次的赢家不再适用
    clear_winners();
}

/// 当前下载源偏好。
pub fn mode() -> Mode {
    MODE.read().map(|g| *g).unwrap_or(Mode::Auto)
}

// ==================== 模板替换 ====================

/// 模板占位符替换：**只认** `{owner}` `{repo}` `{ref}` `{path}` 四个。
///
/// 各段做 URL 编码（只保留 RFC3986 unreserved 字符），但 `{ref}` 与 `{path}` 里的 `/` 保留
/// ——`refs/heads/main` 这种 ref 形态和 `plugins/foo/plugin.json` 这种路径都必须原样进 URL。
///
/// 顺序 `replace` 是安全的：编码后的值里不可能再出现 `{` / `}`（都被编成 `%7B` / `%7D`），
/// 所以先替换进去的内容不会被后一轮当成占位符二次展开。
pub fn render(tpl: &str, c: &GhCoord) -> String {
    tpl.replace("{owner}", &encode(&c.owner, false))
        .replace("{repo}", &encode(&c.repo, false))
        .replace("{ref}", &encode(&c.git_ref, true))
        .replace("{path}", &encode(&c.path, true))
}

/// 百分号编码；`keep_slash` 为真时保留 `/`。
fn encode(s: &str, keep_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else if keep_slash && b == b'/' {
            out.push('/');
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

// ==================== 候选源 ====================

/// 一个下载候选源。
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub label: String,
    pub url: String,
    /// 是否为第三方镜像（决定「能不能带令牌」「内容是否可信」）。
    pub is_mirror: bool,
}

/// 组装候选源列表：官方 + 配置里 `healthy` 的镜像
/// （先内置列表内的主机、后服务端新引入的主机，各自再按 `latencyMs` 升序，无值排最后）。
///
/// **纯逻辑，无 I/O**，可单测。`coord` 为 `None`（Gitee / 自建站 / 带令牌）时只剩官方源。
pub fn build_candidates(
    cfg: &MirrorConfig,
    kind: Kind,
    coord: Option<&GhCoord>,
    official_url: &str,
    official_label: &str,
    mode: Mode,
) -> Vec<Candidate> {
    let official = Candidate {
        id: OFFICIAL_ID.to_string(),
        label: official_label.to_string(),
        url: official_url.to_string(),
        is_mirror: false,
    };
    let Some(coord) = coord else {
        return vec![official];
    };
    if mode == Mode::Official {
        return vec![official];
    }
    let mut ranked: Vec<&MirrorEntry> = cfg.mirrors.iter().filter(|m| m.healthy).collect();
    // 排序键：**内置列表内的主机优先**，其次按服务端探活延迟升序（无值排最后）。
    // 为什么把服务端新引入的主机排在后面：它是一条内置清单未背书的新代码分发路径，
    // 已知主机可用时没必要用它（竞速只并发前 RACE_CONCURRENCY 个，排后面通常压根不会被请求）。
    // 这不是禁用——已知主机全挂时它照样会被用上，「镜像失效了能换新的」这个诉求仍然成立。
    ranked.sort_by_key(|m| (m.unlisted, m.latency_ms.unwrap_or(u64::MAX)));
    let mirrors: Vec<Candidate> = ranked
        .into_iter()
        .map(|m| Candidate {
            id: m.id.clone(),
            label: if m.label.is_empty() {
                m.id.clone()
            } else {
                m.label.clone()
            },
            url: render(
                match kind {
                    Kind::Raw => &m.raw,
                    Kind::Archive => &m.archive,
                },
                coord,
            ),
            is_mirror: true,
        })
        .collect();

    // 这里只定顺序（串行兜底按它来）；「谁先发、谁抢跑」由 [`race_groups`] + [`race_staged`] 决定
    let mut out = Vec::with_capacity(mirrors.len() + 1);
    if mode == Mode::Mirror {
        // 镜像优先；官方仍留在最后当兜底（一个镜像都不可用时不至于装不上，
        // 且真正用了哪个源会如实回填到 Fetched::source_label，不存在「假装走了镜像」）
        out.extend(mirrors);
        out.push(official);
    } else {
        out.push(official);
        out.extend(mirrors);
    }
    out
}

/// 竞速分组：返回 `(先发的一组, 延迟入场的一组)`。**纯逻辑，可单测。**
///
/// - [`Mode::Auto`] / [`Mode::Official`]：官方先发 → 官方可用时镜像收不到请求（安全，见 [`HEAD_START`]）；
/// - [`Mode::Mirror`]：用户显式选了优先镜像 → 镜像先发，官方延迟入场兜底。
pub fn race_groups(cands: &[Candidate], mode: Mode) -> (Vec<Candidate>, Vec<Candidate>) {
    cands
        .iter()
        .cloned()
        .partition(|c| if mode == Mode::Mirror { c.is_mirror } else { !c.is_mirror })
}

// ==================== 失败分类 ====================

/// 一次 HTTP 请求属于**哪个场景**。
///
/// 同一个状态码在不同场景下的真实成因完全不同，文案必须分场景给——本模块曾经只有一套措辞，
/// 于是「从服务端拉镜像配置」拿到 404 时，UI 上显示的是
/// 「目标不存在（仓库/分支/子目录写错…）」，把用户往完全错误的方向引（服务端根本没有
/// `/api/mirrors` 这个端点，跟仓库分支毫无关系）。
///
/// **新增第三个场景时**：给这里加一个成员，编译器会把 [`status_hint`] / [`transport_hint`]
/// 里所有缺失的分支全部报出来——结构上杜绝「一套文案硬套多个场景」再次发生。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scene {
    /// 从 GitHub 官方或第三方镜像**下载内容**（插件归档 / raw 单文件 / 测速探测）。
    Download,
    /// 从同步服务端**拉取镜像配置**（`GET {server_url}/api/mirrors`）。
    MirrorConfig,
}

/// 把一次请求失败翻译成**可自查**的中文原因（区分 DNS / 连接 / 超时 / 403 / 404 …），按场景取词。
fn explain(e: &ureq::Error, scene: Scene) -> String {
    let msg = match e {
        ureq::Error::Status(code, _) => status_hint(scene, *code),
        ureq::Error::Transport(t) => {
            let detail = t.message().unwrap_or_default();
            let timed_out = detail.contains("timed out")
                || detail.contains("timeout")
                || detail.contains("os error 10060");
            format!("{}{}", transport_hint(scene, t.kind(), timed_out), tail(detail))
        }
    };
    // 出口统一脱敏：ureq 的 Transport/Status Display 会带上完整 URL（令牌可能拼在 query 上）
    redact_token(msg)
}

/// 状态码 → 中文成因（**分场景**）。
fn status_hint(scene: Scene, code: u16) -> String {
    match scene {
        Scene::Download => match code {
            401 => "HTTP 401：需要鉴权（私有仓库请设置 ITOOLS_GITHUB_TOKEN）".to_string(),
            403 => "HTTP 403：被拒绝或触发限流（GitHub 匿名限流 / 该镜像不支持此路径形态）".to_string(),
            404 => "HTTP 404：目标不存在（仓库/分支/子目录写错，或该镜像不支持这种路径形态）".to_string(),
            429 => "HTTP 429：请求过于频繁，已被限流".to_string(),
            c if (500..600).contains(&c) => format!("HTTP {c}：该源自身故障"),
            c => format!("HTTP {c}"),
        },
        Scene::MirrorConfig => match code {
            401 => "HTTP 401：服务端要求登录后才能读取镜像配置（请先在「账号」里登录，或确认服务端是否需要鉴权）"
                .to_string(),
            403 => "HTTP 403：服务端拒绝了本次请求（当前账号无权访问该接口，或被反代/网关拦截）".to_string(),
            404 => format!(
                "HTTP 404：该地址没有 {MIRRORS_API_PATH} 端点（服务端未部署本功能、版本过旧，或服务器地址配错）"
            ),
            429 => "HTTP 429：请求过于频繁，已被服务端限流".to_string(),
            // 502/503/504 是「反代活着、后面没有实例」的典型形态，和「服务端自己抛异常」不是一回事
            c @ 502..=504 => format!("HTTP {c}：服务端网关不可用（服务未启动，或反向代理后面没有可用实例）"),
            c if (500..600).contains(&c) => format!("HTTP {c}：服务端内部错误"),
            c => format!("HTTP {c}：服务端返回了非预期的状态码"),
        },
    }
}

/// 传输层错误 → 中文成因（**分场景**）。`timed_out` 由错误详情判定（ureq 不单独区分超时种类）。
fn transport_hint(scene: Scene, kind: ureq::ErrorKind, timed_out: bool) -> &'static str {
    match scene {
        Scene::Download => match kind {
            ureq::ErrorKind::Dns => "DNS 解析失败（本机无法解析该域名）",
            ureq::ErrorKind::ConnectionFailed if timed_out => "连接超时（未能在超时内建立连接）",
            ureq::ErrorKind::ConnectionFailed => "连接失败（可能被墙 / 网络不可达）",
            ureq::ErrorKind::Io if timed_out => "读写超时",
            ureq::ErrorKind::Io => "网络读写失败",
            ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => "地址不合法",
            ureq::ErrorKind::ProxyConnect | ureq::ErrorKind::ProxyUnauthorized => "代理连接失败",
            _ => "请求失败",
        },
        Scene::MirrorConfig => match kind {
            ureq::ErrorKind::Dns => "DNS 解析失败（服务器域名无法解析，地址可能写错）",
            ureq::ErrorKind::ConnectionFailed if timed_out => "连接服务端超时（服务端无响应，或被防火墙丢包）",
            ureq::ErrorKind::ConnectionFailed => "连不上服务端（服务端未启动 / 地址或端口写错 / 被防火墙拦截）",
            ureq::ErrorKind::Io if timed_out => "与服务端通信超时",
            ureq::ErrorKind::Io => "与服务端通信失败",
            ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => {
                "服务器地址不合法（请检查「账号 → 数据同步」里填写的地址）"
            }
            ureq::ErrorKind::ProxyConnect | ureq::ErrorKind::ProxyUnauthorized => {
                "本机代理无法连到服务端（服务端在本机时，请把它加入代理排除列表）"
            }
            _ => "请求服务端失败",
        },
    }
}

/// dev 默认端点的补充说明（**不是故障**）。
///
/// debug 构建下没配服务器地址时，客户端会默认去连 [`crate::account::DEV_DEFAULT_ENDPOINT`]；
/// 本地没跑 `server/` 时它当然连不上——这是**正常状态**，配置会退回内置镜像列表，功能不受影响。
/// 不点破这一点，每个跑 `npm run tauri dev` 的人都会以为自己撞上了故障。
fn dev_endpoint_note(endpoint: &str) -> &'static str {
    if crate::account::is_dev_default_endpoint(endpoint) {
        "。注意：这是开发模式下的默认本地服务端地址，未启动服务端属正常现象（客户端已自动改用内置镜像列表，功能不受影响）；要联调请在 server/ 目录启动服务端，或在「账号 → 数据同步」里填写真实服务器地址"
    } else {
        ""
    }
}

/// 拉取镜像配置失败时的最终文案：场景化原因 + dev 默认地址说明，出口再脱敏一次。
fn config_error(endpoint: &str, cause: String) -> String {
    redact_token(format!("{cause}{}", dev_endpoint_note(endpoint)))
}

fn tail(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!("：{detail}")
    }
}

// ==================== 竞速 ====================

/// 探测函数签名（注入式，便于**不依赖网络**地单测竞速逻辑）。返回耗时毫秒或失败原因。
pub type ProbeFn = Arc<dyn Fn(&Candidate) -> Result<u64, String> + Send + Sync>;

/// 一次竞速的结果。
#[derive(Debug)]
pub struct RaceOutcome {
    /// 胜出的候选与其探测耗时（毫秒）。
    pub winner: Option<(Candidate, u64)>,
    /// 已知失败的候选与真实原因（用于给用户看「每个源分别怎么挂的」）。
    pub failures: Vec<(Candidate, String)>,
}

/// 竞速选源：**分两组、后组延迟入场**，谁先返回成功谁赢，赢了立刻返回。
///
/// - `first` 组先发。它在 `head_start` 窗口内跑出成功 → 直接返回，`then` 组**根本不会被请求**。
/// - `first` 组全部失败（不必等满窗口）或窗口到期仍无结果 → 放 `then` 组入场；
///   此时 `first` 组仍在赛道上，它晚到的成功一样算数（不浪费已经发出去的请求）。
/// - 任意时刻在跑的探测不超过 `concurrency` 个（滑动补位，不是「整批等齐再开下一批」）。
///
/// 这套分组正是安全修复的落点：`auto` 模式把官方放进 `first`，官方可用时恶意镜像再快也抢不到。
/// 详见 [`HEAD_START`]。
///
/// 落败/未完成的探测线程是游离的（不 join）：它们最多再跑一个超时就自然结束，
/// 用 `thread::scope` 反而要等最慢的那个，等于把「先到者胜」退化成「等最慢者」。
pub fn race_staged(
    first: &[Candidate],
    then: &[Candidate],
    concurrency: usize,
    head_start: Duration,
    probe: ProbeFn,
) -> RaceOutcome {
    let conc = concurrency.max(1);
    let mut queue: VecDeque<Candidate> = first.iter().cloned().collect();
    let mut later: VecDeque<Candidate> = then.iter().cloned().collect();
    if queue.is_empty() {
        // 优先组为空（如只走官方 / 没有镜像）→ 后组即全部候选，没有「抢跑」可言
        queue.append(&mut later);
    }
    let mut opened = later.is_empty();
    let deadline = Instant::now() + head_start;

    let mut failures: Vec<(Candidate, String)> = Vec::new();
    let mut inflight = 0usize;
    let (tx, rx) = mpsc::channel::<(Candidate, Result<u64, String>)>();
    loop {
        // 1) 补满并发额度
        while inflight < conc {
            let Some(c) = queue.pop_front() else { break };
            let (tx, probe) = (tx.clone(), probe.clone());
            std::thread::spawn(move || {
                // catch_unwind：探测函数 panic 时也必须回一条消息，否则 inflight 永远减不到 0，
                // 下面的接收循环会一直等下去（本函数持有 tx，recv 不会自己断开）
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe(&c)))
                    .unwrap_or_else(|_| Err("探测线程异常退出".to_string()));
                // 赢家已产生时接收端已丢弃，send 返回 Err —— 忽略即可，不 panic
                let _ = tx.send((c, r));
            });
            inflight += 1;
        }
        // 2) 没有在跑的也没有排队的：优先组已全败就放后组进来，否则收工
        if inflight == 0 {
            if opened {
                break;
            }
            opened = true;
            queue.append(&mut later);
            continue;
        }
        // 3) 收结果；抢跑期间用带窗口的等待
        let got = if opened {
            rx.recv().map_err(|_| ())
        } else {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(v) => Ok(v),
                // 窗口用完、优先组还没跑出成功 → 后组入场（优先组不撤，继续参赛）
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    opened = true;
                    queue.append(&mut later);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(()),
            }
        };
        match got {
            Ok((c, Ok(ms))) => {
                return RaceOutcome {
                    winner: Some((c, ms)),
                    failures,
                }
            }
            Ok((c, Err(e))) => {
                failures.push((c, e));
                inflight -= 1;
            }
            // 发送端全没了：本函数自己持有 tx，正常不会走到这里；真发生就收工交给串行兜底
            Err(()) => break,
        }
    }
    RaceOutcome {
        winner: None,
        failures,
    }
}

/// 真实的探测实现：`Range: bytes=0-0` 极小请求，2xx 即算通。
fn http_probe(c: &Candidate) -> Result<u64, String> {
    let start = Instant::now();
    let resp = crate::http::get(&c.url)
        .set("User-Agent", USER_AGENT)
        .set("Range", "bytes=0-0")
        .timeout(Duration::from_secs(RACE_TIMEOUT_SECS))
        .call()
        .map_err(|e| explain(&e, Scene::Download))?;
    let ms = start.elapsed().as_millis() as u64;
    // 服务器可能忽略 Range 返回整包 —— 只读 1 字节就丢弃连接，绝不能读完
    let mut buf = [0u8; 1];
    let _ = resp.into_reader().take(1).read(&mut buf);
    Ok(ms)
}

// ==================== 赢家缓存 ====================

fn winners() -> &'static Mutex<HashMap<Kind, (String, Instant)>> {
    static W: OnceLock<Mutex<HashMap<Kind, (String, Instant)>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

fn recall_winner(kind: Kind) -> Option<String> {
    let g = winners().lock().ok()?;
    let (id, at) = g.get(&kind)?;
    if at.elapsed() <= WINNER_TTL {
        Some(id.clone())
    } else {
        None
    }
}

fn remember_winner(kind: Kind, id: &str) {
    if let Ok(mut g) = winners().lock() {
        g.insert(kind, (id.to_string(), Instant::now()));
    }
}

fn forget_winner(kind: Kind) {
    if let Ok(mut g) = winners().lock() {
        g.remove(&kind);
    }
}

/// 清空全部赢家缓存（模式切换 / 手动重拉配置后调用）。
pub fn clear_winners() {
    if let Ok(mut g) = winners().lock() {
        g.clear();
    }
}

// ==================== sha256 ====================

/// 计算 sha256 并输出小写 hex。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 校验内容哈希。`expect` 为 None/空 = 调用方没有可信哈希，**不校验并如实返回 false**。
///
/// 返回 `Ok(true)` = 校验通过；`Ok(false)` = 未校验；`Err` = 校验不通过（必须拒绝这份内容）。
pub fn verify_sha256(bytes: &[u8], expect: Option<&str>) -> Result<bool, String> {
    let Some(exp) = expect.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(false);
    };
    let got = sha256_hex(bytes);
    if got.eq_ignore_ascii_case(exp) {
        Ok(true)
    } else {
        Err(format!(
            "内容校验失败：期望 sha256 {} ，实际 {got}（下载源可能被篡改，已拒绝）",
            exp.to_ascii_lowercase()
        ))
    }
}

// ==================== 配置获取（服务端 → 本地缓存 → 内置） ====================

/// 当前配置的来源。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Origin {
    Server,
    Cache,
    Builtin,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Origin::Server => "server",
            Origin::Cache => "cache",
            Origin::Builtin => "builtin",
        }
    }
}

/// 进程内的配置状态。
#[derive(Clone)]
struct Loaded {
    cfg: MirrorConfig,
    origin: Origin,
    etag: Option<String>,
    /// 上次**成功**取得服务端配置的时间（RFC3339，跨重启保留在缓存文件里）。
    fetched_at: Option<String>,
    /// 本进程内上次尝试拉取的时刻（TTL 判定用）。
    checked_at: Option<Instant>,
    /// 上次拉取失败的真实原因（供 UI 如实展示，不掩盖）。
    last_error: Option<String>,
}

/// 缓存文件信封（配置 + ETag + 拉取时间）。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CacheEnvelope {
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    fetched_at: Option<String>,
    config: MirrorConfig,
}

fn state() -> &'static Mutex<Option<Loaded>> {
    static S: OnceLock<Mutex<Option<Loaded>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// 同一时刻只允许一个线程去打服务端接口（并发安装时不至于打出一串重复请求）。
fn fetch_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// 本地缓存文件路径：`%LOCALAPPDATA%\itools\mirrors.json`。
fn cache_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("itools").join(CACHE_FILE))
}

/// 兜底链最后一层：编译进客户端的内置列表。
///
/// 同样过一遍 [`sanitize`]：`host` / `unlisted` 只在那里计算，绕过它内置源就会以
/// 「主机名空、疑似新增」的面目出现在 UI 上（假信息）。
fn builtin_loaded() -> Loaded {
    Loaded {
        cfg: sanitize(builtin_config()),
        origin: Origin::Builtin,
        etag: None,
        fetched_at: None,
        checked_at: None,
        last_error: None,
    }
}

fn load_cache() -> Option<Loaded> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let env: CacheEnvelope = serde_json::from_str(&text).ok()?;
    Some(Loaded {
        cfg: sanitize(env.config),
        origin: Origin::Cache,
        etag: env.etag,
        fetched_at: env.fetched_at,
        checked_at: None,
        last_error: None,
    })
}

fn save_cache(cfg: &MirrorConfig, etag: Option<&str>, fetched_at: &str) {
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let env = CacheEnvelope {
        etag: etag.map(|s| s.to_string()),
        fetched_at: Some(fetched_at.to_string()),
        config: cfg.clone(),
    };
    match serde_json::to_string_pretty(&env) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                ilog!("[iTools] 镜像配置缓存写入失败（不影响使用）: {e}");
            }
        }
        Err(e) => ilog!("[iTools] 镜像配置序列化失败: {e}"),
    }
}

/// 服务端拉取的结果。
enum FetchOutcome {
    /// 200：拿到新配置。
    Fresh(MirrorConfig, Option<String>),
    /// 304：本地缓存仍有效。
    NotModified,
}

fn fetch_from_server(endpoint: &str, etag: Option<&str>) -> Result<FetchOutcome, String> {
    let url = format!("{}{MIRRORS_API_PATH}", endpoint.trim_end_matches('/'));
    // 走统一出口：服务端常常就是本机 127.0.0.1:8787，它必须**直连**
    // （`crate::http` 的绕过规则保证这一点；塞进代理会让本地联调整条断掉）
    let mut req = crate::http::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(CONFIG_TIMEOUT_SECS));
    if let Some(tag) = etag.filter(|s| !s.is_empty()) {
        req = req.set("If-None-Match", tag);
    }
    let resp = match req.call() {
        Ok(r) => r,
        // 304 在 ureq 里也走 Error::Status（非 2xx 一律 Status）
        Err(ureq::Error::Status(304, _)) => return Ok(FetchOutcome::NotModified),
        // 场景是「拉配置」而不是「下载插件包」——同一个状态码在两个场景下的成因完全不同
        Err(e) => return Err(explain(&e, Scene::MirrorConfig)),
    };
    let tag = resp.header("ETag").map(|s| s.to_string());
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("读取镜像配置失败：{e}"))?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err("镜像配置超过体积上限，已拒绝".to_string());
    }
    let cfg: MirrorConfig =
        serde_json::from_slice(&buf).map_err(|e| format!("镜像配置解析失败：{e}"))?;
    Ok(FetchOutcome::Fresh(sanitize(cfg), tag))
}

/// 取当前生效配置（必要时按 TTL 去服务端刷新）。**任何一层失败都不会返回 Err**——
/// 拉不到就用缓存，缓存也没有就用内置，绝不因为配置服务不可用而让插件装不上。
fn ensure(force: bool) -> Loaded {
    // 1) 取当前快照，并判断要不要拉
    let (mut cur, need) = {
        let mut g = state().lock().unwrap_or_else(|e| e.into_inner());
        let cur = g
            .get_or_insert_with(|| load_cache().unwrap_or_else(builtin_loaded))
            .clone();
        let stale = match cur.checked_at {
            Some(t) => t.elapsed() > CONFIG_TTL,
            // 本进程还没拉过：若缓存文件里的时间仍在 TTL 内，就不必开机就打接口
            None => !within_ttl(cur.fetched_at.as_deref()),
        };
        (cur, force || stale)
    };
    if !need {
        return cur;
    }
    // 2) 真正的网络请求不持有状态锁（否则一次 5 秒超时会把设置页一起卡住），
    //    但要串行化：`plugin_check_updates` 会并发起多条线程，每条都可能走到这里，
    //    不串行就会对同一个接口打出 N 个重复请求。
    let _fetching = fetch_lock().lock().unwrap_or_else(|e| e.into_inner());
    // 3) 双重检查：排队等锁期间别的线程可能已经拉好了，此时直接用它的结果
    if !force {
        let g = state().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(fresh) = g.as_ref() {
            if fresh.checked_at.is_some_and(|t| t.elapsed() <= CONFIG_TTL) {
                return fresh.clone();
            }
        }
    }
    let Some(endpoint) = crate::account::cloud_endpoint() else {
        // 未配置服务器 = 只用内置/缓存，这是正常形态，不打扰用户。
        // 同时清掉 last_error：否则用户把服务器地址清空后，面板会一边显示「尚未配置服务器地址」、
        // 一边继续摆着上一次的拉取失败原因，两条状态自相矛盾。
        cur.checked_at = Some(Instant::now());
        cur.last_error = None;
        commit(&cur);
        return cur;
    };
    match fetch_from_server(&endpoint, cur.etag.as_deref()) {
        Ok(FetchOutcome::Fresh(cfg, etag)) => {
            let now = crate::plugin::install::now_rfc3339();
            save_cache(&cfg, etag.as_deref(), &now);
            cur = Loaded {
                cfg,
                origin: Origin::Server,
                etag,
                fetched_at: Some(now),
                checked_at: Some(Instant::now()),
                last_error: None,
            };
            clear_winners();
        }
        Ok(FetchOutcome::NotModified) => {
            cur.origin = Origin::Server;
            cur.checked_at = Some(Instant::now());
            cur.last_error = None;
        }
        Err(e) => {
            // 如实记下失败原因；配置保持原样（缓存或内置），安装流程照常。
            // 统一走 config_error：所有失败路径（HTTP / 传输 / 读取 / 解析 / 超限）都补上
            // 「dev 默认地址未启动属正常」的说明，并再脱敏一次。
            cur.checked_at = Some(Instant::now());
            cur.last_error = Some(config_error(&endpoint, e));
        }
    }
    commit(&cur);
    cur
}

fn commit(l: &Loaded) {
    let mut g = state().lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(l.clone());
}

/// 缓存文件里的 `fetchedAt` 是否仍在 TTL 内（跨重启也不必立刻重拉）。
fn within_ttl(fetched_at: Option<&str>) -> bool {
    let Some(s) = fetched_at else {
        return false;
    };
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) else {
        return false;
    };
    let age = chrono::Local::now().signed_duration_since(t);
    age.num_seconds() >= 0 && (age.num_seconds() as u64) < CONFIG_TTL.as_secs()
}

/// 当前生效的镜像配置（按 TTL 自动刷新）。
pub fn config() -> MirrorConfig {
    ensure(false).cfg
}

// ==================== 下载 ====================

/// 一次下载请求。
pub struct Request {
    pub kind: Kind,
    /// 官方直链（**永远**是候选之一；Gitee / 自建站时它是唯一候选）。
    pub official_url: String,
    /// 官方源的展示名（如 `github.com` / `gitee.com`）。
    pub official_label: String,
    /// 可走镜像的 GitHub 坐标；None = 本次不允许走镜像。
    pub github: Option<GhCoord>,
    /// 直连官方时要带的鉴权头（如 `("Authorization", "Bearer …")`）。
    /// **非空即强制只走官方**——令牌绝不交给第三方镜像。
    pub auth: Option<(String, String)>,
    pub timeout: Duration,
    pub max_bytes: u64,
    /// 动作名，用于拼中文错误（如「下载插件归档」）。
    pub what: String,
    /// 调用方给出的可信 sha256（小写 hex）。None = 无可信来源，下载后**不校验**并如实标注。
    pub expect_sha256: Option<String>,
}

/// 下载结果（含「实际用了哪个源、是否走了镜像、是否做过哈希校验」，供 UI 如实展示）。
pub struct Fetched {
    pub bytes: Vec<u8>,
    /// 实际使用的源的展示名（如 `github.com（官方直连）` / `gh-proxy.com`）。
    pub source_label: String,
    /// 是否经由第三方镜像（镜像**可以篡改内容**，UI 必须据此提示）。
    pub via_mirror: bool,
    /// 本次是否做过 sha256 校验（调用方没有可信哈希时为 false）。
    pub hash_verified: bool,
}

/// **安全铁律的唯一判定点**：带令牌的请求不给镜像坐标（候选集合因此只剩官方直连）。
///
/// 单拎成函数是为了能直接单测这条铁律——它一旦被改错，等于把用户的 PAT 发给第三方反代。
fn mirrorable<'a>(auth: &Option<(String, String)>, github: &'a Option<GhCoord>) -> Option<&'a GhCoord> {
    if auth.is_some() {
        return None;
    }
    github.as_ref()
}

/// 按「竞速选源 → 只用赢家完整下载 → 失败则串行重试其余源」下载内容。
///
/// 竞速**不是**裸的「谁先 2xx 谁赢」：优先组先抢跑（`auto` 是官方、`mirror` 是镜像），
/// 见 [`HEAD_START`] 与 [`race_groups`]。
pub fn fetch(req: &Request) -> Result<Fetched, String> {
    let coord = mirrorable(&req.auth, &req.github);
    let cfg = if coord.is_some() {
        config()
    } else {
        // 不走镜像时没必要为了拉配置多等 5 秒
        MirrorConfig {
            version: 1,
            updated_at: String::new(),
            probe: None,
            mirrors: Vec::new(),
        }
    };
    let mode = mode();
    let cands = build_candidates(
        &cfg,
        req.kind,
        coord,
        &req.official_url,
        &req.official_label,
        mode,
    );
    if cands.is_empty() {
        return Err(format!("{}失败：没有可用的下载源", req.what));
    }

    let mut failures: Vec<(Candidate, String)> = Vec::new();
    let mut winner: Option<Candidate> = None;

    // 只有一个候选（Gitee / 自建站 / 带令牌 / 只走官方）→ 不必竞速
    if cands.len() > 1 {
        // 1) 赢家缓存命中就直接用（TTL 内不重复竞速）
        winner = recall_winner(req.kind).and_then(|id| cands.iter().find(|c| c.id == id).cloned());
        // 2) 否则竞速：优先组先抢跑（auto=官方、mirror=镜像），窗口内胜出则另一组一个请求都不发
        if winner.is_none() {
            let (first, then) = race_groups(&cands, mode);
            let out = race_staged(
                &first,
                &then,
                RACE_CONCURRENCY,
                HEAD_START,
                Arc::new(http_probe),
            );
            failures.extend(out.failures);
            if let Some((c, ms)) = out.winner {
                ilog!(
                    "[iTools] 插件下载源竞速（{}）：{} 胜出（{ms}ms）",
                    req.kind.as_str(),
                    c.label
                );
                remember_winner(req.kind, &c.id);
                winner = Some(c);
            }
        }
    }
    // 3) 赢家优先、其余按原顺序串行兜底（赢家的完整下载也可能中途失败）
    let order = serial_order(&cands, winner.as_ref());

    for c in &order {
        // 校验不通过**不是**直接放弃：单个镜像被篡改时还有别的源可用，
        // 但被篡改的内容一律拒绝、并把「校验失败」如实记进该源的失败原因。
        let attempt = download_one(c, req).and_then(|bytes| {
            let ok = verify_sha256(&bytes, req.expect_sha256.as_deref())?;
            Ok((bytes, ok))
        });
        match attempt {
            Ok((bytes, hash_verified)) => {
                remember_winner(req.kind, &c.id);
                return Ok(Fetched {
                    bytes,
                    source_label: c.label.clone(),
                    via_mirror: c.is_mirror,
                    hash_verified,
                });
            }
            Err(e) => {
                // 任何一个候选下载失败都作废该类型的赢家缓存：宁可下次重新竞速，
                // 也不要抱着一个已经不行的源反复重试（作废是保守方向，代价只是多一次竞速）
                forget_winner(req.kind);
                failures.retain(|(f, _)| f.id != c.id);
                failures.push((c.clone(), e));
            }
        }
    }
    Err(aggregate_error(&req.what, &failures))
}

/// 串行兜底顺序：竞速赢家排第一，其余候选保持原顺序跟在后面（按 id 去重）。
///
/// 「赢了竞速」只说明它**握手最快**，完整下载仍可能中途失败（限流、断流、返回错误页），
/// 所以赢家之后必须还有兜底队列，否则一次偶发失败就等于装不上。纯逻辑，可单测。
fn serial_order(cands: &[Candidate], winner: Option<&Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::with_capacity(cands.len());
    if let Some(w) = winner {
        out.push(w.clone());
    }
    for c in cands {
        if !out.iter().any(|x| x.id == c.id) {
            out.push(c.clone());
        }
    }
    out
}

/// 把每个源的真实失败原因拼成一条**可自查**的错误（而不是笼统一句「下载失败」）。
fn aggregate_error(what: &str, failures: &[(Candidate, String)]) -> String {
    let mut s = format!("{what}失败：所有下载源都不可用");
    for (c, e) in failures {
        let kind = if c.is_mirror { "镜像" } else { "官方直连" };
        s.push_str(&format!("\n· {kind} {}：{e}", c.label));
    }
    s
}

/// 从某个候选源完整下载（带体积上限）。**鉴权头只在非镜像候选上附加。**
fn download_one(c: &Candidate, req: &Request) -> Result<Vec<u8>, String> {
    let mut r = crate::http::get(&c.url)
        .set("User-Agent", USER_AGENT)
        .timeout(req.timeout);
    if !c.is_mirror {
        if let Some((k, v)) = &req.auth {
            r = r.set(k, v);
        }
    }
    let resp = r.call().map_err(|e| explain(&e, Scene::Download))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(req.max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|e| redact_token(format!("读取失败：{e}")))?;
    if buf.len() as u64 > req.max_bytes {
        // 单位自适应：测速探针的上限是 64KB，按 MB 整除会打出「超过 0 MB 上限」这种无意义的话。
        return Err(format!("内容超过 {} 上限", human_bytes(req.max_bytes)));
    }
    if buf.is_empty() {
        return Err("返回内容为空".to_string());
    }
    Ok(buf)
}

// ==================== 命令（设置页 / 诊断） ====================

/// 一个镜像条目的展示视图（原样透出配置字段）。
pub type MirrorView = MirrorEntry;

/// 当前生效的镜像配置 + 来源 + 上次拉取情况。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorConfigView {
    /// `"server"`（服务端下发或刚被服务端 304 确认仍有效）
    /// | `"cache"`（本地缓存，本次没能连上服务端）
    /// | `"builtin"`（编译进客户端的内置列表）。
    pub origin: String,
    /// 服务端地址（未配置为 null —— 此时只用内置/缓存，属正常形态）。
    pub server_url: Option<String>,
    /// 上次成功取得服务端配置的时间（RFC3339）。
    pub fetched_at: Option<String>,
    /// 上次拉取失败的真实原因（成功或未拉过为 null）。
    pub last_error: Option<String>,
    /// 当前下载源偏好：`"auto"` | `"official"` | `"mirror"`。
    pub mode: String,
    pub version: u32,
    pub updated_at: String,
    pub mirrors: Vec<MirrorView>,
    /// 官方源始终参与（不在 mirrors 列表里），这里如实告知 UI。
    pub official_label: String,
}

fn view(l: &Loaded) -> MirrorConfigView {
    MirrorConfigView {
        origin: l.origin.as_str().to_string(),
        server_url: crate::account::cloud_endpoint(),
        fetched_at: l.fetched_at.clone(),
        last_error: l.last_error.clone(),
        mode: mode().as_str().to_string(),
        version: l.cfg.version,
        updated_at: l.cfg.updated_at.clone(),
        mirrors: l.cfg.mirrors.clone(),
        official_label: official_label(OFFICIAL_HOST),
    }
}

/// 命令：查看当前生效的镜像配置（按 TTL 自动刷新，不会每次都打服务端接口）。
///
/// `async fn` + `spawn_blocking`：函数体里可能发生一次 5 秒的服务端请求，
/// 直接跑在异步运行时的 worker 上会挤占其它命令（详见 `install.rs` 的线程模型注释）。
#[tauri::command(async)]
pub async fn plugin_mirror_config() -> Result<MirrorConfigView, String> {
    tauri::async_runtime::spawn_blocking(|| view(&ensure(false)))
        .await
        .map_err(|e| format!("读取镜像配置任务异常终止: {e}"))
}

/// 命令：立即从服务端重拉镜像配置（用户手动点）。失败**如实返回**，不静默吞掉。
#[tauri::command(async)]
pub async fn plugin_mirror_refresh() -> Result<MirrorConfigView, String> {
    let loaded = tauri::async_runtime::spawn_blocking(|| {
        clear_winners();
        ensure(true)
    })
    .await
    .map_err(|e| format!("刷新镜像配置任务异常终止: {e}"))?;
    if crate::account::cloud_endpoint().is_none() {
        return Err("未配置服务器地址：当前使用客户端内置镜像列表（可在「账号 → 数据同步」里填写服务器地址后再试）".to_string());
    }
    if let Some(e) = &loaded.last_error {
        return Err(format!("从服务端拉取镜像配置失败：{e}"));
    }
    Ok(view(&loaded))
}

/// 一个候选源的实测结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorTestResult {
    pub id: String,
    pub label: String,
    /// `"raw"` | `"archive"`：两种路径形态要分开测（实测存在「raw 通、归档 403」的镜像）。
    pub target: String,
    pub is_mirror: bool,
    pub url: String,
    pub ok: bool,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    /// 是否做了内容哈希校验（探测配置没给 sha256 时为 false —— 如实标注，不假装校验过）。
    pub hash_checked: bool,
    /// 哈希校验是否通过（`hash_checked` 为 false 时无意义）。
    pub hash_ok: bool,
}

/// 命令：就地测速所有候选源（含官方），raw 与 archive 两种形态各测一遍。
///
/// raw 目标做一次**完整小下载**（内置探测文件是 13 字节的 README），
/// 配置里给了 `probe.sha256` 时顺带校验内容是否被篡改；archive 目标只发 `Range: bytes=0-0`。
#[tauri::command(async)]
pub async fn plugin_mirror_test() -> Result<Vec<MirrorTestResult>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let cfg = config();
        // 服务端下发的配置若自带 probe（含它自己的 sha256），**以服务端的为准**；
        // 只有完全没给 probe 时才用内置默认探针（默认探针带实测哈希，见 default_probe）
        let p = cfg.probe.clone().unwrap_or_else(default_probe);
        let coord = GhCoord {
            owner: p.owner.clone(),
            repo: p.repo.clone(),
            git_ref: if p.git_ref.trim().is_empty() {
                "HEAD".to_string()
            } else {
                p.git_ref.clone()
            },
            path: p.path.clone(),
        };
        // 测速要看「每个源分别如何」，所以忽略 mode，官方 + 全部镜像（含 unhealthy）都测
        let mut all = cfg.clone();
        for m in &mut all.mirrors {
            m.healthy = true;
        }
        let mut jobs: Vec<(Candidate, Kind)> = Vec::new();
        for kind in [Kind::Raw, Kind::Archive] {
            let official_url = render(
                match kind {
                    Kind::Raw => OFFICIAL_RAW_TPL,
                    Kind::Archive => OFFICIAL_ARCHIVE_TPL,
                },
                &coord,
            );
            // 官方源展示名走统一入口：测速结果的 label 必须与配置视图的 officialLabel 逐字一致，
            // 否则同一个源会在同一个面板里出现两种叫法
            for c in build_candidates(
                &all,
                kind,
                Some(&coord),
                &official_url,
                &official_label(OFFICIAL_HOST),
                Mode::Auto,
            ) {
                jobs.push((c, kind));
            }
        }
        let expect = p.sha256.trim().to_string();
        std::thread::scope(|s| {
            let handles: Vec<_> = jobs
                .into_iter()
                .map(|(c, kind)| {
                    let expect = expect.clone();
                    s.spawn(move || test_one(&c, kind, &expect))
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().ok())
                .collect::<Vec<_>>()
        })
    })
    .await
    .map_err(|e| format!("测速任务异常终止: {e}"))
}

/// 测一个候选源的一种形态。
fn test_one(c: &Candidate, kind: Kind, expect: &str) -> MirrorTestResult {
    let base = MirrorTestResult {
        id: c.id.clone(),
        label: c.label.clone(),
        target: kind.as_str().to_string(),
        is_mirror: c.is_mirror,
        url: c.url.clone(),
        ok: false,
        latency_ms: None,
        error: None,
        hash_checked: false,
        hash_ok: false,
    };
    if kind == Kind::Archive {
        return match http_probe(c) {
            Ok(ms) => MirrorTestResult {
                ok: true,
                latency_ms: Some(ms),
                ..base
            },
            Err(e) => MirrorTestResult {
                error: Some(e),
                ..base
            },
        };
    }
    // raw：完整取回小文件，能顺带做哈希校验
    let start = Instant::now();
    let req = Request {
        kind,
        official_url: c.url.clone(),
        official_label: c.label.clone(),
        github: None,
        auth: None,
        timeout: Duration::from_secs(RACE_TIMEOUT_SECS),
        max_bytes: MAX_PROBE_BYTES,
        what: "探测".to_string(),
        expect_sha256: None,
    };
    match download_one(c, &req) {
        Ok(bytes) => {
            let ms = start.elapsed().as_millis() as u64;
            let exp = if expect.is_empty() { None } else { Some(expect) };
            match verify_sha256(&bytes, exp) {
                Ok(checked) => MirrorTestResult {
                    ok: true,
                    latency_ms: Some(ms),
                    hash_checked: checked,
                    hash_ok: checked,
                    ..base
                },
                Err(e) => MirrorTestResult {
                    ok: false,
                    latency_ms: Some(ms),
                    error: Some(e),
                    hash_checked: true,
                    hash_ok: false,
                    ..base
                },
            }
        }
        Err(e) => MirrorTestResult {
            error: Some(e),
            ..base
        },
    }
}

/// 命令：设置下载源偏好（`"auto"` | `"official"` | `"mirror"`），持久化到设置。
#[tauri::command(async)]
pub async fn plugin_mirror_set_mode(
    mode: String,
    settings: tauri::State<'_, crate::settings::SettingsStore>,
) -> Result<(), String> {
    let m = match mode.trim().to_ascii_lowercase().as_str() {
        "auto" => Mode::Auto,
        "official" => Mode::Official,
        "mirror" => Mode::Mirror,
        other => return Err(format!("不认识的下载源模式：{other}（只支持 auto / official / mirror）")),
    };
    let mut s = settings.get();
    s.plugin_mirror_mode = m.as_str().to_string();
    settings.set(s);
    set_mode(m.as_str());
    Ok(())
}

// ==================== 测试（全部不依赖网络） ====================

#[cfg(test)]
mod tests {
    use super::*;

    fn coord() -> GhCoord {
        GhCoord {
            owner: "octocat".into(),
            repo: "Hello-World".into(),
            git_ref: "HEAD".into(),
            path: "plugin.json".into(),
        }
    }

    fn cand(id: &str, is_mirror: bool) -> Candidate {
        Candidate {
            id: id.to_string(),
            label: id.to_string(),
            url: format!("https://{id}/x"),
            is_mirror,
        }
    }

    /// 不分组的纯竞速（全部候选一起跑、无抢跑窗口）——只有测试需要这种形态，
    /// 生产路径一律走 [`race_groups`] + [`race_staged`]。
    fn race_all(cands: &[Candidate], concurrency: usize, probe: ProbeFn) -> RaceOutcome {
        race_staged(cands, &[], concurrency, Duration::ZERO, probe)
    }

    /// 模板替换：四个占位符、各段编码、ref 与 path 里的 `/` 保留。
    #[test]
    fn render_placeholders() {
        let c = coord();
        assert_eq!(
            render(OFFICIAL_RAW_TPL, &c),
            "https://raw.githubusercontent.com/octocat/Hello-World/HEAD/plugin.json"
        );
        assert_eq!(
            render(OFFICIAL_ARCHIVE_TPL, &c),
            "https://codeload.github.com/octocat/Hello-World/zip/HEAD"
        );

        // ref 里的 / 必须保留（refs/heads/main 是合法 ref 形态）
        let c2 = GhCoord {
            git_ref: "refs/heads/main".into(),
            path: "plugins/foo/plugin.json".into(),
            ..coord()
        };
        assert_eq!(
            render(OFFICIAL_RAW_TPL, &c2),
            "https://raw.githubusercontent.com/octocat/Hello-World/refs/heads/main/plugins/foo/plugin.json"
        );

        // owner/repo 段里的 / 与其它保留字符必须被编码（否则能凭空改变 URL 结构）
        let c3 = GhCoord {
            owner: "a/b".into(),
            repo: "c d?e#f".into(),
            git_ref: "v1.0".into(),
            path: "a b".into(),
        };
        let out = render(OFFICIAL_RAW_TPL, &c3);
        assert_eq!(
            out,
            "https://raw.githubusercontent.com/a%2Fb/c%20d%3Fe%23f/v1.0/a%20b"
        );

        // 编码后的值不含 { }，因此不会被后续替换二次展开
        let evil = GhCoord {
            owner: "{repo}".into(),
            repo: "R".into(),
            git_ref: "HEAD".into(),
            path: "p".into(),
        };
        assert!(render(OFFICIAL_RAW_TPL, &evil).contains("%7Brepo%7D"));

        // 镜像模板（内置默认）也走同一套替换
        let m = &builtin_config().mirrors[0];
        assert_eq!(
            render(&m.archive, &coord()),
            "https://gh-proxy.com/https://github.com/octocat/Hello-World/archive/HEAD.zip"
        );
    }

    /// 候选源组装：官方永远在；镜像按 latency 升序；unhealthy 被剔除；模式影响顺序/集合。
    #[test]
    fn candidate_ordering_and_modes() {
        let mut cfg = builtin_config();
        cfg.mirrors[0].latency_ms = Some(900); // gh-proxy
        cfg.mirrors[1].latency_ms = Some(300); // ghfast
        cfg.mirrors[2].healthy = false; // ghproxy-net 被判失效

        let c = coord();
        let auto = build_candidates(&cfg, Kind::Archive, Some(&c), "https://official/x", "github.com", Mode::Auto);
        let ids: Vec<&str> = auto.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec![OFFICIAL_ID, "ghfast", "gh-proxy"], "官方在前，镜像按延迟升序，失效镜像剔除");
        assert!(!auto[0].is_mirror && auto[1].is_mirror);

        // 只走官方
        let off = build_candidates(&cfg, Kind::Archive, Some(&c), "https://official/x", "github.com", Mode::Official);
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].id, OFFICIAL_ID);

        // 镜像优先，官方仍兜底在最后（不会因为没镜像就装不上）
        let mir = build_candidates(&cfg, Kind::Archive, Some(&c), "https://official/x", "github.com", Mode::Mirror);
        let ids: Vec<&str> = mir.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["ghfast", "gh-proxy", OFFICIAL_ID]);

        // 无坐标（Gitee / 自建站 / 带令牌）→ 只剩官方
        let none = build_candidates(&cfg, Kind::Archive, None, "https://gitee/x", "gitee.com", Mode::Auto);
        assert_eq!(none.len(), 1);
        assert_eq!(none[0].url, "https://gitee/x");

        // 无延迟数据的镜像排在有数据者之后（而不是被当成 0 排最前）
        let mut c2 = builtin_config();
        c2.mirrors[0].latency_ms = None;
        c2.mirrors[1].latency_ms = Some(800);
        c2.mirrors[2].latency_ms = Some(100);
        let v = build_candidates(&c2, Kind::Raw, Some(&c), "https://official/x", "github.com", Mode::Auto);
        let ids: Vec<&str> = v.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec![OFFICIAL_ID, "ghproxy-net", "ghfast", "gh-proxy"]);
    }

    /// 竞速：先返回成功者胜出，并发度受限，全败时收齐每个源的真实原因。
    #[test]
    fn race_first_success_wins() {
        let cands = vec![cand("slow", true), cand("fast", true), cand("dead", true)];
        let probe: ProbeFn = Arc::new(|c: &Candidate| match c.id.as_str() {
            "slow" => {
                std::thread::sleep(Duration::from_millis(300));
                Ok(300)
            }
            "fast" => {
                std::thread::sleep(Duration::from_millis(10));
                Ok(10)
            }
            _ => Err("DNS 解析失败".to_string()),
        });
        let out = race_all(&cands, 3, probe);
        let (w, _) = out.winner.expect("应有赢家");
        assert_eq!(w.id, "fast", "先返回成功的候选胜出");

        // 全败：每个源都要带上自己的真实原因
        let cands = vec![cand("a", true), cand("b", true)];
        let probe: ProbeFn = Arc::new(|c: &Candidate| Err(format!("{} 挂了", c.id)));
        let out = race_all(&cands, 3, probe);
        assert!(out.winner.is_none());
        assert_eq!(out.failures.len(), 2);
        let msg = aggregate_error("下载插件归档", &out.failures);
        assert!(msg.contains("a 挂了") && msg.contains("b 挂了"), "聚合错误要保留每个源的原因: {msg}");

        // 并发度上限：第二批才轮到的候选，在第一批已出赢家时不应被探测
        let probed = Arc::new(Mutex::new(Vec::<String>::new()));
        let p2 = probed.clone();
        let cands: Vec<Candidate> = (0..5).map(|i| cand(&format!("m{i}"), true)).collect();
        let probe: ProbeFn = Arc::new(move |c: &Candidate| {
            p2.lock().unwrap().push(c.id.clone());
            Ok(1)
        });
        let out = race_all(&cands, 3, probe);
        assert!(out.winner.is_some());
        std::thread::sleep(Duration::from_millis(50));
        assert!(probed.lock().unwrap().len() <= 3, "并发度上限 3，第一批出了赢家就不该再探测后续批次");
    }

    /// **安全核心**：抢跑窗口内优先组胜出 ⇒ 另一组**一个请求都不发**。
    ///
    /// 这条一旦被改坏，就退回「谁先 2xx 谁赢」的裸竞速：一个响应极快的恶意镜像
    /// 能稳定压过官方源，而手动粘 URL 安装恰恰没有可信 sha256 可校验。
    #[test]
    fn official_head_start_blocks_mirrors() {
        let official = vec![cand("official", false)];
        let mirrors = vec![cand("m1", true), cand("m2", true)];
        let touched = Arc::new(Mutex::new(Vec::<String>::new()));

        // 官方 20ms 就通，镜像即便 0ms 也不该被请求
        let t = touched.clone();
        let probe: ProbeFn = Arc::new(move |c: &Candidate| {
            t.lock().unwrap().push(c.id.clone());
            if c.is_mirror {
                Ok(0)
            } else {
                std::thread::sleep(Duration::from_millis(20));
                Ok(20)
            }
        });
        let out = race_staged(&official, &mirrors, 3, Duration::from_millis(400), probe);
        assert_eq!(out.winner.expect("官方应在窗口内胜出").0.id, "official");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            *touched.lock().unwrap(),
            vec!["official".to_string()],
            "官方抢跑成功时镜像必须一个请求都收不到"
        );
    }

    /// 抢跑失败的两条路径都必须落到另一组，且不会白等满窗口。
    #[test]
    fn head_start_falls_through_on_failure_and_timeout() {
        let official = vec![cand("official", false)];
        let mirrors = vec![cand("m1", true)];

        // a) 官方**立刻失败** → 不等满 400ms 就该放镜像入场
        let probe: ProbeFn = Arc::new(|c: &Candidate| {
            if c.is_mirror {
                Ok(7)
            } else {
                Err("连接失败（可能被墙 / 网络不可达）".to_string())
            }
        });
        let start = Instant::now();
        let out = race_staged(&official, &mirrors, 3, Duration::from_millis(400), probe);
        assert_eq!(out.winner.expect("镜像应兜底胜出").0.id, "m1");
        assert!(
            start.elapsed() < Duration::from_millis(300),
            "官方已失败就该立刻放镜像入场，不该白等满抢跑窗口：{:?}",
            start.elapsed()
        );
        assert_eq!(out.failures.len(), 1, "官方的真实失败原因要留给用户自查");
        assert!(out.failures[0].1.contains("被墙"));

        // b) 官方**窗口内没响应** → 镜像入场；官方仍在赛道上，晚到的成功一样算数
        let probe: ProbeFn = Arc::new(|c: &Candidate| {
            if c.is_mirror {
                std::thread::sleep(Duration::from_millis(300));
                Ok(300)
            } else {
                std::thread::sleep(Duration::from_millis(80));
                Ok(80)
            }
        });
        let out = race_staged(&official, &mirrors, 3, Duration::from_millis(20), probe);
        assert_eq!(
            out.winner.expect("应有赢家").0.id,
            "official",
            "抢跑超时不等于弃权：官方晚到的成功仍应胜出（镜像更慢）"
        );

        // c) 优先组为空（只走官方 / 没有镜像）→ 退化成普通竞速，不因空组卡住
        let out = race_staged(&[], &mirrors, 3, Duration::from_millis(400), Arc::new(|_: &Candidate| Ok(1)));
        assert_eq!(out.winner.expect("应有赢家").0.id, "m1");
    }

    /// 分组规则：auto/official 官方先发；mirror 是用户显式选择，反过来镜像先发、官方兜底。
    #[test]
    fn race_groups_by_mode() {
        let cands = vec![cand("official", false), cand("m1", true), cand("m2", true)];
        let ids = |v: &[Candidate]| v.iter().map(|c| c.id.clone()).collect::<Vec<_>>();

        for m in [Mode::Auto, Mode::Official] {
            let (first, then) = race_groups(&cands, m);
            assert_eq!(ids(&first), vec!["official"], "{m:?}：官方抢跑");
            assert_eq!(ids(&then), vec!["m1", "m2"], "{m:?}：镜像延迟入场");
        }
        let (first, then) = race_groups(&cands, Mode::Mirror);
        assert_eq!(ids(&first), vec!["m1", "m2"], "mirror 模式：镜像抢跑");
        assert_eq!(ids(&then), vec!["official"], "mirror 模式：官方仍兜底，不是被删掉");
    }

    /// `catch_unwind` 兜底：探测函数 panic 也必须回一条失败，否则接收循环会永远等下去。
    /// （测试输出里那条 panic backtrace 是本用例故意触发、且已被捕获的，不是失败）
    #[test]
    fn panicking_probe_does_not_hang_race() {
        let cands = vec![cand("boom", true), cand("ok", true)];
        let probe: ProbeFn = Arc::new(|c: &Candidate| {
            if c.id == "boom" {
                panic!("探测函数炸了");
            }
            std::thread::sleep(Duration::from_millis(20));
            Ok(20)
        });
        let out = race_all(&cands, 1, probe); // 并发度 1：不处理 panic 就永远轮不到 "ok"
        assert_eq!(out.winner.expect("panic 不该拖死竞速").0.id, "ok");
    }

    /// 串行兜底顺序：赢家第一、其余保持原顺序且不重复；没有赢家时就是原顺序。
    #[test]
    fn serial_fallback_order() {
        let cands = vec![cand("official", false), cand("m1", true), cand("m2", true)];
        let ids = |v: &[Candidate]| v.iter().map(|c| c.id.clone()).collect::<Vec<_>>();

        // 赢家提到最前，且不会在队列里出现两次（否则同一个挂掉的源会被重试两遍）
        let out = serial_order(&cands, Some(&cands[2]));
        assert_eq!(ids(&out), vec!["m2", "official", "m1"]);
        assert_eq!(out.len(), cands.len());

        // 没有赢家（竞速全败 / 只有一个候选）→ 原顺序串行重试
        assert_eq!(ids(&serial_order(&cands, None)), vec!["official", "m1", "m2"]);
    }

    /// sha256：给了期望值就必须校验；不匹配必须拒绝；没给就如实标注「未校验」。
    #[test]
    fn sha256_verification() {
        // 空串的 sha256 是众所周知的常量，可离线核对
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let data = b"hello";
        let want = sha256_hex(data);
        assert_eq!(want, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");

        assert_eq!(verify_sha256(data, Some(&want)), Ok(true));
        // 大小写不敏感
        assert_eq!(verify_sha256(data, Some(&want.to_uppercase())), Ok(true));
        // 未提供期望值 → 未校验（而不是「校验通过」）
        assert_eq!(verify_sha256(data, None), Ok(false));
        assert_eq!(verify_sha256(data, Some("   ")), Ok(false));
        // 不匹配 → 拒绝
        let err = verify_sha256(data, Some(&"0".repeat(64))).unwrap_err();
        assert!(err.contains("校验失败"), "{err}");
    }

    /// 外来配置清洗：非 https / 缺占位符 / archive 混入 {path} / 重复 id 一律丢弃。
    #[test]
    fn sanitize_rejects_bad_entries() {
        let mk = |id: &str, raw: &str, archive: &str| MirrorEntry {
            id: id.to_string(),
            label: String::new(),
            raw: raw.to_string(),
            archive: archive.to_string(),
            healthy: true,
            latency_ms: None,
            last_ok_at: None,
            host: String::new(),
            unlisted: false,
        };
        let good_raw = "https://m.tld/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}";
        let good_arc = "https://m.tld/https://github.com/{owner}/{repo}/archive/{ref}.zip";
        let cfg = MirrorConfig {
            version: 0,
            updated_at: String::new(),
            probe: None,
            mirrors: vec![
                mk("ok", good_raw, good_arc),
                // 明文 http：会被中间人改包
                mk("plain", "http://m/{owner}/{repo}/{ref}/{path}", good_arc),
                // raw 缺 {path}：所有插件会被指向同一个固定文件
                mk("nopath", "https://m/{owner}/{repo}/{ref}", good_arc),
                // archive 混入 {path}：模板写串了
                mk("badarc", good_raw, "https://m/{owner}/{repo}/{ref}/{path}.zip"),
                // 重复 id
                mk("ok", good_raw, good_arc),
                // 空 id
                mk("  ", good_raw, good_arc),
            ],
        };
        let out = sanitize(cfg);
        let ids: Vec<&str> = out.mirrors.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["ok"]);
        assert_eq!(out.mirrors[0].label, "ok", "label 为空时回退成 id");
        assert_eq!(out.version, 1, "version 缺失补成 1");

        // 条目数上限
        let many = MirrorConfig {
            version: 1,
            updated_at: String::new(),
            probe: None,
            mirrors: (0..40).map(|i| mk(&format!("m{i}"), good_raw, good_arc)).collect(),
        };
        assert_eq!(sanitize(many).mirrors.len(), MAX_MIRRORS);
    }

    /// 服务端下发的**新主机**必须被标出来，且这个标记不接受外部输入。
    #[test]
    fn unlisted_hosts_are_flagged_and_not_forgeable() {
        // 主机名解析：userinfo 形态取 @ 之后那段，否则 `https://gh-proxy.com@evil.tld/…`
        // 会被当成内置源（肉眼也容易看错）
        assert_eq!(tpl_host("https://gh-proxy.com/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}"), "gh-proxy.com");
        assert_eq!(tpl_host("https://GH-Proxy.COM/x/{owner}/{repo}/{ref}/{path}"), "gh-proxy.com");
        assert_eq!(tpl_host("https://gh-proxy.com@evil.tld/{owner}/{repo}/{ref}/{path}"), "evil.tld");
        assert_eq!(tpl_host("https://evil.tld:8443/{owner}/{repo}/{ref}/{path}"), "evil.tld:8443");
        assert_eq!(tpl_host("http://x/{owner}"), "", "非 https 拿不到主机（valid_tpl 也会先丢弃它）");

        // 服务端下发一条新主机 + 一条冒充内置的：unlisted 由客户端判定，服务端说了不算
        let json = r#"{"version":9,"mirrors":[
            {"id":"evil","label":"gh-proxy.com","unlisted":false,"host":"gh-proxy.com",
             "raw":"https://evil.tld/{owner}/{repo}/{ref}/{path}",
             "archive":"https://evil.tld/{owner}/{repo}/{ref}.zip"},
            {"id":"gh-proxy","label":"gh-proxy.com",
             "raw":"https://gh-proxy.com/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}",
             "archive":"https://gh-proxy.com/https://github.com/{owner}/{repo}/archive/{ref}.zip"},
            {"id":"mixed","label":"半真半假",
             "raw":"https://gh-proxy.com/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}",
             "archive":"https://evil.tld/{owner}/{repo}/{ref}.zip"}
        ]}"#;
        let cfg = sanitize(serde_json::from_str::<MirrorConfig>(json).unwrap());
        let by = |id: &str| cfg.mirrors.iter().find(|m| m.id == id).unwrap();
        assert!(by("evil").unlisted, "内置列表外的主机必须标成新增（服务端塞的 unlisted:false 不作数）");
        assert_eq!(by("evil").host, "evil.tld", "host 由模板现场解析，服务端塞的 host 不作数");
        assert_eq!(by("evil").label, "gh-proxy.com", "label 仍是服务端给的文本——正因为它能撒谎，UI 展示主机必须用 host");
        assert!(!by("gh-proxy").unlisted, "主机确实是内置的那家 → 不标新增");
        assert!(by("mixed").unlisted, "raw 内置、archive 指向别处 → 仍算新主机（两个模板都要在内置列表内）");

        // 新主机不被禁用，只是排在已知主机之后（已知主机全挂时它照样会被用上）
        let c = coord();
        let ranked = build_candidates(&cfg, Kind::Archive, Some(&c), "https://official/x", "github.com", Mode::Mirror);
        let ids: Vec<&str> = ranked.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["gh-proxy", "evil", "mixed", OFFICIAL_ID], "内置主机优先，新主机仍在候选里");
    }

    /// 内置默认列表本身必须过得了自己的清洗规则（否则兜底层就是空的）。
    #[test]
    fn builtin_config_is_valid() {
        let b = builtin_config();
        let s = sanitize(b.clone());
        assert_eq!(s.mirrors.len(), b.mirrors.len(), "内置镜像不该被自己的校验刷掉");
        // 内置源必须自报家门为「非新增」，且主机名与其模板一致（否则 UI 会把内置源标成可疑）
        assert!(s.mirrors.iter().all(|m| !m.unlisted), "内置源不该被标成服务端新增");
        assert_eq!(s.mirrors[0].host, "gh-proxy.com");
        assert!(builtin_loaded().cfg.mirrors.iter().all(|m| !m.host.is_empty()), "兜底层也必须带上主机名");
        assert!(b.mirrors.iter().any(|m| m.id == "gh-proxy"));
        assert!(b.mirrors.iter().any(|m| m.id == "ghfast"));
        // 内置探针必须带**实测**哈希，且 ref 必须 pin 到固定 commit：
        // 用分支（HEAD/master）的话上游一变内容就变，所有源都会被误报成「内容与官方不一致」
        let p = b.probe.as_ref().unwrap();
        assert_eq!(
            p.sha256, "03ba204e50d126e4674c005e04d82e84c21366780af1f43bd54a37816b6ab340",
            "内置探针哈希被改动过：改探测文件必须同时实测新哈希（curl … | sha256sum），不许照抄旧值"
        );
        assert_eq!(p.sha256.len(), 64, "sha256 必须是 64 位小写 hex");
        assert!(p.sha256.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(p.git_ref.len(), 40, "探针 ref 必须是 40 位 commit sha（不能用分支名）");
        assert!(p.git_ref.chars().all(|c| c.is_ascii_hexdigit()), "探针 ref 必须是 commit sha");
        // 这份内容是已知的：13 字节的 "Hello World!\n"，其哈希可离线自洽核对
        assert_eq!(sha256_hex(b"Hello World!\n"), p.sha256, "写死的哈希与已知探针内容不一致");
        // 官方模板不在 mirrors 里（官方是内置固定候选）
        assert!(!b.mirrors.iter().any(|m| m.raw.starts_with("https://raw.githubusercontent.com")));
    }

    /// 同一个状态码在「下载插件包」与「拉镜像配置」两个场景下必须给**不同**的成因。
    ///
    /// 本轮修复的真实故障：dev 下服务端没起，`/api/mirrors` 返回 404，UI 却显示
    /// 「仓库/分支/子目录写错」——用户照着这句话去查仓库地址，方向完全错了。
    #[test]
    fn status_wording_is_scene_specific() {
        // 404：两个场景的措辞必须各自说中自己的成因，且不许串台
        let dl404 = status_hint(Scene::Download, 404);
        let cfg404 = status_hint(Scene::MirrorConfig, 404);
        assert_ne!(dl404, cfg404, "两个场景的 404 文案不能是同一句");
        assert!(dl404.contains("仓库"), "{dl404}");
        assert!(cfg404.contains(MIRRORS_API_PATH), "拉配置的 404 要点名缺的是哪个端点：{cfg404}");
        assert!(
            !cfg404.contains("仓库") && !cfg404.contains("分支"),
            "拉配置失败与仓库/分支毫无关系，出现这些词就是误导：{cfg404}"
        );

        // 401/403：下载场景谈 GitHub 令牌与限流，配置场景谈服务端鉴权
        assert!(status_hint(Scene::Download, 401).contains("ITOOLS_GITHUB_TOKEN"));
        let cfg401 = status_hint(Scene::MirrorConfig, 401);
        assert!(cfg401.contains("服务端") && !cfg401.contains("ITOOLS_GITHUB_TOKEN"), "{cfg401}");
        assert!(status_hint(Scene::MirrorConfig, 403).contains("服务端"));

        // 5xx：下载场景是「该源自身故障」，配置场景要区分「网关不可用」与「服务端内部错误」
        assert!(status_hint(Scene::Download, 500).contains("该源"));
        assert!(status_hint(Scene::MirrorConfig, 500).contains("服务端内部错误"));
        assert!(status_hint(Scene::MirrorConfig, 502).contains("网关"), "502 是反代后面没实例的典型形态");
        assert!(status_hint(Scene::MirrorConfig, 418).contains("服务端"));

        // 传输层同理：下载谈「被墙」，拉配置谈「服务端未启动 / 地址端口写错」
        let dl = transport_hint(Scene::Download, ureq::ErrorKind::ConnectionFailed, false);
        let cf = transport_hint(Scene::MirrorConfig, ureq::ErrorKind::ConnectionFailed, false);
        assert!(dl.contains("被墙"), "{dl}");
        assert!(cf.contains("服务端未启动") && !cf.contains("被墙"), "{cf}");
        assert_ne!(
            transport_hint(Scene::Download, ureq::ErrorKind::Dns, false),
            transport_hint(Scene::MirrorConfig, ureq::ErrorKind::Dns, false)
        );
        // 超时与非超时是两条不同的结论，不能合并
        assert_ne!(
            transport_hint(Scene::MirrorConfig, ureq::ErrorKind::ConnectionFailed, true),
            transport_hint(Scene::MirrorConfig, ureq::ErrorKind::ConnectionFailed, false)
        );
    }

    /// dev 默认地址（debug 下没配服务器时的兜底）连不上是**正常现象**，必须说清楚，
    /// 否则每个跑 `npm run tauri dev` 的人都会以为出了故障。
    #[test]
    fn dev_default_endpoint_gets_a_not_a_bug_note() {
        let dev = crate::account::DEV_DEFAULT_ENDPOINT;
        let note = dev_endpoint_note(dev);
        assert!(!note.is_empty(), "dev 默认地址必须带说明");
        assert!(note.contains("开发模式") && note.contains("正常"), "{note}");
        assert!(note.contains("server/"), "要告诉用户去哪儿启动服务端：{note}");
        // 末尾斜杠等规范化差异不该让提示丢失
        assert!(!dev_endpoint_note(&format!("{dev}/")).is_empty());
        // 别的地址不许套这句话（那是假信息）
        assert!(dev_endpoint_note("https://sync.example.com").is_empty());
        assert!(dev_endpoint_note("http://127.0.0.1:9999").is_empty());

        // 完整文案 = 场景化原因 + dev 说明
        let full = config_error(dev, status_hint(Scene::MirrorConfig, 404));
        assert!(full.contains(MIRRORS_API_PATH) && full.contains("开发模式"), "{full}");
        // 非 dev 地址只有原因，不带 dev 说明
        let plain = config_error("https://sync.example.com", status_hint(Scene::MirrorConfig, 404));
        assert!(!plain.contains("开发模式"), "{plain}");
        // 脱敏约束不能破：任何出口都要过 redact_token
        let redacted = config_error("https://s.tld?access_token=SECRET", "拉取失败：https://s.tld?access_token=SECRET".to_string());
        assert!(!redacted.contains("SECRET"), "{redacted}");
    }

    /// 内置探针**能真的比对哈希**：内容对得上 → 通过；被改一个字节 → 判为不一致（而不是「未校验」）。
    ///
    /// 这条守的是 `plugin_mirror_test` 的 raw 分支：它把 `probe.sha256` 交给 [`verify_sha256`]，
    /// `Ok(true)` → `hashChecked=true, hashOk=true`；`Err` → `hashChecked=true, hashOk=false`。
    #[test]
    fn builtin_probe_hash_actually_checks_content() {
        let p = default_probe();
        let body = b"Hello World!\n";
        assert_eq!(verify_sha256(body, Some(&p.sha256)), Ok(true), "官方内容应校验通过");
        let tampered = b"Hello World!\nevil";
        let err = verify_sha256(tampered, Some(&p.sha256)).unwrap_err();
        assert!(err.contains("篡改"), "内容被改必须给出「可能被篡改」的结论：{err}");
        // 服务端下发的 probe 优先：它给空哈希时就是「未校验」，不许悄悄套用内置哈希冒充校验过
        assert_eq!(verify_sha256(body, Some("")), Ok(false));

        // 探针坐标能被模板正确渲染（commit sha 形态不会被编码破坏）
        let coord = GhCoord {
            owner: p.owner.clone(),
            repo: p.repo.clone(),
            git_ref: p.git_ref.clone(),
            path: p.path.clone(),
        };
        assert_eq!(
            render(OFFICIAL_RAW_TPL, &coord),
            "https://raw.githubusercontent.com/octocat/Hello-World/7fd1a60b01f91b314f59955a4e4d4e80d8edf11d/README"
        );
    }

    /// 配置兜底链：缓存信封能原样往返；损坏的缓存不会让配置整体作废。
    #[test]
    fn cache_envelope_roundtrip_and_fallback() {
        let env = CacheEnvelope {
            etag: Some("W/\"abc\"".to_string()),
            fetched_at: Some("2026-08-12T10:00:00+08:00".to_string()),
            config: builtin_config(),
        };
        let text = serde_json::to_string(&env).unwrap();
        assert!(text.contains("\"fetchedAt\""), "缓存字段应为 camelCase: {text}");
        assert!(text.contains("\"latencyMs\""));
        let back: CacheEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(back.etag.as_deref(), Some("W/\"abc\""));
        assert_eq!(back.config.mirrors.len(), 3);

        // 损坏 JSON → 解析失败（调用方据此回落到内置，不 panic）
        assert!(serde_json::from_str::<CacheEnvelope>("{ not json").is_err());

        // 服务端可能只给最小字段：healthy 默认 true、latencyMs/lastOkAt 可缺省
        let minimal = r#"{"version":1,"mirrors":[{"id":"m","raw":"https://m/{owner}/{repo}/{ref}/{path}","archive":"https://m/{owner}/{repo}/{ref}.zip"}]}"#;
        let cfg: MirrorConfig = serde_json::from_str(minimal).unwrap();
        assert!(cfg.mirrors[0].healthy, "healthy 缺省应为 true");
        assert!(cfg.mirrors[0].latency_ms.is_none());
        assert_eq!(sanitize(cfg).mirrors.len(), 1);
    }

    /// TTL 判定：新鲜时间戳内不重拉；过期/非法/未来时间都要重拉。
    #[test]
    fn config_ttl_window() {
        let fresh = chrono::Local::now().to_rfc3339();
        assert!(within_ttl(Some(&fresh)));
        let old = (chrono::Local::now() - chrono::Duration::minutes(31)).to_rfc3339();
        assert!(!within_ttl(Some(&old)));
        assert!(!within_ttl(None));
        assert!(!within_ttl(Some("not a date")));
        let future = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(!within_ttl(Some(&future)), "时钟异常的未来时间不该被当成新鲜");
    }

    /// 模式解析与持久化字符串一一对应（设置里存的是这几个字面量）。
    #[test]
    fn mode_parse_roundtrip() {
        for (s, m) in [
            ("auto", Mode::Auto),
            ("official", Mode::Official),
            ("MIRROR", Mode::Mirror),
            ("  official ", Mode::Official),
            ("乱写", Mode::Auto),
        ] {
            assert_eq!(Mode::parse(s), m, "{s}");
        }
        assert_eq!(Mode::parse(Mode::Mirror.as_str()), Mode::Mirror);
    }

    /// 安全铁律：带令牌 ⇒ 不给镜像坐标 ⇒ 候选只剩官方直连（凭据绝不发给第三方反代）。
    #[test]
    fn token_never_reaches_mirrors() {
        let gh = Some(coord());
        let none: Option<(String, String)> = None;
        let auth = Some(("Authorization".to_string(), "Bearer secret".to_string()));

        assert!(mirrorable(&none, &gh).is_some(), "无令牌时才允许走镜像");
        assert!(mirrorable(&auth, &gh).is_none(), "带令牌必须屏蔽镜像坐标");

        // 端到端到候选集合：带令牌时无论配置里有多少健康镜像，都只剩官方一个候选
        let cfg = builtin_config();
        assert!(cfg.mirrors.len() >= 2);
        let with_token = build_candidates(
            &cfg,
            Kind::Archive,
            mirrorable(&auth, &gh),
            "https://codeload.github.com/o/r/zip/HEAD",
            "github.com",
            Mode::Auto,
        );
        assert_eq!(with_token.len(), 1);
        assert!(!with_token[0].is_mirror);
        // 连「强制走镜像」模式也不能例外
        let forced = build_candidates(
            &cfg,
            Kind::Archive,
            mirrorable(&auth, &gh),
            "https://codeload.github.com/o/r/zip/HEAD",
            "github.com",
            Mode::Mirror,
        );
        assert_eq!(forced.len(), 1);
        assert!(!forced[0].is_mirror, "mirror 模式也不许把带令牌的请求发给镜像");
    }

    /// 赢家缓存：记住 → 命中 → 失败后作废。
    #[test]
    fn winner_cache_lifecycle() {
        clear_winners();
        assert!(recall_winner(Kind::Archive).is_none());
        remember_winner(Kind::Archive, "ghfast");
        assert_eq!(recall_winner(Kind::Archive).as_deref(), Some("ghfast"));
        // raw 与 archive 各自独立（实测存在「raw 通、归档 403」的镜像）
        assert!(recall_winner(Kind::Raw).is_none());
        forget_winner(Kind::Archive);
        assert!(recall_winner(Kind::Archive).is_none());
        clear_winners();
    }

    /// 官方源展示名只有一个出处：配置视图（`MirrorConfigView.officialLabel`）与
    /// 测速结果（`MirrorTestResult.label`）必须逐字一致，否则同一个源会在同一个面板里
    /// 出现两种叫法（本轮之前测速传的是裸 `"github.com"`）。
    #[test]
    fn official_label_is_single_sourced() {
        let cfg = MirrorConfig {
            version: 1,
            updated_at: String::new(),
            probe: None,
            mirrors: Vec::new(),
        };
        let coord = GhCoord {
            owner: "o".into(),
            repo: "r".into(),
            git_ref: "HEAD".into(),
            path: "p".into(),
        };
        // 与 plugin_mirror_test 走同一个入口取官方源标签
        let cands = build_candidates(
            &cfg,
            Kind::Archive,
            Some(&coord),
            "https://official/x",
            &official_label(OFFICIAL_HOST),
            Mode::Auto,
        );
        assert_eq!(cands[0].id, OFFICIAL_ID);
        // view() 里 MirrorConfigView.official_label 用的也是这一个函数
        assert_eq!(cands[0].label, official_label(OFFICIAL_HOST));
        assert_eq!(official_label(OFFICIAL_HOST), "github.com（官方直连）");
    }
}
