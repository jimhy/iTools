//! 账号 / 同步集成测试：直接打路由，覆盖客户端契约的全部路径。
//!
//! 运行：`cargo test --test api`。**连真实 MariaDB**（连接参数取自 `SYNC_DB_*` 环境变量）。
//! 起一个测试库：
//! ```bash
//! docker run --name itools-mariadb-test -e MARIADB_ROOT_PASSWORD=roottest \
//!   -e MARIADB_DATABASE=itools -p 3307:3306 -d mariadb:11
//! SYNC_DB_PORT=3307 SYNC_DB_PASSWORD=roottest cargo test --test api
//! ```
//!
//! 各用例使用**互不相同的用户名**，因此可以并行跑而互不干扰（数据本就按用户隔离）。

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use itools_sync::config::Config;
use itools_sync::mirrors::{MirrorRegistry, MirrorRegistryOptions, SilentLogger};
use itools_sync::ratelimit::{RateLimiterOptions, SlidingWindowLimiter};
use itools_sync::routes::{build_router, AppState};
use itools_sync::store::MariaDbStore;

/// 连真实库并组装服务。连不上就直接失败——**不做静默跳过**，
/// 否则「测试全绿」会变成一句空话。
async fn harness(user: &str) -> (Router, Arc<MariaDbStore>) {
    let config = Config::from_map(&std::env::vars().collect::<HashMap<_, _>>())
        .expect("测试环境的配置应合法");
    let store = match MariaDbStore::connect(&config.db).await {
        Ok(s) => Arc::new(s),
        Err(err) => panic!(
            "连接 MariaDB 失败（{}@{}:{}/{}）：{err}\n\
             请先起一个测试库，例如：\n  \
             docker run --name itools-mariadb-test -e MARIADB_ROOT_PASSWORD=roottest \
             -e MARIADB_DATABASE=itools -p 3307:3306 -d mariadb:11\n  \
             SYNC_DB_PORT=3307 SYNC_DB_PASSWORD=roottest cargo test --test api",
            config.db.user, config.db.host, config.db.port, config.db.database
        ),
    };
    // 清理上次可能残留的测试账号，保证干净起点。
    store.delete_user(user).await.expect("清理测试账号");

    let clock = itools_sync::system_clock();
    // 镜像注册表在这些用例里用不到，但 AppState 需要一个：给它一个不存在的文件（回落内置兜底）
    // 且不启动定时探测——账号/同步测试不该打外网。
    let mirrors = MirrorRegistry::new(MirrorRegistryOptions {
        file: std::path::PathBuf::from("不存在的目录").join("mirrors.json"),
        probe_interval: std::time::Duration::from_secs(900),
        probe_timeout: std::time::Duration::from_secs(10),
        fail_threshold: 3,
        concurrency: 4,
        reload_check_ms: 5_000,
        ok_at_granularity_ms: 3_600_000,
        fetcher: Arc::new(itools_sync::mirrors::HttpFetcher::new(std::time::Duration::from_secs(10))),
        logger: Arc::new(SilentLogger),
        now: clock.clone(),
    });
    let limiter = SlidingWindowLimiter::new(RateLimiterOptions::new(0, 60_000, clock.clone()));
    let app = build_router(Arc::new(AppState {
        store: store.clone(),
        config: Arc::new(config),
        mirrors,
        mirror_limiter: Arc::new(limiter),
        clock,
    }));
    (app, store)
}

struct Res {
    status: StatusCode,
    body: String,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

async fn send(app: &Router, method: &str, uri: &str, payload: Option<Value>, token: Option<&str>) -> Res {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let body = match payload {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(builder.body(body).expect("构造请求"))
        .await
        .expect("路由应有响应");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.expect("读响应体");
    Res { status, body: String::from_utf8_lossy(&bytes).into_owned() }
}

async fn post(app: &Router, uri: &str, payload: Value, token: Option<&str>) -> Res {
    send(app, "POST", uri, Some(payload), token).await
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> Res {
    send(app, "GET", uri, None, token).await
}

/// 取回拉记录并按 key 索引，便于断言。
fn by_key(res: &Res) -> HashMap<String, Value> {
    res.json()["records"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r["key"].as_str().unwrap_or_default().to_string(), r))
        .collect()
}

#[tokio::test]
async fn health_is_reachable() {
    let (app, store) = harness("rust_health").await;
    let res = get(&app, "/health", None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["ok"], json!(true));
    assert_eq!(res.json()["service"], json!("itools-sync"));
    store.close().await;
}

#[tokio::test]
async fn auth_flow_register_login_and_logout() {
    let user = "rust_auth";
    let (app, store) = harness(user).await;

    // 首登自动注册并返回 token
    let res = post(&app, "/auth/login", json!({"username": user, "password": "s3cret"}), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let token = res.json()["token"].as_str().unwrap_or_default().to_string();
    assert!(!token.is_empty(), "必须返回会话令牌");
    assert_eq!(res.json()["username"], json!(user));

    // 同凭据再登录成功（非重复注册）
    let again = post(&app, "/auth/login", json!({"username": user, "password": "s3cret"}), None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(!again.json()["token"].as_str().unwrap_or_default().is_empty());

    // 密码错误 → 401
    let bad = post(&app, "/auth/login", json!({"username": user, "password": "wrong"}), None).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);

    // 缺字段 → 400
    let missing = post(&app, "/auth/login", json!({"username": user}), None).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    // 无令牌访问 /data → 401
    let no_auth = post(&app, "/data/app", json!({"records": []}), None).await;
    assert_eq!(no_auth.status, StatusCode::UNAUTHORIZED);

    // 退出登录使令牌失效
    let out = post(&app, "/auth/logout", json!({"allDevices": false}), Some(&token)).await;
    assert_eq!(out.status, StatusCode::OK);
    let after = post(&app, "/data/app", json!({"records": []}), Some(&token)).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED, "已吊销的令牌必须失效");

    store.close().await;
}

#[tokio::test]
async fn logout_all_devices_revokes_every_session() {
    let user = "rust_logout_all";
    let (app, store) = harness(user).await;

    let first = post(&app, "/auth/login", json!({"username": user, "password": "pw"}), None).await;
    let t1 = first.json()["token"].as_str().unwrap().to_string();
    let second = post(&app, "/auth/login", json!({"username": user, "password": "pw"}), None).await;
    let t2 = second.json()["token"].as_str().unwrap().to_string();
    assert_ne!(t1, t2, "两次登录应是两个不同会话");

    let out = post(&app, "/auth/logout", json!({"allDevices": true}), Some(&t1)).await;
    assert_eq!(out.status, StatusCode::OK);
    for t in [&t1, &t2] {
        let res = post(&app, "/data/app", json!({"records": []}), Some(t)).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "全设备退出后所有会话都该失效");
    }
    store.close().await;
}

#[tokio::test]
async fn data_sync_merge_pull_and_usage() {
    let user = "rust_data";
    let (app, store) = harness(user).await;
    let token = post(&app, "/auth/login", json!({"username": user, "password": "pw"}), None)
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // 首次推送两条：本次响应应排除刚推送的回声 → 空
    let push = post(
        &app,
        "/data/app",
        json!({"records": [
            {"key": "nickname", "value": "海风哥", "updatedAt": 1000},
            {"key": "opts", "value": {"theme": "dark"}, "updatedAt": 1000}
        ]}),
        Some(&token),
    )
    .await;
    assert_eq!(push.status, StatusCode::OK);
    assert_eq!(push.json()["records"], json!([]), "纯回声不该回传");

    // 「另一台设备」（同用户、空 dirty）来同步 → 应拉到服务端已存的两条
    let pull = post(&app, "/data/app", json!({"records": []}), Some(&token)).await;
    assert_eq!(pull.status, StatusCode::OK);
    let recs = by_key(&pull);
    assert_eq!(recs.len(), 2);
    assert_eq!(recs["nickname"]["value"], json!("海风哥"), "中文值应原样往返");
    assert_eq!(recs["opts"]["value"], json!({"theme": "dark"}));

    // last-write-wins：旧 updatedAt 不覆盖新值
    post(
        &app,
        "/data/app",
        json!({"records": [{"key": "nickname", "value": "新名", "updatedAt": 2000}]}),
        Some(&token),
    )
    .await;
    post(
        &app,
        "/data/app",
        json!({"records": [{"key": "nickname", "value": "旧名", "updatedAt": 1500}]}),
        Some(&token),
    )
    .await;
    let pull = post(&app, "/data/app", json!({"records": []}), Some(&token)).await;
    let recs = by_key(&pull);
    assert_eq!(recs["nickname"]["value"], json!("新名"), "应保留较新的值");
    assert_eq!(recs["nickname"]["updatedAt"], json!(2000));

    // 命名空间隔离 + plugin:ns 路由可用
    let plugin = post(
        &app,
        "/data/plugin:demo",
        json!({"records": [{"key": "k", "value": 42, "updatedAt": 1000}]}),
        Some(&token),
    )
    .await;
    assert_eq!(plugin.status, StatusCode::OK);
    let app_pull = post(&app, "/data/app", json!({"records": []}), Some(&token)).await;
    assert!(!by_key(&app_pull).contains_key("k"), "命名空间应隔离");
    let plugin_pull = post(&app, "/data/plugin:demo", json!({"records": []}), Some(&token)).await;
    let pr = by_key(&plugin_pull);
    assert_eq!(pr.len(), 1);
    assert_eq!(pr["k"]["value"], json!(42));

    // 用量统计：无令牌 401；有令牌返回真实条数与字节
    let no_auth = get(&app, "/data/_usage", None).await;
    assert_eq!(no_auth.status, StatusCode::UNAUTHORIZED);
    let usage = get(&app, "/data/_usage", Some(&token)).await;
    assert_eq!(usage.status, StatusCode::OK);
    let body = usage.json();
    assert_eq!(body["counts"]["app"], json!(2), "app 命名空间应有 2 条");
    assert_eq!(body["counts"]["plugin:demo"], json!(1), "plugin:demo 应有 1 条");
    assert!(body["bytes"].as_i64().unwrap_or(0) > 0, "占用字节应为真实正值");

    store.close().await;
}

#[tokio::test]
async fn malformed_records_are_dropped_not_crashing() {
    let user = "rust_malformed";
    let (app, store) = harness(user).await;
    let token = post(&app, "/auth/login", json!({"username": user, "password": "pw"}), None)
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let res = post(
        &app,
        "/data/app",
        json!({"records": [
            {"key": "good", "value": 1, "updatedAt": 5},
            {"value": "缺 key", "updatedAt": 5},
            {"key": "缺 updatedAt", "value": 2},
            "根本不是对象"
        ]}),
        Some(&token),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "结构不合法的记录被丢弃，不该 500");

    let pull = post(&app, "/data/app", json!({"records": []}), Some(&token)).await;
    let recs = by_key(&pull);
    assert_eq!(recs.len(), 1, "只有合法记录被写入");
    assert!(recs.contains_key("good"));
    store.close().await;
}

#[tokio::test]
async fn account_delete_requires_password_and_wipes_data() {
    let user = "rust_delete";
    let (app, store) = harness(user).await;
    let token = post(&app, "/auth/login", json!({"username": user, "password": "s3cret"}), None)
        .await
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();
    post(
        &app,
        "/data/app",
        json!({"records": [{"key": "k", "value": "v", "updatedAt": 1}]}),
        Some(&token),
    )
    .await;

    // 错误口令
    let bad = post(&app, "/account/delete", json!({"username": user, "password": "wrong"}), None).await;
    assert_eq!(bad.status, StatusCode::UNAUTHORIZED);
    // 正确口令
    let ok = post(&app, "/account/delete", json!({"username": user, "password": "s3cret"}), None).await;
    assert_eq!(ok.status, StatusCode::OK);

    // 注销后重新登录 → 因 allowRegister 默认开，会作为「新账号」自动注册，其旧数据应已清空
    let relogin = post(&app, "/auth/login", json!({"username": user, "password": "s3cret"}), None).await;
    assert_eq!(relogin.status, StatusCode::OK);
    let new_token = relogin.json()["token"].as_str().unwrap().to_string();
    let pull = post(&app, "/data/app", json!({"records": []}), Some(&new_token)).await;
    assert_eq!(pull.json()["records"], json!([]), "注销后数据应已清空");

    store.close().await;
}

#[tokio::test]
async fn password_hash_is_never_stored_in_plaintext() {
    let user = "rust_hashcheck";
    let (app, store) = harness(user).await;
    post(&app, "/auth/login", json!({"username": user, "password": "s3cret"}), None).await;

    let record = store.get_user(user).await.expect("查询用户").expect("账号应已建立");
    assert_ne!(record.password_hash, "s3cret", "口令绝不能明文落库");
    assert!(!record.password_hash.contains("s3cret"));
    assert_eq!(record.password_hash.len(), 128, "scrypt 64 字节派生值的 hex 长度");
    assert_eq!(record.salt.len(), 32, "每用户 16 字节随机盐");
    store.close().await;
}
