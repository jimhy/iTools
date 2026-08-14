//! 插件市场：索引拉取、插件包下载与**内容哈希**校验。
//!
//! # 市场在自建服务端上，不在 GitHub
//!
//! 早期版本的市场索引是 GitHub 仓库里的 `registry/index.json`，插件包从 GitHub 归档下载，
//! 收录靠人工提 Issue/PR。现在索引与包**都由自建服务端提供**：
//!
//! - 索引：`GET {服务器}/api/market/index`
//! - 插件包：`GET {服务器}/api/market/package/{name}`（zip）
//!
//! 服务器地址就是「设置 → 网络」里那一个（与云同步共用），用户没填时用内置默认服务。
//! 作者在开发者中心点「提交审核」把包传上去，服务端跑大模型审核，通过即发布——
//! 客户端这边不需要知道 GitHub 的任何事。
//!
//! 手动粘 Git 仓库地址安装那条路**仍然保留**（见 `install.rs`），它是「不经市场、自己装」的能力，
//! 与市场是两条独立链路。
//!
//! # 内容哈希是这条信任链的收口
//!
//! 市场条目带着审核时算出的内容哈希，客户端下载解压后重算一遍比对，对不上一律拒绝安装。
//! 这挡的是「传输途中包被改过」——包与索引来自同一台服务器，服务器本身是信任起点。
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

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::logging::ilog;

/// 索引缓存文件名（放在 `%LOCALAPPDATA%\itools\` 下，与镜像配置缓存同级）。
const CACHE_FILE: &str = "market-index.json";
/// 索引拉取超时（秒）与体积上限。索引是纯文本 JSON，2MB 足够放几千个条目。
const FETCH_TIMEOUT_SECS: u64 = 10;
const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;
/// 插件包下载超时（秒）与体积上限。与 `install.rs` 的归档上限一致（32MB）。
const PACKAGE_TIMEOUT_SECS: u64 = 120;
const MAX_PACKAGE_BYTES: u64 = 32 * 1024 * 1024;

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
    #[serde(default)]
    pub version: String,
    /// 插件包在服务端的**相对**下载路径（如 `/api/market/package/deskbox`）。
    ///
    /// 服务端给的是相对路径，客户端拼上自己配置的服务器地址——这样反代、内网、端口转发
    /// 都不影响下载，服务端也不需要知道自己被从哪个域名访问。
    #[serde(default)]
    pub package: String,
    /// 审核时算出的内容哈希（`sha256:…`）。空串表示索引里没给，
    /// 此时安装会**退化为不校验**并如实告知用户，绝不假装校验过。
    #[serde(default)]
    pub content_hash: String,
    /// 审核该版本的模型名（如 `gpt-5.5`）。空串 = 没有自动审核记录。
    /// UI 必须据此如实措辞：「由 X 自动审核」≠「人工审计过」。
    #[serde(default)]
    pub reviewed_by: String,
    /// 上线时间与最近更新时间（Unix 毫秒；0 = 索引未提供）。
    #[serde(default)]
    pub published_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub readme: String,
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
    /// 本条目在安装记录里的来源标识（`itools-market://<name>`）。
    ///
    /// 它不是可访问的 URL，只是一个**稳定的来源身份**：锁文件按它判「同源覆盖」，
    /// 更新检查按它路由到市场分支而不是去请求 Git 托管站。
    pub fn source_url(&self) -> String {
        format!("{}{}", super::install::MARKET_SCHEME, self.name)
    }

    /// 插件包的完整下载地址（拼上当前配置的服务器地址）。
    ///
    /// 索引里给的是相对路径；服务端没给时按约定路径兜底，免得整个条目因为一个字段缺失而装不了。
    pub fn package_url(&self, endpoint: &str) -> String {
        let base = endpoint.trim_end_matches('/');
        let rel = if self.package.is_empty() {
            format!("/api/market/package/{}", self.name)
        } else {
            self.package.clone()
        };
        format!("{base}{}", if rel.starts_with('/') { rel } else { format!("/{rel}") })
    }
}

/// 索引文件的顶层结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketIndex {
    #[serde(default)]
    pub version: u32,
    /// `llm` = 服务端启用了大模型自动审核；`manual` = 审核模型未接入，条目由维护者人工放行。
    /// 客户端市场页据此如实措辞——「在市场里」到底意味着什么，用户有权知道准确答案。
    #[serde(default)]
    pub review_mode: String,
    #[serde(default)]
    pub review_model: String,
    #[serde(default)]
    pub plugins: Vec<MarketEntry>,
}

/// 当前市场服务器地址（与云同步共用「设置 → 网络」里那一个）。
///
/// 没配置时返回 `Err` 并给出可照做的指引——**不猜、不回落到某个写死的地址**。
fn endpoint() -> Result<String, String> {
    crate::account::cloud_endpoint().ok_or_else(|| {
        "还没有配置服务器地址，插件市场不可用。请到「设置 → 网络 → 服务器地址」填写后重试。"
            .to_string()
    })
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
    let ep = endpoint()?;
    let url = format!("{}/api/market/index", ep.trim_end_matches('/'));
    let bytes = fetch_capped(&url, FETCH_TIMEOUT_SECS, MAX_INDEX_BYTES, "拉取插件市场索引")?;
    let index: MarketIndex = serde_json::from_slice(&bytes)
        .map_err(|e| format!("插件市场索引解析失败: {e}（服务端返回的可能不是索引，请核对服务器地址）"))?;
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // 缓存写失败不影响本次结果：内存里已经有索引了，下次重新拉一遍即可。
        if let Err(e) = std::fs::write(&p, &bytes) {
            ilog!("[iTools] 市场索引缓存写入失败（不影响本次使用）: {e}");
        }
    }
    Ok(index)
}

/// 下载已上线的插件包（zip 字节）。
pub fn fetch_package(entry: &MarketEntry) -> Result<Vec<u8>, String> {
    let ep = endpoint()?;
    let url = entry.package_url(&ep);
    fetch_capped(
        &url,
        PACKAGE_TIMEOUT_SECS,
        MAX_PACKAGE_BYTES,
        &format!("下载插件包 {}", entry.name),
    )
}

/// 带超时与体积上限的 GET。
///
/// 走 [`crate::http`] 这个**唯一出站出口**，因此「设置 → 网络」里的代理配置对市场同样生效
/// （直接 `ureq::get` 会绕过代理，那就又成了一个「开着却不生效」的开关）。
fn fetch_capped(url: &str, timeout_secs: u64, max: u64, what: &str) -> Result<Vec<u8>, String> {
    let resp = crate::http::get(url)
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .call()
        .map_err(|e| format!("{what}失败: {}", describe(e)))?;

    // 先看声明长度：能提前拒掉超大响应，省得读完才发现
    if let Some(len) = resp.header("Content-Length").and_then(|v| v.parse::<u64>().ok()) {
        if len > max {
            return Err(format!("{what}失败：响应体 {len} 字节超过上限 {max}"));
        }
    }
    let mut buf = Vec::new();
    resp.into_reader()
        .take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("{what}失败：读取响应出错: {e}"))?;
    if buf.len() as u64 > max {
        return Err(format!("{what}失败：响应体超过上限 {max} 字节"));
    }
    Ok(buf)
}

/// 把 ureq 错误翻成能照做的中文。
///
/// 直接把 `e.to_string()` 甩给用户，多数情况是一句 `https://…: connection refused`——
/// 用户既不知道那是市场服务器还是别的什么，也不知道下一步该干嘛。
fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body: String = resp
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            match code {
                404 => "HTTP 404：服务器上没有这个端点或插件（请确认服务器地址与版本）".to_string(),
                429 => "HTTP 429：请求过于频繁，已被服务端限流，请稍后再试".to_string(),
                500..=599 => format!("HTTP {code}：服务端出错。{body}"),
                _ => format!("HTTP {code}。{body}"),
            }
        }
        ureq::Error::Transport(t) => {
            format!("连接失败：{t}（服务器地址是否正确？服务端是否在运行？）")
        }
    }
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
    /// 索引来源的可读描述（当前服务器地址），便于用户确认自己连的是哪个市场。
    pub source: String,
    /// 服务端的审核方式：`llm` = 大模型自动审核；`manual` = 未接入自动审核，人工放行。
    /// 空串 = 索引里没说（老服务端）。UI 必须据此如实措辞。
    pub review_mode: String,
    /// 审核模型名（`review_mode = "llm"` 时有值）。
    pub review_model: String,
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
    let source = crate::account::cloud_endpoint().unwrap_or_else(|| "（未配置服务器地址）".into());

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
            review_mode: index.review_mode,
            review_model: index.review_model,
        },
        Err(e) => match cached_index() {
            Some(index) => MarketView {
                plugins: index.plugins,
                origin: "cache".into(),
                error: Some(e),
                installed,
                source,
                review_mode: index.review_mode,
                review_model: index.review_model,
            },
            None => MarketView {
                plugins: Vec::new(),
                origin: "none".into(),
                error: Some(e),
                installed,
                source,
                review_mode: String::new(),
                review_model: String::new(),
            },
        },
    })
}

/// 命令：从市场安装某个插件（预览阶段）。
///
/// 与手动粘 URL 安装共用**同一套**解压、路径、清单、可执行文件校验（`install.rs` 里那一份），
/// 差别只在包从哪来、以及带不带可信哈希：市场条目带着审核时算出的内容哈希，
/// 装到的必须逐字节等于审核过的那份，否则拒绝。
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
    // 正常情况下服务端发布时一定会写入 contentHash，走到这里说明索引是手工改过或版本过旧。
    let expect = (!entry.content_hash.is_empty()).then(|| entry.content_hash.clone());
    if expect.is_none() {
        ilog!("[iTools] 市场条目 {name} 没有 contentHash，本次安装将不做内容校验");
    }

    let source_label = crate::account::cloud_endpoint().unwrap_or_default();
    let entry_for_dl = entry.clone();
    let bytes = tauri::async_runtime::spawn_blocking(move || fetch_package(&entry_for_dl))
        .await
        .map_err(|e| format!("插件包下载任务异常终止: {e}"))?
        .inspect_err(|e| ilog!("[iTools] 市场插件包下载失败 {name}：{e}"))?;

    // 失败原因落日志：安装是「网络 + 解压 + 落盘」的长链路，UI 上那行红字一闪而过、
    // 用户复述常常缺关键信息，没有日志就只能靠猜。
    let preview = super::install::preview_from_market_package(
        bytes,
        &entry.source_url(),
        &entry.version,
        &source_label,
        expect,
        &registry,
        &staging,
    )
    .await
    .inspect_err(|e| ilog!("[iTools] 市场安装预览失败 {name}：{e}"))?;

    // 纵深防御：索引条目声称的 name 必须与包里 plugin.json 的 name 一致。
    // 有 contentHash 时这条其实是冗余的（哈希对上就说明整包内容一致），
    // 但没有哈希时它是唯一能挡住「索引条目指向了别的插件」的检查。
    ilog!(
        "[iTools] 市场安装预览完成 {name}: {} 个文件，哈希校验={}，token={}",
        preview.file_count, preview.hash_verified, &preview.token[..8.min(preview.token.len())]
    );
    if preview.name != entry.name {
        // 暂存目录留给 sweep 清理；这里只拒绝，不动用户已装的任何东西
        return Err(format!(
            "市场条目与实际内容不符：条目名「{}」，包里的 plugin.json 写的是「{}」。已拒绝安装。",
            entry.name, preview.name
        ));
    }
    Ok(preview)
}

/// 查一个插件在市场里的当前版本（供更新检查）。
///
/// 直接用索引，不额外发请求——索引本来就带着每个插件的 `version`。
pub fn latest_version(name: &str) -> Result<Option<String>, String> {
    let index = fetch_index()?;
    Ok(index
        .plugins
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.version))
}

/// 查一个插件在市场里的完整条目（供更新时下载新包）。
pub fn find_entry(name: &str) -> Result<MarketEntry, String> {
    fetch_index()?
        .plugins
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("市场里已经没有名为「{name}」的插件了"))
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
