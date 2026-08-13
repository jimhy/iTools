//! GitHub 镜像源配置 + 定时健康探测。
//!
//! 背景：插件生态只用 GitHub，而 GitHub 在国内可达性差。服务端维护一份**可编辑**的镜像源列表
//! （`config/mirrors.json`），定时**经每个镜像**去拉一个已知文件、比对 SHA-256，把探测结果
//! （healthy / latencyMs / lastOkAt / 失败原因）写回内存；客户端定期 `GET /api/mirrors`
//! 取最新配置来挑下载源。
//!
//! 关键事实（运维必读，README 同步）：
//! - 探测**不需要服务器能直连 GitHub**——经镜像访问本身就是探测内容；期望哈希由维护者在
//!   能访问 GitHub 的机器上算好后预置在配置文件里，服务端不去官方源核对。
//! - 只看 HTTP 200 不够：镜像站挂掉时常返回 200 + 错误页/验证页。必须比对内容哈希，
//!   这样能同时检出「失效」与「被劫持」。
//! - 官方源（raw.githubusercontent.com / github.com）**不在**本列表里：它是客户端内置的固定
//!   候选，始终参与竞速。本文件只描述「额外的镜像候选」。
//!
//! ## ETag 稳定性（为什么响应体里没有「本轮探测时刻」）
//!
//! ETag 是**按响应体内容算的强 ETag**：内容变一个字节 ETag 就变，这是它唯一诚实的语义。
//! 所以「304 能不能命中」完全取决于**响应体本身是否稳定**——不能靠「把某些字段排除在 ETag
//! 计算之外」来伪造稳定（那会让两份不同的响应体共用一个 ETag，客户端拿到 304 后手里的数据
//! 与服务端不一致却毫不知情）。
//!
//! 因此规则是：**每轮必变、且客户端不消费的诊断数据一律不进响应体**。
//! - `lastCheckedAt`（本轮探测时刻）→ 移到响应头 `X-Mirror-Probe-At`（200/304 都带）。
//! - `consecutiveFailures`（连续失败次数）→ 只留在内存与日志里。
//! - `latencyMs` / `lastOkAt` → 量化 + 滞回后再发布（见 [`stable_latency`] / [`floor_instant`]）。
//! - `updatedAt` 与 ETag 只在**实质变化**（健康翻转 / 延迟跨阈值 / 失败原因变化 / 配置重载）时才刷新。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::task::{JoinHandle, JoinSet};

use crate::{iso_millis, Clock};

/// 探测样本：经镜像拉取该文件并比对 sha256。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorProbeSpec {
    pub owner: String,
    pub repo: String,
    /// 字面量 `HEAD` 表示默认分支（官方与镜像都支持）。
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub path: String,
    /// 期望内容哈希，小写 hex。
    pub sha256: String,
}

/// 一个镜像源。
///
/// `raw` / `archive` 各自声明 URL 模板——**不能假设各家路径形态统一**
/// （实测：ghfast.top 不支持 codeload.github.com，返回 403；github.com/archive 形态则各家都可用）。
/// 占位符只有四个：`{owner}` `{repo}` `{ref}` `{path}`（archive 模板不含 `{path}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorEntry {
    pub id: String,
    pub label: String,
    pub raw: String,
    pub archive: String,
    pub healthy: bool,
    /// 最近一次成功探测的耗时，**量化后的近似值**（见 [`stable_latency`]）：
    /// 按 50ms 取整，且只在与已发布值相差「≥100ms 或 ≥25%」时才更新。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<i64>,
    /// 最近一次探测成功的时间，**低精度**：按 `ok_at_granularity_ms`（默认 1 小时）向下取整。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<String>,
    /// 最近一次失败原因（DNS / 超时 / 状态码 / 哈希不匹配）。成功时不带此字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// `GET /api/mirrors` 的响应体 = 配置文件格式（客户端内置默认 / 本地缓存 / 服务端返回三处同一格式）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MirrorConfig {
    pub version: i64,
    pub updated_at: String,
    pub probe: MirrorProbeSpec,
    pub mirrors: Vec<MirrorEntry>,
}

/// 代码里的内置兜底（配置文件缺失 / 不合法时使用）。
///
/// 镜像站寿命很短，**正常运维应改 `config/mirrors.json`，而不是改这里**——这份只保证
/// 「配置文件没准备好时端点仍返回一份可用配置」，不作为长期维护入口。
/// 探测样本 octocat/Hello-World 的 README 内容为 `"Hello World!\n"`（13 字节），哈希已实测核对。
pub fn builtin_mirror_config() -> MirrorConfig {
    let mirror = |id: &str, label: &str, host: &str| MirrorEntry {
        id: id.into(),
        label: label.into(),
        raw: format!("https://{host}/https://raw.githubusercontent.com/{{owner}}/{{repo}}/{{ref}}/{{path}}"),
        archive: format!("https://{host}/https://github.com/{{owner}}/{{repo}}/archive/{{ref}}.zip"),
        healthy: true,
        latency_ms: None,
        last_ok_at: None,
        last_error: None,
    };
    MirrorConfig {
        version: 1,
        updated_at: "1970-01-01T00:00:00.000Z".into(),
        probe: MirrorProbeSpec {
            owner: "octocat".into(),
            repo: "Hello-World".into(),
            // ⚠ 必须钉死 commit sha，不能写 HEAD 或分支名：下面的 sha256 是**这个 commit 下**的内容哈希。
            //   写 HEAD 的话，上游哪天改一下这个文件，所有镜像（连同 GitHub 官方直连）会在同一时刻
            //   被判「哈希不匹配 → 不健康」——那是一次全网误杀，而且原因极难排查。
            //   换探测文件时，必须同时更新 ref 与 sha256，并实测核对。
            git_ref: "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d".into(),
            path: "README".into(),
            sha256: "03ba204e50d126e4674c005e04d82e84c21366780af1f43bd54a37816b6ab340".into(),
        },
        mirrors: vec![
            mirror("gh-proxy", "gh-proxy.com", "gh-proxy.com"),
            mirror("ghproxy-net", "ghproxy.net", "ghproxy.net"),
            mirror("ghfast", "ghfast.top", "ghfast.top"),
        ],
    }
}

// ---------------------------------------------------------------- 模板展开

/// `encodeURIComponent` 的不转义集合：`A-Za-z0-9` 与 `-_.!~*'()`。
/// 与 JS 逐字符对齐——旧版测试断言的就是 JS 的编码结果。
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

fn encode_component(v: &str) -> String {
    utf8_percent_encode(v, COMPONENT).to_string()
}

/// 按段编码：`/` 保留为路径分隔符，每段各自 `encodeURIComponent`。
/// 用于 `{ref}`（`refs/heads/main` 这种形态要保留斜杠）与 `{path}`（仓库内路径本就含斜杠）。
fn encode_segments(v: &str) -> String {
    v.split('/').map(encode_component).collect::<Vec<_>>().join("/")
}

/// 展开 URL 模板；owner/repo 整体编码，ref/path 按段编码（斜杠保留）。
pub fn expand_template(template: &str, owner: &str, repo: &str, git_ref: &str, path: &str) -> String {
    template
        .replace("{owner}", &encode_component(owner))
        .replace("{repo}", &encode_component(repo))
        .replace("{ref}", &encode_segments(git_ref))
        .replace("{path}", &encode_segments(path))
}

/// 用探测样本展开某个模板（`archive` 模板不含 `{path}`，多传无害）。
pub fn expand_with_probe(template: &str, probe: &MirrorProbeSpec) -> String {
    expand_template(template, &probe.owner, &probe.repo, &probe.git_ref, &probe.path)
}

// ---------------------------------------------------------------- 配置解析

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

fn is_http_url(v: &str) -> bool {
    let probe = v
        .replace("{owner}", "x")
        .replace("{repo}", "x")
        .replace("{ref}", "x")
        .replace("{path}", "x");
    match url::Url::parse(&probe) {
        Ok(u) => matches!(u.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

fn is_hex64(v: &str) -> bool {
    v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit())
}

/// 解析并校验一份镜像配置。**不合法就返回 Err**（调用方回退到内置兜底），
/// 不做「静默半吞半吐」——配置写错必须有明确日志。
pub fn parse_mirror_config(raw: &Value, now_ms: i64) -> Result<MirrorConfig, String> {
    let o = raw.as_object().ok_or("配置根节点不是对象")?;

    let version = o.get("version").and_then(Value::as_i64).unwrap_or(0);
    if version < 1 {
        return Err("version 必须是 >=1 的数字".into());
    }

    let empty = Value::Object(Default::default());
    let p = o.get("probe").unwrap_or(&empty);
    let owner = non_empty_str(p.get("owner"));
    let repo = non_empty_str(p.get("repo"));
    let (owner, repo) = match (owner, repo) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err("probe.owner / probe.repo 必填".into()),
    };
    let git_ref = non_empty_str(p.get("ref"));
    let path = non_empty_str(p.get("path"));
    let (git_ref, path) = match (git_ref, path) {
        (Some(a), Some(b)) => (a, b),
        _ => return Err("probe.ref / probe.path 必填".into()),
    };
    let sha = p.get("sha256").and_then(Value::as_str).unwrap_or("");
    if !is_hex64(sha) {
        return Err("probe.sha256 必须是 64 位 hex".into());
    }
    let probe = MirrorProbeSpec {
        owner: owner.to_string(),
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        path: path.to_string(),
        sha256: sha.to_ascii_lowercase(),
    };

    let list = o.get("mirrors").and_then(Value::as_array).ok_or("mirrors 必须是数组")?;
    let mut mirrors: Vec<MirrorEntry> = Vec::with_capacity(list.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for item in list {
        let m = item.as_object().ok_or("mirrors 元素不是对象")?;
        let id = non_empty_str(m.get("id")).ok_or("镜像缺少 id")?;
        if !seen.insert(id) {
            return Err(format!("镜像 id 重复: {id}"));
        }
        let raw_tpl = non_empty_str(m.get("raw")).unwrap_or("");
        if !raw_tpl.contains("{owner}") || !raw_tpl.contains("{path}") {
            return Err(format!("镜像 {id} 的 raw 模板缺少 {{owner}}/{{path}} 占位符"));
        }
        let archive_tpl = non_empty_str(m.get("archive")).unwrap_or("");
        if !archive_tpl.contains("{owner}") || !archive_tpl.contains("{ref}") {
            return Err(format!("镜像 {id} 的 archive 模板缺少 {{owner}}/{{ref}} 占位符"));
        }
        if !is_http_url(raw_tpl) || !is_http_url(archive_tpl) {
            return Err(format!("镜像 {id} 的模板不是合法的 http(s) URL"));
        }
        mirrors.push(MirrorEntry {
            id: id.to_string(),
            label: non_empty_str(m.get("label")).unwrap_or(id).to_string(),
            raw: raw_tpl.to_string(),
            archive: archive_tpl.to_string(),
            // 探测跑起来之前先用文件里的初值（缺省视为可用，避免刚启动时返回空可用列表）
            healthy: m.get("healthy").and_then(Value::as_bool).unwrap_or(true),
            latency_ms: m.get("latencyMs").and_then(Value::as_i64),
            last_ok_at: non_empty_str(m.get("lastOkAt")).map(str::to_string),
            last_error: None,
        });
    }
    if mirrors.is_empty() {
        return Err("mirrors 为空".into());
    }

    Ok(MirrorConfig {
        version,
        updated_at: non_empty_str(o.get("updatedAt"))
            .map(str::to_string)
            .unwrap_or_else(|| iso_millis(now_ms)),
        probe,
        mirrors,
    })
}

// ---------------------------------------------------------------- 探测

/// 一次 HTTP 取回的结果（只关心状态码与响应体）。
#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// 取回失败的分类——文案直接进 `lastError` 给运维看，所以分类要有信息量。
#[derive(Debug, Clone)]
pub enum FetchError {
    Timeout,
    Dns(String),
    Connect(String),
    Other(String),
}

impl FetchError {
    /// 翻译成人能看懂的原因。
    pub fn describe(&self) -> String {
        match self {
            FetchError::Timeout => "超时".into(),
            FetchError::Dns(code) => format!("DNS 解析失败 ({code})"),
            FetchError::Connect(code) => format!("连接失败 ({code})"),
            FetchError::Other(msg) => format!("请求失败: {msg}"),
        }
    }
}

/// 可注入的取回器（测试用 mock 替换，测试不打外网）。
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<FetchResponse, FetchError>;
}

/// 真实实现：reqwest（rustls + 内置根证书）。
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("itools-sync/mirror-probe")
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchResponse, FetchError> {
        let res = self.client.get(url).send().await.map_err(classify_reqwest)?;
        let status = res.status().as_u16();
        let body = res
            .bytes()
            .await
            .map_err(classify_reqwest)?
            .to_vec();
        Ok(FetchResponse { status, body })
    }
}

fn classify_reqwest(err: reqwest::Error) -> FetchError {
    if err.is_timeout() {
        return FetchError::Timeout;
    }
    // reqwest 不透出 errno，只能看错误链的文本；DNS 与其它连接失败对运维意义不同，值得分开。
    let mut detail = String::new();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&err);
    while let Some(e) = src {
        detail = e.to_string();
        src = e.source();
    }
    if err.is_connect() {
        let lower = detail.to_ascii_lowercase();
        if lower.contains("dns") || lower.contains("name or service") || lower.contains("resolve") {
            return FetchError::Dns(if detail.is_empty() { "DNS".into() } else { detail });
        }
        return FetchError::Connect(if detail.is_empty() { err.to_string() } else { detail });
    }
    FetchError::Other(if detail.is_empty() { err.to_string() } else { detail })
}

/// 一次探测的结果。
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub ok: bool,
    pub latency_ms: i64,
    /// 失败原因（DNS / 超时 / 状态码 / 哈希不匹配）；成功时为 None。
    pub error: Option<String>,
}

/// 探测单个镜像：经该镜像的 `raw` 模板拉取探测样本，比对 SHA-256。
///
/// 只看状态码是不够的（挂掉的镜像常返回 200 + 错误页），必须比对内容哈希。
pub async fn probe_mirror(
    raw_template: &str,
    probe: &MirrorProbeSpec,
    fetcher: &dyn Fetcher,
    timeout: Duration,
) -> ProbeOutcome {
    let url = expand_with_probe(raw_template, probe);
    let started = Instant::now();
    let elapsed = |started: Instant| started.elapsed().as_millis() as i64;

    let res = match tokio::time::timeout(timeout, fetcher.fetch(&url)).await {
        Err(_) => {
            return ProbeOutcome {
                ok: false,
                latency_ms: elapsed(started),
                error: Some(FetchError::Timeout.describe()),
            }
        }
        Ok(Err(err)) => {
            return ProbeOutcome {
                ok: false,
                latency_ms: elapsed(started),
                error: Some(err.describe()),
            }
        }
        Ok(Ok(res)) => res,
    };

    if !(200..300).contains(&res.status) {
        return ProbeOutcome {
            ok: false,
            latency_ms: elapsed(started),
            error: Some(format!("HTTP {}", res.status)),
        };
    }
    let got = hex::encode(Sha256::digest(&res.body));
    let latency_ms = elapsed(started);
    if got != probe.sha256 {
        // 内容对不上 = 失效页 / 验证页 / 被劫持，一律判失败。
        return ProbeOutcome {
            ok: false,
            latency_ms,
            error: Some(format!(
                "哈希不匹配 (期望 {}… 实得 {}…, {} 字节)",
                &probe.sha256[..12.min(probe.sha256.len())],
                &got[..12],
                res.body.len()
            )),
        };
    }
    ProbeOutcome { ok: true, latency_ms, error: None }
}

// ---------------------------------------------------------------- 发布值量化

/// 延迟量化粒度（毫秒）：发布值一律取整到它的倍数。
const LATENCY_BUCKET_MS: f64 = 50.0;
/// 延迟被认为「实质变化」的绝对下限（毫秒）。
const LATENCY_MIN_DELTA_MS: f64 = 100.0;
/// 延迟被认为「实质变化」的相对下限（相对已发布值）。
const LATENCY_REL_DELTA: f64 = 0.25;
/// `lastOkAt` 默认量化粒度：1 小时。
pub const DEFAULT_OK_AT_GRANULARITY_MS: i64 = 3_600_000;

/// 把本轮实测延迟折算成**可发布**的延迟：
/// - 没有历史值 → 取整到 [`LATENCY_BUCKET_MS`] 的倍数；
/// - 有历史值且变化不实质（同时小于 100ms 与 25%）→ **沿用旧值**（滞回，避免在桶边界来回跳）；
/// - 变化实质 → 取整后发布。
///
/// 用途只是给候选源排序，这点精度损失换来的是「稳定期响应体逐字节不变 → 304 真能命中」。
pub fn stable_latency(raw_ms: i64, published: Option<i64>) -> i64 {
    let bucketed = (raw_ms as f64 / LATENCY_BUCKET_MS).round() * LATENCY_BUCKET_MS;
    let Some(prev) = published else {
        return bucketed as i64;
    };
    let threshold = LATENCY_MIN_DELTA_MS.max((prev as f64).abs() * LATENCY_REL_DELTA);
    if ((raw_ms - prev) as f64).abs() < threshold {
        prev
    } else {
        bucketed as i64
    }
}

/// 时间戳向下取整到 `granularity_ms`（<=0 表示不量化，按精确值输出）。
pub fn floor_instant(ms: i64, granularity_ms: i64) -> String {
    if granularity_ms <= 0 {
        return iso_millis(ms);
    }
    iso_millis(ms.div_euclid(granularity_ms) * granularity_ms)
}

/// 解析 `If-None-Match` 头：匹配到当前 ETag（或 `*`）则返回 true。
pub fn matches_etag(header: Option<&str>, etag: &str) -> bool {
    let Some(header) = header else { return false };
    let normalize = |v: &str| v.trim().trim_start_matches("W/").to_string();
    let want = normalize(etag);
    header.split(',').any(|v| {
        let n = normalize(v);
        n == "*" || n == want
    })
}

// ---------------------------------------------------------------- 注册表

/// 单个镜像的运行期探测状态（内存态，不落库，理由见 README）。
///
/// `healthy` / `latency_ms` / `last_ok_at` / `last_error` 存的就是**已发布值**（写入时已量化），
/// 快照直接取用；`consecutive_failures` 只用于日志与阈值判定，不进响应体。
#[derive(Debug, Clone, Default)]
struct MirrorState {
    healthy: bool,
    latency_ms: Option<i64>,
    last_ok_at: Option<String>,
    last_error: Option<String>,
    consecutive_failures: u32,
}

/// 日志出口（可注入，测试里静默）。
pub trait MirrorLogger: Send + Sync {
    fn info(&self, msg: &str);
    fn warn(&self, msg: &str);
    fn error(&self, msg: &str);
}

/// 走 tracing 的默认实现。
pub struct TracingLogger;

impl MirrorLogger for TracingLogger {
    fn info(&self, msg: &str) {
        tracing::info!("{msg}");
    }
    fn warn(&self, msg: &str) {
        tracing::warn!("{msg}");
    }
    fn error(&self, msg: &str) {
        tracing::error!("{msg}");
    }
}

/// 全静默实现（测试用）。
pub struct SilentLogger;

impl MirrorLogger for SilentLogger {
    fn info(&self, _msg: &str) {}
    fn warn(&self, _msg: &str) {}
    fn error(&self, _msg: &str) {}
}

pub struct MirrorRegistryOptions {
    /// 可编辑的配置文件路径（随部署提供）。找不到 / 不合法则用内置兜底。
    pub file: PathBuf,
    /// 探测周期。
    pub probe_interval: Duration,
    /// 单次探测超时。
    pub probe_timeout: Duration,
    /// 连续失败多少次才判 unhealthy（恢复则立即标回）。
    pub fail_threshold: u32,
    /// 探测并发上限。
    pub concurrency: usize,
    /// 配置文件 mtime 轮询的最小间隔（毫秒），避免每个请求都 stat。
    pub reload_check_ms: i64,
    /// `lastOkAt` 的量化粒度（毫秒，默认 1 小时；0 表示不量化）。
    pub ok_at_granularity_ms: i64,
    pub fetcher: Arc<dyn Fetcher>,
    pub logger: Arc<dyn MirrorLogger>,
    pub now: Clock,
}

impl MirrorRegistryOptions {
    /// 按运行期配置构造（真实 HTTP 取回 + tracing 日志）。
    pub fn from_config(cfg: &crate::config::MirrorsConfig, now: Clock) -> Self {
        Self {
            file: cfg.file.clone(),
            probe_interval: cfg.probe_interval,
            probe_timeout: cfg.probe_timeout,
            fail_threshold: cfg.fail_threshold,
            concurrency: 4,
            reload_check_ms: 5_000,
            ok_at_granularity_ms: cfg.ok_at_granularity_ms,
            fetcher: Arc::new(HttpFetcher::new(cfg.probe_timeout)),
            logger: Arc::new(TracingLogger),
            now,
        }
    }
}

/// 序列化后的响应体 + 强 ETag。
#[derive(Debug, Clone)]
pub struct MirrorBody {
    pub json: String,
    pub etag: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Builtin,
    File,
}

struct Inner {
    config: MirrorConfig,
    source: ConfigSource,
    states: HashMap<String, MirrorState>,
    /// 文件指纹（mtime:size），用于判断是否需要重载。
    stamp: String,
    last_check_at: i64,
    loaded: bool,
    missing_warned: bool,
    /// 最近一次**实质**内容变更时间（配置重载 / 探测结果实质变化），作为响应的 updatedAt。
    mutated_at: String,
    /// 内容修订号：每次实质变更 +1，用作 body 缓存键。
    rev: u64,
    /// 最近一轮探测结束的时刻（每轮都变，只走响应头/日志，不进响应体）。
    probe_at: Option<String>,
    cache: Option<(u64, MirrorBody)>,
}

/// 镜像配置注册表：装载配置文件（支持 mtime 热更新）+ 保存探测状态 + 产出对外快照与 ETag。
///
/// 热更新方式选择：**文件 mtime 轮询**（而非「手动触发端点」）。理由：`/api/mirrors` 是公开端点，
/// 再加一个改状态的端点就得单独做鉴权与防滥用；轮询 stat 成本极低（还带最小间隔节流），
/// 维护者改完 `mirrors.json` 存盘即生效，无需重启、无需额外凭据。
pub struct MirrorRegistry {
    inner: Mutex<Inner>,
    opts: MirrorRegistryOptions,
    probing: AtomicBool,
    timer: Mutex<Option<JoinHandle<()>>>,
}

impl MirrorRegistry {
    pub fn new(opts: MirrorRegistryOptions) -> Arc<Self> {
        let mutated_at = iso_millis((opts.now)());
        Arc::new(Self {
            inner: Mutex::new(Inner {
                config: builtin_mirror_config(),
                source: ConfigSource::Builtin,
                states: HashMap::new(),
                stamp: String::new(),
                last_check_at: 0,
                loaded: false,
                missing_warned: false,
                mutated_at,
                rev: 0,
                probe_at: None,
                cache: None,
            }),
            opts,
            probing: AtomicBool::new(false),
            timer: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("镜像注册表锁中毒")
    }

    /// 当前使用的配置来源（诊断用）。
    pub fn config_source(&self) -> ConfigSource {
        self.lock().source
    }

    /// 最近一轮探测结束时刻（ISO；从未探测过则 None）。
    ///
    /// 走响应头 `X-Mirror-Probe-At`——它每轮都变，进响应体就会把 ETag 打废。
    pub fn last_probe_at(&self) -> Option<String> {
        self.lock().probe_at.clone()
    }

    /// 按需装载 / 重载配置文件：首次调用必读；之后按 `reload_check_ms` 节流，只在 mtime+size 变化时重载。
    /// 任何失败都只记日志并沿用当前配置——**拉配置失败不能阻塞客户端安装插件**。
    pub async fn refresh_if_needed(self: &Arc<Self>) {
        let now = (self.opts.now)();
        {
            let inner = self.lock();
            if inner.loaded && now - inner.last_check_at < self.opts.reload_check_ms {
                return;
            }
        }
        self.lock().last_check_at = now;

        let stamp = match tokio::fs::metadata(&self.opts.file).await {
            Ok(meta) => {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                format!("{mtime}:{}", meta.len())
            }
            Err(err) => {
                // 只在「状态由可读变为不可读」时喊一次，避免刷屏；配置沿用当前这份。
                let mut inner = self.lock();
                if !inner.missing_warned {
                    inner.missing_warned = true;
                    let fallback = if inner.source == ConfigSource::File {
                        "上一份配置"
                    } else {
                        "内置兜底列表"
                    };
                    self.opts.logger.warn(&format!(
                        "[mirrors] 配置文件不可读 ({}: {err})，沿用{fallback}",
                        self.opts.file.display()
                    ));
                }
                inner.loaded = true;
                return;
            }
        };

        {
            let mut inner = self.lock();
            inner.missing_warned = false;
            if inner.loaded && stamp == inner.stamp {
                return;
            }
        }

        let text = tokio::fs::read_to_string(&self.opts.file).await;
        let parsed = text
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
            .and_then(|v| parse_mirror_config(&v, (self.opts.now)()));

        let changed = match parsed {
            Ok(cfg) => {
                let count = cfg.mirrors.len();
                let changed = {
                    let mut inner = self.lock();
                    let changed = Self::apply_config(&mut inner, cfg, &self.opts);
                    inner.stamp = stamp;
                    inner.source = ConfigSource::File;
                    inner.loaded = true;
                    changed
                };
                self.opts
                    .logger
                    .info(&format!("[mirrors] 已装载 {}（{count} 个镜像）", self.opts.file.display()));
                changed
            }
            Err(msg) => {
                // 配置写坏了不能把服务带下水：沿用上一份可用配置（或内置兜底），并把原因喊出来。
                let mut inner = self.lock();
                inner.stamp = stamp;
                inner.loaded = true;
                let fallback = if inner.source == ConfigSource::File {
                    "上一份配置"
                } else {
                    "内置兜底列表"
                };
                self.opts.logger.error(&format!(
                    "[mirrors] 配置文件不合法 ({}): {msg}；沿用{fallback}",
                    self.opts.file.display()
                ));
                false
            }
        };

        // 配置刚变且定时探测在跑，立刻补一次探测（不阻塞调用方）。
        if changed && self.timer.lock().expect("定时器锁中毒").is_some() {
            let me = self.clone();
            tokio::spawn(async move { me.run_probe().await });
        }
    }

    /// 应用新配置：模板未变的镜像保留既有探测状态，模板变了的重新探测。
    ///
    /// 返回「响应体是否真的变了」——改文件里的注释/缩进导致 mtime 变化但内容等价时，
    /// 客户端不该被迫重下一份一模一样的配置。
    fn apply_config(inner: &mut Inner, next: MirrorConfig, opts: &MirrorRegistryOptions) -> bool {
        let before = Self::snapshot_of(inner);
        let prev_raw: HashMap<&str, &str> =
            inner.config.mirrors.iter().map(|m| (m.id.as_str(), m.raw.as_str())).collect();
        let stale: Vec<String> = inner
            .states
            .keys()
            .filter(|id| {
                match next.mirrors.iter().find(|m| &&m.id == id) {
                    None => true,
                    Some(m) => prev_raw.get(id.as_str()) != Some(&m.raw.as_str()),
                }
            })
            .cloned()
            .collect();
        for id in stale {
            inner.states.remove(&id);
        }
        inner.config = next;
        if Self::snapshot_of(inner) != before {
            Self::touch(inner, opts);
            true
        } else {
            false
        }
    }

    /// 标记「内容发生了实质变化」：刷新 updatedAt 与修订号，作废 body 缓存。
    fn touch(inner: &mut Inner, opts: &MirrorRegistryOptions) {
        inner.rev += 1;
        inner.mutated_at = iso_millis((opts.now)());
        inner.cache = None;
    }

    /// 对外快照：健康的在前，同为健康的按 latencyMs 升序（无延迟数据的排在有数据的之后）。
    pub fn snapshot(&self) -> MirrorConfig {
        Self::snapshot_of(&self.lock())
    }

    fn snapshot_of(inner: &Inner) -> MirrorConfig {
        let mut mirrors: Vec<MirrorEntry> = inner
            .config
            .mirrors
            .iter()
            .map(|m| match inner.states.get(&m.id) {
                // 还没探测过 → 用配置文件里的初值
                None => m.clone(),
                // 只发布「稳定字段」：lastCheckedAt / consecutiveFailures 每轮必变，走响应头与日志。
                Some(st) => MirrorEntry {
                    id: m.id.clone(),
                    label: m.label.clone(),
                    raw: m.raw.clone(),
                    archive: m.archive.clone(),
                    healthy: st.healthy,
                    latency_ms: st.latency_ms,
                    last_ok_at: st.last_ok_at.clone(),
                    last_error: st.last_error.clone(),
                },
            })
            .collect();
        mirrors.sort_by(|a, b| {
            b.healthy
                .cmp(&a.healthy)
                .then_with(|| {
                    a.latency_ms
                        .unwrap_or(i64::MAX)
                        .cmp(&b.latency_ms.unwrap_or(i64::MAX))
                })
                // 稳定输出，避免 ETag 无谓抖动
                .then_with(|| a.id.cmp(&b.id))
        });
        MirrorConfig {
            version: inner.config.version,
            updated_at: inner.mutated_at.clone(),
            probe: inner.config.probe.clone(),
            mirrors,
        }
    }

    /// 序列化后的响应体 + 强 ETag（**按响应体逐字节内容算**：内容不变则 ETag 不变、内容变则必变）。
    ///
    /// 不做「排除某些字段再算 ETag」这种事——那样两份不同的响应体会共用一个 ETag，
    /// 客户端拿到 304 会以为自己手里的数据仍然正确。
    pub fn body(&self) -> MirrorBody {
        let mut inner = self.lock();
        if let Some((rev, body)) = &inner.cache {
            if *rev == inner.rev {
                return body.clone();
            }
        }
        let json = serde_json::to_string(&Self::snapshot_of(&inner))
            .expect("镜像配置为纯数据结构，序列化不会失败");
        let etag = format!("\"{}\"", &hex::encode(Sha256::digest(json.as_bytes()))[..32]);
        let body = MirrorBody { json, etag };
        let rev = inner.rev;
        inner.cache = Some((rev, body.clone()));
        body
    }

    /// 跑一轮探测：并发上限 `concurrency`，逐个写回状态。
    ///
    /// **只有在发布值实质变化时才 touch**：否则连着两轮结果一样却刷新 updatedAt/ETag，
    /// 客户端每次带 If-None-Match 来都会拿到 200 全量，README 里承诺的 304 就成了空话。
    pub async fn run_probe(&self) {
        if self.probing.swap(true, Ordering::SeqCst) {
            return;
        }
        let (mirrors, probe) = {
            let inner = self.lock();
            (inner.config.mirrors.clone(), inner.config.probe.clone())
        };

        let mut set: JoinSet<(MirrorEntry, ProbeOutcome)> = JoinSet::new();
        let sem = Arc::new(tokio::sync::Semaphore::new(self.opts.concurrency.max(1)));
        for m in mirrors.iter().cloned() {
            let sem = sem.clone();
            let fetcher = self.opts.fetcher.clone();
            let probe = probe.clone();
            let timeout = self.opts.probe_timeout;
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let outcome = probe_mirror(&m.raw, &probe, fetcher.as_ref(), timeout).await;
                (m, outcome)
            });
        }

        let mut flipped = false;
        let mut changed = false;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((mirror, outcome)) => {
                    if let Some(err) = &outcome.error {
                        let failures = self
                            .lock()
                            .states
                            .get(&mirror.id)
                            .map(|s| s.consecutive_failures + 1)
                            .unwrap_or(1);
                        self.opts
                            .logger
                            .warn(&format!("[mirrors] {} 探测失败（第 {failures} 次）: {err}", mirror.id));
                    }
                    let res = self.apply_outcome(&mirror, &outcome);
                    flipped |= res.0;
                    changed |= res.1;
                }
                Err(err) => {
                    // 探测任务自身崩了（panic / 被取消）：记日志，下轮继续，绝不拖垮主服务。
                    self.opts
                        .logger
                        .error(&format!("[mirrors] 探测任务异常（已忽略，下轮继续）: {err}"));
                }
            }
        }

        {
            let mut inner = self.lock();
            // 本轮探测时刻：只走响应头 X-Mirror-Probe-At，不进响应体（否则 ETag 每轮必变）。
            if !inner.config.mirrors.is_empty() {
                inner.probe_at = Some(iso_millis((self.opts.now)()));
            }
            if changed {
                Self::touch(&mut inner, &self.opts);
            }
        }
        if flipped {
            let inner = self.lock();
            let bad: Vec<&str> = inner
                .config
                .mirrors
                .iter()
                .filter(|m| inner.states.get(&m.id).map(|s| !s.healthy).unwrap_or(false))
                .map(|m| m.id.as_str())
                .collect();
            let msg = if bad.is_empty() {
                "[mirrors] 健康状态变化，全部镜像可用".to_string()
            } else {
                format!("[mirrors] 健康状态变化，当前不可用: {}", bad.join(", "))
            };
            drop(inner);
            self.opts.logger.warn(&msg);
        }
        self.probing.store(false, Ordering::SeqCst);
    }

    /// 写回单次探测结果。
    ///
    /// 写回的就是**已发布值**（延迟量化+滞回、lastOkAt 按粒度取整），因此
    /// 「本轮写回后与写回前是否有差异」= 「响应体是否会变」，可直接拿来决定要不要 touch。
    ///
    /// 返回 `(flipped, changed)`：`flipped` = healthy 是否翻转（用于日志）；
    /// `changed` = 发布值是否有任何变化（用于 ETag）。
    fn apply_outcome(&self, mirror: &MirrorEntry, outcome: &ProbeOutcome) -> (bool, bool) {
        let now_ms = (self.opts.now)();
        let mut inner = self.lock();
        let prev = inner.states.get(&mirror.id).cloned();
        // 没探测过时，快照发布的是配置文件里的初值——比较基准也必须是它，否则首轮会漏 touch。
        let prev_healthy = prev.as_ref().map(|p| p.healthy).unwrap_or(mirror.healthy);
        let prev_latency = prev.as_ref().map_or(mirror.latency_ms, |p| p.latency_ms);
        let prev_ok_at = prev.as_ref().map_or(mirror.last_ok_at.clone(), |p| p.last_ok_at.clone());
        let prev_error = prev.as_ref().and_then(|p| p.last_error.clone());

        let next = if outcome.ok {
            // 恢复立即生效。
            MirrorState {
                healthy: true,
                latency_ms: Some(stable_latency(outcome.latency_ms, prev_latency)),
                last_ok_at: Some(floor_instant(now_ms, self.opts.ok_at_granularity_ms)),
                last_error: None,
                consecutive_failures: 0,
            }
        } else {
            let failures = prev.as_ref().map(|p| p.consecutive_failures).unwrap_or(0) + 1;
            // 连续失败达阈值才判 unhealthy，避免一次网络抖动误杀。
            MirrorState {
                healthy: if failures < self.opts.fail_threshold { prev_healthy } else { false },
                latency_ms: prev_latency,
                last_ok_at: prev_ok_at.clone(),
                last_error: outcome.error.clone(),
                consecutive_failures: failures,
            }
        };

        let flipped = next.healthy != prev_healthy;
        let changed = flipped
            || next.latency_ms != prev_latency
            || next.last_ok_at != prev_ok_at
            || next.last_error != prev_error;
        inner.states.insert(mirror.id.clone(), next);
        (flipped, changed)
    }

    /// 启动定时探测：先立即跑一次，之后按周期跑。
    pub fn start(self: &Arc<Self>) {
        let mut timer = self.timer.lock().expect("定时器锁中毒");
        if timer.is_some() {
            return;
        }
        let me = self.clone();
        let interval = self.opts.probe_interval;
        *timer = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // 漏掉的 tick 直接跳过（探测慢于周期时不该攒着补跑）
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await; // 第一次立即返回 → 启动即跑一轮
                me.run_probe().await;
            }
        }));
    }

    /// 停止定时探测（进程退出 / 测试收尾）。
    pub fn stop(&self) {
        if let Some(handle) = self.timer.lock().expect("定时器锁中毒").take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_template_encodes_like_js() {
        let tpl = "https://m.test/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}";
        assert_eq!(
            expand_template(tpl, "octo cat", "Hello-World", "refs/heads/main", "src/a b.json"),
            "https://m.test/https://raw.githubusercontent.com/octo%20cat/Hello-World/refs/heads/main/src/a%20b.json"
        );
        // archive 模板不含 {path}
        assert_eq!(
            expand_template(
                "https://m.test/https://github.com/{owner}/{repo}/archive/{ref}.zip",
                "o",
                "r",
                "HEAD",
                ""
            ),
            "https://m.test/https://github.com/o/r/archive/HEAD.zip"
        );
    }

    #[test]
    fn stable_latency_absorbs_jitter() {
        assert_eq!(stable_latency(63, None), 50, "首次发布：按 50ms 取整");
        assert_eq!(stable_latency(90, Some(50)), 50, "变化 40ms（<100ms 且 <25%）→ 沿用旧值");
        assert_eq!(stable_latency(149, Some(50)), 50, "变化 99ms → 仍算抖动");
        assert_eq!(stable_latency(160, Some(50)), 150, "变化 110ms → 实质变化");
        assert_eq!(stable_latency(1000, Some(900)), 900, "900ms 基准下变化 100ms 只占 11%");
        assert_eq!(stable_latency(1400, Some(900)), 1400, "变化 500ms（>25%）→ 实质变化");
    }

    #[test]
    fn floor_instant_quantizes() {
        let t = 1_786_617_000_000i64 + 47 * 60_000 + 31_123; // 任意带零头的时刻
        let hourly = floor_instant(t, 3_600_000);
        assert!(hourly.ends_with(":00:00.000Z"), "按小时取整应抹掉分秒，实得 {hourly}");
        assert_eq!(floor_instant(t, 0), iso_millis(t), "粒度 0 = 不量化");
    }

    #[test]
    fn matches_etag_handles_weak_and_lists() {
        assert!(matches_etag(Some("\"abc\""), "\"abc\""));
        assert!(matches_etag(Some("W/\"abc\""), "\"abc\""));
        assert!(matches_etag(Some("\"x\", \"abc\""), "\"abc\""));
        assert!(matches_etag(Some("*"), "\"abc\""));
        assert!(!matches_etag(Some("\"x\""), "\"abc\""));
        assert!(!matches_etag(None, "\"abc\""));
    }

    #[test]
    fn builtin_config_is_valid() {
        let b = builtin_mirror_config();
        assert!(!b.mirrors.is_empty());
        let json = serde_json::to_value(&b).unwrap();
        parse_mirror_config(&json, 0).expect("内置兜底列表自身必须合法");
    }
}
