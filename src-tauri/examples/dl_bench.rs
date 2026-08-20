//! 下载吞吐基准：回答验收报告遗留第 1 条——「debug 下 1.4 MB/s 与 curl 15.4 MB/s 的剩余差距，
//! 到底是不是 debug 未优化造成的」。
//!
//! # 为什么不是直接跑 release 版 iTools 复测
//!
//! 自检插件 `apitest` 声明了全部 20 项高危权限。release 构建用的是**用户真实数据目录**
//! （`%LOCALAPPDATA%\itools`，见 `paths.rs`），为了一个性能数字把这样一个插件授权进正式
//! 环境，代价远大于收益。所以这里把 `runtime_api::download_and_verify` 的核心循环原样搬出来
//! ——同一个 ureq Agent 配置、同样的 64 KB 分块、同样的逐块 SHA-256、同样累积进内存——
//! 用 `--release` 与 debug 各跑一遍同一个 URL 做 A/B。
//!
//! 唯一刻意去掉的是**进度事件推送**：那一半已经在真机上单独验过了（事件数 430 → 17，
//! 速率 166 KB/s → 1.42 MB/s）。这里要隔离的正是剩下的那一半：TLS 解密与逐块哈希本身
//! 在两种构建下差多少。
//!
//! # 必须先解决的一个陷阱：两条链路不可比
//!
//! 这台机器上 `all_proxy=http://127.0.0.1:7897`、系统代理也开着，所以 **curl 默认走代理**；
//! 而 iTools 的设置里代理是关的，`direct_agent()` 又显式 `try_proxy_from_env(false)`，
//! 走的是**直连**。同一时刻实测同一个 URL：经代理 22.3 MB/s，直连 56 KB/s——差 400 倍。
//! 拿「curl 的数」跟「iTools 的数」直接相减，量到的是链路差，不是构建类型差。
//! 所以本工具接受第二个参数指定代理，debug / release 必须在**同一条链路**上比。
//!
//! 用法：
//! ```text
//! cargo run --example dl_bench -- <url> [proxy]                # debug
//! cargo run --example dl_bench --release -- <url> [proxy]      # release
//! # 例：cargo run --example dl_bench --release -- <url> 127.0.0.1:7897
//! ```
//! 只读网络、只写内存，不落盘、不碰任何用户数据。

use std::io::Read;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

fn main() {
    let url = std::env::args().nth(1).expect("用法: dl_bench <url> [proxy]");
    let proxy = std::env::args().nth(2);
    let build = if cfg!(debug_assertions) { "debug" } else { "release" };

    // 与 http.rs 同款配置：显式关掉 env 代理（免得链路在两次运行间偷偷变了），
    // 并带上和产品一致的空闲超时——僵死时报错退出，而不是无限期挂着。
    let mut b = ureq::AgentBuilder::new()
        .try_proxy_from_env(false)
        .timeout_read(Duration::from_secs(60))
        .timeout_write(Duration::from_secs(60));
    if let Some(p) = &proxy {
        b = b.proxy(ureq::Proxy::new(p).expect("代理地址无法解析"));
    }
    let agent = b.build();
    println!("链路={}", proxy.as_deref().unwrap_or("直连"));

    let t0 = Instant::now();
    let resp = agent.get(&url).call().expect("请求失败");
    let ttfb = t0.elapsed();
    let total: Option<u64> = resp
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok());

    let mut reader = resp.into_reader();
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let mut hasher = Sha256::new();
    // 分别累计「等网络/解密」与「算哈希」的耗时，才能说清慢在哪一半，而不是只给一个总数。
    let mut read_time = Duration::ZERO;
    let mut hash_time = Duration::ZERO;
    let mut chunks: u64 = 0;

    let loop_start = Instant::now();
    loop {
        let a = Instant::now();
        let n = reader.read(&mut chunk).expect("读取失败");
        read_time += a.elapsed();
        if n == 0 {
            break;
        }
        let b = Instant::now();
        hasher.update(&chunk[..n]);
        hash_time += b.elapsed();
        buf.extend_from_slice(&chunk[..n]);
        chunks += 1;
    }
    let wall = loop_start.elapsed();
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    // 再单独测一遍「纯哈希」吞吐：同一份数据一次性喂进去，不掺网络，作为交叉印证。
    let h0 = Instant::now();
    let mut h2 = Sha256::new();
    h2.update(&buf);
    let _ = h2.finalize();
    let pure_hash = h0.elapsed();

    let mb = buf.len() as f64 / 1024.0 / 1024.0;
    println!("build={build}");
    println!("bytes={} ({:.1} MB)  content_length={:?}", buf.len(), mb, total);
    println!("sha256={hex}");
    println!("ttfb={:.3}s  下载循环总耗时={:.3}s  吞吐={:.2} MB/s", ttfb.as_secs_f64(), wall.as_secs_f64(), mb / wall.as_secs_f64());
    println!("  其中 read(网络+TLS解密)={:.3}s  hash(逐块SHA256)={:.3}s  chunks={chunks}", read_time.as_secs_f64(), hash_time.as_secs_f64());
    println!("纯哈希吞吐（同一份数据一次性算）={:.2} MB/s（{:.3}s）", mb / pure_hash.as_secs_f64(), pure_hash.as_secs_f64());
}
