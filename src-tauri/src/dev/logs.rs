//! 调试日志环形缓冲：API 调用 / console 输出 / 未捕获异常的统一时间线。
//!
//! 写入方有两个：
//! - **bridge 探针**（插件窗口内）批量调 `dev_log_push` 上报；
//! - **后端**在探针看不见的地方补记（如会话打开、同步 Mock 的判定结果）。
//!
//! 读取方是管理中心：`dev_log_list(after_seq)` 增量拉取。`seq` 由**后端**统一分配且
//! 全局单调递增（清空后也不回退）——前端可以拿上一次收到的最大 seq 直接续拉，
//! 不会因为清空 / 乱序上报而漏读或重读。
//!
//! 写入后还会向管理中心推一个 [`DEV_LOG_EVENT`] 事件让它立刻续拉。**事件只是加速器**：
//! 前端的定时增量拉取才是保证，事件丢了/没监听也不会丢日志。

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

/// 环形缓冲容量：调试时几分钟就能刷出上千条，超出后丢最旧的。
/// 与管理中心日志面板的前端上限（`src/admin/dev-log.ts` 的 `MAX_ROWS`）取齐。
const CAPACITY: usize = 2000;

/// 「有新日志」事件名（管理中心 `src/admin/dev-log.ts` 监听它，收到就立刻增量拉一次）。
///
/// 只推给管理中心窗口：被调试的插件页没有理由收到调试器的心跳。
pub const DEV_LOG_EVENT: &str = "dev-log";

/// 管理中心窗口 label（事件的唯一投递目标）。
const ADMIN_WINDOW_LABEL: &str = "admin";

/// 单条 `args` / `result` 的最大保留长度（字符）。
///
/// 必须截断：插件一次 `writeFile(base64图片)` 的入参就可能是几 MB，
/// 原样堆进环形缓冲会把内存吃光，且 UI 也渲染不动。
const MAX_TEXT: usize = 2000;

/// 一条调试日志。
///
/// `#[serde(default)]` 是**给探针留的余地**：上报方（bridge.js）只需填它真正知道的字段
/// （如 `kind` / `method` / `args`），缺的字段按默认值补齐即可，不会因为少一个字段
/// 就整批上报反序列化失败——调试工具本身绝不能变成新的故障源。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DevLogEntry {
    /// 后端分配的全局单调序号（上报方填什么都会被覆盖）。
    pub seq: u64,
    /// 时间（RFC3339）。上报方留空时由后端补当前时间。
    pub at: String,
    /// 产生日志的调试插件 id。
    #[serde(rename = "pluginId")]
    pub plugin_id: String,
    /// `"api"`（itools.* 调用）/ `"console"`（console.*）/ `"error"`（未捕获异常）。
    pub kind: String,
    /// `"info"` / `"warn"` / `"error"`。
    pub level: String,
    /// API 方法名（如 `db.set`）或 console 级别来源。
    pub method: String,
    /// 入参摘要（JSON 文本，超长截断）。
    pub args: String,
    /// 返回值 / 错误摘要（JSON 文本，超长截断）。
    pub result: String,
    /// 耗时（毫秒）。非 API 类为 0。
    pub ms: f64,
    /// 是否成功（`kind="error"` 一律 false）。
    pub ok: bool,
}

impl DevLogEntry {
    /// 后端自用的构造器（会话打开、Mock 同步等探针看不到的事件）。
    pub fn backend(plugin_id: &str, kind: &str, level: &str, method: &str, detail: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_string(),
            kind: kind.to_string(),
            level: level.to_string(),
            method: method.to_string(),
            result: detail.to_string(),
            ok: level != "error",
            ..Default::default()
        }
    }
}

/// 按字符截断（不在多字节中间切断，避免产生半个汉字）。
fn clip(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_TEXT).collect();
    out.push_str("…（已截断）");
    out
}

/// 日志环形缓冲（managed state 的一部分，见 [`super::DevRuntime::logs`]）。
#[derive(Default)]
pub struct DevLogs {
    inner: Mutex<Ring>,
    /// 推送事件用的句柄，`setup` 阶段 [`DevLogs::attach`] 注入。
    /// 未注入时（单测、以及 `setup` 完成前）只写缓冲、不推事件——绝不 panic。
    app: OnceLock<AppHandle>,
}

#[derive(Default)]
struct Ring {
    /// 下一条要分配的序号（从 1 开始；0 是「还没有任何日志」的哨兵）。
    next_seq: u64,
    buf: VecDeque<DevLogEntry>,
}

impl DevLogs {
    /// 注入 [`AppHandle`]（`setup` 阶段调用一次）。之后每批新日志都会给管理中心推
    /// [`DEV_LOG_EVENT`]。重复调用被忽略（`OnceLock` 语义），不会 panic。
    pub fn attach(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }

    /// 批量写入：**后端统一分配 seq 与缺省时间**，并对超长文本截断。返回写入后的最大 seq。
    ///
    /// 为什么不信上报方的 seq：探针在页面里，页面刷新即归零，多个窗口/多次刷新会产生重复
    /// 序号，前端的增量拉取就再也对不齐了。
    pub fn push_many(&self, entries: Vec<DevLogEntry>) -> u64 {
        // 事件必须在**释放锁之后**再推：emit 会走到 Tauri 的事件系统，持锁期间做这件事
        // 一旦对端回调里再读日志就是自死锁。
        let (max_seq, wrote) = {
            let Ok(mut g) = self.inner.lock() else {
                return 0;
            };
            let mut wrote = false;
            for mut e in entries {
                g.next_seq += 1;
                e.seq = g.next_seq;
                if e.at.trim().is_empty() {
                    e.at = crate::plugin::install::now_rfc3339();
                }
                e.args = clip(&e.args);
                e.result = clip(&e.result);
                if g.buf.len() >= CAPACITY {
                    g.buf.pop_front();
                }
                g.buf.push_back(e);
                wrote = true;
            }
            (g.next_seq, wrote)
        };
        if wrote {
            if let Some(app) = self.app.get() {
                // 投递失败（管理中心没开着）不是错误：前端下次打开面板会用 after_seq=0 全量补齐。
                let _ = app.emit_to(ADMIN_WINDOW_LABEL, DEV_LOG_EVENT, ());
            }
        }
        max_seq
    }

    /// 写一条（后端补记用）。
    pub fn push(&self, entry: DevLogEntry) {
        self.push_many(vec![entry]);
    }

    /// 取 `seq > after_seq` 的日志（升序）。前端据此增量拉取。
    pub fn list_after(&self, after_seq: u64) -> Vec<DevLogEntry> {
        self.inner
            .lock()
            .map(|g| {
                g.buf
                    .iter()
                    .filter(|e| e.seq > after_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 清空缓冲。**seq 不回退**——否则前端手里的 after_seq 会比新日志还大，永远拉不到新内容。
    pub fn clear(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.buf.clear();
        }
    }
}

// ==================== 命令：探针上报（**只在 capabilities/plugin-dev.json 放行**） ====================

/// bridge 探针批量上报调试日志。
///
/// 三点服务端加固（都不影响探针的写法，反而让它更简单）：
/// 1. **只接受调试窗口的上报**：正式插件窗口即便被授予了这个命令也会被拒——调试日志是
///    开发者中心的视图，不该被正式插件写脏；
/// 2. **plugin_id 由后端按会话盖章**：探针填什么都会被覆盖成当前调试会话的插件 id，
///    页面伪造不了归属（探针可以直接留空）；
/// 3. **单批上限**：被调试的页面是「正在开发、随时可能写崩」的代码，一次推来几十万条不是
///    不可能。超出环形缓冲容量的部分本来就会被立刻挤掉，这里直接只留最后 [`CAPACITY`] 条，
///    并**如实记一条**说明丢了多少——不静默吞。
#[tauri::command]
pub fn dev_log_push(
    entries: Vec<DevLogEntry>,
    webview: tauri::Webview,
    registry: tauri::State<'_, crate::plugin::PluginRegistry>,
) -> Result<(), String> {
    let session = registry
        .session_for(webview.label())
        .ok_or_else(|| "没有正在运行的插件".to_string())?;
    if !session.dev {
        return Err("调试日志只接受插件调试窗口的上报".to_string());
    }
    registry
        .dev
        .logs
        .push_many(trim_batch(entries, &session.id));
    Ok(())
}

/// 按上限裁剪单批上报，并给被裁掉的部分补一条说明（返回值已盖好 plugin_id）。
fn trim_batch(entries: Vec<DevLogEntry>, plugin_id: &str) -> Vec<DevLogEntry> {
    let total = entries.len();
    let mut out: Vec<DevLogEntry> = entries
        .into_iter()
        .skip(total.saturating_sub(CAPACITY))
        .map(|mut e| {
            e.plugin_id = plugin_id.to_string();
            e
        })
        .collect();
    if total > CAPACITY {
        out.insert(
            0,
            DevLogEntry::backend(
                plugin_id,
                "console",
                "warn",
                "probe.batchTooLarge",
                &format!(
                    "单批上报 {total} 条，超过缓冲容量 {CAPACITY}，只保留最后 {CAPACITY} 条"
                ),
            ),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(method: &str) -> DevLogEntry {
        DevLogEntry::backend("demo", "api", "info", method, "")
    }

    /// seq 由后端分配、单调递增；增量拉取按 seq 过滤。
    #[test]
    fn seq_is_assigned_by_backend_and_monotonic() {
        let logs = DevLogs::default();
        // 上报方填了乱七八糟的 seq，一律被覆盖
        let mut a = entry("db.set");
        a.seq = 999;
        let mut b = entry("db.get");
        b.seq = 0;
        logs.push_many(vec![a, b]);
        let all = logs.list_after(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 2);
        assert!(!all[0].at.is_empty(), "缺省时间必须由后端补上");
        // 增量
        assert_eq!(logs.list_after(1).len(), 1);
        assert_eq!(logs.list_after(2).len(), 0);
    }

    /// 清空后 seq 不回退（否则前端 after_seq 会永远大于新日志）。
    #[test]
    fn clear_keeps_seq_monotonic() {
        let logs = DevLogs::default();
        logs.push_many(vec![entry("a"), entry("b")]);
        logs.clear();
        assert!(logs.list_after(0).is_empty());
        logs.push(entry("c"));
        let after = logs.list_after(2);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq, 3);
    }

    /// 探针只填部分字段也能被接收（缺的走 serde 默认值，绝不整批失败）。
    #[test]
    fn partial_entry_from_probe_deserializes() {
        let e: DevLogEntry =
            serde_json::from_str(r#"{"kind":"console","level":"warn","method":"console.warn"}"#)
                .expect("探针可以只填它知道的字段");
        assert_eq!(e.kind, "console");
        assert_eq!(e.seq, 0);
        assert!(e.at.is_empty(), "时间留空由后端补");
        assert!(!e.ok, "ok 缺省为 false —— 探针要显式填 true");
    }

    /// 超长入参被截断（否则一次 base64 大图就能把缓冲撑爆）。
    #[test]
    fn long_text_is_clipped() {
        let logs = DevLogs::default();
        let mut e = entry("write.file");
        e.args = "x".repeat(MAX_TEXT * 3);
        logs.push(e);
        let got = &logs.list_after(0)[0];
        assert!(got.args.chars().count() <= MAX_TEXT + 8);
        assert!(got.args.ends_with("（已截断）"));
    }

    /// 环形缓冲超出容量后丢最旧的，不会无限增长。
    #[test]
    fn ring_drops_oldest_over_capacity() {
        let logs = DevLogs::default();
        let batch: Vec<DevLogEntry> = (0..CAPACITY + 10).map(|_| entry("x")).collect();
        logs.push_many(batch);
        let all = logs.list_after(0);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all[0].seq, 11, "最旧的 10 条被丢弃");
    }

    /// 超大单批被裁到容量上限，并**如实**补一条说明（不静默吞）。
    #[test]
    fn oversized_batch_is_trimmed_with_notice() {
        let mut batch: Vec<DevLogEntry> = (0..CAPACITY + 5).map(|_| entry("x")).collect();
        batch.last_mut().unwrap().method = "最后一条".to_string();
        let out = trim_batch(batch, "demo");
        assert_eq!(out.len(), CAPACITY + 1, "裁到上限 + 1 条说明");
        assert_eq!(out[0].method, "probe.batchTooLarge");
        assert!(out[0].result.contains(&(CAPACITY + 5).to_string()));
        assert_eq!(out.last().unwrap().method, "最后一条", "保留的是最后一批");
    }

    /// 正常大小的批原样通过，且 plugin_id 一律由后端盖章（探针伪造不了归属）。
    #[test]
    fn normal_batch_keeps_all_and_stamps_plugin_id() {
        let mut a = entry("db.set");
        a.plugin_id = "我伪造的插件".to_string();
        let out = trim_batch(vec![a, entry("db.get")], "real-id");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|e| e.plugin_id == "real-id"));
    }

    /// 探针的上报通道**绝不能**经过它自己钩住的门面 invoke（否则每次上报又生成一条日志 → 无限自激）。
    /// bridge.js 是注入脚本、无法在 Rust 单测里执行，这里退而求其次做**源码级**回归防线。
    #[test]
    fn bridge_probe_reports_through_raw_channel() {
        let js = crate::plugin::commands::BRIDGE_JS;
        assert!(
            js.contains("rawInvoke(\"dev_log_push\""),
            "上报必须走绕过探针的 rawInvoke"
        );
        // 大小写敏感：`rawInvoke("dev_log_push"` 里是大写 I，不会误命中这一条
        assert!(
            !js.contains("invoke(\"dev_log_push\""),
            "不允许用门面 invoke 上报（会被探针自己记录 → 自我递归）"
        );
        assert!(
            js.contains("__ITOOLS_DEBUG__"),
            "探针必须由调试窗口标志开关，正式插件窗口零开销"
        );
    }
}
