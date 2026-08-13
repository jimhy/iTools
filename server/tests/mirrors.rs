//! 镜像源配置与健康探测测试：直接打 `/api/mirrors`，探测的网络部分全部注入 mock。
//!
//! 运行：`cargo test --test mirrors`。**不连数据库、不打外网。**
//! （存储层用懒连接的池：这些用例本就不该碰库，一旦误访问会立刻报连接错误而不是静默通过。）

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use itools_sync::config::Config;
use itools_sync::mirrors::{
    builtin_mirror_config, parse_mirror_config, probe_mirror, ConfigSource, FetchError, FetchResponse,
    Fetcher, MirrorConfig, MirrorRegistry, MirrorRegistryOptions, SilentLogger,
};
use itools_sync::ratelimit::{RateLimiterOptions, SlidingWindowLimiter};
use itools_sync::routes::{build_router, AppState};
use itools_sync::store::MariaDbStore;
use itools_sync::Clock;

const PROBE_BODY: &str = "Hello World!\n";

fn probe_sha() -> String {
    hex::encode(Sha256::digest(PROBE_BODY.as_bytes()))
}

// ---------------------------------------------------------------- 测试脚手架

/// 一个 mock 规则：按主机名给出「成功 / 状态码错 / 内容错 / 直接失败」，并可注入延迟。
#[derive(Clone, Default)]
struct Rule {
    status: u16,
    body: Option<String>,
    error: Option<&'static str>,
    delay_ms: u64,
}

impl Rule {
    fn ok() -> Self {
        Rule { status: 200, ..Default::default() }
    }
    fn with_status(status: u16) -> Self {
        Rule { status, ..Default::default() }
    }
    fn with_body(body: &str) -> Self {
        Rule { status: 200, body: Some(body.into()), ..Default::default() }
    }
    fn with_delay(ms: u64) -> Self {
        Rule { status: 200, delay_ms: ms, ..Default::default() }
    }
    fn failing(kind: &'static str) -> Self {
        Rule { error: Some(kind), ..Default::default() }
    }
}

/// 可在测试中途改写计划表的 mock 取回器（对应 Node 版的 mockFetch）。
struct MockFetcher {
    plan: Mutex<HashMap<String, Rule>>,
}

impl MockFetcher {
    fn new(plan: &[(&str, Rule)]) -> Arc<Self> {
        Arc::new(Self {
            plan: Mutex::new(plan.iter().map(|(h, r)| ((*h).to_string(), r.clone())).collect()),
        })
    }

    fn set_plan(&self, plan: &[(&str, Rule)]) {
        *self.plan.lock().unwrap() =
            plan.iter().map(|(h, r)| ((*h).to_string(), r.clone())).collect();
    }
}

#[async_trait]
impl Fetcher for MockFetcher {
    async fn fetch(&self, url: &str) -> Result<FetchResponse, FetchError> {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string));
        let rule = host.and_then(|h| self.plan.lock().unwrap().get(&h).cloned());
        // 计划表里没有的主机 = DNS 解析不到（与 Node mock 的默认分支一致）
        let Some(rule) = rule else {
            return Err(FetchError::Dns("ENOTFOUND".into()));
        };
        if rule.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(rule.delay_ms)).await;
        }
        if let Some(kind) = rule.error {
            return Err(match kind {
                "timeout" => FetchError::Timeout,
                other => FetchError::Other(other.into()),
            });
        }
        Ok(FetchResponse {
            status: rule.status,
            body: rule.body.unwrap_or_else(|| PROBE_BODY.to_string()).into_bytes(),
        })
    }
}

/// 永远 panic 的取回器：验证「探测任务内部异常不拖垮主服务」。
struct PanicFetcher;

#[async_trait]
impl Fetcher for PanicFetcher {
    async fn fetch(&self, _url: &str) -> Result<FetchResponse, FetchError> {
        panic!("mock 内部炸了");
    }
}

fn fixture_config() -> Value {
    let entry = |id: &str, host: &str| {
        json!({
            "id": id,
            "label": format!("{host}"),
            "raw": format!("https://{host}/https://raw.githubusercontent.com/{{owner}}/{{repo}}/{{ref}}/{{path}}"),
            "archive": format!("https://{host}/https://github.com/{{owner}}/{{repo}}/archive/{{ref}}.zip"),
            "healthy": true,
        })
    };
    json!({
        "version": 1,
        "updatedAt": "2026-08-12T10:00:00Z",
        "probe": {
            "owner": "octocat", "repo": "Hello-World", "ref": "HEAD",
            "path": "README", "sha256": probe_sha(),
        },
        "mirrors": [entry("alpha", "alpha.test"), entry("beta", "beta.test")],
    })
}

/// 可推进的假时钟。
fn fake_clock(start_ms: i64) -> (Arc<AtomicI64>, Clock) {
    let t = Arc::new(AtomicI64::new(start_ms));
    let handle = t.clone();
    (t, Arc::new(move || handle.load(Ordering::SeqCst)))
}

struct Fixture {
    registry: Arc<MirrorRegistry>,
    /// 临时目录（Drop 时自动清理）
    _dir: Option<tempfile::TempDir>,
    file: std::path::PathBuf,
}

struct RegistryOpts {
    fail_threshold: u32,
    probe_timeout: Duration,
    ok_at_granularity_ms: i64,
    clock: Clock,
}

impl Default for RegistryOpts {
    fn default() -> Self {
        Self {
            fail_threshold: 3,
            probe_timeout: Duration::from_secs(10),
            ok_at_granularity_ms: 3_600_000,
            clock: itools_sync::system_clock(),
        }
    }
}

/// 造一个注册表：`cfg` 为 None 表示「配置文件不存在」，用于验证内置兜底。
async fn new_registry(cfg: Option<Value>, fetcher: Arc<dyn Fetcher>, opts: RegistryOpts) -> Fixture {
    let (dir, file) = match &cfg {
        Some(v) => {
            let dir = tempfile::tempdir().expect("建临时目录");
            let file = dir.path().join("mirrors.json");
            std::fs::write(&file, serde_json::to_string(v).unwrap()).expect("写配置文件");
            (Some(dir), file)
        }
        None => (None, std::path::PathBuf::from("不存在的目录").join("mirrors.json")),
    };
    let registry = MirrorRegistry::new(MirrorRegistryOptions {
        file: file.clone(),
        probe_interval: Duration::from_secs(900),
        probe_timeout: opts.probe_timeout,
        fail_threshold: opts.fail_threshold,
        concurrency: 4,
        reload_check_ms: 0, // 测试里不节流，改文件立刻可见
        ok_at_granularity_ms: opts.ok_at_granularity_ms,
        fetcher,
        logger: Arc::new(SilentLogger),
        now: opts.clock,
    });
    registry.refresh_if_needed().await;
    Fixture { registry, _dir: dir, file }
}

/// 用给定注册表组装一个只跑 `/api/mirrors` 的服务（存储层懒连接，不碰库）。
fn serve(registry: Arc<MirrorRegistry>, env: &[(&str, &str)]) -> Router {
    let map: HashMap<String, String> =
        env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let config = Config::from_map(&map).expect("测试配置应合法");
    let clock = itools_sync::system_clock();
    let limiter = SlidingWindowLimiter::new(RateLimiterOptions::new(
        config.mirrors.rate_limit_max,
        config.mirrors.rate_limit_window_sec as i64 * 1000,
        clock.clone(),
    ));
    build_router(Arc::new(AppState {
        store: Arc::new(MariaDbStore::lazy(&config.db)),
        config: Arc::new(config),
        mirrors: registry,
        mirror_limiter: Arc::new(limiter),
        clock,
    }))
}

struct Res {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("响应体应为 JSON")
    }
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    }
}

/// 打一个请求（可选：`If-None-Match` 头与模拟的对端地址）。
async fn get(app: &Router, uri: &str, extra: &[(&str, &str)], remote: Option<&str>) -> Res {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    let mut req = builder.body(Body::empty()).expect("构造请求");
    if let Some(addr) = remote {
        let sock: SocketAddr = format!("{addr}:12345").parse().expect("合法地址");
        req.extensions_mut().insert(ConnectInfo(sock));
    }
    let res = app.clone().oneshot(req).await.expect("路由应有响应");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("读响应体");
    Res { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

fn parse_body(res: &Res) -> MirrorConfig {
    serde_json::from_value(res.json()).expect("响应体应能解析成镜像配置")
}

// ---------------------------------------------------------------- 配置解析

#[test]
fn parse_accepts_valid_and_rejects_broken_configs() {
    let ok = parse_mirror_config(&fixture_config(), 0).expect("合法配置应通过");
    assert_eq!(ok.mirrors.len(), 2);
    assert_eq!(ok.probe.sha256, probe_sha());

    let broken = |mutate: &dyn Fn(&mut Value)| {
        let mut c = fixture_config();
        mutate(&mut c);
        parse_mirror_config(&c, 0).expect_err("非法配置必须被整份拒绝")
    };
    assert!(broken(&|c| c["probe"]["sha256"] = json!("not-hex")).contains("sha256"));
    assert!(broken(&|c| c["mirrors"][1]["id"] = json!("alpha")).contains("重复"));
    assert!(broken(&|c| c["mirrors"][0]["raw"] = json!("https://a.test/no-placeholder")).contains("raw"));
    assert!(broken(&|c| c["mirrors"] = json!([])).contains("为空"));
    assert!(broken(&|c| c["version"] = json!(0)).contains("version"));
    assert!(broken(&|c| c["mirrors"][0]["raw"] = json!("ftp://a.test/{owner}/{path}")).contains("URL"));
}

#[tokio::test]
async fn shipped_config_file_is_valid_and_served_by_default_path() {
    // 不注入 registry 时走默认路径（工作目录下的 config/mirrors.json），且不起定时探测。
    let config = Config::from_map(&HashMap::new()).unwrap();
    let clock = itools_sync::system_clock();
    let registry = MirrorRegistry::new(MirrorRegistryOptions::from_config(&config.mirrors, clock));
    registry.refresh_if_needed().await;
    assert_eq!(
        registry.config_source(),
        ConfigSource::File,
        "随部署的 config/mirrors.json 应被真正装载（而不是悄悄用内置兜底）"
    );
    let app = serve(registry, &[]);
    let res = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(res.status, StatusCode::OK);
    let body = res.json();
    assert!(!body["mirrors"].as_array().unwrap().is_empty(), "至少有一个镜像");
    parse_mirror_config(&body, 0).expect("响应体本身必须是合法配置格式");
}

// ---------------------------------------------------------------- 端点契约

#[tokio::test]
async fn endpoint_is_public_and_serves_initial_config_before_any_probe() {
    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(fx.registry.clone(), &[]);

    // 不带任何 Authorization 头
    let res = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(res.status, StatusCode::OK, "公开端点，未登录必须可用");
    let body = parse_body(&res);
    assert_eq!(body.version, 1);
    assert_eq!(body.mirrors.len(), 2);
    assert!(body.mirrors.iter().all(|m| m.healthy), "未探测时用配置文件里的初值");
    assert_eq!(body.probe.sha256, probe_sha());

    // 带一个乱写的令牌也不该被拒（端点根本不看鉴权）
    let bad = get(&app, "/api/mirrors", &[("authorization", "Bearer garbage")], None).await;
    assert_eq!(bad.status, StatusCode::OK);
}

#[tokio::test]
async fn missing_config_file_falls_back_to_builtin() {
    let fx = new_registry(None, MockFetcher::new(&[]), Default::default()).await;
    assert_eq!(fx.registry.config_source(), ConfigSource::Builtin);
    let app = serve(fx.registry.clone(), &[]);
    let res = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(res.status, StatusCode::OK, "不空列表、不 500");
    let mut got: Vec<String> = parse_body(&res).mirrors.iter().map(|m| m.id.clone()).collect();
    let mut want: Vec<String> = builtin_mirror_config().mirrors.iter().map(|m| m.id.clone()).collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
}

#[tokio::test]
async fn etag_and_if_none_match() {
    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(fx.registry.clone(), &[]);

    let first = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(first.status, StatusCode::OK);
    let etag = first.header("etag").expect("必须带 ETag");
    assert!(
        etag.len() == 34 && etag.starts_with('"') && etag.ends_with('"'),
        "ETag 形如 \"32 位 hex\"，实得 {etag}"
    );
    assert!(first.header("cache-control").unwrap().contains("max-age="));

    let second = get(&app, "/api/mirrors", &[("if-none-match", &etag)], None).await;
    assert_eq!(second.status, StatusCode::NOT_MODIFIED);
    assert_eq!(second.body, "", "304 不带响应体");
    assert_eq!(second.header("etag").as_deref(), Some(etag.as_str()));

    // 不匹配的 ETag → 正常 200
    let third = get(&app, "/api/mirrors", &[("if-none-match", "\"deadbeef\"")], None).await;
    assert_eq!(third.status, StatusCode::OK);

    // 探测把状态改了（首轮把失败原因写进了响应体）→ ETag 变化 → 旧 ETag 不再命中 304
    fx.registry.run_probe().await;
    let fourth = get(&app, "/api/mirrors", &[("if-none-match", &etag)], None).await;
    assert_eq!(
        fourth.status,
        StatusCode::OK,
        "内容已变必须回 200，不能拿旧 ETag 糊弄客户端"
    );
}

// ---------------------------------------------------------------- ETag 稳定性（304 真能命中）

#[tokio::test]
async fn stable_rounds_keep_etag_so_304_actually_hits() {
    // 固定时钟：模拟「15 分钟一轮探测、客户端 30 分钟来问一次」的稳定态。
    // 起点对齐到整点：lastOkAt 的量化粒度是 1 小时，跨过整点本就该刷新 ETag，
    // 那样测的就不是「结果没变」这件事了。
    let (clock, now) = fake_clock(1_786_600_000_000i64.div_euclid(3_600_000) * 3_600_000);
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::ok()), ("beta.test", Rule::ok())]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher,
        RegistryOpts { clock: now, ..Default::default() },
    )
    .await;
    let app = serve(fx.registry.clone(), &[]);

    fx.registry.run_probe().await; // 第 1 轮：从「未探测」变为「已探测」，内容确实变了
    let first = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(first.status, StatusCode::OK);
    let etag = first.header("etag").unwrap();
    let updated_at = parse_body(&first).updated_at;

    clock.fetch_add(15 * 60_000, Ordering::SeqCst);
    fx.registry.run_probe().await; // 第 2 轮：结果一模一样
    clock.fetch_add(15 * 60_000, Ordering::SeqCst);
    fx.registry.run_probe().await; // 第 3 轮：还是一模一样

    let revalidate = get(&app, "/api/mirrors", &[("if-none-match", &etag)], None).await;
    assert_eq!(
        revalidate.status,
        StatusCode::NOT_MODIFIED,
        "结果没有实质变化就必须命中 304（否则 README 的承诺是空话）"
    );
    assert_eq!(revalidate.body, "");

    let full = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(full.header("etag").unwrap(), etag, "ETag 不该无谓抖动");
    assert_eq!(
        parse_body(&full).updated_at,
        updated_at,
        "updatedAt 是「最后一次实质变化」，不是「最后一次探测」"
    );
}

#[tokio::test]
async fn persistent_failure_does_not_churn_etag() {
    let (clock, now) = fake_clock(1_786_600_000_000);
    let fx = new_registry(
        Some(fixture_config()),
        MockFetcher::new(&[]), // 全部 DNS 失败
        RegistryOpts { fail_threshold: 1, clock: now, ..Default::default() },
    )
    .await;
    let app = serve(fx.registry.clone(), &[]);

    fx.registry.run_probe().await; // 首轮：healthy 翻转 + 写入失败原因 → 内容变
    let etag = get(&app, "/api/mirrors", &[], None).await.header("etag").unwrap();

    for _ in 0..3 {
        clock.fetch_add(15 * 60_000, Ordering::SeqCst);
        fx.registry.run_probe().await;
    }
    let res = get(&app, "/api/mirrors", &[("if-none-match", &etag)], None).await;
    assert_eq!(
        res.status,
        StatusCode::NOT_MODIFIED,
        "连续失败次数是诊断数据，不该逐轮把 ETag 打废"
    );
}

#[tokio::test]
async fn probe_at_header_refreshes_every_round_on_200_and_304() {
    let (clock, now) = fake_clock(1_786_600_800_000); // 2026-08-13T00:40:00.000Z 之类的整分时刻
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::ok()), ("beta.test", Rule::ok())]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher,
        RegistryOpts { clock: now, ..Default::default() },
    )
    .await;
    let app = serve(fx.registry.clone(), &[]);

    let before = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(
        before.header("x-mirror-probe-at"),
        None,
        "还没探测过就不该谎报探测时刻"
    );

    fx.registry.run_probe().await;
    let first = get(&app, "/api/mirrors", &[], None).await;
    let etag = first.header("etag").unwrap();
    let first_at = first.header("x-mirror-probe-at").expect("探测后必须带探测时刻");
    assert_eq!(first_at, itools_sync::iso_millis(clock.load(Ordering::SeqCst)));
    assert!(!first.body.contains("lastCheckedAt"), "每轮必变的诊断字段不进响应体");
    assert!(!first.body.contains("consecutiveFailures"), "每轮必变的诊断字段不进响应体");

    clock.fetch_add(15 * 60_000, Ordering::SeqCst);
    fx.registry.run_probe().await;
    let revalidate = get(&app, "/api/mirrors", &[("if-none-match", &etag)], None).await;
    assert_eq!(revalidate.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        revalidate.header("x-mirror-probe-at").unwrap(),
        itools_sync::iso_millis(clock.load(Ordering::SeqCst)),
        "304 也要带最新探测时刻——运维不能因为缓存命中就看不到新鲜度"
    );
    assert_ne!(revalidate.header("x-mirror-probe-at").unwrap(), first_at);
}

#[tokio::test]
async fn last_ok_at_is_quantized_by_granularity() {
    // 10:05 起步，粒度 1 小时
    let (clock, now) = fake_clock(1_786_600_000_000);
    let base = 1_786_600_000_000i64;
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::ok()), ("beta.test", Rule::ok())]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher,
        RegistryOpts { clock: now, ok_at_granularity_ms: 3_600_000, ..Default::default() },
    )
    .await;

    fx.registry.run_probe().await;
    let ok_at = fx.registry.snapshot().mirrors[0].last_ok_at.clone().expect("成功后应有 lastOkAt");
    assert_eq!(
        ok_at,
        itools_sync::iso_millis(base.div_euclid(3_600_000) * 3_600_000),
        "按小时向下取整"
    );
    let etag = fx.registry.body().etag;

    // 同一小时内再探测：时间戳不该被改写
    clock.store(base.div_euclid(3_600_000) * 3_600_000 + 20 * 60_000, Ordering::SeqCst);
    fx.registry.run_probe().await;
    assert_eq!(fx.registry.body().etag, etag, "同一粒度内不该改写时间戳");

    // 跨到下一个小时
    clock.store(base.div_euclid(3_600_000) * 3_600_000 + 65 * 60_000, Ordering::SeqCst);
    fx.registry.run_probe().await;
    assert_ne!(fx.registry.body().etag, etag, "跨粒度后时间戳真的前进，ETag 随之变化");
    assert_eq!(
        fx.registry.snapshot().mirrors[0].last_ok_at.clone().unwrap(),
        itools_sync::iso_millis(base.div_euclid(3_600_000) * 3_600_000 + 3_600_000)
    );
}

// ---------------------------------------------------------------- 探测

#[tokio::test]
async fn successful_probe_records_latency_and_sorts_by_it() {
    let fetcher = MockFetcher::new(&[
        ("alpha.test", Rule::with_delay(150)),
        ("beta.test", Rule::ok()),
    ]);
    let fx = new_registry(Some(fixture_config()), fetcher, Default::default()).await;
    fx.registry.run_probe().await;

    let app = serve(fx.registry.clone(), &[]);
    let body = parse_body(&get(&app, "/api/mirrors", &[], None).await);
    assert_eq!(body.mirrors.len(), 2);
    assert_eq!(body.mirrors[0].id, "beta", "更快的排前面");
    assert_eq!(body.mirrors[1].id, "alpha");
    for m in &body.mirrors {
        assert!(m.healthy);
        assert!(m.latency_ms.is_some());
        assert!(m.last_ok_at.is_some(), "lastOkAt 应为 ISO 时间");
        assert!(m.last_error.is_none());
        assert_eq!(m.latency_ms.unwrap() % 50, 0, "发布的延迟是量化值（50ms 的倍数）");
    }
    assert!(
        body.mirrors[1].latency_ms.unwrap() >= 100,
        "注入的 150ms 延迟应体现在 latencyMs 上，实得 {:?}",
        body.mirrors[1].latency_ms
    );
}

#[tokio::test]
async fn hash_mismatch_counts_as_failure_and_needs_threshold() {
    // beta 返回 200 但内容是验证页 —— 只看状态码会误判为健康。
    let fetcher = MockFetcher::new(&[
        ("alpha.test", Rule::ok()),
        ("beta.test", Rule::with_body("<html>正在验证您的浏览器…</html>")),
    ]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher,
        RegistryOpts { fail_threshold: 3, ..Default::default() },
    )
    .await;

    fx.registry.run_probe().await; // 第 1 次失败
    let snap = fx.registry.snapshot();
    let beta = snap.mirrors.iter().find(|m| m.id == "beta").unwrap();
    assert!(beta.healthy, "单次失败不该误杀（抖动容忍）");
    assert!(
        beta.last_error.as_deref().unwrap_or("").contains("哈希不匹配"),
        "实得 {:?}",
        beta.last_error
    );

    fx.registry.run_probe().await; // 第 2 次
    fx.registry.run_probe().await; // 第 3 次 → 达阈值
    let snap = fx.registry.snapshot();
    let beta = snap.mirrors.iter().find(|m| m.id == "beta").unwrap();
    assert!(!beta.healthy, "连续 3 次失败应判 unhealthy");
    assert_eq!(snap.mirrors[0].id, "alpha", "健康的排在不健康的前面");
    assert_eq!(snap.mirrors[1].id, "beta");
}

#[tokio::test]
async fn failure_reasons_are_classified_and_recovery_is_immediate() {
    // alpha → 403（如 ghfast.top 对 codeload 的表现）；beta → DNS 失败（计划表里没有它）
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::with_status(403))]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher.clone(),
        RegistryOpts { fail_threshold: 1, ..Default::default() },
    )
    .await;

    fx.registry.run_probe().await;
    let snap = fx.registry.snapshot();
    let find = |id: &str| snap.mirrors.iter().find(|m| m.id == id).unwrap().clone();
    assert_eq!(find("alpha").last_error.as_deref(), Some("HTTP 403"));
    assert!(find("beta").last_error.as_deref().unwrap_or("").contains("DNS"));
    assert!(snap.mirrors.iter().all(|m| !m.healthy), "全部应判 unhealthy");

    // 超时
    fetcher.set_plan(&[("alpha.test", Rule::failing("timeout"))]);
    fx.registry.run_probe().await;
    let snap = fx.registry.snapshot();
    assert_eq!(
        snap.mirrors.iter().find(|m| m.id == "alpha").unwrap().last_error.as_deref(),
        Some("超时")
    );

    // 恢复：立刻标回 healthy 且清掉失败计数与错误
    fetcher.set_plan(&[("alpha.test", Rule::ok()), ("beta.test", Rule::ok())]);
    fx.registry.run_probe().await;
    for m in fx.registry.snapshot().mirrors {
        assert!(m.healthy, "{} 恢复后应立即标回 healthy", m.id);
        assert!(m.last_error.is_none());
    }
}

#[tokio::test]
async fn probe_task_panic_does_not_take_down_the_service() {
    let fx = new_registry(
        Some(fixture_config()),
        Arc::new(PanicFetcher),
        RegistryOpts { fail_threshold: 1, ..Default::default() },
    )
    .await;
    // 探测任务内部 panic 被吞在任务里：这里不 panic、不挂起，端点照常可用。
    fx.registry.run_probe().await;
    let app = serve(fx.registry.clone(), &[]);
    let res = get(&app, "/api/mirrors", &[], None).await;
    assert_eq!(res.status, StatusCode::OK, "探测炸了，端点仍必须能服务");
    assert_eq!(parse_body(&res).mirrors.len(), 2, "沿用现有配置，不丢镜像");
}

#[tokio::test]
async fn fetch_error_is_reported_not_swallowed() {
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::failing("mock 内部炸了"))]);
    let fx = new_registry(
        Some(fixture_config()),
        fetcher,
        RegistryOpts { fail_threshold: 1, ..Default::default() },
    )
    .await;
    fx.registry.run_probe().await;
    let snap = fx.registry.snapshot();
    assert!(snap.mirrors.iter().all(|m| !m.healthy));
    let alpha = snap.mirrors.iter().find(|m| m.id == "alpha").unwrap();
    assert!(
        alpha.last_error.as_deref().unwrap_or("").contains("mock 内部炸了"),
        "失败原因要如实带出来，实得 {:?}",
        alpha.last_error
    );
}

#[tokio::test]
async fn probe_mirror_times_out_instead_of_reporting_success() {
    let fetcher = MockFetcher::new(&[("slow.test", Rule::with_delay(5_000))]);
    let probe = parse_mirror_config(&fixture_config(), 0).unwrap().probe;
    let out = probe_mirror(
        "https://slow.test/{owner}/{repo}/{ref}/{path}",
        &probe,
        fetcher.as_ref(),
        Duration::from_millis(20),
    )
    .await;
    assert!(!out.ok);
    assert_eq!(out.error.as_deref(), Some("超时"));
}

// ---------------------------------------------------------------- 热更新

#[tokio::test]
async fn config_file_hot_reload_keeps_state_of_unchanged_mirrors() {
    let fetcher = MockFetcher::new(&[("alpha.test", Rule::ok()), ("beta.test", Rule::ok())]);
    let fx = new_registry(Some(fixture_config()), fetcher, Default::default()).await;
    fx.registry.run_probe().await;
    assert_eq!(fx.registry.snapshot().mirrors.len(), 2);
    let alpha_latency = fx
        .registry
        .snapshot()
        .mirrors
        .iter()
        .find(|m| m.id == "alpha")
        .unwrap()
        .latency_ms;
    assert!(alpha_latency.is_some());

    // 维护者删掉 beta、加了 gamma（alpha 模板未动）
    let mut next = fixture_config();
    next["mirrors"] = json!([
        next["mirrors"][0].clone(),
        {
            "id": "gamma",
            "label": "gamma.test",
            "raw": "https://gamma.test/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}",
            "archive": "https://gamma.test/https://github.com/{owner}/{repo}/archive/{ref}.zip",
            "healthy": true,
        }
    ]);
    std::fs::write(&fx.file, serde_json::to_string_pretty(&next).unwrap()).unwrap();
    fx.registry.refresh_if_needed().await;

    let snap = fx.registry.snapshot();
    let mut ids: Vec<&str> = snap.mirrors.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["alpha", "gamma"]);
    assert_eq!(fx.registry.config_source(), ConfigSource::File);
    assert_eq!(
        snap.mirrors.iter().find(|m| m.id == "alpha").unwrap().latency_ms,
        alpha_latency,
        "模板未变的镜像应保留既有探测状态"
    );
    assert_eq!(
        snap.mirrors.iter().find(|m| m.id == "gamma").unwrap().latency_ms,
        None,
        "新镜像还没探测过"
    );

    // 写坏配置：沿用上一份，不空列表、不崩
    std::fs::write(&fx.file, "{ 这不是 JSON").unwrap();
    fx.registry.refresh_if_needed().await;
    let mut ids: Vec<String> =
        fx.registry.snapshot().mirrors.iter().map(|m| m.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, ["alpha", "gamma"]);
}

// ---------------------------------------------------------------- 限流（唯一的免认证读端点）

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after_per_ip() {
    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(
        fx.registry.clone(),
        &[("SYNC_MIRROR_RATE_MAX", "3"), ("SYNC_MIRROR_RATE_WINDOW_SEC", "60")],
    );

    for i in 1..=3 {
        let ok = get(&app, "/api/mirrors", &[], Some("203.0.113.7")).await;
        assert_eq!(ok.status, StatusCode::OK, "配额内第 {i} 次应正常返回");
        assert_eq!(ok.header("x-ratelimit-limit").as_deref(), Some("3"));
    }
    let limited = get(&app, "/api/mirrors", &[], Some("203.0.113.7")).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        limited.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1,
        "429 必须给 Retry-After"
    );
    assert!(limited.json()["error"].as_str().unwrap().contains("频繁"));

    // 另一个客户端不该被邻居连累
    let other = get(&app, "/api/mirrors", &[], Some("198.51.100.9")).await;
    assert_eq!(other.status, StatusCode::OK, "限流按 IP 分桶，不是全局闸门");
}

#[tokio::test]
async fn default_quota_is_generous_enough_for_real_clients() {
    let defaults = Config::from_map(&HashMap::new()).unwrap().mirrors;
    assert_eq!(defaults.rate_limit_max, 120);
    assert_eq!(defaults.rate_limit_window_sec, 60);

    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(fx.registry.clone(), &[]); // 用默认配额
    for i in 0..100 {
        let res = get(&app, "/api/mirrors", &[], Some("203.0.113.20")).await;
        assert_eq!(res.status, StatusCode::OK, "默认配额下第 {} 次请求不该被拦", i + 1);
    }
}

#[tokio::test]
async fn rate_limit_can_be_disabled_without_faking_headers() {
    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(fx.registry.clone(), &[("SYNC_MIRROR_RATE_MAX", "0")]);
    for _ in 0..30 {
        let res = get(&app, "/api/mirrors", &[], Some("203.0.113.30")).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(
            res.header("x-ratelimit-limit"),
            None,
            "关闭限流时不该发 X-RateLimit-* 头，不谎报限流存在"
        );
    }
}

#[tokio::test]
async fn health_endpoint_is_never_rate_limited() {
    let fx = new_registry(Some(fixture_config()), MockFetcher::new(&[]), Default::default()).await;
    let app = serve(fx.registry.clone(), &[("SYNC_MIRROR_RATE_MAX", "1")]);
    // 健康检查被限流会让负载均衡/监控误判服务已死，这是明确的例外。
    for _ in 0..10 {
        let res = get(&app, "/health", &[], Some("203.0.113.40")).await;
        assert_eq!(res.status, StatusCode::OK);
        assert_eq!(res.json()["ok"], json!(true));
    }
}
