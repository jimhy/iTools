//! 轻量按 IP 限流（滑动窗口计数），自实现、零第三方限流组件。
//!
//! 为什么自己写：全服只有 `GET /api/mirrors` 一个免认证读端点需要防护，需求就是「按 IP 计数 + 超限 429」。
//! 这里 ~120 行、纯内存、可注入时钟，测试能完全确定性覆盖。
//!
//! 算法：**滑动窗口计数器**（sliding window counter）——固定窗口计数 + 按当前窗口已过比例
//! 对上一窗口计数加权。比固定窗口不易出现「窗口交界处两倍配额」，又不像滑动日志那样按请求存时间戳。
//!
//! 语义约定（README 同步）：
//! - **被拒的请求也计数**：否则被限的客户端可以靠不停重试把窗口拖住，且日志/带宽照样被打。
//! - `Retry-After` 给的是「加权计数降到配额以下的最早时刻」，不是简单的窗口剩余时间。

use std::sync::Mutex;

use hashlink::LinkedHashMap;

use crate::Clock;

/// 一次限流判定的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitDecision {
    /// 是否放行。
    pub allowed: bool,
    /// 当前配额（窗口内最大请求数）。
    pub limit: u32,
    /// 本次之后本窗口大致还剩多少配额（下限 0）。
    pub remaining: u32,
    /// 建议重试等待秒数（>=1），用于 `Retry-After`。
    pub retry_after_sec: u64,
    /// 本次是否为该 key「刚刚越限」的第一次。
    ///
    /// 只在这一次打日志——否则限流日志本身就成了新的日志放大源。
    pub first_reject: bool,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 当前窗口起点（对齐到 window_ms 的整数倍）。
    start: i64,
    /// 当前窗口内计数。
    count: u64,
    /// 上一窗口计数（用于加权）。
    prev: u64,
    /// 是否已就「越限」打过日志（回到放行状态或换窗口时复位）。
    notified: bool,
}

pub struct RateLimiterOptions {
    /// 窗口内最大请求数。0 视为不限（`hit()` 恒放行）。
    pub max: u32,
    /// 窗口长度（毫秒）。
    pub window_ms: i64,
    /// 内存里最多保留多少个 key（默认 20000）。
    ///
    /// 超出时先清掉过期桶，仍超出则按最近最少使用淘汰——限流器自己绝不能变成内存放大器。
    pub max_keys: usize,
    /// 可注入的时钟（测试用）。
    pub now: Clock,
}

impl RateLimiterOptions {
    pub fn new(max: u32, window_ms: i64, now: Clock) -> Self {
        Self { max, window_ms, max_keys: 20_000, now }
    }
}

pub struct SlidingWindowLimiter {
    buckets: Mutex<LinkedHashMap<String, Bucket>>,
    max: u32,
    window_ms: i64,
    max_keys: usize,
    now: Clock,
}

impl SlidingWindowLimiter {
    pub fn new(opts: RateLimiterOptions) -> Self {
        Self {
            buckets: Mutex::new(LinkedHashMap::new()),
            max: opts.max,
            window_ms: opts.window_ms.max(1),
            max_keys: opts.max_keys.max(1),
            now: opts.now,
        }
    }

    /// 是否真的在限流（配额 0 表示关闭）。调用方据此决定要不要发 `X-RateLimit-*` 头。
    pub fn enabled(&self) -> bool {
        self.max > 0
    }

    /// 当前跟踪的 key 数量（诊断/测试用）。
    pub fn size(&self) -> usize {
        self.buckets.lock().expect("限流器锁中毒").len()
    }

    /// 记一次请求并给出判定。
    pub fn hit(&self, key: &str) -> RateLimitDecision {
        if !self.enabled() {
            return RateLimitDecision {
                allowed: true,
                limit: self.max,
                remaining: u32::MAX,
                retry_after_sec: 0,
                first_reject: false,
            };
        }
        let now = (self.now)();
        let win = self.window_ms;
        let start = now.div_euclid(win) * win;

        let mut buckets = self.buckets.lock().expect("限流器锁中毒");
        let mut b = match buckets.get(key).copied() {
            None => Bucket { start, count: 0, prev: 0, notified: false },
            Some(old) if old.start != start => {
                // 只有「紧邻的上一窗口」才参与加权；隔了更久等于从零开始。
                let prev = if start - old.start == win { old.count } else { 0 };
                Bucket { start, count: 0, prev, notified: false }
            }
            Some(old) => old,
        };

        // 加权计数：上一窗口按「本窗口尚未过去的比例」折算。
        let elapsed = (now - start) as f64 / win as f64;
        let weighted_before = b.prev as f64 * (1.0 - elapsed) + b.count as f64;
        let allowed = weighted_before < self.max as f64;

        b.count += 1; // 被拒的请求也计数（见文件头说明）
        let weighted_after = b.prev as f64 * (1.0 - elapsed) + b.count as f64;

        let mut first_reject = false;
        if allowed {
            b.notified = false;
        } else if !b.notified {
            b.notified = true;
            first_reject = true;
        }

        let retry_after_sec = if allowed { 0 } else { self.retry_after_sec(&b, now) };

        // LRU：重新插入让迭代顺序 = 最近最少使用在前，淘汰时直接从头删。
        buckets.remove(key);
        buckets.insert(key.to_string(), b);
        if buckets.len() > self.max_keys {
            self.evict(&mut buckets, now);
        }

        RateLimitDecision {
            allowed,
            limit: self.max,
            remaining: (self.max as f64 - weighted_after).floor().max(0.0) as u32,
            retry_after_sec,
            first_reject,
        }
    }

    /// 加权计数降到配额以下的最早时刻（秒，向上取整，下限 1）。
    ///
    /// 进入下一窗口后加权计数 = `count * (1 - e)`（`e` 为下一窗口已过比例），
    /// 要它 < max 需 `e > 1 - max/count`，故最早可用时刻为
    /// `下一窗口起点 + window_ms * max(0, 1 - max/count)`。
    fn retry_after_sec(&self, b: &Bucket, now: i64) -> u64 {
        let next_start = b.start + self.window_ms;
        let extra = if b.count > self.max as u64 {
            self.window_ms as f64 * (1.0 - self.max as f64 / b.count as f64)
        } else {
            0.0
        };
        let secs = ((next_start as f64 + extra - now as f64) / 1000.0).ceil();
        secs.max(1.0) as u64
    }

    /// 先清过期桶，仍超上限则按 LRU 淘汰到上限以内。
    fn evict(&self, buckets: &mut LinkedHashMap<String, Bucket>, now: i64) {
        let cutoff = now - 2 * self.window_ms;
        let stale: Vec<String> = buckets
            .iter()
            .filter(|(_, b)| b.start < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            buckets.remove(&k);
        }
        while buckets.len() > self.max_keys {
            if buckets.pop_front().is_none() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    /// 可推进的假时钟。
    fn fake_clock() -> (Arc<AtomicI64>, Clock) {
        let t = Arc::new(AtomicI64::new(0));
        let handle = t.clone();
        (t, Arc::new(move || handle.load(Ordering::SeqCst)))
    }

    #[test]
    fn rejects_over_quota_and_reports_once() {
        let (clock, now) = fake_clock();
        let lim = SlidingWindowLimiter::new(RateLimiterOptions::new(3, 1000, now));

        for i in 1..=3 {
            let d = lim.hit("1.2.3.4");
            assert!(d.allowed, "第 {i} 次应放行");
            assert_eq!(d.limit, 3);
        }
        let first = lim.hit("1.2.3.4");
        assert!(!first.allowed, "第 4 次超配额");
        assert!(first.first_reject, "越限的第一次要报（用于打一条日志）");
        assert!(first.retry_after_sec >= 1, "Retry-After 至少 1 秒");
        assert_eq!(first.remaining, 0);

        let again = lim.hit("1.2.3.4");
        assert!(!again.allowed);
        assert!(!again.first_reject, "后续被拒不再重复报，避免限流日志自己成为放大源");

        // 另一个 IP 不受影响（按 IP 分桶）
        assert!(lim.hit("5.6.7.8").allowed);

        // 进入下一窗口 90% 处：上一窗口计数只剩 10% 权重 → 恢复放行
        clock.store(1900, Ordering::SeqCst);
        assert!(lim.hit("1.2.3.4").allowed, "滑动窗口应按比例恢复配额");

        // 隔了一个完整窗口没来 → 从零开始
        clock.store(3000, Ordering::SeqCst);
        let fresh = lim.hit("1.2.3.4");
        assert!(fresh.allowed);
        assert_eq!(fresh.remaining, 2);
    }

    #[test]
    fn key_count_is_bounded() {
        let (_clock, now) = fake_clock();
        let lim = SlidingWindowLimiter::new(RateLimiterOptions {
            max: 10,
            window_ms: 1000,
            max_keys: 2,
            now,
        });
        for i in 0..50 {
            lim.hit(&format!("10.0.0.{i}"));
        }
        assert!(lim.size() <= 2, "key 数应被限制在 max_keys 内，实得 {}", lim.size());
    }

    #[test]
    fn zero_max_disables_limiting() {
        let (_clock, now) = fake_clock();
        let lim = SlidingWindowLimiter::new(RateLimiterOptions::new(0, 1000, now));
        assert!(!lim.enabled());
        for _ in 0..100 {
            assert!(lim.hit("1.2.3.4").allowed);
        }
        assert_eq!(lim.size(), 0, "关闭时不该占内存");
    }
}
