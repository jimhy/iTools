//! 取「真实客户端 IP」：决定 `/api/mirrors` 的限流按谁计数。
//!
//! 语义与旧 Node 版所用的 fastify `trustProxy`（proxy-addr）保持一致：
//! - `false`（默认）：只认 TCP 对端地址，`X-Forwarded-For` 一概不看——该头可被任意伪造。
//! - `true`：信任整条链，取 `X-Forwarded-For` **最左**的地址（最初的客户端）。
//! - 数字 `n`：信任最近的 n 跳，从右往左跳过 n 个地址后取那一个。
//! - IP / CIDR 列表：从右往左跳过所有「受信任」的地址，取第一个不受信任的。
//!
//! 置于 Nginx/Caddy 之后而不打开它，所有客户端在服务端看来都来自代理那一个 IP，
//! 会共用同一个限流桶而互相误伤（README 与越限日志都有提示）。

use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;

/// `SYNC_TRUST_PROXY` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustProxy {
    /// 不信任任何代理（默认）。
    No,
    /// 信任整条转发链。
    All,
    /// 信任最近的 n 跳。
    Hops(usize),
    /// 只信任列表内的 IP / CIDR。
    Trusted(Vec<IpNet>),
}

impl TrustProxy {
    /// 解析 `SYNC_TRUST_PROXY`：`true`/`false`（默认 false）、跳数（正整数）、或受信任的 IP/CIDR 列表。
    pub fn parse(v: Option<&str>) -> Self {
        let s = match v {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => return TrustProxy::No,
        };
        let lower = s.to_ascii_lowercase();
        if matches!(lower.as_str(), "false" | "0" | "no" | "off") {
            return TrustProxy::No;
        }
        if matches!(lower.as_str(), "true" | "yes" | "on") {
            return TrustProxy::All;
        }
        if let Ok(n) = s.parse::<usize>() {
            if n > 0 {
                return TrustProxy::Hops(n);
            }
        }
        // IP / CIDR 列表（逗号分隔）。裸 IP 视为 /32（IPv6 为 /128）。
        let nets: Vec<IpNet> = s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                IpNet::from_str(p)
                    .ok()
                    .or_else(|| IpAddr::from_str(p).ok().map(IpNet::from))
            })
            .collect();
        if nets.is_empty() {
            // 写了个既不是布尔也不是跳数、又解析不出任何 IP/CIDR 的值：按最安全的方式处理（不信任）。
            TrustProxy::No
        } else {
            TrustProxy::Trusted(nets)
        }
    }

    fn trusts(&self, addr: Option<IpAddr>, index: usize) -> bool {
        match self {
            TrustProxy::No => false,
            TrustProxy::All => true,
            TrustProxy::Hops(n) => index < *n,
            TrustProxy::Trusted(nets) => match addr {
                Some(ip) => nets.iter().any(|n| n.contains(&ip)),
                None => false,
            },
        }
    }
}

/// 计算限流用的客户端标识。
///
/// `remote` 是 TCP 对端地址，`xff` 是 `X-Forwarded-For` 头的原始值。
/// 返回字符串而非 `IpAddr`：转发链里可能出现无法解析的条目（如 `unknown`），
/// 此时**照样按它分桶**——总比把这些请求全归到某一个桶里互相误伤要好。
pub fn client_ip(trust: &TrustProxy, remote: Option<IpAddr>, xff: Option<&str>) -> String {
    let fallback = || remote.map(|ip| ip.to_string()).unwrap_or_else(|| "unknown".into());
    if matches!(trust, TrustProxy::No) {
        return fallback();
    }
    // 地址链：[对端地址, XFF 从右到左...]，与 proxy-addr 的顺序一致。
    let mut chain: Vec<String> = vec![fallback()];
    if let Some(raw) = xff {
        for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()).rev() {
            chain.push(part.to_string());
        }
    }
    for (i, addr) in chain.iter().enumerate() {
        let parsed = normalize(addr);
        if !trust.trusts(parsed, i) {
            return addr.clone();
        }
    }
    chain.last().cloned().unwrap_or_else(fallback)
}

/// 把地址字符串解析成 `IpAddr`：兼容 `[::1]:1234` / `1.2.3.4:80` 这类带端口的写法。
fn normalize(addr: &str) -> Option<IpAddr> {
    if let Ok(ip) = IpAddr::from_str(addr) {
        return Some(ip);
    }
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return IpAddr::from_str(host).ok();
        }
    }
    if let Some((host, _)) = addr.rsplit_once(':') {
        return IpAddr::from_str(host).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        IpAddr::from_str(s).ok()
    }

    #[test]
    fn parse_covers_all_forms() {
        assert_eq!(TrustProxy::parse(None), TrustProxy::No);
        assert_eq!(TrustProxy::parse(Some("")), TrustProxy::No);
        assert_eq!(TrustProxy::parse(Some("false")), TrustProxy::No);
        assert_eq!(TrustProxy::parse(Some("TRUE")), TrustProxy::All);
        assert_eq!(TrustProxy::parse(Some("2")), TrustProxy::Hops(2));
        assert_eq!(
            TrustProxy::parse(Some("10.0.0.0/8, 127.0.0.1")),
            TrustProxy::Trusted(vec![
                IpNet::from_str("10.0.0.0/8").unwrap(),
                IpNet::from(ip("127.0.0.1").unwrap()),
            ])
        );
        // 完全无法解析的值按「不信任」处理，而不是悄悄当成 true
        assert_eq!(TrustProxy::parse(Some("随便写的")), TrustProxy::No);
    }

    #[test]
    fn no_trust_ignores_forwarded_header() {
        let got = client_ip(&TrustProxy::No, ip("203.0.113.7"), Some("1.1.1.1"));
        assert_eq!(got, "203.0.113.7", "不信任代理时 XFF 必须被无视（可伪造）");
    }

    #[test]
    fn trust_all_takes_leftmost() {
        let got = client_ip(&TrustProxy::All, ip("10.0.0.1"), Some("198.51.100.9, 10.0.0.2"));
        assert_eq!(got, "198.51.100.9");
    }

    #[test]
    fn hops_skips_given_number_of_proxies() {
        // 一跳：跳过对端地址，取 XFF 最右
        let one = client_ip(&TrustProxy::Hops(1), ip("10.0.0.1"), Some("198.51.100.9, 10.0.0.2"));
        assert_eq!(one, "10.0.0.2");
        // 两跳：再跳一个
        let two = client_ip(&TrustProxy::Hops(2), ip("10.0.0.1"), Some("198.51.100.9, 10.0.0.2"));
        assert_eq!(two, "198.51.100.9");
    }

    #[test]
    fn trusted_list_stops_at_first_untrusted() {
        let trust = TrustProxy::parse(Some("10.0.0.0/8"));
        let got = client_ip(&trust, ip("10.0.0.1"), Some("198.51.100.9, 10.0.0.2"));
        assert_eq!(got, "198.51.100.9", "跳过所有内网代理，取第一个外部地址");
        // 链上全是不受信的地址 → 取对端地址，不给伪造头任何机会
        let spoof = client_ip(&trust, ip("203.0.113.7"), Some("1.2.3.4"));
        assert_eq!(spoof, "203.0.113.7");
    }

    #[test]
    fn no_remote_addr_degrades_to_unknown() {
        assert_eq!(client_ip(&TrustProxy::No, None, None), "unknown");
    }

    #[test]
    fn normalize_handles_ports() {
        assert_eq!(normalize("1.2.3.4:80"), ip("1.2.3.4"));
        assert_eq!(normalize("[::1]:8080"), ip("::1"));
        assert_eq!(normalize("::1"), ip("::1"));
        assert_eq!(normalize("unknown"), None);
    }
}
