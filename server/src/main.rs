//! 入口：装载配置 → 连接 MariaDB → 起 HTTP(S) 服务。

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tower_http::trace::TraceLayer;

use itools_sync::config::{Config, TlsConfig};
use itools_sync::market::MarketService;
use itools_sync::mirrors::{MirrorRegistry, MirrorRegistryOptions};
use itools_sync::ratelimit::{RateLimiterOptions, SlidingWindowLimiter};
use itools_sync::routes::{build_router, AppState};
use itools_sync::store::MariaDbStore;
use itools_sync::system_clock;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("[itools-sync] 配置不合法：{msg}");
            return ExitCode::FAILURE;
        }
    };

    init_tracing(config.logger);
    // 进程级 rustls 加密后端（TLS 终止与镜像探测共用；不装的话首次握手会 panic）。
    if rustls::crypto::ring::default_provider().install_default().is_err() {
        tracing::debug!("rustls 加密后端已由其它组件安装，沿用之");
    }

    let clock = system_clock();

    // 诚实：连不上 DB 就别假装起来了——打印清晰错误并退出。
    let store = match MariaDbStore::connect(&config.db).await {
        Ok(s) => Arc::new(s),
        Err(err) => {
            eprintln!(
                "[itools-sync] 连接 MariaDB 失败（{}@{}:{}/{}）：{err}",
                config.db.user, config.db.host, config.db.port, config.db.database
            );
            eprintln!("请检查 SYNC_DB_HOST/PORT/USER/PASSWORD/NAME 环境变量与数据库可达性。");
            return ExitCode::FAILURE;
        }
    };

    // 镜像源注册表：先装载配置文件（失败则内置兜底），再按配置起定时探测。
    // 探测经镜像访问，**不需要本机能直连 GitHub**（详见 README「镜像源与健康探测」）。
    let mirrors = MirrorRegistry::new(MirrorRegistryOptions::from_config(&config.mirrors, clock.clone()));
    mirrors.refresh_if_needed().await;
    if config.mirrors.probe_enabled {
        mirrors.start(); // 启动即先跑一轮，之后按周期跑；异常自吞不拖垮主服务
    } else {
        tracing::info!("[mirrors] 定时探测已关闭（SYNC_MIRROR_PROBE=false），/api/mirrors 只返回配置文件初值");
    }

    let limiter = SlidingWindowLimiter::new(RateLimiterOptions::new(
        config.mirrors.rate_limit_max,
        config.mirrors.rate_limit_window_sec as i64 * 1000,
        clock.clone(),
    ));

    let logger_on = config.logger;
    let tls = config.tls.clone();
    let addr = format!("{}:{}", config.host, config.port);

    // 插件包目录必须可写：审核通过的包要落在这里，客户端从这里下载。
    // 建不出来就直接退出——留到第一次有人提审时才炸，是把故障推给用户去发现。
    if let Err(e) = std::fs::create_dir_all(&config.market.packages_dir) {
        eprintln!(
            "[itools-sync] 创建插件包目录 {} 失败：{e}",
            config.market.packages_dir.display()
        );
        eprintln!("请检查 SYNC_PACKAGES_DIR 指向的路径是否存在且可写（容器里应挂持久卷）。");
        return ExitCode::FAILURE;
    }
    if !config.llm.enabled() {
        tracing::warn!(
            "[market] 未配置 ITOOLS_LLM_API_KEY：插件提审只做机械校验，代码审核不会执行，\
             提交上来的插件会停在「待人工处理」而不会自动上线"
        );
    }
    let market_limiter = SlidingWindowLimiter::new(RateLimiterOptions::new(
        config.market.rate_limit_max,
        config.market.rate_limit_window_sec as i64 * 1000,
        clock.clone(),
    ));

    let config = Arc::new(config);
    let market = MarketService::new(store.clone(), config.clone(), clock.clone());
    market.bootstrap().await;

    let state = Arc::new(AppState {
        store: store.clone(),
        config,
        mirrors: mirrors.clone(),
        mirror_limiter: Arc::new(limiter),
        market,
        market_limiter: Arc::new(market_limiter),
        clock,
    });

    let mut app = build_router(state);
    if logger_on {
        app = app.layer(TraceLayer::new_for_http());
    }

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(err) => {
            eprintln!("[itools-sync] 监听 {addr} 失败：{err}");
            mirrors.stop();
            return ExitCode::FAILURE;
        }
    };
    let scheme = if tls.is_some() { "https" } else { "http" };
    println!("itools-sync 已启动: {scheme}://{addr}");

    let served = match &tls {
        Some(t) => serve_tls(listener, app, t).await,
        None => axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| e.to_string()),
    };

    mirrors.stop();
    store.close().await;
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[itools-sync] 服务异常退出：{err}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(logger: bool) {
    // 日志关掉时只留 warn 以上（启动提示与致命错误仍会打印），不做「彻底静默」。
    let default = if logger { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("SYNC_RUST_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// 配了 TLS 证书时：本进程直接做 TLS 终止（适配 frps 纯 TCP 透传部署）。
///
/// 手写 accept 循环而不是引 `axum-server`：后者会把 aws-lc-rs 拉进依赖树，
/// 这里只需要 rustls + ring 就够，几十行换一个更小的构建面。
async fn serve_tls(listener: TcpListener, app: Router, tls: &TlsConfig) -> Result<(), String> {
    let certs = load_certs(&tls.cert_file)?;
    let key = load_key(&tls.key_file)?;
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
                // 单个连接的 accept 失败（对端 RST、fd 用尽等）不该让整个服务退出。
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
            // ConnectInfo 由这里注入：限流要按真实对端地址计数。
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
