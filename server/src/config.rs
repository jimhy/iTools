//! 服务端配置：全部从环境变量读取，均有合理默认。凭据/密钥不写死在源码里。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::proxy::TrustProxy;

/// MariaDB 连接配置（凭据全走环境变量，源码零明文）。
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    /// 连接池上限
    pub connection_limit: u32,
}

/// GitHub 镜像源配置与健康探测（供公开端点 `GET /api/mirrors`）。
#[derive(Debug, Clone)]
pub struct MirrorsConfig {
    /// 可编辑的镜像列表文件路径（随部署提供；缺失/不合法则用代码里的内置兜底）。
    pub file: PathBuf,
    /// 是否启用定时健康探测（关掉则只返回配置文件里的初值）。
    pub probe_enabled: bool,
    /// 探测周期，下限 60s。
    pub probe_interval: Duration,
    /// 单次探测超时。
    pub probe_timeout: Duration,
    /// 连续失败多少次才判 unhealthy（恢复立即标回）。
    pub fail_threshold: u32,
    /// 响应的 `Cache-Control` max-age（秒）。
    pub cache_max_age_sec: u64,
    /// `lastOkAt` 时间戳的量化粒度（毫秒，默认 3600_000）。0 = 不量化。
    ///
    /// 不量化则每轮探测都改写时间戳 → ETag 每轮失效、304 永不命中（见 `mirrors.rs` 文件头）。
    pub ok_at_granularity_ms: i64,
    /// 公开端点 `/api/mirrors` 的按 IP 限流：窗口内最大请求数。0 = 关闭限流。
    pub rate_limit_max: u32,
    /// 限流窗口长度（秒）。
    pub rate_limit_window_sec: u64,
}

/// 插件审核用的大模型（OpenAI 兼容协议）。
///
/// **凭据只从环境变量读**（`ITOOLS_LLM_API_KEY`），源码与镜像零明文。
/// 没配 key 就是「审核能力未接入」——此时提审单会停在「需人工处理」，
/// 绝不自动放行（见 `llm.rs` 的诚信约束）。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// 服务基址（不含 `/v1/...`），如 `https://punkcodeai.myverse.site`。
    pub base_url: String,
    /// API key。空 = 审核未接入。
    pub api_key: String,
    /// 模型名。
    pub model: String,
    /// 单次审核超时（秒）。审代码的 prompt 很长，默认给足。
    pub timeout_sec: u64,
    /// 输出上限（含推理 token，给小了会让裁决被截断成不可解析的半截 JSON）。
    pub max_tokens: u32,
}

impl LlmConfig {
    /// 审核能力是否真的可用。**只看 key**：base_url / model 都有默认值，
    /// 唯独 key 没法编一个。
    pub fn enabled(&self) -> bool {
        !self.api_key.trim().is_empty()
    }
}

/// 插件市场与提审。
#[derive(Debug, Clone)]
pub struct MarketConfig {
    /// 已上线插件包的存放目录（每个包一个 zip）。容器里应挂成持久卷，
    /// 否则重启后市场里的插件全部下载失败。
    pub packages_dir: PathBuf,
    /// 允许提审的最大包体积（字节），与客户端安装侧上限对齐。
    pub max_upload_bytes: u64,
    /// 维护者账号（逗号分隔的用户名）。只有他们能下架 / 强制改判。
    /// 空 = 没有任何人能做管理操作（比留一个默认管理员安全）。
    pub admins: Vec<String>,
    /// 公开端点 `/api/market/*` 的按 IP 限流：窗口内最大请求数。0 = 关闭。
    pub rate_limit_max: u32,
    pub rate_limit_window_sec: u64,
    /// 同一账号两次提审之间的最小间隔（秒）。审核要花模型的钱，必须挡住连点。
    pub submit_cooldown_sec: i64,
}

/// TLS 证书（可选）：配了则以 HTTPS 起服务，直接在本进程做 TLS 终止。
///
/// 用于「frps 纯 TCP 透传 → 本机做 TLS」的部署（证书/私钥文件路径走环境变量，源码零明文）。
/// 不配则纯 HTTP（置于反向代理之后时用）。
#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// MariaDB 连接配置
    pub db: DbConfig,
    /// 镜像源配置与探测
    pub mirrors: MirrorsConfig,
    /// 插件审核用的大模型
    pub llm: LlmConfig,
    /// 插件市场与提审
    pub market: MarketConfig,
    /// 是否允许「首次登录即自动注册」（自托管默认开）
    pub allow_register: bool,
    /// 会话令牌随机字节数
    pub token_bytes: usize,
    /// 是否启用访问日志
    pub logger: bool,
    /// 是否信任反向代理的 `X-Forwarded-For`（决定客户端 IP 取值，进而决定限流按谁计数）。
    ///
    /// 默认「不信任」（取 TCP 对端地址，无法被伪造）。
    /// **置于 Nginx/Caddy 之后时必须打开**，否则所有客户端在服务端看来都来自代理那一个 IP，
    /// 会共用同一个限流桶而误伤真实用户。
    pub trust_proxy: TrustProxy,
    pub tls: Option<TlsConfig>,
}

/// 环境变量来源：进程环境或测试传入的映射。
pub type Env = HashMap<String, String>;

/// 读取当前进程的环境变量。
pub fn process_env() -> Env {
    std::env::vars().collect()
}

fn get<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.get(key).map(|s| s.as_str())
}

fn bool_env(v: Option<&str>, dflt: bool) -> bool {
    match v {
        None => dflt,
        Some(s) => !matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
    }
}

/// 数字环境变量：非法/缺省时回落到默认值，并按 `[min, max]` 夹紧。
fn num_env(v: Option<&str>, dflt: f64, min: f64, max: f64) -> f64 {
    match v {
        Some(s) if !s.trim().is_empty() => match s.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => n.clamp(min, max),
            _ => dflt,
        },
        _ => dflt,
    }
}

/// 简单数字环境变量：非法/缺省回落默认值（不夹紧）。
fn plain_num<T: std::str::FromStr>(v: Option<&str>, dflt: T) -> T {
    v.and_then(|s| s.trim().parse::<T>().ok()).unwrap_or(dflt)
}

/// 解析 `mysql://user:pass@host:port/db` 形式的连接串（各段可缺省）。
fn parse_db_url(raw: &str) -> Result<PartialDb, String> {
    let u = url::Url::parse(raw).map_err(|_| format!("SYNC_DB_URL 不是合法的连接串: {raw}"))?;
    let mut out = PartialDb::default();
    if let Some(h) = u.host_str() {
        if !h.is_empty() {
            out.host = Some(decode(h));
        }
    }
    if let Some(p) = u.port() {
        out.port = Some(p);
    }
    if !u.username().is_empty() {
        out.user = Some(decode(u.username()));
    }
    // 显式空口令（mysql://root:@host）也算「设置了口令为空」
    if let Some(pw) = u.password() {
        out.password = Some(decode(pw));
    }
    let db = u.path().trim_start_matches('/');
    if !db.is_empty() {
        out.database = Some(decode(db));
    }
    Ok(out)
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

#[derive(Default)]
struct PartialDb {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
}

impl Config {
    /// 从进程环境变量装载配置。
    pub fn from_env() -> Result<Self, String> {
        Self::from_map(&process_env())
    }

    /// 从给定的环境变量映射装载配置（测试用）。
    pub fn from_map(env: &Env) -> Result<Self, String> {
        // 单一 SYNC_DB_URL 存在时覆盖对应的分项配置。
        let url = match get(env, "SYNC_DB_URL") {
            Some(raw) if !raw.trim().is_empty() => parse_db_url(raw)?,
            _ => PartialDb::default(),
        };
        let db = DbConfig {
            host: url
                .host
                .or_else(|| get(env, "SYNC_DB_HOST").map(str::to_owned))
                .unwrap_or_else(|| "127.0.0.1".into()),
            port: url.port.unwrap_or_else(|| plain_num(get(env, "SYNC_DB_PORT"), 3306)),
            user: url
                .user
                .or_else(|| get(env, "SYNC_DB_USER").map(str::to_owned))
                .unwrap_or_else(|| "root".into()),
            password: url
                .password
                .or_else(|| get(env, "SYNC_DB_PASSWORD").map(str::to_owned))
                .unwrap_or_default(),
            database: url
                .database
                .or_else(|| get(env, "SYNC_DB_NAME").map(str::to_owned))
                .unwrap_or_else(|| "itools".into()),
            connection_limit: plain_num(get(env, "SYNC_DB_CONNLIMIT"), 10),
        };

        let mirrors = MirrorsConfig {
            // 默认取工作目录下的 config/mirrors.json：容器里 WORKDIR=/app（→ /app/config/mirrors.json），
            // 开发时 `cd server && cargo run`（→ server/config/mirrors.json）都自然命中。
            file: get(env, "SYNC_MIRRORS_FILE")
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("config").join("mirrors.json")),
            probe_enabled: bool_env(get(env, "SYNC_MIRROR_PROBE"), true),
            // 频率 15~30 分钟可配；下限 60s 防误配成高频打镜像站。
            probe_interval: Duration::from_secs_f64(num_env(
                get(env, "SYNC_MIRROR_PROBE_INTERVAL_SEC"),
                900.0,
                60.0,
                86_400.0,
            )),
            probe_timeout: Duration::from_secs_f64(
                num_env(get(env, "SYNC_MIRROR_PROBE_TIMEOUT_MS"), 10_000.0, 1_000.0, 60_000.0) / 1000.0,
            ),
            fail_threshold: num_env(get(env, "SYNC_MIRROR_FAIL_THRESHOLD"), 3.0, 1.0, 100.0) as u32,
            cache_max_age_sec: num_env(get(env, "SYNC_MIRROR_CACHE_SEC"), 300.0, 0.0, 86_400.0) as u64,
            ok_at_granularity_ms: num_env(
                get(env, "SYNC_MIRROR_OKAT_GRANULARITY_SEC"),
                3_600.0,
                0.0,
                86_400.0,
            ) as i64
                * 1000,
            // 默认 120 次 / 60 秒 / IP：正常客户端本地 TTL 30 分钟（约 2 次/小时），留了三个数量级的余量，
            // 连同一个 NAT 出口后的整栋楼也打不满；同时把恶意刷取压到「每 IP 每秒 2 次」级别。
            rate_limit_max: num_env(get(env, "SYNC_MIRROR_RATE_MAX"), 120.0, 0.0, 1_000_000.0) as u32,
            rate_limit_window_sec: num_env(get(env, "SYNC_MIRROR_RATE_WINDOW_SEC"), 60.0, 1.0, 3_600.0)
                as u64,
        };

        let llm = LlmConfig {
            // 默认基址可覆盖：审核服务换供应商时不需要改代码重新出镜像。
            base_url: get(env, "ITOOLS_LLM_BASE_URL")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("https://punkcodeai.myverse.site")
                .to_string(),
            // 只从环境变量读，源码零明文（doc/开发准则.md 安全红线）
            api_key: get(env, "ITOOLS_LLM_API_KEY").map(str::trim).unwrap_or("").to_string(),
            model: get(env, "ITOOLS_LLM_MODEL")
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("gpt-5.5")
                .to_string(),
            timeout_sec: num_env(get(env, "ITOOLS_LLM_TIMEOUT_SEC"), 300.0, 30.0, 1_800.0) as u64,
            // 推理型模型的 reasoning token 也算在这里，给小了裁决会被截断成半截 JSON
            max_tokens: num_env(get(env, "ITOOLS_LLM_MAX_TOKENS"), 16_000.0, 1_000.0, 200_000.0) as u32,
        };

        let market = MarketConfig {
            // 与 mirrors.file 同理：容器 WORKDIR=/app → /app/data/packages（应挂持久卷）
            packages_dir: get(env, "SYNC_PACKAGES_DIR")
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("data").join("packages")),
            max_upload_bytes: num_env(
                get(env, "SYNC_MAX_UPLOAD_MB"),
                32.0,
                1.0,
                256.0,
            ) as u64
                * 1024
                * 1024,
            // 没配就是空表：宁可「谁都不能下架」，也不要留一个默认管理员账号
            admins: get(env, "SYNC_ADMIN_USERS")
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            rate_limit_max: num_env(get(env, "SYNC_MARKET_RATE_MAX"), 120.0, 0.0, 1_000_000.0) as u32,
            rate_limit_window_sec: num_env(get(env, "SYNC_MARKET_RATE_WINDOW_SEC"), 60.0, 1.0, 3_600.0)
                as u64,
            submit_cooldown_sec: num_env(get(env, "SYNC_SUBMIT_COOLDOWN_SEC"), 60.0, 0.0, 86_400.0) as i64,
        };

        let tls = match (get(env, "SYNC_TLS_CERT_FILE"), get(env, "SYNC_TLS_KEY_FILE")) {
            (Some(c), Some(k)) if !c.trim().is_empty() && !k.trim().is_empty() => Some(TlsConfig {
                cert_file: PathBuf::from(c),
                key_file: PathBuf::from(k),
            }),
            _ => None,
        };

        Ok(Config {
            host: get(env, "SYNC_HOST").map(str::to_owned).unwrap_or_else(|| "127.0.0.1".into()),
            port: plain_num(get(env, "SYNC_PORT").or_else(|| get(env, "PORT")), 8787),
            db,
            mirrors,
            llm,
            market,
            allow_register: bool_env(get(env, "SYNC_ALLOW_REGISTER"), true),
            token_bytes: plain_num(get(env, "SYNC_TOKEN_BYTES"), 32),
            logger: bool_env(get(env, "SYNC_LOG"), true),
            trust_proxy: TrustProxy::parse(get(env, "SYNC_TRUST_PROXY")),
            tls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_match_legacy() {
        let c = Config::from_map(&env(&[])).unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 8787);
        assert_eq!(c.db.host, "127.0.0.1");
        assert_eq!(c.db.port, 3306);
        assert_eq!(c.db.user, "root");
        assert_eq!(c.db.database, "itools");
        assert_eq!(c.db.connection_limit, 10);
        assert!(c.allow_register);
        assert_eq!(c.token_bytes, 32);
        assert!(c.logger);
        assert_eq!(c.mirrors.probe_interval, Duration::from_secs(900));
        assert_eq!(c.mirrors.probe_timeout, Duration::from_secs(10));
        assert_eq!(c.mirrors.fail_threshold, 3);
        assert_eq!(c.mirrors.cache_max_age_sec, 300);
        assert_eq!(c.mirrors.ok_at_granularity_ms, 3_600_000);
        assert_eq!(c.mirrors.rate_limit_max, 120);
        assert_eq!(c.mirrors.rate_limit_window_sec, 60);
        assert!(c.tls.is_none());
        // 没配 key 时审核能力必须是「未接入」，不能有任何隐式默认
        assert!(!c.llm.enabled());
        assert_eq!(c.llm.model, "gpt-5.5");
        assert_eq!(c.market.max_upload_bytes, 32 * 1024 * 1024);
        assert!(c.market.admins.is_empty(), "没配就该是空表，不留默认管理员");
        assert_eq!(c.market.submit_cooldown_sec, 60);
    }

    #[test]
    fn llm_enabled_only_with_key() {
        let c = Config::from_map(&env(&[("ITOOLS_LLM_API_KEY", "  ")])).unwrap();
        assert!(!c.llm.enabled(), "空白 key 不算配了");
        let c = Config::from_map(&env(&[("ITOOLS_LLM_API_KEY", "sk-x")])).unwrap();
        assert!(c.llm.enabled());
        assert_eq!(c.llm.api_key, "sk-x");
    }

    #[test]
    fn admins_parsed_from_csv() {
        let c = Config::from_map(&env(&[("SYNC_ADMIN_USERS", " jimhy , alice ,, ")])).unwrap();
        assert_eq!(c.market.admins, vec!["jimhy", "alice"]);
    }

    #[test]
    fn db_url_overrides_fields() {
        let c = Config::from_map(&env(&[
            ("SYNC_DB_HOST", "ignored"),
            ("SYNC_DB_URL", "mysql://u%20ser:p%40ss@db.internal:3307/itools_prod"),
        ]))
        .unwrap();
        assert_eq!(c.db.host, "db.internal");
        assert_eq!(c.db.port, 3307);
        assert_eq!(c.db.user, "u ser");
        assert_eq!(c.db.password, "p@ss");
        assert_eq!(c.db.database, "itools_prod");
    }

    #[test]
    fn db_url_invalid_errors() {
        assert!(Config::from_map(&env(&[("SYNC_DB_URL", "不是 URL")])).is_err());
    }

    #[test]
    fn numeric_env_clamped_and_fallback() {
        let c = Config::from_map(&env(&[
            ("SYNC_MIRROR_PROBE_INTERVAL_SEC", "5"), // 低于下限 60
            ("SYNC_MIRROR_FAIL_THRESHOLD", "abc"),   // 非法 → 默认 3
            ("SYNC_MIRROR_RATE_MAX", "0"),           // 0 合法：关闭限流
        ]))
        .unwrap();
        assert_eq!(c.mirrors.probe_interval, Duration::from_secs(60));
        assert_eq!(c.mirrors.fail_threshold, 3);
        assert_eq!(c.mirrors.rate_limit_max, 0);
    }

    #[test]
    fn bool_env_recognizes_off_words() {
        for v in ["false", "0", "no", "off", "OFF"] {
            let c = Config::from_map(&env(&[("SYNC_ALLOW_REGISTER", v)])).unwrap();
            assert!(!c.allow_register, "{v} 应视为关闭");
        }
        let c = Config::from_map(&env(&[("SYNC_ALLOW_REGISTER", "true")])).unwrap();
        assert!(c.allow_register);
    }

    #[test]
    fn tls_requires_both_files() {
        let only_cert = Config::from_map(&env(&[("SYNC_TLS_CERT_FILE", "/c.pem")])).unwrap();
        assert!(only_cert.tls.is_none(), "只给一半不该假装启用 TLS");
        let both = Config::from_map(&env(&[
            ("SYNC_TLS_CERT_FILE", "/c.pem"),
            ("SYNC_TLS_KEY_FILE", "/k.pem"),
        ]))
        .unwrap();
        assert!(both.tls.is_some());
    }
}
