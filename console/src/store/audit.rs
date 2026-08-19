//! 操作审计。控制台的**每一个写操作**都在这里留一行，成功失败都记。
//!
//! 主服务至今一条审计都没有（`server/` 里 revoke、删号都不留痕），
//! 控制台是权限更高的运营入口，不能沿用那个状态。

use sqlx::Row;

use super::{Page, Store, StoreResult};

/// `console_audit_log.action` 的取值。集中在这里定义，避免各处手写字符串写飘。
pub mod action {
    pub const LOGIN: &str = "login";
    pub const LOGIN_FAILED: &str = "login_failed";
    pub const LOGOUT: &str = "logout";
    pub const CHANGE_PASSWORD: &str = "change_password";
    pub const ADMIN_CREATE: &str = "admin_create";
    pub const ADMIN_DELETE: &str = "admin_delete";
    pub const ADMIN_SET_ROLE: &str = "admin_set_role";
    pub const ADMIN_SET_STATUS: &str = "admin_set_status";
    pub const ADMIN_RESET_PASSWORD: &str = "admin_reset_password";
    /// 强制某个终端用户下线（删其全部 `sessions`）
    pub const USER_KICK: &str = "user_kick";
    /// 删除某个终端用户（三张表级联）
    pub const USER_DELETE: &str = "user_delete";
}

/// 一条审计记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub ip: String,
    pub ok: bool,
}

impl Store {
    /// 写一条审计。
    ///
    /// 刻意**吞掉写入失败**（只打日志）：审计写不进去不该让业务操作跟着失败，
    /// 但也绝不能静悄悄——日志里必须留下来，运维才能发现审计链断了。
    ///
    /// 参数确实多（clippy 的阈值是 7）。包成结构体反而更难用：调用点分散在十几个
    /// handler 里，每处都要先造一个临时结构体，噪音比收益大。这里保持平铺。
    #[allow(clippy::too_many_arguments)]
    pub async fn audit(
        &self,
        actor: &str,
        action_name: &str,
        target: &str,
        detail: &str,
        ip: &str,
        ok: bool,
        now: i64,
    ) {
        let res = sqlx::query(
            "INSERT INTO console_audit_log (at, actor, action, target, detail, ip, ok)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now)
        .bind(actor)
        .bind(action_name)
        .bind(target)
        .bind(detail)
        .bind(ip)
        .bind(i8::from(ok))
        .execute(self.pool())
        .await;
        if let Err(e) = res {
            tracing::error!("[audit] 审计写入失败（action={action_name}）：{e}");
        }
    }

    pub async fn list_audit(
        &self,
        actor: Option<&str>,
        action_name: Option<&str>,
        page: Page,
    ) -> StoreResult<(Vec<AuditEntry>, i64)> {
        // 两个可选过滤条件用 `(? IS NULL OR col = ?)` 拼，避免动态拼 SQL 字符串。
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM console_audit_log
             WHERE (? IS NULL OR actor = ?) AND (? IS NULL OR action = ?)",
        )
        .bind(actor)
        .bind(actor)
        .bind(action_name)
        .bind(action_name)
        .fetch_one(self.pool())
        .await?;

        let rows = sqlx::query(
            "SELECT id, at, actor, action, target, detail, ip, ok
             FROM console_audit_log
             WHERE (? IS NULL OR actor = ?) AND (? IS NULL OR action = ?)
             ORDER BY id DESC LIMIT ? OFFSET ?",
        )
        .bind(actor)
        .bind(actor)
        .bind(action_name)
        .bind(action_name)
        .bind(page.size)
        .bind(page.offset)
        .fetch_all(self.pool())
        .await?;

        let list = rows
            .into_iter()
            .map(|r| AuditEntry {
                id: r.get("id"),
                at: r.get("at"),
                actor: r.get("actor"),
                action: r.get("action"),
                target: r.get("target"),
                detail: r.get("detail"),
                ip: r.get("ip"),
                ok: r.get::<i8, _>("ok") != 0,
            })
            .collect();
        Ok((list, total))
    }

    /// 按保留期清理旧审计。默认不调用——审计日志是安全资产，
    /// 什么时候清、留多久应当由运维显式决定，而不是程序偷偷删。
    pub async fn purge_audit_before(&self, before: i64) -> StoreResult<u64> {
        Ok(sqlx::query("DELETE FROM console_audit_log WHERE at < ?")
            .bind(before)
            .execute(self.pool())
            .await?
            .rows_affected())
    }
}
