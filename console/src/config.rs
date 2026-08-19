//! 运营控制台的配置装载。**全部走环境变量**，源码与镜像里零明文凭据
//! （与 `server/src/config.rs` 同一条纪律：这个仓库是公开的，运维拓扑与口令都不进源码）。
//!
//! 命名一律以 `CONSOLE_` 开头，避免与主服务的 `SYNC_*` 混淆——两者会在同一台机器上
//! 用同一份 `--env-file` 的可能性是存在的，前缀撞了会互相污染。

use std::collections::HashMap;

/// 会话默认有效期：8 小时。运营后台不做「永不过期」，与主服务客户端会话是两套语义。
const DEFAULT_SESSION_TTL_SEC: u64 = 8 * 3600;

/// 数据库连接参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub name: String,
    pub conn_limit: u32,
    /// 直接给完整 URL 时覆盖上面各分项（便于运维一把梭）。
    pub url: Option<String>,
}

impl DbConfig {
    /// 拼出 sqlx 用的连接串。口令做百分号编码，避免特殊字符把 URL 拆坏。
    pub fn connect_url(&self) -> String {
        if let Some(u) = &self.url {
            return u.clone();
        }
        let user = percent_encode(&self.user);
        let pass = percent_encode(&self.password);
        format!(
            "mysql://{}:{}@{}:{}/{}",
            user, pass, self.host, self.port, self.name
        )
    }

    /// 日志里可以安全打印的形态：口令抹成 `***`。
    pub fn redacted(&self) -> String {
        if self.url.is_some() {
            return "mysql://***（由 CONSOLE_DB_URL 提供）".to_string();
        }
        format!(
            "mysql://{}:***@{}:{}/{}",
            self.user, self.host, self.port, self.name
        )
    }
}

/// TLS 证书对。两个都配了才启用 HTTPS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub cert_file: String,
    pub key_file: String,
}

/// 首次启动时的引导管理员。**只在 `console_admins` 表为空时生效**，
/// 建完账号后会强制要求改密（`must_change_password`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapAdmin {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub db: DbConfig,
    pub tls: Option<TlsConfig>,
    /// 后台前端静态资源目录。留空 → 只提供 API，不托管页面。
    pub web_dir: Option<String>,
    pub session_ttl_sec: u64,
    pub bootstrap: Option<BootstrapAdmin>,
    /// 登录限流：每 IP 每窗口最多几次。0 = 关闭（不建议）。
    pub login_rate_max: u32,
    pub login_rate_window_sec: u64,
    /// 主服务健康探测地址。留空 → 系统页如实显示「未配置」，不做假绿灯。
    pub upstream_health_url: Option<String>,
    /// 主服务用自签证书时置 true，否则健康探测永远失败。
    pub upstream_insecure: bool,
    /// 置于反向代理/内网穿透之后时是否信任 `X-Forwarded-For`。
    pub trust_proxy: bool,
    /// 统计按天/按小时分桶时用的时区偏移（分钟）。默认 +480 = 东八区。
    ///
    /// 刻意不依赖数据库的 `time_zone`：那个跟着机器走，同一份数据换台机器
    /// 就能画出不同的曲线。这里显式给定，分桶行为完全确定。
    pub tz_offset_min: i64,
    pub logger: bool,
}

impl Config {
    /// 从进程环境装载。
    pub fn from_env() -> Result<Self, String> {
        let map: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&map)
    }

    /// 从任意 map 装载（测试用）。
    pub fn from_map(env: &HashMap<String, String>) -> Result<Self, String> {
        let get = |k: &str| env.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

        let host = get("CONSOLE_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
        let port = get("CONSOLE_PORT")
            .as_deref()
            .map(parse_port)
            .transpose()?
            .unwrap_or(7005);

        let db = DbConfig {
            host: get("CONSOLE_DB_HOST").unwrap_or_else(|| "127.0.0.1".to_string()),
            port: get("CONSOLE_DB_PORT")
                .as_deref()
                .map(parse_port)
                .transpose()?
                .unwrap_or(3306),
            user: get("CONSOLE_DB_USER").unwrap_or_else(|| "root".to_string()),
            // 口令允许为空（本机 socket 无密的场景），但生产必须给。
            password: env
                .get("CONSOLE_DB_PASSWORD")
                .cloned()
                .unwrap_or_default(),
            name: get("CONSOLE_DB_NAME").unwrap_or_else(|| "itools".to_string()),
            conn_limit: num_env(get("CONSOLE_DB_CONNLIMIT").as_deref(), 5, 1, 100),
            url: get("CONSOLE_DB_URL"),
        };

        // 证书与私钥必须成对出现：只配一个是运维笔误，这时候「静默降级成 HTTP」
        // 比直接报错危险得多——后台是带凭据的页面，绝不能悄悄裸奔。
        let cert = get("CONSOLE_TLS_CERT_FILE");
        let key = get("CONSOLE_TLS_KEY_FILE");
        let tls = match (cert, key) {
            (Some(cert_file), Some(key_file)) => Some(TlsConfig { cert_file, key_file }),
            (None, None) => None,
            _ => {
                return Err(
                    "CONSOLE_TLS_CERT_FILE 与 CONSOLE_TLS_KEY_FILE 必须同时配置（只配一个会导致后台以明文 HTTP 暴露）"
                        .to_string(),
                )
            }
        };

        let bootstrap = match (get("CONSOLE_BOOTSTRAP_USER"), get("CONSOLE_BOOTSTRAP_PASSWORD")) {
            (Some(username), Some(password)) => {
                if password.chars().count() < 8 {
                    return Err("CONSOLE_BOOTSTRAP_PASSWORD 至少 8 个字符".to_string());
                }
                Some(BootstrapAdmin { username, password })
            }
            (None, None) => None,
            _ => {
                return Err(
                    "CONSOLE_BOOTSTRAP_USER 与 CONSOLE_BOOTSTRAP_PASSWORD 必须同时配置".to_string()
                )
            }
        };

        Ok(Config {
            host,
            port,
            db,
            tls,
            web_dir: get("CONSOLE_WEB_DIR"),
            session_ttl_sec: num_env(
                get("CONSOLE_SESSION_TTL_SEC").as_deref(),
                DEFAULT_SESSION_TTL_SEC,
                300,
                30 * 24 * 3600,
            ),
            bootstrap,
            login_rate_max: num_env(get("CONSOLE_LOGIN_RATE_MAX").as_deref(), 10, 0, 10_000),
            login_rate_window_sec: num_env(
                get("CONSOLE_LOGIN_RATE_WINDOW_SEC").as_deref(),
                300,
                10,
                86_400,
            ),
            upstream_health_url: get("CONSOLE_UPSTREAM_HEALTH_URL"),
            upstream_insecure: bool_env(get("CONSOLE_UPSTREAM_INSECURE").as_deref(), false),
            trust_proxy: bool_env(get("CONSOLE_TRUST_PROXY").as_deref(), false),
            // ±14 小时覆盖全部真实时区（含 UTC+14 的 Kiribati）
            tz_offset_min: num_env(get("CONSOLE_TZ_OFFSET_MIN").as_deref(), 480, -840, 840),
            logger: bool_env(get("CONSOLE_LOG").as_deref(), true),
        })
    }
}

fn parse_port(v: &str) -> Result<u16, String> {
    v.parse::<u16>()
        .map_err(|_| format!("端口必须是 1~65535 的整数，收到 `{v}`"))
        .and_then(|p| {
            if p == 0 {
                Err("端口不能为 0".to_string())
            } else {
                Ok(p)
            }
        })
}

/// `false|0|no|off`（大小写不敏感）视为关，其余非空值视为开；缺省用 `dflt`。
fn bool_env(v: Option<&str>, dflt: bool) -> bool {
    match v {
        None => dflt,
        Some(s) => !matches!(s.to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
    }
}

/// 数值型环境变量：非法值回落默认，合法值夹紧到 `[min, max]`。
/// **不因为一个笔误就拒绝启动**——但也绝不接受越界值。
fn num_env<T>(v: Option<&str>, dflt: T, min: T, max: T) -> T
where
    T: std::str::FromStr + PartialOrd + Copy,
{
    match v.and_then(|s| s.parse::<T>().ok()) {
        Some(n) if n < min => min,
        Some(n) if n > max => max,
        Some(n) => n,
        None => dflt,
    }
}

/// URL 组件的百分号编码：只保留 unreserved 字符，其余一律转义。
/// 自己写而不引 `percent-encoding`：这里只有用户名/口令两处用途，规则固定。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_safe() {
        let c = Config::from_map(&env(&[])).unwrap();
        assert_eq!(c.host, "127.0.0.1", "默认只听本机，不裸奔到公网");
        assert_eq!(c.port, 7005);
        assert_eq!(c.db.name, "itools");
        assert!(c.tls.is_none());
        assert!(c.bootstrap.is_none(), "不配引导变量就不建默认管理员");
        assert!(c.upstream_health_url.is_none());
        assert!(!c.trust_proxy);
    }

    #[test]
    fn tls_must_be_configured_in_pairs() {
        let only_cert = Config::from_map(&env(&[("CONSOLE_TLS_CERT_FILE", "/certs/a.pem")]));
        assert!(only_cert.is_err(), "只配证书不配私钥必须报错，不能静默降级成 HTTP");
        let only_key = Config::from_map(&env(&[("CONSOLE_TLS_KEY_FILE", "/certs/k.pem")]));
        assert!(only_key.is_err());
        let both = Config::from_map(&env(&[
            ("CONSOLE_TLS_CERT_FILE", "/certs/a.pem"),
            ("CONSOLE_TLS_KEY_FILE", "/certs/k.pem"),
        ]))
        .unwrap();
        assert!(both.tls.is_some());
    }

    #[test]
    fn bootstrap_requires_both_and_min_length() {
        assert!(Config::from_map(&env(&[("CONSOLE_BOOTSTRAP_USER", "ops")])).is_err());
        assert!(Config::from_map(&env(&[
            ("CONSOLE_BOOTSTRAP_USER", "ops"),
            ("CONSOLE_BOOTSTRAP_PASSWORD", "short"),
        ]))
        .is_err());
        let c = Config::from_map(&env(&[
            ("CONSOLE_BOOTSTRAP_USER", "ops"),
            ("CONSOLE_BOOTSTRAP_PASSWORD", "longenough"),
        ]))
        .unwrap();
        assert_eq!(c.bootstrap.unwrap().username, "ops");
    }

    #[test]
    fn password_with_special_chars_is_encoded() {
        let c = Config::from_map(&env(&[("CONSOLE_DB_PASSWORD", "p@ss:w/rd#1")])).unwrap();
        let url = c.db.connect_url();
        assert!(url.contains("p%40ss%3Aw%2Frd%231"), "口令里的 @ : / # 必须转义，否则 URL 被拆坏");
        assert!(!c.db.redacted().contains("p@ss"), "脱敏形态里绝不能出现口令");
    }

    #[test]
    fn numbers_fall_back_and_clamp() {
        let c = Config::from_map(&env(&[
            ("CONSOLE_SESSION_TTL_SEC", "不是数字"),
            ("CONSOLE_DB_CONNLIMIT", "9999"),
        ]))
        .unwrap();
        assert_eq!(c.session_ttl_sec, DEFAULT_SESSION_TTL_SEC, "非法值回落默认");
        assert_eq!(c.db.conn_limit, 100, "越界值夹紧到上限");
    }

    #[test]
    fn invalid_port_is_rejected() {
        assert!(Config::from_map(&env(&[("CONSOLE_PORT", "0")])).is_err());
        assert!(Config::from_map(&env(&[("CONSOLE_PORT", "70000")])).is_err());
    }

    #[test]
    fn bool_env_parsing() {
        assert!(!bool_env(Some("false"), true));
        assert!(!bool_env(Some("OFF"), true));
        assert!(!bool_env(Some("0"), true));
        assert!(bool_env(Some("yes"), false));
        assert!(bool_env(None, true));
    }
}
