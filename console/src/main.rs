//! 入口：装载配置 → 连接 MariaDB → 引导首个管理员 → 起 HTTP(S) 服务。
//!
//! TLS 终止与优雅关停的做法与 `server/src/main.rs` 一致（生产是 frps 纯 TCP 透传，
//! 证书必须由本进程终结）。手写 accept 循环而不引 `axum-server`，理由同主服务：
//! 后者会把 aws-lc-rs 拉进依赖树，这里只要 rustls + ring。

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::trace::TraceLayer;

use itools_console::config::{Config, TlsConfig};
use itools_console::ratelimit::RateLimiter;
use itools_console::routes::{build_router, AppState};
use itools_console::store::{role, Store};
use itools_console::system_clock;

/// 过期会话回收周期。
const SESSION_GC_INTERVAL_SEC: u64 = 600;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[itools-console] 配置不合法：{msg}");
            return ExitCode::FAILURE;
        }
    };

    init_tracing(config.logger);
    // 进程级 rustls 加密后端（TLS 终止与健康探测共用；不装的话首次握手会 panic）
    if rustls::crypto::ring::default_provider().install_default().is_err() {
        tracing::debug!("rustls 加密后端已由其它组件安装，沿用之");
    }

    let clock = system_clock();

    // 连不上库就别假装起来了——控制台没有库等于什么都做不了。
    let store = match Store::connect(&config.db).await {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "[itools-console] 连接 MariaDB 失败（{}）：{err}",
                config.db.redacted()
            );
            eprintln!("请检查 CONSOLE_DB_HOST/PORT/USER/PASSWORD/NAME 与数据库可达性。");
            return ExitCode::FAILURE;
        }
    };

    // 主服务的表缺失说明连错库了。这不该让控制台退出（万一是主服务还没初始化），
    // 但必须在启动日志里大声说出来，否则运营看到空列表会以为「一个用户都没有」。
    match store.verify_upstream_tables().await {
        Ok(missing) if !missing.is_empty() => {
            tracing::warn!(
                "[store] 目标库里缺少云同步服务端的表：{}。控制台会照常启动，但相关页面将是空的——请确认 CONSOLE_DB_NAME 指向的是主服务在用的那个库。",
                missing.join("、")
            );
        }
        Ok(_) => tracing::info!("[store] 云同步服务端的 5 张表都在，库连接正确"),
        Err(e) => tracing::warn!("[store] 校验主服务表失败：{e}"),
    }

    if let Err(code) = bootstrap_admin(&store, &config, (clock)()).await {
        return code;
    }

    let http = match build_probe_client(&config) {
        Ok(c) => Some(c),
        Err(e) => {
            // 探测器建不起来只影响系统页的一个卡片，不该拖垮整个控制台
            tracing::warn!("[system] 健康探测客户端初始化失败：{e}，系统页将显示「探测器不可用」");
            None
        }
    };

    let state = Arc::new(AppState {
        store,
        login_limiter: RateLimiter::new(config.login_rate_max, config.login_rate_window_sec),
        http,
        config,
        clock: clock.clone(),
    });

    // 过期会话回收。不清理的话 console_sessions 只增不减——
    // 主服务的 sessions 就是这么长成一张垃圾表的，别重蹈。
    spawn_session_gc(state.clone());

    let logger_on = state.config.logger;
    let mut app = build_router(state.clone());
    if logger_on {
        app = app.layer(TraceLayer::new_for_http());
    }

    let addr = format!("{}:{}", state.config.host, state.config.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[itools-console] 监听 {addr} 失败：{err}");
            state.store.close().await;
            return ExitCode::FAILURE;
        }
    };
    let tls = state.config.tls.clone();
    let scheme = if tls.is_some() { "https" } else { "http" };
    println!("itools-console 已启动: {scheme}://{addr}");
    if tls.is_none() {
        println!(
            "⚠ 未配置 TLS：控制台会以明文 HTTP 提供服务。除非它只监听在受信内网上，\n\
   否则请配上 CONSOLE_TLS_CERT_FILE / CONSOLE_TLS_KEY_FILE——后台登录口令会明文过网。"
        );
    }

    let served = match &tls {
        Some(t) => serve_tls(listener, app, t).await,
        None => axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| e.to_string()),
    };

    state.store.close().await;
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[itools-console] 服务异常退出：{err}");
            ExitCode::FAILURE
        }
    }
}

/// 首个管理员的引导。
///
/// 规则很硬：
/// - 表里**已有**任何管理员 → 什么都不做（引导变量不会覆盖已有账号，
///   否则每次重启都能用环境变量把口令改回去，等于给了一把后门钥匙）。
/// - 表是空的且**没配**引导变量 → 打印清晰指引后**退出**。
///   起一个谁也登不进去的控制台没有意义，而且会让人误以为「服务好了」。
/// - 表是空的且配了引导变量 → 建号，并标记 `must_change_password`。
async fn bootstrap_admin(store: &Store, config: &Config, now: i64) -> Result<(), ExitCode> {
    let count = match store.count_admins().await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[itools-console] 查询控制台账号失败：{e}");
            return Err(ExitCode::FAILURE);
        }
    };

    if count > 0 {
        if config.bootstrap.is_some() {
            tracing::info!(
                "[bootstrap] 控制台已有 {count} 个账号，引导变量被忽略（引导只在首次初始化时生效）"
            );
        }
        return Ok(());
    }

    let Some(b) = &config.bootstrap else {
        eprintln!(
            "[itools-console] 控制台还没有任何管理员账号，且未配置引导变量，无法启动。\n\
             请设置以下两个环境变量后重启，首次登录会强制要求改口令：\n\
             \x20   CONSOLE_BOOTSTRAP_USER=<用户名>\n\
             \x20   CONSOLE_BOOTSTRAP_PASSWORD=<至少 8 位的口令>\n\
             建好账号后即可移除这两个变量（已有账号时它们不再生效）。"
        );
        return Err(ExitCode::FAILURE);
    };

    match store
        .create_admin(&b.username, &b.password, role::SUPER, true, now)
        .await
    {
        Ok(true) => {
            println!(
                "[itools-console] 已创建初始超级管理员 `{}`，首次登录必须修改口令。\n\
   建议登录改密后，从运行环境里移除 CONSOLE_BOOTSTRAP_* 两个变量。",
                b.username
            );
            Ok(())
        }
        Ok(false) => {
            // 并发启动时另一个实例先建好了，不算错
            tracing::info!("[bootstrap] 账号已存在，跳过创建");
            Ok(())
        }
        Err(e) => {
            eprintln!("[itools-console] 创建初始管理员失败：{e}");
            Err(ExitCode::FAILURE)
        }
    }
}

fn build_probe_client(config: &Config) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("itools-console/", env!("CARGO_PKG_VERSION")));
    if config.upstream_insecure {
        // 只在显式开启时才跳过证书校验：主服务用自签证书的场景确实存在，
        // 但默认必须校验，且这条要在日志里留痕。
        tracing::warn!("[system] CONSOLE_UPSTREAM_INSECURE 已开启：健康探测不校验上游证书");
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build()
}

fn spawn_session_gc(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(SESSION_GC_INTERVAL_SEC));
        // 第一次 tick 立刻返回，跳过它，避免刚启动就跑一次没必要的清理
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = state.now();
            match state.store.purge_expired_console_sessions(now).await {
                Ok(n) if n > 0 => tracing::info!("[gc] 回收了 {n} 条过期的控制台会话"),
                Ok(_) => {}
                // 清理失败只是垃圾多留一会儿，不该让任务终止
                Err(e) => tracing::warn!("[gc] 回收过期会话失败：{e}"),
            }
        }
    });
}

fn init_tracing(logger: bool) {
    let default = if logger { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("CONSOLE_RUST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// 本进程终止 TLS（适配 frps 纯 TCP 透传部署）。
async fn serve_tls(listener: TcpListener, app: Router, tls: &TlsConfig) -> Result<(), String> {
    let certs = load_certs(std::path::Path::new(&tls.cert_file))?;
    let key = load_key(std::path::Path::new(&tls.key_file))?;
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS 证书与私钥不匹配或不可用: {e}"))?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                // 单个连接 accept 失败（对端 RST、fd 用尽等）不该让整个服务退出
                Err(err) => {
                    tracing::warn!("接受连接失败: {err}");
                    continue;
                }
            },
            _ = &mut shutdown => return Ok(()),
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::debug!("TLS 握手失败（{peer}）: {err}");
                    return;
                }
            };
            // ConnectInfo 由这里注入：登录限流要按真实对端地址计数
            let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer));
                app.clone().oneshot(req)
            });
            if let Err(err) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!("连接处理结束（{peer}）: {err}");
            }
        });
    }
}

fn load_certs(path: &std::path::Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取证书 {} 失败: {e}", path.display()))?;
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut data.as_slice()).collect();
    let certs = certs.map_err(|e| format!("解析证书 {} 失败: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("证书文件 {} 里没有任何证书", path.display()));
    }
    Ok(certs)
}

fn load_key(path: &std::path::Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>, String> {
    let data = std::fs::read(path).map_err(|e| format!("读取私钥 {} 失败: {e}", path.display()))?;
    rustls_pemfile::private_key(&mut data.as_slice())
        .map_err(|e| format!("解析私钥 {} 失败: {e}", path.display()))?
        .ok_or_else(|| format!("私钥文件 {} 里没有可用私钥", path.display()))
}

/// Ctrl-C / SIGTERM 触发优雅关停。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!("注册 SIGTERM 处理器失败: {err}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("收到退出信号，正在关闭…");
}
