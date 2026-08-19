//! **统一出站 HTTP 出口**：全应用**每一条**对外请求都必须从这里取 `ureq::Agent`。
//!
//! # 为什么必须有这个文件
//!
//! 「设置 → 网络」里的代理开关与地址在本轮之前是一个**纯摆设**：配置确实写进了 settings，
//! 但全仓没有任何一处读它——11 个 `ureq::get/post` 调用点各自裸起一个默认 Agent，
//! 一个字节都不走代理。用户填了 `127.0.0.1:7897`、开了开关，界面上看着生效，实际全是直连
//! （违反 `doc/开发准则.md` 的「不得有看着能用、点了不生效的控件」）。
//!
//! 根因是**没有唯一出口**：只要还允许调用方直接 `ureq::get(...)`，代理配置就永远可能被绕过。
//! 所以本模块提供 [`get`] / [`post`] / [`request`] 三个与 `ureq::` 同名同形的入口，
//! 调用点只需把 `ureq::get(` 换成 `crate::http::get(`，其余（`.timeout()` / `.set()` / `.call()`）
//! 一字不改——各调用点原有的超时值（更新下载 600s、镜像竞速 5s …）因此**原样保留**，
//! 代理只承载「走哪条链路」，不改变任何超时语义。
//!
//! # 硬约束：本地与内网地址绝不走代理
//!
//! 用户的代理是 `127.0.0.1:7897`，而本地服务端是 `127.0.0.1:8787`。把本机地址也塞进代理，
//! 联调会当场断掉（实测：curl 走系统代理得到 502、ureq 直连得到 404，两者根本不是同一条路）。
//! 因此 [`is_bypass_host`] 是一条**内置、不可配置**的规则（保持简单，也杜绝用户把自己配死）：
//! `localhost`、任意 `*.local` 域名、**整个** `127.0.0.0/8` 回环段、`::1`，以及
//! `10.0.0.0/8` / `172.16.0.0/12` / `192.168.0.0/16` 三段内网地址一律直连；
//! IPv4-mapped IPv6（如 `::ffff:127.0.0.1`）按其折回的 IPv4 判定。
//! ⚠ 这一句是 UI / 文档描述该规则的**唯一准绳**——写窄了（比如只列 `127.0.0.1`）
//! 就成了「界面说的比实际做的少」，同样是不一致。
//!
//! # 两个 Agent，不是每请求一个
//!
//! `ureq` 的代理是 **Agent 级**的（`AgentBuilder::proxy`），不能按请求切换，所以进程内维护
//! 「直连」与「代理」两个 Agent，按目标主机现场选（[`agent_for`]）。这同时白捡了连接池复用
//! ——以前每次 `ureq::get` 都是一个全新 Agent，连接一次都没复用过。
//!
//! # 凭据安全
//!
//! 代理地址可能形如 `user:pass@host:port`。本模块：
//! - [`ProxySpec`] **手写 `Debug`**（等同脱敏后的 [`ProxySpec::display`]），杜绝 `{:?}` 顺手打印出密码；
//! - 一切可能回到 UI / 日志的字符串都过 [`redact_proxy_credentials`]；
//! - ⚠ **永远不要 `{:?}` 打印 `ureq::Agent`**：它的 `Debug` 会把 `Proxy { user, password }` 明文打出来。

use std::io::Read;
use std::net::IpAddr;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use ureq::{Agent, AgentBuilder};

use crate::logging::ilog;

/// 「测试」按钮的整体超时（秒）。短一些：这是个用户盯着看结果的按钮，
/// 代理不通时最多让他等 8 秒，而不是跟着下载超时一起等 600 秒。
const TEST_TIMEOUT_SECS: u64 = 8;
/// 测试探测的响应读取上限（字节）。目标只有 13 字节，留足余量即可。
const TEST_MAX_BYTES: u64 = 4 * 1024;

// ==================== 代理地址规范化 ====================

/// 规范化后的代理配置。**不派生 `Debug`**（手写实现，见下），避免密码被顺手打进日志。
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySpec {
    /// 归一化后的协议，取值只有四种：`http` / `socks5` / `socks4` / `socks4a`
    /// （`https://` → `http`，`socks://` 与 `socks5h://` → `socks5`，见 [`normalize_proxy`]）。
    /// **归一后的串必须是 ureq 认得的**，否则等于放进来一个连接期才炸的配置。
    scheme: &'static str,
    /// `user:pass`（原样保留，含明文密码）。None = 无鉴权。
    credentials: Option<String>,
    /// 主机名或 IPv4 字面量（小写）。
    host: String,
    /// 端口（必填，1~65535）。
    port: u16,
}

impl ProxySpec {
    /// 交给 `ureq::Proxy::new` 的串。**含明文凭据 —— 禁止进日志 / UI / 错误信息。**
    fn ureq_url(&self) -> String {
        match &self.credentials {
            Some(c) => format!("{}://{}@{}:{}", self.scheme, c, self.host, self.port),
            None => format!("{}://{}:{}", self.scheme, self.host, self.port),
        }
    }

    /// **脱敏**展示串（凭据一律 `***`），可以安全地进日志、错误信息与 UI。
    pub fn display(&self) -> String {
        let creds = if self.credentials.is_some() { "***@" } else { "" };
        format!("{}://{creds}{}:{}", self.scheme, self.host, self.port)
    }

    /// 是否走 SOCKS。用来把失败原因说到点子上：SOCKS 与 HTTP 代理的失败形态完全不同
    /// （SOCKS 没有 CONNECT 隧道、没有 407，一切失败都从 ureq 的 `ConnectionFailed` 出来）。
    fn is_socks(&self) -> bool {
        self.scheme.starts_with("socks")
    }
}

/// 与 [`ProxySpec::display`] 完全一致的脱敏输出：即便有人写了 `{:?}` 也漏不出密码。
impl std::fmt::Debug for ProxySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

/// 解析并**严格校验**用户填写的代理地址。
///
/// 接受形态（大小写不敏感，允许尾随 `/`）：
/// - `127.0.0.1:7897`（无 scheme 视为 `http://`）
/// - `http://127.0.0.1:7897`、`https://proxy.corp:8443`
/// - `socks5://127.0.0.1:7897`、`socks5h://`、`socks://`、`socks4://`、`socks4a://`（见下）
/// - `user:pass@127.0.0.1:7897`（凭据按**最后一个** `@` 切分，与 ureq 口径一致，密码里可含 `@`）
///
/// 四处刻意比 ureq 更严（都是为了不做出「填得进去、永远连不上」的假配置）：
/// 1. **端口必填且必须合法**。`ureq::Proxy::new` 对 `127.0.0.1:abc` 是
///    `port.parse().ok()` → `None` → 静默回落到 80/1080，用户填错端口却毫无察觉地连了别的端口
///    ——这正是「看着能用、点了不生效」的典型形态，必须在入口拦下并给出中文原因。
/// 2. **拒绝 IPv6 字面量**。ureq 2 的代理解析按 `split(':')` 取首段，`[::1]:7897` 会被解析成
///    主机 `[`，等于配了个永远连不上的代理。与其让它静默坏掉，不如诚实拒绝。
/// 3. **拒绝给 `socks4://` / `socks4a://` 配用户名密码**，理由见下。
/// 4. **归一 `socks5h://`**：ureq 不认这个字面量，见下。
///
/// 关于 `https://`：ureq 2 不支持「与代理之间再套一层 TLS」，而本机代理工具（Clash / v2rayN 等）
/// 的所谓 https 端口本身就是明文的 HTTP CONNECT 代理。所以 `https://` 一律按 `http://` 处理
/// ——这与这些工具的真实行为一致；真正需要 TLS-to-proxy 的企业代理场景本模块不支持。
///
/// # SOCKS 支持形态（以 ureq 2.12.1 的**实际实现**为准，不是照着协议想当然）
///
/// 依据：`ureq::Proxy::new`（`src/proxy.rs`，认哪些 scheme）与 `stream.rs::connect_socks`
/// （域名在哪端解析、鉴权走不走）。`Cargo.toml` 已开 `socks-proxy` feature。
///
/// | 用户填 | 归一为 | ureq 实际行为 |
/// |---|---|---|
/// | `socks5://` | `socks5` | SOCKS5；**域名交给代理解析**；支持 `user:pass` 鉴权 |
/// | `socks://` | `socks5` | ureq 把 `socks` 当 SOCKS5 的别名，同上 |
/// | `socks5h://` | `socks5` | ureq **不认**这个字面量（`InvalidProxyUrl`），但它的 socks5 本来就是远端解析域名（`connect_socks` 对非 IP 主机传 `TargetAddr::Domain`），语义正是 socks5h ⇒ 归一而不是拒绝 |
/// | `socks4://` | `socks4` | SOCKS4；**域名在本地解析**后只发 IP；**无鉴权** |
/// | `socks4a://` | `socks4a` | SOCKS4A；域名交给代理解析；**无鉴权** |
///
/// **SOCKS4 / 4A 带凭据一律拒绝**：ureq 的 `get_socks4_stream` 把 userid 写死成空串，
/// 填了的用户名密码会被**静默丢掉**——那就是个「填得进去、根本没送出去」的假配置，
/// 不如当场告诉用户改用 `socks5://`。
///
/// ⚠ 曾经这里是「拒绝一切 `socks*://`」，理由写着「加 `socks-proxy` 会让二进制启动即崩」。
/// **那是误判**（2026-08-12 复盘）：崩的是 lib 单测二进制，真实原因是它拿不到应用清单，
/// 与 socks 无关，已在 `build.rs::emit_app_manifest` 持久修复。细节见 `Cargo.toml` 里 ureq 那段。
///
/// 错误信息里**绝不回显**用户输入的原串或端口串（`user:pass` 这类内容可能藏在里面）。
pub fn normalize_proxy(raw: &str) -> Result<ProxySpec, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("代理地址为空：请填写形如 127.0.0.1:7897 的地址".to_string());
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("代理地址不能包含空格或控制字符".to_string());
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => {
            let scheme = match s.to_ascii_lowercase().as_str() {
                "http" => "http",
                // 见函数文档：与代理之间不做 TLS，https:// 按 HTTP CONNECT 代理处理
                "https" => "http",
                // 见函数文档「SOCKS 支持形态」：`socks` 是 ureq 里 SOCKS5 的别名；
                // `socks5h` ureq 不认字面量，但其 socks5 本来就是远端解析域名，语义等同 ⇒ 归一
                "socks" | "socks5" | "socks5h" => "socks5",
                "socks4" => "socks4",
                "socks4a" => "socks4a",
                _ => {
                    return Err(
                        "不支持的代理协议：只支持 http:// https:// socks5:// socks5h:// \
                         socks4:// socks4a://"
                            .to_string(),
                    )
                }
            };
            (scheme, r)
        }
        None => ("http", trimmed),
    };
    if rest.is_empty() {
        return Err("代理地址缺少主机与端口（形如 127.0.0.1:7897）".to_string());
    }
    if rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err("代理地址不应包含路径或参数：只填 主机:端口（如 127.0.0.1:7897）".to_string());
    }

    // 凭据：按**最后一个** `@` 切分（密码里可能含 `@`，ureq 也是 rsplitn(2,'@')）
    let (credentials, hostport) = match rest.rsplit_once('@') {
        Some((creds, hp)) => {
            if !creds.contains(':') {
                return Err("代理凭据格式应为 用户名:密码（如 user:pass@127.0.0.1:7897）".to_string());
            }
            (Some(creds.to_string()), hp)
        }
        None => (None, rest),
    };
    // SOCKS4 / 4A 协议本身没有「用户名 + 密码」鉴权，ureq 的 `get_socks4_stream` 也把 userid
    // 写死成空串——填了会被**静默丢掉**。与其做一个「填得进去、根本没送出去」的配置，
    // 不如当场说清怎么改。（错误串不回显任何输入，凭据不会顺着漏出去。）
    if credentials.is_some() && (scheme == "socks4" || scheme == "socks4a") {
        return Err(
            "SOCKS4 / SOCKS4A 不支持用户名密码鉴权（协议本身没有这个字段，填了也送不出去）：\
             需要鉴权请把地址改成 socks5://主机:端口（端口不用变）"
                .to_string(),
        );
    }

    if hostport.starts_with('[') || hostport.matches(':').count() > 1 {
        return Err(
            "暂不支持 IPv6 字面量代理地址（ureq 2 的代理解析不认这种写法），请改用域名或 IPv4 地址"
                .to_string(),
        );
    }
    let (host, port_raw) = hostport
        .rsplit_once(':')
        .ok_or_else(|| "代理地址缺少端口：请写成 主机:端口（如 127.0.0.1:7897）".to_string())?;
    if host.is_empty() {
        return Err("代理地址缺少主机名（如 127.0.0.1:7897）".to_string());
    }
    // 端口串**不回显**：pathological 输入（如把 `user:pass` 当地址填）会让密码顺着错误串漏出去
    let port: u16 = port_raw
        .parse()
        .map_err(|_| "代理端口不合法：应为 1~65535 的数字（如 127.0.0.1:7897）".to_string())?;
    if port == 0 {
        return Err("代理端口不合法：应为 1~65535 的数字（如 127.0.0.1:7897）".to_string());
    }

    Ok(ProxySpec {
        scheme,
        credentials,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// 把任意字符串里的 `userinfo@` 抹成 `***@`（**凭据不外泄的最后一道闸**）。
///
/// 与 `install.rs::redact_token`（专抹 `access_token=`）互补：那条管的是拼在 URL 上的令牌，
/// 这条管的是代理地址里的 `user:pass@`。
///
/// 规则：按空白切段，每段里取「`://` 之后到第一个 `/` 之前」为 authority（没有 `://` 就从段首起），
/// 若 authority 内有 `@`，把**最后一个** `@` 之前的全部内容替换为 `***`。
/// 取最后一个而不是第一个，是因为密码本身可以含 `@`（`p@ssw0rd`）——按第一个切会把密码剩一半留在明文里。
/// 宁可多抹（正文里的邮箱地址也会被抹）也绝不少抹。
pub fn redact_proxy_credentials(s: &str) -> String {
    // 只在 ASCII 分隔符（`://` `/` `@` 空白）处切分，永远落在 UTF-8 字符边界上
    s.split_inclusive(char::is_whitespace)
        .map(|chunk| {
            let start = chunk.find("://").map(|i| i + 3).unwrap_or(0);
            let body = &chunk[start..];
            let authority_end = body.find('/').unwrap_or(body.len());
            match body[..authority_end].rfind('@') {
                Some(at) => format!("{}***{}", &chunk[..start], &body[at..]),
                None => chunk.to_string(),
            }
        })
        .collect()
}

// ==================== 绕过规则（内置，不暴露给用户配置） ====================

/// 该主机是否**绕过代理直连**。
///
/// 命中任一即直连（全部小写比较，忽略 FQDN 尾点）：
/// - `localhost`
/// - `127.0.0.0/8`（`Ipv4Addr::is_loopback`）
/// - `::1`（`Ipv6Addr::is_loopback`）
/// - `10.0.0.0/8`、`172.16.0.0/12`、`192.168.0.0/16`（三段合起来正是 `Ipv4Addr::is_private`）
/// - `*.local`（mDNS 域）
///
/// 另外把 IPv4-mapped IPv6（`::ffff:127.0.0.1`）折回 IPv4 再判——同一台机器换个写法就绕过失效，
/// 那是实打实的坑。
///
/// **刻意不做成可配置项**：这条规则存在的唯一目的是保证「本机 / 内网服务永远连得上」，
/// 给用户开放配置只会多出一种把自己配死的方式（本地服务端被塞进代理 → 联调全断）。
pub fn is_bypass_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if h.is_empty() {
        return false;
    }
    if h == "localhost" || h.ends_with(".local") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private(),
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.is_loopback() || v4.is_private(),
            None => v6.is_loopback(),
        },
        Err(_) => false,
    }
}

/// 从 URL 取**真实主机名**（小写；去 scheme、去 `userinfo@`、去端口、去 IPv6 方括号）。
///
/// `userinfo@host` 取 `@` **之后**那段：与浏览器 / curl 口径一致，
/// 否则 `https://127.0.0.1@evil.tld/` 这种地址会被误判成本机而绕过代理。
pub fn host_of(url: &str) -> Option<String> {
    let s = url.trim();
    let rest = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// 该 URL 是否走直连（命中绕过规则，或主机名压根解析不出来）。
///
/// 解析不出主机名时**保守直连**：那种 URL 本就发不出去，没必要再引一次代理连接。
fn bypassed(url: &str) -> bool {
    host_of(url).map(|h| is_bypass_host(&h)).unwrap_or(true)
}

// ==================== Agent 缓存 ====================

/// 直连 Agent（进程内唯一，连接池随之复用）。
///
/// `try_proxy_from_env(false)` 是**显式**写的：ureq 的这个开关默认值随 `proxy-from-env`
/// feature 变化，将来任何人（或某个依赖）打开它，"直连" Agent 就会偷偷跟随 `HTTP_PROXY`
/// 把本机地址也塞进代理——正是本模块要根除的故障。写死 false，与 feature 无关。
fn direct_agent() -> &'static Agent {
    static A: OnceLock<Agent> = OnceLock::new();
    A.get_or_init(|| AgentBuilder::new().try_proxy_from_env(false).build())
}

/// 当前代理出口。`None` = 未开启 / 未配置 / 配置无效（此时一切请求走直连）。
fn proxy_slot() -> &'static RwLock<Option<(Agent, ProxySpec)>> {
    static P: OnceLock<RwLock<Option<(Agent, ProxySpec)>>> = OnceLock::new();
    P.get_or_init(|| RwLock::new(None))
}

/// 取当前代理 Agent 的克隆（`Agent` 内部是 `Arc`，克隆是廉价的）。
///
/// 锁中毒（持锁线程 panic）时取内层值继续用：代理位只是两个不可变值，读到它不会更糟；
/// 反过来若在这里返回 None，一次无关的 panic 就会让全应用悄悄退回直连。
fn proxy_agent() -> Option<Agent> {
    let guard = proxy_slot().read().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|(a, _)| a.clone())
}

/// 当前是否**已经**有一个可用的代理出口。
pub fn proxy_configured() -> bool {
    proxy_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

fn build_agent(spec: &ProxySpec) -> Result<Agent, String> {
    // ureq_url() 含明文凭据，**只**喂给 ureq，绝不进错误串
    let proxy = ureq::Proxy::new(spec.ureq_url()).map_err(|e| {
        redact_proxy_credentials(&format!("代理地址 {} 无法被解析：{e}", spec.display()))
    })?;
    Ok(AgentBuilder::new().proxy(proxy).build())
}

/// 按当前设置重建代理出口（**热生效**，无需重启）。
///
/// 由启动装载设置时与每次 `save_settings` 后调用（与 `account::set_user_endpoint` 同一位置）。
/// 返回后运行期状态**一定**等于本次入参：地址非法时代理位被清空（fail-closed 到直连）并返回 Err，
/// 绝不留着上一份配置继续用（那会让「我明明改了地址」变成又一个说不清的幽灵）。
///
/// **配置没变就什么都不做**（连 Agent 都不重建）：`save_settings` 是整包保存，
/// 拖一次透明度滑块就是一串 300ms 防抖的连发保存，每次都重建 Agent 会把本模块特意引入的
/// 连接池复用一次次丢掉（旧 Agent 一被替换，它池子里的 keep-alive 连接就再也用不上了）。
pub fn refresh(enabled: bool, address: &str) -> Result<(), String> {
    refresh_inner(enabled, address).map(|_| ())
}

/// [`refresh`] 的实现，返回**本次是否真的换掉了代理出口**（供测试证明「配置没变不重建」，
/// 而不是靠读代码想当然）。
fn refresh_inner(enabled: bool, address: &str) -> Result<bool, String> {
    if !enabled || address.trim().is_empty() {
        let had = set_proxy(None);
        if had {
            ilog!("[iTools] 出站代理已关闭：全部请求直连");
        }
        return Ok(had);
    }
    let spec = match normalize_proxy(address) {
        Ok(s) => s,
        Err(e) => {
            set_proxy(None);
            return Err(e);
        }
    };
    // 与当前生效的配置逐字段相同 ⇒ 运行期状态已经等于入参，直接返回（保持 Agent 与其连接池）。
    // 比的是 ProxySpec 本身而**不是** display()：后者把凭据抹成 `***`，
    // 只改了密码会被误判成「没变」，于是新密码永远不生效——那又是一个「存了不生效」的控件。
    if proxy_spec_is(&spec) {
        return Ok(false);
    }
    let agent = match build_agent(&spec) {
        Ok(a) => a,
        Err(e) => {
            set_proxy(None);
            return Err(e);
        }
    };
    let label = spec.display();
    set_proxy(Some((agent, spec)));
    // 走到这里必定是**真的变了**（没变的已在上面返回），所以日志不会被重复保存刷屏
    ilog!("[iTools] 出站代理已启用：{label}（本机 / 内网地址仍直连）");
    Ok(true)
}

/// 当前代理位是否**恰好**就是这份配置（含凭据）。
fn proxy_spec_is(want: &ProxySpec) -> bool {
    let guard = proxy_slot().read().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().is_some_and(|(_, s)| s == want)
}

/// 写入代理位，返回**写入前**是否有代理（仅用于决定要不要打日志）。
///
/// 锁中毒时照样写：否则一次无关的 panic 会让代理设置从此再也改不动。
fn set_proxy(next: Option<(Agent, ProxySpec)>) -> bool {
    let mut guard = proxy_slot().write().unwrap_or_else(|e| e.into_inner());
    let had = guard.is_some();
    *guard = next;
    had
}

// ==================== 出站入口（调用方只用这三个） ====================

/// 选出该用哪个 Agent：命中绕过规则 → 直连；否则有代理就走代理。
///
/// 刻意**由 [`uses_proxy`] 驱动**：路由结论只有一处实现，
/// 「UI 说走了代理、实际却直连」这种自相矛盾在结构上就不可能发生。
pub fn agent_for(url: &str) -> Agent {
    if uses_proxy(url) {
        if let Some(agent) = proxy_agent() {
            return agent;
        }
    }
    direct_agent().clone()
}

/// 这条 URL 本次**是否真的**经由代理（未配置代理 / 命中绕过规则都是 false）。
/// 用于如实向 UI 汇报「测的确实是代理这条路」。
pub fn uses_proxy(url: &str) -> bool {
    !bypassed(url) && proxy_configured()
}

/// `ureq::get` 的替代品（唯一区别：Agent 由 [`agent_for`] 选）。
pub fn get(url: &str) -> ureq::Request {
    agent_for(url).get(url)
}

/// `ureq::post` 的替代品。
pub fn post(url: &str) -> ureq::Request {
    agent_for(url).post(url)
}

/// `ureq::request` 的替代品（任意方法）。
pub fn request(method: &str, url: &str) -> ureq::Request {
    agent_for(url).request(method, url)
}

// ==================== 命令：测试代理 ====================

/// 「测试」按钮的返回。**全部是本次真实请求测出来的事实**，没有任何一项是推断或写死的。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResult {
    /// 是否连通（经代理成功取回了探测内容）。
    pub ok: bool,
    /// 端到端耗时（毫秒，含建隧道 + TLS + 取回内容）。失败为 None。
    pub latency_ms: Option<u64>,
    /// 失败的真实原因（连不上代理 / 代理拒绝 / 需要鉴权 / 目标不可达 …，可区分）。成功为 None。
    pub error: Option<String>,
    /// 本次实际测试的目标 URL（UI 直接展示，让用户知道测的是什么）。
    pub target: String,
    /// 本次是否**真的**经由代理（证明测的是代理这条路，而不是悄悄直连测了个寂寞）。
    pub via_proxy: bool,
}

/// 命令：用**传入的**地址真实测一次代理。
///
/// 注意用的是**用户当前正在输入的值**（可能还没保存），所以这里临时构造 Agent，
/// 不复用已保存配置——否则「改了地址点测试」测的还是旧地址，又是一个骗人的按钮。
/// 本命令也**不会**改动运行期代理配置（测试就是测试，不产生副作用）。
///
/// `async fn` + `spawn_blocking`：同步命令的函数体会被内联进 IPC handler，在 Windows 上就是
/// 主 UI 线程（详见 `updater.rs` 头部「线程模型」），一次 8 秒的探测会把界面整个卡住。
#[tauri::command]
pub async fn test_proxy(address: String) -> Result<ProxyTestResult, String> {
    tauri::async_runtime::spawn_blocking(move || test_proxy_blocking(&address))
        .await
        .map_err(|e| format!("代理测试任务异常终止: {e}"))?
}

/// 探测目标：GitHub 官方 raw 上那个 13 字节的 `octocat/Hello-World@7fd1a60…/README`。
///
/// 选它的理由（不是随手挑的）：
/// 1. **极小**：13 字节，测出来的耗时基本等于链路 RTT，不会把带宽算进延迟；
/// 2. **稳定**：GitHub 官方示例仓库，且 ref pin 到固定 commit，内容永不变、长期存在；
/// 3. **匿名无限流**：raw 直链不像 `api.github.com`（匿名 60 次/小时）那样点几下就 403
///    ——一个会被自己限流的「测试」按钮比没有还糟；
/// 4. **测的是真实用途**：https 目标 ⇒ 走 CONNECT 隧道，正是「代理能不能带我们访问 GitHub」这条路；
/// 5. **不新增外部依赖**：与镜像测速用的是同一个探针坐标（`mirror::default_probe` 唯一出处）。
fn probe_target() -> String {
    crate::plugin::mirror::probe_raw_url()
}

fn test_proxy_blocking(address: &str) -> Result<ProxyTestResult, String> {
    // 地址不合法不是「测试失败」而是「没法测」，走 Err 让 UI 直接显示怎么改
    let spec = normalize_proxy(address)?;
    let agent = build_agent(&spec)?;
    let target = probe_target();
    // 目标是公网主机、必然不命中绕过规则；仍然现场判一次，保证 viaProxy 是**事实**而非假设
    let via_proxy = !bypassed(&target);
    let fail = |error: String| ProxyTestResult {
        ok: false,
        latency_ms: None,
        error: Some(error),
        target: target.clone(),
        via_proxy,
    };

    let start = Instant::now();
    let resp = match agent
        .get(&target)
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECS))
        .call()
    {
        Ok(r) => r,
        Err(e) => return Ok(fail(explain_proxy_error(&e, &spec))),
    };
    let mut buf = Vec::new();
    if let Err(e) = resp.into_reader().take(TEST_MAX_BYTES).read_to_end(&mut buf) {
        return Ok(fail(redact_proxy_credentials(&format!(
            "代理已连通，但读取响应失败：{e}"
        ))));
    }
    if buf.is_empty() {
        return Ok(fail("代理已连通，但取回的内容为空（可能被中间设备拦截或改写）".to_string()));
    }
    Ok(ProxyTestResult {
        ok: true,
        latency_ms: Some(start.elapsed().as_millis() as u64),
        error: None,
        target,
        via_proxy,
    })
}

/// 把一次代理探测失败翻译成**可自查、可区分**的中文原因。
///
/// 要点是让用户能分清「代理本身连不上」「代理拒绝了我们」「代理通了但目标不可达」——
/// 这三者的修法完全不同，笼统一句「测试失败」等于什么都没说。
fn explain_proxy_error(e: &ureq::Error, spec: &ProxySpec) -> String {
    let label = spec.display(); // 已脱敏
    let msg = match e {
        ureq::Error::Status(407, _) => {
            "代理要求鉴权（HTTP 407）：请在地址里带上凭据，形如 user:pass@127.0.0.1:7897".to_string()
        }
        ureq::Error::Status(403, _) => {
            "HTTP 403：代理或目标拒绝了本次请求（代理策略拦截 / 目标限流）".to_string()
        }
        ureq::Error::Status(code, _) => {
            format!("目标返回 HTTP {code}（代理链路本身是通的，是目标侧的问题）")
        }
        ureq::Error::Transport(t) => {
            let detail = transport_detail(e, t);
            let timed_out = detail.contains("timed out")
                || detail.contains("timeout")
                || detail.contains("os error 10060");
            let head = match t.kind() {
                // 只有 HTTP 代理会走 CONNECT 隧道，SOCKS 永远到不了这个分支
                ureq::ErrorKind::ProxyConnect => format!(
                    "代理 {label} 拒绝建立隧道（CONNECT 失败：该端口很可能不是 HTTP 代理。\
                     若它其实是 SOCKS 端口，把地址改成 socks5://主机:端口 即可，端口不用变）"
                ),
                ureq::ErrorKind::ProxyUnauthorized => {
                    format!("代理 {label} 拒绝：需要用户名 / 密码（407）")
                }
                ureq::ErrorKind::Dns => {
                    format!("代理主机名无法解析（{label}）：地址可能写错")
                }
                ureq::ErrorKind::ConnectionFailed if timed_out => {
                    format!("连接代理 {label} 超时（{TEST_TIMEOUT_SECS} 秒内没建立连接）")
                }
                // SOCKS 没有 CONNECT / 407 这些可区分的信号：握手失败（端口不收 SOCKS、
                // 版本不对、用户名密码不对）在 ureq 里全部归到 ConnectionFailed，只能一并列出
                ureq::ErrorKind::ConnectionFailed if spec.is_socks() => format!(
                    "连不上 SOCKS 代理 {label}（代理未启动 / 端口写错 / 该端口其实不收 SOCKS / \
                     用户名密码不对——SOCKS 的握手失败都落在这一步）"
                ),
                ureq::ErrorKind::ConnectionFailed => {
                    format!("连不上代理 {label}（代理未启动 / 端口写错 / 被防火墙拦截）")
                }
                ureq::ErrorKind::Io if timed_out => {
                    "经代理访问目标超时（代理已连上，但目标无响应）".to_string()
                }
                ureq::ErrorKind::Io => "经代理读写失败（代理已连上，链路中途断了）".to_string(),
                ureq::ErrorKind::InvalidUrl | ureq::ErrorKind::UnknownScheme => {
                    "测试目标地址不合法（这是客户端缺陷，请反馈）".to_string()
                }
                _ => format!("经代理 {label} 请求失败"),
            };
            if detail.is_empty() {
                head
            } else {
                format!("{head}：{detail}")
            }
        }
    };
    // 出口再兜一次底：ureq 的错误详情里不该出现代理凭据，但这是最后一道闸，宁可多抹
    redact_proxy_credentials(&msg)
}

/// 拼出一次传输失败的**可定位**细节。
///
/// `Transport::message()` 常常只有一句笼统的 `"Connect error"`，真正有用的
/// 「os error 10061 / timed out / SOCKS proxy: … timed out connecting」是底层 io 错误，
/// 挂在 `std::error::Error::source` 上（`ureq::Transport` 的 `Display` 会带上它，但
/// `message()` 不会）。SOCKS 路径尤其明显：超时 / 拒绝 / 握手失败在 ureq 里全部归到
/// `ConnectionFailed`，不取 source 就等于什么都没告诉用户。
fn transport_detail(e: &ureq::Error, t: &ureq::Transport) -> String {
    use std::error::Error as _;
    let head = t.message().unwrap_or_default();
    match e.source() {
        Some(src) => {
            let src = src.to_string();
            if head.is_empty() {
                src
            } else {
                format!("{head}：{src}")
            }
        }
        None => head.to_string(),
    }
}

/// 从服务端的错误响应体里取出那句给用户看的话（`{"error": "..."}`）。
///
/// # 为什么需要它
///
/// 服务端对 4xx 会给一句明确的中文原因，比如账号被停用时是
/// 「该账号已被停用：<运营填的原因>」。客户端如果按状态码折叠成自己写死的文案
/// （历史实现把 401 和 403 一起映射成「用户名或密码错误」），被停用的用户就会
/// 以为是自己记错了密码，反复重试还不知道为什么进不去——这正是
/// `doc/开发准则.md` 里点名的「二次误导」。
///
/// 取不到就返回 `None`，由调用方回落到按状态码的固定文案：服务端可能返回
/// 非 JSON（被中间网关截胡）或压根没有 `error` 字段，那种情况下猜不如不猜。
///
/// 长度上限是防御性的：这段文字会直接进 toast / 输入框下方，
/// 服务端若返回一大段东西会把界面撑坏。超长即视为不可用。
pub(crate) fn server_error_message(resp: ureq::Response) -> Option<String> {
    const MAX_CHARS: usize = 200;
    let body = resp.into_string().ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let msg = v.get("error")?.as_str()?.trim();
    if msg.is_empty() || msg.chars().count() > MAX_CHARS {
        return None;
    }
    Some(msg.to_string())
}

/// 把 `ureq` 错误拆成「HTTP 状态错误」与「传输错误」两类。
///
/// `ureq::Error::Status` 持有 `Response`，而读 body 会消费掉整个错误值。
/// 调用方通常还要用到状态码，所以这里返回 `(code, 可选的服务端文案)`。
///
/// 传输错误直接返回**已格式化且已脱敏**的字符串：它最终会进 toast 与日志，
/// 而错误文本里可能带代理地址（`http://user:pass@host`）。让每个调用点各自记得
/// 脱敏是靠不住的——只要有一处忘了，凭据就进日志了。
pub(crate) fn split_status_error(e: ureq::Error) -> Result<(u16, Option<String>), String> {
    match e {
        ureq::Error::Status(code, resp) => Ok((code, server_error_message(resp))),
        ureq::Error::Transport(t) => Err(redact_proxy_credentials(&t.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 地址规范化 ----------

    #[test]
    fn normalize_accepts_common_forms() {
        // 无 scheme → http
        let p = normalize_proxy("127.0.0.1:7897").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 7897);
        assert_eq!(p.display(), "http://127.0.0.1:7897");
        assert_eq!(p.ureq_url(), "http://127.0.0.1:7897");

        // 显式 http / 大小写 / 首尾空白 / 尾随斜杠
        assert_eq!(
            normalize_proxy("  HTTP://Proxy.Corp:8080/  ").unwrap().display(),
            "http://proxy.corp:8080"
        );

        // https 归一为 http（ureq 不支持 TLS-to-proxy，见函数文档）
        assert_eq!(
            normalize_proxy("https://127.0.0.1:7897").unwrap().display(),
            "http://127.0.0.1:7897"
        );

        // 凭据：按最后一个 @ 切，密码里可含 @
        let p = normalize_proxy("http://user:p@ssw0rd@127.0.0.1:7897").unwrap();
        assert_eq!(p.credentials.as_deref(), Some("user:p@ssw0rd"));
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.ureq_url(), "http://user:p@ssw0rd@127.0.0.1:7897");

        // 无 scheme + 凭据
        let p = normalize_proxy("user:pass@proxy.corp:8080").unwrap();
        assert_eq!(p.scheme, "http");
        assert_eq!(p.credentials.as_deref(), Some("user:pass"));
    }

    #[test]
    fn normalize_rejects_bad_input() {
        let cases: &[(&str, &str)] = &[
            ("", "空串"),
            ("   ", "全空白"),
            ("127.0.0.1", "缺端口"),
            ("http://127.0.0.1", "缺端口"),
            ("127.0.0.1:", "端口空"),
            ("127.0.0.1:abc", "端口非数字"),
            ("127.0.0.1:0", "端口 0"),
            ("127.0.0.1:70000", "端口越界"),
            ("127.0.0.1:-1", "负端口"),
            ("ftp://127.0.0.1:21", "不支持的协议"),
            ("socks6://127.0.0.1:7897", "不存在的 socks 版本"),
            ("socks4://user:pass@127.0.0.1:7897", "SOCKS4 不支持鉴权"),
            ("socks4a://user:pass@127.0.0.1:7897", "SOCKS4A 不支持鉴权"),
            ("socks5://127.0.0.1", "SOCKS 也必须填端口"),
            ("://127.0.0.1:7897", "空协议"),
            (":7897", "缺主机"),
            ("127.0.0.1 :7897", "含空格"),
            ("[::1]:7897", "IPv6 字面量"),
            ("::1:7897", "IPv6 字面量（无方括号）"),
            ("user@127.0.0.1:7897", "凭据缺密码"),
            ("127.0.0.1:7897/path", "带路径"),
            ("127.0.0.1:7897?a=1", "带参数"),
        ];
        for (input, why) in cases {
            let err = normalize_proxy(input).unwrap_err();
            assert!(!err.is_empty(), "{why} 应给出可读错误");
            assert!(
                err.contains("代理") || err.contains("协议"),
                "{why} 的错误应是中文可读文案，实际: {err}"
            );
        }
    }

    /// 端口非法 / 缺失时**绝不**沿用 ureq 的静默回落（80/1080）——那正是「填错却毫无察觉」的坑。
    #[test]
    fn normalize_never_silently_defaults_port() {
        assert!(normalize_proxy("127.0.0.1:abc").is_err(), "非法端口必须拦下");
        assert!(normalize_proxy("http://127.0.0.1").is_err(), "缺端口必须拦下");
        // 对照组（本函数存在的理由）：同样的输入 ureq 自己是**照单全收**的，
        // 它会把 `abc` 解析失败后静默回落到 80 端口，用户填错却毫无察觉。
        assert!(
            ureq::Proxy::new("127.0.0.1:abc").is_ok(),
            "若哪天 ureq 自己开始校验端口了，本模块的前置校验可以简化"
        );
        assert!(ureq::Proxy::new("http://127.0.0.1").is_ok());
    }

    /// SOCKS 的支持形态。**每一条都必须与 ureq 的实际实现对得上**——本模块的存在价值就是
    /// 「入口就说清楚」，若归一出一个 ureq 不认的串，就成了连接期才炸的假配置。
    ///
    /// 曾经这里断言的是「socks 一律被拒」，依据是「加 `socks-proxy` 会让二进制启动即崩」；
    /// 那是误判（真因是 lib 单测二进制缺应用清单，见 `build.rs::emit_app_manifest`），
    /// 本用例即为改正后的口径。
    #[test]
    fn socks_forms_are_accepted() {
        // socks5 / socks（ureq 里的别名）/ socks5h（ureq 不认字面量，语义等同 ⇒ 归一）
        let p = normalize_proxy("socks5://127.0.0.1:7897").unwrap();
        assert_eq!(p.scheme, "socks5");
        assert_eq!(p.display(), "socks5://127.0.0.1:7897");
        assert!(p.is_socks());
        for alias in ["socks://127.0.0.1:7897", "SOCKS5H://127.0.0.1:7897"] {
            assert_eq!(
                normalize_proxy(alias).unwrap().display(),
                "socks5://127.0.0.1:7897",
                "{alias} 应归一成唯一的 socks5 写法"
            );
        }
        // socks4 / socks4a 保持原样（两者在 ureq 里域名解析端不同，不能混为一谈）
        assert_eq!(normalize_proxy("socks4://127.0.0.1:7897").unwrap().scheme, "socks4");
        assert_eq!(normalize_proxy("socks4a://127.0.0.1:7897").unwrap().scheme, "socks4a");
        assert!(!normalize_proxy("127.0.0.1:7897").unwrap().is_socks());

        // 带鉴权的 socks5：凭据原样交给 ureq，但一切对外展示 / Debug 一律脱敏
        let p = normalize_proxy("socks5://user:p@ssw0rd@127.0.0.1:7897").unwrap();
        assert_eq!(p.credentials.as_deref(), Some("user:p@ssw0rd"));
        assert_eq!(p.ureq_url(), "socks5://user:p@ssw0rd@127.0.0.1:7897");
        assert_eq!(p.display(), "socks5://***@127.0.0.1:7897");
        assert!(!format!("{p:?}").contains("ssw0rd"), "Debug 泄露了密码");

        // 归一后的串 ureq 必须**认得**，否则等于放进来一个连接期才炸的配置
        for addr in [
            "socks5://127.0.0.1:7897",
            "socks5h://127.0.0.1:7897",
            "socks://127.0.0.1:7897",
            "socks4://127.0.0.1:7897",
            "socks4a://127.0.0.1:7897",
        ] {
            let spec = normalize_proxy(addr).unwrap();
            assert!(
                ureq::Proxy::new(spec.ureq_url()).is_ok(),
                "{addr} 归一成 {} 后 ureq 仍不认",
                spec.display()
            );
        }
        // 对照组（本模块为什么要归一 socks5h）：ureq 自己**不认**这个字面量
        assert!(
            ureq::Proxy::new("socks5h://127.0.0.1:7897").is_err(),
            "若哪天 ureq 自己认了 socks5h，本模块的归一可以去掉"
        );

        // SOCKS4 / 4A 没有鉴权字段（ureq 的 userid 写死空串），填了会被静默丢掉 ⇒ 入口拒绝
        for addr in [
            "socks4://user:secret@127.0.0.1:7897",
            "socks4a://user:secret@127.0.0.1:7897",
        ] {
            let err = normalize_proxy(addr).unwrap_err();
            assert!(err.contains("socks5://"), "{addr} 的错误要给出可照做的改法：{err}");
            assert!(!err.contains("secret"), "错误信息泄露了密码：{err}");
        }
    }

    /// 起一个**只服务一条连接**的极简 SOCKS5 服务端（RFC 1928 / 1929），供
    /// [`socks5_really_speaks_socks`] 真的跑一次握手。绑 `127.0.0.1:0`（随机端口，不会撞车）。
    ///
    /// `want_auth = Some((user, pass))` 时要求用户名密码鉴权，并在服务端**断言收到的凭据**
    /// ——这是「凭据真的送出去了」的唯一硬证据。
    ///
    /// 线程 join 后返回客户端请求连接的 `域名:端口`：CONNECT 里带的若是域名（ATYP=3），
    /// 就证明域名是**交给代理解析**的（socks5h 语义），而不是本地解析成 IP 再发出去。
    fn spawn_socks5_once(
        want_auth: Option<(&'static str, &'static str)>,
    ) -> (u16, std::thread::JoinHandle<String>) {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};

        fn exact(s: &mut TcpStream, n: usize) -> Vec<u8> {
            let mut b = vec![0u8; n];
            s.read_exact(&mut b).expect("SOCKS 报文读取失败");
            b
        }

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本机随机端口");
        let port = listener.local_addr().expect("取监听端口").port();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("没有客户端连上来");
            // 1) 方法协商：VER=5, NMETHODS, METHODS…
            let hello = exact(&mut s, 2);
            assert_eq!(hello[0], 5, "客户端发的不是 SOCKS5");
            let _methods = exact(&mut s, hello[1] as usize);
            match want_auth {
                None => s.write_all(&[5, 0]).expect("回方法选择"), // 选「无需鉴权」
                Some((user, pass)) => {
                    s.write_all(&[5, 2]).expect("回方法选择"); // 选「用户名/密码」
                    // 2) RFC 1929 子协商：VER=1, ULEN, UNAME, PLEN, PASSWD
                    let head = exact(&mut s, 2);
                    assert_eq!(head[0], 1, "鉴权子协商版本应为 1");
                    let u = exact(&mut s, head[1] as usize);
                    let plen = exact(&mut s, 1)[0] as usize;
                    let p = exact(&mut s, plen);
                    assert_eq!(String::from_utf8_lossy(&u), user, "用户名没原样送到代理");
                    assert_eq!(String::from_utf8_lossy(&p), pass, "密码没原样送到代理");
                    s.write_all(&[1, 0]).expect("回鉴权结果"); // 通过
                }
            }
            // 3) 请求：VER=5, CMD=1(CONNECT), RSV=0, ATYP
            let req = exact(&mut s, 4);
            assert_eq!((req[0], req[1]), (5, 1), "只应发 CONNECT");
            assert_eq!(req[3], 3, "目标应是**域名**形态（ATYP=3），即交给代理解析");
            let dlen = exact(&mut s, 1)[0] as usize;
            let domain = String::from_utf8(exact(&mut s, dlen)).expect("域名非 UTF-8");
            let pb = exact(&mut s, 2);
            let dport = u16::from_be_bytes([pb[0], pb[1]]);
            // 4) 成功应答（BND.ADDR 用 0.0.0.0:0 即可）
            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).expect("回 CONNECT 成功");
            // 5) 此后这条连接就是「到目标站点」的隧道：冒充目标回一个最小 HTTP 响应
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf); // 读掉客户端的请求头（够用即可）
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("回 HTTP 响应");
            s.flush().expect("刷出响应");
            format!("{domain}:{dport}")
        });
        (port, handle)
    }

    /// **真的**跑通一次 SOCKS5：本机起个极简 SOCKS5 服务端，让
    /// `normalize_proxy` → [`build_agent`] 产出的 Agent 经它取回一个响应。
    ///
    /// 为什么非要这条：只断言 Agent 的 Debug 里有 `SOCKS5` 只能证明**配置到位**，
    /// 证明不了链路真能走通——而「配置得进去、其实连不上」正是本模块要根除的东西。
    /// 目标域名用 `.invalid`（RFC 2606 保留，全球不可解析）：它能取回内容，就说明域名
    /// 压根没在本地解析过，确实是交给代理解析的。
    #[test]
    fn socks5_really_speaks_socks() {
        for (auth, addr_creds, host) in [
            (None, String::new(), "socks-plain.invalid"),
            (Some(("u1", "p@ss")), "u1:p@ss@".to_string(), "socks-auth.invalid"),
        ] {
            let (port, srv) = spawn_socks5_once(auth);
            let spec = normalize_proxy(&format!("socks5://{addr_creds}127.0.0.1:{port}")).unwrap();
            let body = build_agent(&spec)
                .expect("SOCKS5 地址应能构造出 Agent")
                .get(&format!("http://{host}/probe"))
                .timeout(Duration::from_secs(5))
                .call()
                .expect("经 SOCKS5 的请求应当成功")
                .into_string()
                .expect("读取响应体");
            assert_eq!(body, "ok", "{host}：经 SOCKS5 没取回正确内容");
            assert_eq!(
                srv.join().expect("SOCKS5 服务端线程内断言失败"),
                format!("{host}:80"),
                "目标域名应原样交给代理（本地根本解析不了 .invalid）"
            );
        }
    }

    /// 错误信息里绝不能出现用户输入的密码。
    #[test]
    fn errors_never_leak_credentials() {
        // 把 `user:secret` 整个当地址填（没有 @、没有端口）→ 端口解析失败
        let err = normalize_proxy("user:secret").unwrap_err();
        assert!(!err.contains("secret"), "错误信息泄露了密码: {err}");
        let err = normalize_proxy("http://user:secret@host").unwrap_err();
        assert!(!err.contains("secret"), "错误信息泄露了密码: {err}");
        // display / Debug 一律脱敏
        let p = normalize_proxy("http://user:secret@127.0.0.1:7897").unwrap();
        assert_eq!(p.display(), "http://***@127.0.0.1:7897");
        assert!(!format!("{p:?}").contains("secret"), "Debug 泄露了密码");
    }

    #[test]
    fn redaction_masks_userinfo() {
        assert_eq!(
            redact_proxy_credentials("连不上代理 http://user:secret@127.0.0.1:7897：超时"),
            "连不上代理 http://***@127.0.0.1:7897：超时"
        );
        // 密码含 @：必须按**最后一个** @ 切，否则密码剩一半留在明文里
        let out = redact_proxy_credentials("http://user:p@ssw0rd@127.0.0.1:7897 失败");
        assert!(!out.contains("ssw0rd"), "密码没抹干净: {out}");
        assert!(out.contains("***@127.0.0.1:7897"));
        // 无 scheme 的裸形态
        let out = redact_proxy_credentials("user:secret@proxy.corp:8080 拒绝了连接");
        assert!(!out.contains("secret"), "{out}");
        // 不含凭据的串原样返回（不误伤正常 URL / 中文正文）
        let plain = "从 https://raw.githubusercontent.com/a/b/c 下载失败：连接超时";
        assert_eq!(redact_proxy_credentials(plain), plain);
        assert_eq!(redact_proxy_credentials(""), "");
        // 路径里的 @ 不属于 authority，不该触发脱敏
        let path_at = "https://example.com/a@b/c";
        assert_eq!(redact_proxy_credentials(path_at), path_at);
    }

    // ---------- 绕过规则 ----------

    #[test]
    fn bypass_covers_local_and_private() {
        // localhost / *.local
        for h in ["localhost", "LOCALHOST", "localhost.", "printer.local", "A.LOCAL"] {
            assert!(is_bypass_host(h), "{h} 应绕过代理");
        }
        // 127.0.0.0/8
        for h in ["127.0.0.1", "127.0.0.2", "127.255.255.254"] {
            assert!(is_bypass_host(h), "{h} 应绕过代理");
        }
        // ::1 与 IPv4-mapped
        assert!(is_bypass_host("::1"));
        assert!(is_bypass_host("::ffff:127.0.0.1"));
        assert!(is_bypass_host("::ffff:192.168.1.1"));
        // 私网三段（边界都测到）
        for h in [
            "10.0.0.0",
            "10.255.255.255",
            "172.16.0.0",
            "172.31.255.255",
            "192.168.0.0",
            "192.168.255.255",
        ] {
            assert!(is_bypass_host(h), "{h} 应绕过代理");
        }
    }

    #[test]
    fn bypass_excludes_public_hosts() {
        for h in [
            "github.com",
            "raw.githubusercontent.com",
            "gh-proxy.com",
            "8.8.8.8",
            "1.1.1.1",
            // 私网段的**紧邻**外侧：多绕过一个就是把公网流量漏出代理
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "11.0.0.1",
            "9.255.255.255",
            "126.255.255.255",
            "128.0.0.1",
            // 名字里带 local 但不是 .local 域
            "mylocal",
            "local.example.com",
            "localhost.evil.tld",
            // 公网 IPv6
            "2001:db8::1",
            "",
        ] {
            assert!(!is_bypass_host(h), "{h} 不该绕过代理");
        }
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://github.com/a/b").as_deref(), Some("github.com"));
        assert_eq!(host_of("http://127.0.0.1:8787/api/mirrors").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_of("127.0.0.1:8787").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_of("http://GitHub.COM").as_deref(), Some("github.com"));
        assert_eq!(host_of("http://[::1]:8787/x").as_deref(), Some("::1"));
        assert_eq!(host_of("https://user:pass@example.com:443/x").as_deref(), Some("example.com"));
        // userinfo 伪装：真实主机是 @ 之后那段，绝不能判成本机而绕过代理
        assert_eq!(host_of("https://127.0.0.1@evil.tld/x").as_deref(), Some("evil.tld"));
        assert!(!is_bypass_host(&host_of("https://127.0.0.1@evil.tld/x").unwrap()));
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https:///onlypath"), None);
    }

    // ---------- agent_for 选择逻辑 ----------

    /// 本用例独占操作进程级的代理位（全局状态），故所有断言合并在一个用例里、结束时复位，
    /// 避免与其它用例产生读写竞态（`account.rs` 的全局端点用例是同一套写法）。
    #[test]
    fn agent_selection_respects_bypass() {
        // 1) 未配置代理：一切都走直连
        refresh(false, "").unwrap();
        assert!(!proxy_configured());
        assert!(!uses_proxy("https://github.com/x"));
        assert!(!uses_proxy("http://127.0.0.1:8787/api/mirrors"));

        // 2) 开关开着但地址为空 = 没配 → 直连（不报错，UI 层由 save_settings 拦）
        refresh(true, "   ").unwrap();
        assert!(!proxy_configured());

        // 3) 地址非法：代理位必须被清空（fail-closed 到直连），并返回可读错误
        let err = refresh(true, "127.0.0.1:abc").unwrap_err();
        assert!(err.contains("代理端口"), "实际: {err}");
        assert!(!proxy_configured(), "非法配置不得留下半生效的代理");

        // 4) 配好代理：公网走代理、本机/内网走直连
        refresh(true, "127.0.0.1:7897").unwrap();
        assert!(proxy_configured());
        assert!(
            proxy_spec_is(&normalize_proxy("127.0.0.1:7897").unwrap()),
            "运行期生效的必须**就是**本次入参"
        );
        assert!(uses_proxy("https://github.com/x"));
        assert!(uses_proxy("https://raw.githubusercontent.com/a/b"));
        assert!(uses_proxy("https://gitee.com/api/v5/x"));
        // 硬需求：本地服务端不得被塞进代理（127.0.0.1:7897 代理 + 127.0.0.1:8787 服务端）
        assert!(!uses_proxy("http://127.0.0.1:8787/api/mirrors"));
        assert!(!uses_proxy("http://localhost:8787/data/app"));
        assert!(!uses_proxy("http://192.168.1.10:8787/x"));
        assert!(!uses_proxy("http://[::1]:8787/x"));

        // 5) agent_for 与 uses_proxy 必须给出一致的结论。
        //    ⚠ 这里用 Agent 的 Debug 只是**测试内**的取证手段：它会打印 Proxy 的明文字段，
        //    所以本用例刻意用不带凭据的地址；生产代码永远不许 {:?} 打印 Agent。
        let via = format!("{:?}", agent_for("https://github.com/x"));
        assert!(via.contains("proxy: Some"), "公网请求应拿到带代理的 Agent");
        let direct = format!("{:?}", agent_for("http://127.0.0.1:8787/x"));
        assert!(direct.contains("proxy: None"), "本机请求必须拿到直连 Agent");

        // 5b) SOCKS **真的**以 SOCKS 出站：证明 `socks-proxy` feature 确实在起作用，
        //     而不是被我们悄悄归一成 HTTP 走了另一条路（那就又是一个「看着能用」的假配置）。
        for (addr, proto) in [
            ("socks5://127.0.0.1:7897", "SOCKS5"),
            ("socks5h://127.0.0.1:7897", "SOCKS5"),
            ("socks4://127.0.0.1:7897", "SOCKS4"),
            ("socks4a://127.0.0.1:7897", "SOCKS4A"),
        ] {
            refresh(true, addr).unwrap();
            let via = format!("{:?}", agent_for("https://github.com/x"));
            assert!(via.contains(proto), "{addr} 应以 {proto} 出站，实际: {via}");
            // 绕过规则与代理协议无关：SOCKS 下本机/内网同样直连
            assert!(!uses_proxy("http://127.0.0.1:8787/x"), "{addr}：本机地址仍须直连");
        }
        refresh(true, "127.0.0.1:7897").unwrap(); // 复位回 HTTP 代理，下面接着用

        // 6) 配置**没变**就不重建 Agent（否则连接池被反复丢弃）。
        //    背景：`save_settings` 每保存一次就调一次 refresh，而拖动透明度滑块是 300ms
        //    防抖的连发保存——无条件重建会让本模块特意引入的连接池复用作废。
        for _ in 0..3 {
            assert!(
                !refresh_inner(true, "127.0.0.1:7897").unwrap(),
                "配置没变却重建了 Agent"
            );
        }
        // 写法不同但语义相同（scheme 归一 / 大小写 / 尾随斜杠）同样算「没变」
        assert!(!refresh_inner(true, "http://127.0.0.1:7897/").unwrap());
        assert!(!refresh_inner(true, "HTTP://127.0.0.1:7897").unwrap());
        // 端口变了 = 真的变了，必须重建
        assert!(refresh_inner(true, "127.0.0.1:7898").unwrap(), "端口变了必须重建");
        // 只改密码也必须重建：display() 会把凭据抹成 ***，
        // 若拿它当「变没变」的判据，新密码会永远不生效——又一个「存了不生效」的控件
        assert!(refresh_inner(true, "http://u:p1@127.0.0.1:7898").unwrap());
        assert!(
            refresh_inner(true, "http://u:p2@127.0.0.1:7898").unwrap(),
            "只改密码也必须真的生效"
        );
        assert!(!refresh_inner(true, "http://u:p2@127.0.0.1:7898").unwrap());

        // 7) 关掉代理立刻生效（热更新，不需要重启）
        refresh(false, "127.0.0.1:7897").unwrap();
        assert!(!proxy_configured());
        assert!(!uses_proxy("https://github.com/x"));
        let direct = format!("{:?}", agent_for("https://github.com/x"));
        assert!(direct.contains("proxy: None"));
        // 已经是直连时再关一次：状态没变，不必再动代理位
        assert!(!refresh_inner(false, "").unwrap());

        refresh(false, "").unwrap(); // 复位全局，勿污染其它用例
    }

    /// 测试目标必须是**公网 https**：它若命中绕过规则，「测试代理」就会变成悄悄直连的假按钮。
    #[test]
    fn probe_target_is_public_https() {
        let t = probe_target();
        assert!(t.starts_with("https://"), "探测目标必须走 TLS（测的是 CONNECT 隧道）: {t}");
        let host = host_of(&t).expect("探测目标必须能解析出主机名");
        assert!(!is_bypass_host(&host), "探测目标不能命中绕过规则，否则测的不是代理: {host}");
        assert_eq!(host, "raw.githubusercontent.com");
    }

    // ---------- 服务端错误文案 ----------

    /// 造一个带 body 的响应。ureq 2.x 的 `Response::new` 就是为测试准备的。
    fn resp(status: u16, body: &str) -> ureq::Response {
        ureq::Response::new(status, "test", body).expect("构造测试响应")
    }

    #[test]
    fn server_message_is_passed_through_verbatim() {
        let r = resp(403, r#"{"error":"该账号已被停用：违反使用条款"}"#);
        assert_eq!(
            server_error_message(r).as_deref(),
            Some("该账号已被停用：违反使用条款"),
            "服务端给的原因必须原样透传——折叠成自己写死的文案就是误导用户"
        );
    }

    #[test]
    fn non_json_or_missing_field_falls_back_to_none() {
        // 被中间网关截胡时返回的是 HTML，不是 JSON
        assert_eq!(server_error_message(resp(502, "<html>bad gateway</html>")), None);
        // JSON 但没有 error 字段
        assert_eq!(server_error_message(resp(400, r#"{"ok":false}"#)), None);
        // error 不是字符串
        assert_eq!(server_error_message(resp(400, r#"{"error":123}"#)), None);
        // 空体
        assert_eq!(server_error_message(resp(401, "")), None);
        // 空串与纯空白都不算有效文案：显示一个空 toast 比不显示更让人困惑
        assert_eq!(server_error_message(resp(401, r#"{"error":""}"#)), None);
        assert_eq!(server_error_message(resp(401, r#"{"error":"   "}"#)), None);
    }

    #[test]
    fn overlong_message_is_rejected() {
        // 这段文字会直接进 toast，服务端返回一大段东西会把界面撑坏
        let long = "封".repeat(201);
        let body = format!(r#"{{"error":"{long}"}}"#);
        assert_eq!(server_error_message(resp(403, &body)), None, "超长文案不采用");

        let ok = "封".repeat(200);
        let body = format!(r#"{{"error":"{ok}"}}"#);
        assert!(server_error_message(resp(403, &body)).is_some(), "刚好到上限仍可用");
    }

    #[test]
    fn message_is_trimmed() {
        let r = resp(403, r#"{"error":"  账号已停用  "}"#);
        assert_eq!(server_error_message(r).as_deref(), Some("账号已停用"));
    }

    #[test]
    fn split_status_error_separates_code_and_message() {
        let e = ureq::Error::Status(403, resp(403, r#"{"error":"停用了"}"#));
        let (code, msg) = super::split_status_error(e).expect("状态类错误");
        assert_eq!(code, 403);
        assert_eq!(msg.as_deref(), Some("停用了"));

        // 没有可用文案时只回状态码，由调用方决定兜底
        let e = ureq::Error::Status(500, resp(500, "boom"));
        let (code, msg) = super::split_status_error(e).expect("状态类错误");
        assert_eq!(code, 500);
        assert_eq!(msg, None);
    }

    #[test]
    fn transport_errors_are_redacted_before_leaving_this_module() {
        // 传输错误文本会进 toast 与日志；代理地址里的凭据必须先抹掉。
        // 这里直接验证脱敏这一步本身——构造真实的 ureq::Transport 需要发一次请求，
        // 而 split_status_error 的传输分支就是把它交给 redact_proxy_credentials。
        let raw = "连不上代理 socks5://user:secret@127.0.0.1:1080：超时";
        let out = redact_proxy_credentials(raw);
        assert!(!out.contains("secret"), "凭据没抹干净：{out}");
        assert!(out.contains("127.0.0.1:1080"), "地址本身要保留，否则没法排查：{out}");
    }
}
