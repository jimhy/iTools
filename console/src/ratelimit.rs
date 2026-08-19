//! 登录限流：固定窗口计数，按 IP 分桶，纯内存。
//!
//! 主服务的 `/auth/login` **至今零限流**（`server/README.md` 已自认这个缺口）。
//! 控制台是运营入口、一把钥匙开全库，绝不能沿用那个状态——所以登录这条路径
//! 从第一版就带限流。
//!
//! 内存实现意味着进程重启后计数清零。对暴力破解防护来说这是可接受的
//! （攻击者无法让服务端重启），换来的是零外部依赖。

use std::collections::HashMap;
use std::sync::Mutex;

/// 单个分桶的状态。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 当前窗口的起点（Unix 毫秒）
    window_start: i64,
    count: u32,
}

/// 固定窗口限流器。
pub struct RateLimiter {
    max: u32,
    window_ms: i64,
    /// key → 桶。key 是客户端 IP。
    buckets: Mutex<HashMap<String, Bucket>>,
    /// 分桶数上限，防止被伪造 IP 撑爆内存。
    max_keys: usize,
}

/// 一次限流判定的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    /// 本窗口还剩几次机会。
    pub remaining: u32,
    /// 窗口重置前还有多少秒。
    pub retry_after_sec: i64,
}

impl RateLimiter {
    /// `max = 0` 表示关闭限流（一律放行）。
    pub fn new(max: u32, window_sec: u64) -> Self {
        Self {
            max,
            window_ms: (window_sec as i64) * 1000,
            buckets: Mutex::new(HashMap::new()),
            max_keys: 10_000,
        }
    }

    pub fn enabled(&self) -> bool {
        self.max > 0
    }

    /// 记一次尝试并判定是否放行。
    pub fn check(&self, key: &str, now_ms: i64) -> Decision {
        if self.max == 0 {
            return Decision {
                allowed: true,
                remaining: u32::MAX,
                retry_after_sec: 0,
            };
        }

        // Mutex 中毒（某个线程持锁时 panic）不该让登录直接瘫痪，
        // 但也绝不能因此放弃限流——取回内层数据继续用。
        let mut map = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // 桶数超限时先清掉所有已过期的桶；清完还满就拒绝新 key，
        // 宁可误伤也不让内存被伪造 IP 撑爆。
        if map.len() >= self.max_keys && !map.contains_key(key) {
            map.retain(|_, b| now_ms - b.window_start < self.window_ms);
            if map.len() >= self.max_keys {
                return Decision {
                    allowed: false,
                    remaining: 0,
                    retry_after_sec: self.window_ms / 1000,
                };
            }
        }

        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            window_start: now_ms,
            count: 0,
        });
        if now_ms - bucket.window_start >= self.window_ms {
            bucket.window_start = now_ms;
            bucket.count = 0;
        }
        bucket.count += 1;

        let elapsed = now_ms - bucket.window_start;
        let retry_after_sec = ((self.window_ms - elapsed) + 999) / 1000;
        if bucket.count > self.max {
            Decision {
                allowed: false,
                remaining: 0,
                retry_after_sec: retry_after_sec.max(1),
            }
        } else {
            Decision {
                allowed: true,
                remaining: self.max - bucket.count,
                retry_after_sec,
            }
        }
    }

    /// 登录成功后清掉该 IP 的计数——正常人输错几次再输对，不该继续被罚。
    pub fn reset(&self, key: &str) {
        let mut map = match self.buckets.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.remove(key);
    }

    /// 当前活跃分桶数（诊断用）。
    pub fn size(&self) -> usize {
        match self.buckets.lock() {
            Ok(g) => g.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_rejects() {
        let rl = RateLimiter::new(3, 60);
        let t = 1_000_000;
        assert!(rl.check("1.2.3.4", t).allowed);
        assert!(rl.check("1.2.3.4", t).allowed);
        let third = rl.check("1.2.3.4", t);
        assert!(third.allowed);
        assert_eq!(third.remaining, 0, "第三次用完额度");
        let fourth = rl.check("1.2.3.4", t);
        assert!(!fourth.allowed, "超出必须拒绝");
        assert!(fourth.retry_after_sec > 0, "拒绝时必须告诉对方多久后再试");
    }

    #[test]
    fn window_resets_after_expiry() {
        let rl = RateLimiter::new(2, 60);
        let t = 1_000_000;
        rl.check("ip", t);
        rl.check("ip", t);
        assert!(!rl.check("ip", t).allowed);
        // 窗口过去之后重新计数
        assert!(rl.check("ip", t + 60_000).allowed);
    }

    #[test]
    fn buckets_are_per_key() {
        let rl = RateLimiter::new(1, 60);
        let t = 1_000_000;
        assert!(rl.check("a", t).allowed);
        assert!(!rl.check("a", t).allowed);
        assert!(rl.check("b", t).allowed, "另一个 IP 不受影响");
    }

    #[test]
    fn reset_clears_penalty_after_success() {
        let rl = RateLimiter::new(2, 60);
        let t = 1_000_000;
        rl.check("ip", t);
        rl.check("ip", t);
        assert!(!rl.check("ip", t).allowed);
        rl.reset("ip");
        assert!(rl.check("ip", t).allowed, "登录成功后清零，不再罚");
    }

    #[test]
    fn zero_max_disables_limiting() {
        let rl = RateLimiter::new(0, 60);
        assert!(!rl.enabled());
        for _ in 0..1000 {
            assert!(rl.check("ip", 1).allowed);
        }
        assert_eq!(rl.size(), 0, "关闭时不该占任何内存");
    }
}
