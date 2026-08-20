//! **高危能力审计日志**：哪个插件、什么时候、用了什么能力，用户可查。
//!
//! # 为什么需要它
//!
//! 权限开关只回答「能不能用」，回答不了「用了没有、用了多少次」。用户给某个插件开了
//! `runCommand`，之后它到底执行过什么、执行了几次，界面上一点痕迹都没有——出了事也无从追溯。
//! 能力越开越多，这个盲区就越大。
//!
//! # 为什么钩在 `plugin_granted` 上
//!
//! 那是**所有**高危能力的必经入口：每个受权限门禁的命令，第一件事就是调它。钩在这里，
//! 一处改动覆盖全部高危能力，也不可能出现「新加了个命令忘记加审计」的漏网。
//! 代价是记录的是「权限判定」而非「命令名」——判定发生时命令一定要执行，两者一一对应，
//! 对追溯来说够用。
//!
//! # 有意的取舍
//!
//! - **只记内存，不落盘**：审计日志本身若落盘，就成了又一份要防篡改、要考虑体积与清理的资产。
//!   当前目标是「用户能看见最近发生了什么」，内存环形缓冲足够，重启即清也符合预期。
//!   要做长期留存需要连同防篡改一起设计，不在此处顺手做半套。
//! - **同一插件同一能力的连续调用会合并计数**：一个录屏插件每秒调几十次截图，逐条记录会把
//!   缓冲瞬间冲满，把真正值得看的东西挤掉。合并成「N 次，最后一次在 X」信息量更大。
//! - **不记参数**：参数里常有用户的文件路径、剪贴板内容、命令行。审计日志是给用户看安全的，
//!   不该自己变成一处隐私泄漏点。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use super::ActiveSession;

/// 环形缓冲容量。够翻看「刚才发生了什么」，又不至于长期占内存。
const CAPACITY: usize = 500;

/// 两次调用间隔在此之内且插件与能力都相同，就并进上一条计数，不新增记录。
const MERGE_WINDOW_SECS: u64 = 5;

/// 一条审计记录。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    /// 插件 id。
    pub plugin_id: String,
    /// 是否调试会话（调试插件与同名正式插件必须分得清）。
    pub dev: bool,
    /// 能力标识，如 `runCommand` / `camera`。
    pub permission: String,
    /// 首次发生时间（Unix 秒）。
    pub first_at: u64,
    /// 最近一次发生时间（Unix 秒）。
    pub last_at: u64,
    /// 合并后的调用次数。
    pub count: u64,
    /// 该次判定是否放行。**被拒的调用同样要记**——插件反复尝试未授权能力，
    /// 本身就是最值得用户看到的信号。
    pub granted: bool,
}

fn buffer() -> &'static Mutex<VecDeque<AuditEntry>> {
    static B: OnceLock<Mutex<VecDeque<AuditEntry>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 记一次高危能力判定。由 `plugin_granted` 调用，不需要别处再调。
pub fn record(session: &ActiveSession, permission: &str, granted: bool) {
    let Ok(mut buf) = buffer().lock() else {
        return;
    };
    let now = now_secs();
    // 合并：只看最后一条，不做全表扫描——审计记录是按时间追加的，
    // 需要合并的一定是紧挨着的那条。
    if let Some(last) = buf.back_mut() {
        if last.plugin_id == session.id
            && last.dev == session.dev
            && last.permission == permission
            && last.granted == granted
            && now.saturating_sub(last.last_at) <= MERGE_WINDOW_SECS
        {
            last.count += 1;
            last.last_at = now;
            return;
        }
    }
    if buf.len() >= CAPACITY {
        buf.pop_front();
    }
    buf.push_back(AuditEntry {
        plugin_id: session.id.clone(),
        dev: session.dev,
        permission: permission.to_string(),
        first_at: now,
        last_at: now,
        count: 1,
        granted,
    });
}

/// 取审计记录（最近的在前），可按插件过滤。
#[tauri::command]
pub fn plugin_audit_log(plugin_id: Option<String>, limit: Option<usize>) -> Vec<AuditEntry> {
    let Ok(buf) = buffer().lock() else {
        return Vec::new();
    };
    let limit = limit.unwrap_or(200).min(CAPACITY);
    buf.iter()
        .rev()
        // 用 map_or(true, ..) 而不是 Option::is_none_or：后者要 Rust 1.82，
        // 而 Cargo.toml 里 rust-version = "1.77"——写它等于在 MSRV 上直接编不过。
        .filter(|e| plugin_id.as_ref().map_or(true, |id| &e.plugin_id == id))
        .take(limit)
        .cloned()
        .collect()
}

/// 清空审计记录（用户主动清理）。
#[tauri::command]
pub fn plugin_audit_clear() {
    if let Ok(mut buf) = buffer().lock() {
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> ActiveSession {
        ActiveSession {
            id: id.to_string(),
            dev: false,
        }
    }

    #[test]
    fn 连续同类调用合并计数而不是刷屏() {
        plugin_audit_clear();
        for _ in 0..10 {
            record(&session("demo-merge"), "screen-capture", true);
        }
        let log = plugin_audit_log(Some("demo-merge".into()), None);
        assert_eq!(log.len(), 1, "同插件同能力的连续调用应合并成一条");
        assert_eq!(log[0].count, 10);
    }

    #[test]
    fn 放行与被拒分开记录() {
        plugin_audit_clear();
        record(&session("demo-deny"), "runCommand", true);
        record(&session("demo-deny"), "runCommand", false);
        let log = plugin_audit_log(Some("demo-deny".into()), None);
        assert_eq!(log.len(), 2, "被拒的调用必须单独成条，那正是要给用户看的信号");
        assert!(!log[0].granted, "最近的一条是被拒那次");
    }

    #[test]
    fn 调试会话与正式会话不混记() {
        plugin_audit_clear();
        record(&session("demo-dev"), "camera", true);
        record(
            &ActiveSession {
                id: "demo-dev".into(),
                dev: true,
            },
            "camera",
            true,
        );
        let log = plugin_audit_log(Some("demo-dev".into()), None);
        assert_eq!(log.len(), 2, "同名的调试插件与正式插件必须分开记");
    }
}
