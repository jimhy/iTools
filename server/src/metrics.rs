//! 请求指标的内存聚合。
//!
//! # 为什么是「内存聚合 + 定期落库」而不是每请求写一行
//!
//! 服务端跑在群晖那块盘上，每请求一次 INSERT 会把写压力放大到请求量的一倍，
//! 而运营面板要看的从来不是单条请求，是「这一小时有多少请求、多少出错、多慢」。
//! 聚合后每小时每路由每状态码只有一行，一天几百行，查询和存储都可以忽略不计。
//!
//! 代价是**进程崩溃会丢掉最后一个未落库的窗口**（默认最多 1 分钟）。这一点必须
//! 在面板上如实标注，不能让人以为那段时间真的没有流量。
//!
//! # 记录的是什么、不是什么
//!
//! - 字节数取自 body 的 `size_hint().exact()`。分块传输（无确定长度）的响应记为 0——
//!   **少记而不是猜**，宁可让带宽偏小也不编一个数出来。
//! - 延迟按四个桶计数（<50ms / <200ms / <1s / ≥1s）外加总和与最大值。
//!   **不算 P95**：从桶里插值出来的百分位是估算，而 avg 与 max 是精确的。
//!   与其给一个看着精确、其实是猜的 P95，不如给精确的分布。
//! - 不记 IP、不记 User-Agent、不记请求体内容。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 一小时的秒数。
pub const HOUR_SEC: i64 = 3_600;

/// 聚合键：小时桶 + 路由 + 方法 + 状态码大类。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    /// 小时桶起点（Unix 秒，UTC）
    pub hour_ts: i64,
    /// 路由模板（如 `/api/market/package/{name}`），不是具体路径——
    /// 用具体路径会让基数随插件数、用户数无限增长。
    pub route: String,
    pub method: String,
    /// 状态码大类：2/3/4/5。其它（理论上不会出现）记 0。
    pub status_class: u8,
}

/// 一个桶里的累计量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Agg {
    pub reqs: i64,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub dur_sum_ms: i64,
    pub dur_max_ms: i64,
    /// < 50ms
    pub b_fast: i64,
    /// 50ms ~ 200ms
    pub b_mid: i64,
    /// 200ms ~ 1s
    pub b_slow: i64,
    /// ≥ 1s
    pub b_vslow: i64,
}

impl Agg {
    fn add(&mut self, dur_ms: i64, bytes_in: i64, bytes_out: i64) {
        self.reqs += 1;
        self.bytes_in += bytes_in;
        self.bytes_out += bytes_out;
        self.dur_sum_ms += dur_ms;
        self.dur_max_ms = self.dur_max_ms.max(dur_ms);
        if dur_ms < 50 {
            self.b_fast += 1;
        } else if dur_ms < 200 {
            self.b_mid += 1;
        } else if dur_ms < 1000 {
            self.b_slow += 1;
        } else {
            self.b_vslow += 1;
        }
    }
}

/// 指标聚合器。`record` 在每个请求上调用，必须极轻。
pub struct Metrics {
    /// 采集开关。关掉时 `record` 直接返回，一条都不记——
    /// 面板据此显示「未采集」，而不是一条会被误读成「没有流量」的零线。
    enabled: bool,
    buckets: Mutex<HashMap<Key, Agg>>,
    /// 插件下载计数：(小时桶, 插件名) → 次数。
    ///
    /// 单列出来而不是塞进 `buckets` 的 route 里：那样会让路由基数跟着插件数涨，
    /// 而这里的基数本来就该跟着插件数涨，语义不同、生命周期也不同。
    downloads: Mutex<HashMap<(i64, String), i64>>,
    /// 键数上限。正常情况下只有几百个键（路由数 × 方法 × 状态类 × 小时数），
    /// 这个上限纯粹是防御——万一路由模板取错变成了具体路径，不至于把内存吃穿。
    max_keys: usize,
    /// 因超过上限而被丢弃的请求数。**不静默**：flush 时会打日志。
    dropped: AtomicU64,
}

impl Metrics {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            buckets: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            max_keys: 20_000,
            dropped: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 记一次请求。`now_ms` 用毫秒时间戳（与全服一致）。
    ///
    /// 参数确实多（clippy 阈值 7）。包成结构体在这里是负收益：它在**每个请求**上被调用，
    /// 多造一个临时结构体不划算，而调用点只有中间件一处，平铺反而看得清传了什么。
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        route: &str,
        method: &str,
        status: u16,
        dur_ms: i64,
        bytes_in: i64,
        bytes_out: i64,
        now_ms: i64,
    ) {
        if !self.enabled {
            return;
        }
        let key = Key {
            hour_ts: hour_bucket(now_ms),
            route: route.to_string(),
            method: method.to_string(),
            status_class: status_class(status),
        };

        // Mutex 中毒（持锁线程 panic）不该让请求处理跟着挂，取回内层继续用。
        let mut map = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if map.len() >= self.max_keys && !map.contains_key(&key) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        map.entry(key)
            .or_default()
            .add(dur_ms.max(0), bytes_in.max(0), bytes_out.max(0));
    }

    /// 记一次插件包下载。
    pub fn record_download(&self, plugin: &str, now_ms: i64) {
        if !self.enabled {
            return;
        }
        let mut map = match self.downloads.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let key = (hour_bucket(now_ms), plugin.to_string());
        if map.len() >= self.max_keys && !map.contains_key(&key) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        *map.entry(key).or_insert(0) += 1;
    }

    /// 取出全部聚合并清空。落库任务调用。
    ///
    /// 先取走再写库：写库期间新来的请求进的是新的空表，不会丢也不会重复计。
    /// 代价是**写库失败这一批就没了**——所以调用方必须把失败大声打进日志。
    pub fn drain(&self) -> Drained {
        let buckets = {
            let mut map = match self.buckets.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *map)
        };
        let downloads = {
            let mut map = match self.downloads.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *map)
        };
        Drained {
            buckets: buckets.into_iter().collect(),
            downloads: downloads.into_iter().collect(),
            dropped: self.dropped.swap(0, Ordering::Relaxed),
        }
    }

    /// 当前未落库的键数（诊断用）。
    pub fn pending(&self) -> usize {
        match self.buckets.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

/// 一次 drain 的结果。
#[derive(Debug, Default)]
pub struct Drained {
    pub buckets: Vec<(Key, Agg)>,
    pub downloads: Vec<((i64, String), i64)>,
    /// 上一个窗口里因超过键数上限被丢弃的请求数。非 0 就是配置或代码有问题。
    pub dropped: u64,
}

impl Drained {
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty() && self.downloads.is_empty()
    }
}

/// 毫秒时间戳 → 所在小时桶的起点（Unix 秒，UTC）。
pub fn hour_bucket(now_ms: i64) -> i64 {
    // 负数（1970 之前）在这里不可能出现，但整数除法对负数是向零取整，
    // 会把桶算到未来去。夹到 0 而不是留个隐患。
    let sec = (now_ms / 1000).max(0);
    sec / HOUR_SEC * HOUR_SEC
}

/// 状态码 → 大类。
fn status_class(status: u16) -> u8 {
    match status {
        200..=299 => 2,
        300..=399 => 3,
        400..=499 => 4,
        500..=599 => 5,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hour_bucket_is_aligned() {
        // 2026-08-19 04:37:12 UTC
        let ms = 1_787_114_232_000;
        let b = hour_bucket(ms);
        assert_eq!(b % HOUR_SEC, 0, "桶起点必须对齐到整小时");
        assert!(b <= ms / 1000, "桶起点不能落在未来");
        assert!(ms / 1000 - b < HOUR_SEC, "桶起点必须落在同一小时内");
        // 同一小时内的两个时刻落进同一个桶
        assert_eq!(hour_bucket(ms), hour_bucket(ms + 600_000));
        // 跨小时则不同
        assert_ne!(hour_bucket(ms), hour_bucket(ms + HOUR_SEC * 1000));
        // 负数不会算到未来
        assert_eq!(hour_bucket(-5000), 0);
    }

    #[test]
    fn status_classes() {
        assert_eq!(status_class(200), 2);
        assert_eq!(status_class(204), 2);
        assert_eq!(status_class(304), 3);
        assert_eq!(status_class(401), 4);
        assert_eq!(status_class(429), 4);
        assert_eq!(status_class(500), 5);
        assert_eq!(status_class(999), 0, "非法状态码归到 0 而不是硬塞进某一类");
    }

    #[test]
    fn record_aggregates_by_key() {
        let m = Metrics::new(true);
        let now = 1_787_114_232_000;
        m.record("/health", "GET", 200, 10, 0, 20, now);
        m.record("/health", "GET", 200, 30, 0, 22, now);
        // 不同状态码 → 不同桶
        m.record("/health", "GET", 500, 5, 0, 0, now);

        let d = m.drain();
        assert_eq!(d.buckets.len(), 2, "同路由不同状态类要分开");
        let ok = d
            .buckets
            .iter()
            .find(|(k, _)| k.status_class == 2)
            .expect("应有 2xx 桶");
        assert_eq!(ok.1.reqs, 2);
        assert_eq!(ok.1.bytes_out, 42);
        assert_eq!(ok.1.dur_sum_ms, 40);
        assert_eq!(ok.1.dur_max_ms, 30, "最大值取两次里更大的那个");
        assert_eq!(ok.1.b_fast, 2, "10ms 与 30ms 都落在最快的桶");
    }

    #[test]
    fn latency_buckets_partition_correctly() {
        let m = Metrics::new(true);
        let now = 1_787_114_232_000;
        for d in [0, 49, 50, 199, 200, 999, 1000, 5000] {
            m.record("/x", "GET", 200, d, 0, 0, now);
        }
        let d = m.drain();
        let a = d.buckets[0].1;
        assert_eq!(a.reqs, 8);
        assert_eq!(a.b_fast, 2, "0 与 49");
        assert_eq!(a.b_mid, 2, "50 与 199");
        assert_eq!(a.b_slow, 2, "200 与 999");
        assert_eq!(a.b_vslow, 2, "1000 与 5000");
        assert_eq!(a.b_fast + a.b_mid + a.b_slow + a.b_vslow, a.reqs, "桶之和必须等于总数");
    }

    #[test]
    fn drain_clears_state() {
        let m = Metrics::new(true);
        m.record("/x", "GET", 200, 1, 0, 0, 1_787_114_232_000);
        assert_eq!(m.pending(), 1);
        let first = m.drain();
        assert_eq!(first.buckets.len(), 1);
        assert_eq!(m.pending(), 0, "drain 之后必须清空，否则下次落库会重复计");
        assert!(m.drain().is_empty());
    }

    #[test]
    fn downloads_counted_per_plugin_and_hour() {
        let m = Metrics::new(true);
        let now = 1_787_114_232_000;
        m.record_download("deskbox", now);
        m.record_download("deskbox", now);
        m.record_download("json-format", now);
        // 下一个小时单独计
        m.record_download("deskbox", now + HOUR_SEC * 1000);

        let d = m.drain();
        assert_eq!(d.downloads.len(), 3);
        let same_hour = d
            .downloads
            .iter()
            .find(|((h, n), _)| *h == hour_bucket(now) && n == "deskbox")
            .expect("应有该小时的 deskbox 计数");
        assert_eq!(same_hour.1, 2);
    }

    #[test]
    fn negative_values_are_clamped() {
        let m = Metrics::new(true);
        // 时钟回拨或 header 解析异常都可能给出负数，不能让它把累计值拉成负的
        m.record("/x", "GET", 200, -100, -5, -7, 1_787_114_232_000);
        let d = m.drain();
        let a = d.buckets[0].1;
        assert_eq!(a.dur_sum_ms, 0);
        assert_eq!(a.bytes_in, 0);
        assert_eq!(a.bytes_out, 0);
        assert_eq!(a.reqs, 1, "请求本身仍要计数");
    }

    #[test]
    fn over_limit_drops_are_counted_not_silent() {
        let m = Metrics {
            enabled: true,
            buckets: Mutex::new(HashMap::new()),
            downloads: Mutex::new(HashMap::new()),
            max_keys: 2,
            dropped: AtomicU64::new(0),
        };
        let now = 1_787_114_232_000;
        m.record("/a", "GET", 200, 1, 0, 0, now);
        m.record("/b", "GET", 200, 1, 0, 0, now);
        m.record("/c", "GET", 200, 1, 0, 0, now); // 超限被丢
        m.record("/a", "GET", 200, 1, 0, 0, now); // 已有键仍能累加

        let d = m.drain();
        assert_eq!(d.buckets.len(), 2);
        assert_eq!(d.dropped, 1, "丢弃必须被计数，不能静默");
        let a = d.buckets.iter().find(|(k, _)| k.route == "/a").unwrap();
        assert_eq!(a.1.reqs, 2, "已存在的键不受上限影响");
    }
}
