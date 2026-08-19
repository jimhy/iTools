//! 控制台管理员账号与会话。
//!
//! 与主服务的账号体系**完全隔离**：这里的用户名即便与某个 iTools 云账号同名，
//! 也是两个不相干的主体，口令、令牌、会话表都不共享。运营人员登控制台
//! 不需要、也不应该拥有任何终端用户账号。

use sqlx::Row;

use super::{role, AdminRecord, Page, Store, StoreResult};
use crate::auth;

/// 一条控制台会话（校验通过后的结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleSession {
    pub username: String,
    pub role: String,
    pub must_change_password: bool,
    pub expires_at: i64,
}

impl Store {
    // ---------- 管理员 ----------

    pub async fn get_admin(&self, username: &str) -> StoreResult<Option<AdminRecord>> {
        let row = sqlx::query(
            "SELECT username, password_hash, salt, role, status, must_change_password,
                    created_at, last_login_at, last_login_ip
             FROM console_admins WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| AdminRecord {
            username: r.get("username"),
            password_hash: r.get("password_hash"),
            salt: r.get("salt"),
            role: r.get("role"),
            status: r.get("status"),
            must_change_password: r.get::<i8, _>("must_change_password") != 0,
            created_at: r.get("created_at"),
            last_login_at: r.get("last_login_at"),
            last_login_ip: r.get("last_login_ip"),
        }))
    }

    pub async fn count_admins(&self) -> StoreResult<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM console_admins")
            .fetch_one(self.pool())
            .await
    }

    pub async fn list_admins(&self) -> StoreResult<Vec<AdminRecord>> {
        let rows = sqlx::query(
            "SELECT username, password_hash, salt, role, status, must_change_password,
                    created_at, last_login_at, last_login_ip
             FROM console_admins ORDER BY created_at ASC",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminRecord {
                username: r.get("username"),
                password_hash: r.get("password_hash"),
                salt: r.get("salt"),
                role: r.get("role"),
                status: r.get("status"),
                must_change_password: r.get::<i8, _>("must_change_password") != 0,
                created_at: r.get("created_at"),
                last_login_at: r.get("last_login_at"),
                last_login_ip: r.get("last_login_ip"),
            })
            .collect())
    }

    /// 新建管理员。用户名已存在时返回 `Ok(false)`，**不覆盖**——
    /// 覆盖等于给了「用建号接口悄悄改掉别人口令」的路子。
    pub async fn create_admin(
        &self,
        username: &str,
        password: &str,
        role_name: &str,
        must_change: bool,
        now: i64,
    ) -> StoreResult<bool> {
        let hashed = auth::hash_password(password);
        let affected = sqlx::query(
            "INSERT IGNORE INTO console_admins
             (username, password_hash, salt, role, status, must_change_password,
              created_at, last_login_at, last_login_ip)
             VALUES (?, ?, ?, ?, 'active', ?, ?, 0, '')",
        )
        .bind(username)
        .bind(&hashed.hash)
        .bind(&hashed.salt)
        .bind(role_name)
        .bind(i8::from(must_change))
        .bind(now)
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// 改口令，并清掉 `must_change_password`。
    pub async fn set_admin_password(&self, username: &str, password: &str) -> StoreResult<bool> {
        let hashed = auth::hash_password(password);
        let affected = sqlx::query(
            "UPDATE console_admins
             SET password_hash = ?, salt = ?, must_change_password = 0
             WHERE username = ?",
        )
        .bind(&hashed.hash)
        .bind(&hashed.salt)
        .bind(username)
        .execute(self.pool())
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    pub async fn set_admin_status(&self, username: &str, status: &str) -> StoreResult<bool> {
        let affected = sqlx::query("UPDATE console_admins SET status = ? WHERE username = ?")
            .bind(status)
            .bind(username)
            .execute(self.pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    pub async fn set_admin_role(&self, username: &str, role_name: &str) -> StoreResult<bool> {
        let affected = sqlx::query("UPDATE console_admins SET role = ? WHERE username = ?")
            .bind(role_name)
            .bind(username)
            .execute(self.pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 删除管理员，连同其全部会话。
    pub async fn delete_admin(&self, username: &str) -> StoreResult<bool> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM console_sessions WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        let affected = sqlx::query("DELETE FROM console_admins WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(affected > 0)
    }

    pub async fn touch_admin_login(&self, username: &str, now: i64, ip: &str) -> StoreResult<()> {
        sqlx::query("UPDATE console_admins SET last_login_at = ?, last_login_ip = ? WHERE username = ?")
            .bind(now)
            .bind(truncate_chars(ip, 64))
            .bind(username)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    // ---------- 会话 ----------

    /// 建会话。传入的是**明文令牌**，落库的是它的 sha256。
    pub async fn create_console_session(
        &self,
        token: &str,
        username: &str,
        now: i64,
        ttl_sec: i64,
        ip: &str,
        user_agent: &str,
    ) -> StoreResult<i64> {
        let expires_at = now + ttl_sec;
        sqlx::query(
            "INSERT INTO console_sessions (token_hash, username, created_at, expires_at, ip, user_agent)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(auth::token_digest(token))
        .bind(username)
        .bind(now)
        .bind(expires_at)
        .bind(truncate_chars(ip, 64))
        .bind(truncate_chars(user_agent, 255))
        .execute(self.pool())
        .await?;
        Ok(expires_at)
    }

    /// 校验令牌。过期、账号被停用、角色未知都会判失败。
    ///
    /// 每次都联表查 `console_admins`，而不是把角色缓存在会话里：
    /// 这样超管一改角色/停用账号，对方**下一个请求**就生效，不用等会话过期。
    pub async fn verify_console_session(
        &self,
        token: &str,
        now: i64,
    ) -> StoreResult<Option<ConsoleSession>> {
        let row = sqlx::query(
            "SELECT s.username, s.expires_at, a.role, a.status, a.must_change_password
             FROM console_sessions s
             JOIN console_admins a ON a.username = s.username
             WHERE s.token_hash = ?",
        )
        .bind(auth::token_digest(token))
        .fetch_optional(self.pool())
        .await?;

        let Some(r) = row else { return Ok(None) };
        let expires_at: i64 = r.get("expires_at");
        if expires_at <= now {
            return Ok(None);
        }
        let status: String = r.get("status");
        if status != "active" {
            return Ok(None);
        }
        let role_name: String = r.get("role");
        if !role::is_known(&role_name) {
            tracing::warn!("[auth] 管理员角色值未知，按拒绝处理");
            return Ok(None);
        }
        Ok(Some(ConsoleSession {
            username: r.get("username"),
            role: role_name,
            must_change_password: r.get::<i8, _>("must_change_password") != 0,
            expires_at,
        }))
    }

    pub async fn delete_console_session(&self, token: &str) -> StoreResult<()> {
        sqlx::query("DELETE FROM console_sessions WHERE token_hash = ?")
            .bind(auth::token_digest(token))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// 踢掉某个管理员的全部会话（改密、停用、删号时调用）。
    pub async fn delete_console_sessions_of(&self, username: &str) -> StoreResult<u64> {
        Ok(
            sqlx::query("DELETE FROM console_sessions WHERE username = ?")
                .bind(username)
                .execute(self.pool())
                .await?
                .rows_affected(),
        )
    }

    /// 回收过期会话。定时任务调用——不清理的话这张表只增不减，
    /// 主服务的 `sessions` 就是这么长成一张垃圾表的，别重蹈。
    pub async fn purge_expired_console_sessions(&self, now: i64) -> StoreResult<u64> {
        Ok(
            sqlx::query("DELETE FROM console_sessions WHERE expires_at <= ?")
                .bind(now)
                .execute(self.pool())
                .await?
                .rows_affected(),
        )
    }

    pub async fn count_active_console_sessions(&self, now: i64) -> StoreResult<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM console_sessions WHERE expires_at > ?")
            .bind(now)
            .fetch_one(self.pool())
            .await
    }

    /// 分页读审计日志之外，这里再给一个「某管理员的活跃会话」用于账号页展示。
    pub async fn list_console_sessions(&self, now: i64, page: Page) -> StoreResult<Vec<(String, i64, i64, String)>> {
        let rows = sqlx::query(
            "SELECT username, created_at, expires_at, ip
             FROM console_sessions WHERE expires_at > ?
             ORDER BY created_at DESC LIMIT ? OFFSET ?",
        )
        .bind(now)
        .bind(page.size)
        .bind(page.offset)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get("username"),
                    r.get("created_at"),
                    r.get("expires_at"),
                    r.get("ip"),
                )
            })
            .collect())
    }
}

/// 按**字符**截断（不是字节），避免把 UTF-8 多字节序列切一半塞进库里。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_never_splits_multibyte_chars() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        // 每个汉字 3 字节：按字符截断后仍是合法 UTF-8
        let s = truncate_chars(&"海风哥".repeat(50), 5);
        assert_eq!(s.chars().count(), 5);
        assert_eq!(s, "海风哥海风");
    }
}
