//! 终端用户（iTools 云账号）的查询与运营动作。
//!
//! ## 时间单位
//!
//! 主服务的全部时间戳都是**毫秒**（`server/src/lib.rs` 的 `system_clock` 用
//! `chrono::Utc::now().timestamp_millis()`）。本模块及控制台自己的表一律沿用毫秒，
//! 任何按天/按小时分桶都必须先除以 1000——搞错一次，整个统计面板就全是错的。
//!
//! ## 隐私红线
//!
//! `data_records.v` 是用户真实的同步内容（剪贴板、插件数据等）。控制台
//! **只统计条数与字节数，绝不读取、绝不展示 value**。本文件里没有任何
//! `SELECT v` —— 只有 `LENGTH(v)`。这条不是风格问题，是红线。

use sqlx::Row;

use super::{escape_like, Page, Store, StoreResult};

/// 用户列表里的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    pub username: String,
    pub created_at: i64,
    /// 当前存在的会话数（= 主服务 `sessions` 表里该用户的行数）。
    /// 注意主服务的会话**永不过期**，所以这更接近「历史登录设备数」而非「当前在线数」。
    pub session_count: i64,
    /// 最近一次会话创建时间。用户登出会删行，所以它是「现存会话里最新的一条」，
    /// **不等于最后登录时间**——UI 上必须如实标注。
    pub last_session_at: i64,
    pub record_count: i64,
    pub bytes: i64,
    /// 该用户在市场上线的插件数（含已下架）。
    pub plugin_count: i64,
}

/// 用户详情里的一个命名空间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NsUsage {
    pub ns: String,
    pub count: i64,
    pub bytes: i64,
    pub last_updated_at: i64,
}

/// 用户详情里的一条会话。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSession {
    pub created_at: i64,
}

/// 列表排序字段。**白名单**——`ORDER BY` 不能参数化绑定，
/// 直接把前端字符串拼进 SQL 就是注入口子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserSort {
    CreatedAt,
    Username,
    Bytes,
    Records,
    Sessions,
}

impl UserSort {
    pub fn parse(s: &str) -> Self {
        match s {
            "username" => Self::Username,
            "bytes" => Self::Bytes,
            "records" => Self::Records,
            "sessions" => Self::Sessions,
            // 未知值一律回落到默认，不报错——排序字段写错不该让整页打不开
            _ => Self::CreatedAt,
        }
    }

    /// 拼进 SQL 的列表达式。这些都是本文件里的字面量，不含任何外部输入。
    fn column(self) -> &'static str {
        match self {
            Self::CreatedAt => "u.created_at",
            Self::Username => "u.username",
            Self::Bytes => "bytes",
            Self::Records => "record_count",
            Self::Sessions => "session_count",
        }
    }
}

fn direction(desc: bool) -> &'static str {
    if desc {
        "DESC"
    } else {
        "ASC"
    }
}

impl Store {
    /// 用户列表。搜索按用户名模糊匹配。
    ///
    /// 聚合子查询会全表扫 `data_records`。当前用户与数据规模下（几十个账号）
    /// 这毫无压力；若某天涨到十万级，这里要改成物化统计表——届时应当由主服务
    /// 在写入时维护，而不是控制台每次现算。
    pub async fn list_users(
        &self,
        query: Option<&str>,
        sort: UserSort,
        desc: bool,
        page: Page,
    ) -> StoreResult<(Vec<UserRow>, i64)> {
        let like = query.map(|q| format!("%{}%", escape_like(q)));

        let total: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*) FROM users u WHERE (? IS NULL OR u.username LIKE ? ESCAPE '\\')",
        )
        .bind(like.as_deref())
        .bind(like.as_deref())
        .fetch_one(self.pool())
        .await?;

        // SUM() 在 MariaDB 里返回 DECIMAL，必须 CAST 成 SIGNED 才能稳定解码成 i64。
        let sql = format!(
            r"SELECT u.username,
                     u.created_at,
                     COALESCE(s.cnt, 0)          AS session_count,
                     COALESCE(s.last_at, 0)      AS last_session_at,
                     COALESCE(d.cnt, 0)          AS record_count,
                     COALESCE(d.bytes, 0)        AS bytes,
                     COALESCE(m.cnt, 0)          AS plugin_count
              FROM users u
              LEFT JOIN (
                  SELECT username, COUNT(*) AS cnt, MAX(created_at) AS last_at
                  FROM sessions GROUP BY username
              ) s ON s.username = u.username
              LEFT JOIN (
                  SELECT username, COUNT(*) AS cnt,
                         CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) AS bytes
                  FROM data_records GROUP BY username
              ) d ON d.username = u.username
              LEFT JOIN (
                  SELECT owner, COUNT(*) AS cnt FROM market_entries GROUP BY owner
              ) m ON m.owner = u.username
              WHERE (? IS NULL OR u.username LIKE ? ESCAPE '\\')
              ORDER BY {} {}, u.username ASC
              LIMIT ? OFFSET ?",
            sort.column(),
            direction(desc)
        );

        let rows = sqlx::query(&sql)
            .bind(like.as_deref())
            .bind(like.as_deref())
            .bind(page.size)
            .bind(page.offset)
            .fetch_all(self.pool())
            .await?;

        let list = rows
            .into_iter()
            .map(|r| UserRow {
                username: r.get("username"),
                created_at: r.get("created_at"),
                session_count: r.get("session_count"),
                last_session_at: r.get("last_session_at"),
                record_count: r.get("record_count"),
                bytes: r.get("bytes"),
                plugin_count: r.get("plugin_count"),
            })
            .collect();
        Ok((list, total))
    }

    /// 单个用户是否存在。
    pub async fn user_exists(&self, username: &str) -> StoreResult<bool> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(self.pool())
            .await?;
        Ok(n > 0)
    }

    pub async fn get_user_row(&self, username: &str) -> StoreResult<Option<UserRow>> {
        let (list, _) = self
            .list_users(Some(username), UserSort::CreatedAt, false, Page::new(1, 200))
            .await?;
        // 模糊搜索可能命中多个，取精确相等的那个
        Ok(list.into_iter().find(|u| u.username == username))
    }

    /// 某用户按命名空间的用量明细。**只统计，不读内容。**
    pub async fn user_ns_usage(&self, username: &str) -> StoreResult<Vec<NsUsage>> {
        let rows = sqlx::query(
            "SELECT ns,
                    COUNT(*) AS c,
                    CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) AS b,
                    COALESCE(MAX(updated_at), 0) AS last_at
             FROM data_records WHERE username = ?
             GROUP BY ns ORDER BY b DESC",
        )
        .bind(username)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| NsUsage {
                ns: r.get("ns"),
                count: r.get("c"),
                bytes: r.get("b"),
                last_updated_at: r.get("last_at"),
            })
            .collect())
    }

    /// 某用户的会话列表（只有创建时间——主服务没记 IP 和 UA）。
    pub async fn user_sessions(&self, username: &str) -> StoreResult<Vec<UserSession>> {
        let rows = sqlx::query(
            "SELECT created_at FROM sessions WHERE username = ? ORDER BY created_at DESC LIMIT 100",
        )
        .bind(username)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| UserSession {
                created_at: r.get("created_at"),
            })
            .collect())
    }

    // ---------- 运营动作 ----------

    /// 强制某用户下线：删掉他在主服务的全部会话。
    ///
    /// **立即生效**——主服务每个受保护端点都会实时 `SELECT username FROM sessions
    /// WHERE token = ?`（`server/src/routes.rs` 的 `authenticate`），行没了下一个请求就 401。
    /// 返回删掉的会话数。
    ///
    /// 注意这**不阻止他重新登录**。要真正封停账号需要 `users.status` 禁用位，
    /// 那是主服务侧的改动（见 `Caps::users_status`），控制台不会假装自己能做到。
    pub async fn kick_user(&self, username: &str) -> StoreResult<u64> {
        Ok(sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(self.pool())
            .await?
            .rows_affected())
    }

    /// 删除终端用户：同步数据 → 会话 → 账号，单事务。
    ///
    /// 顺序与删除范围**与主服务的 `/account/delete` 完全一致**
    /// （`server/src/store.rs:262` 的 `delete_user`），保证两条路径语义相同。
    ///
    /// 刻意**不动** `market_entries` 与 `plugin_submissions`：插件是已经分发给
    /// 其他用户的公共资产，作者销号不该让别人装着的东西凭空消失。这与主服务的
    /// 行为一致；如需下架请单独处置。
    pub async fn delete_user(&self, username: &str) -> StoreResult<bool> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("DELETE FROM data_records WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        let affected = sqlx::query("DELETE FROM users WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(affected > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_parsing_falls_back_to_default() {
        assert_eq!(UserSort::parse("bytes"), UserSort::Bytes);
        assert_eq!(UserSort::parse("username"), UserSort::Username);
        assert_eq!(UserSort::parse("sessions"), UserSort::Sessions);
        assert_eq!(UserSort::parse("records"), UserSort::Records);
        assert_eq!(UserSort::parse("created_at"), UserSort::CreatedAt);
        // 未知与恶意输入都回落默认，绝不进入 SQL
        assert_eq!(UserSort::parse("u.username; DROP TABLE users--"), UserSort::CreatedAt);
        assert_eq!(UserSort::parse(""), UserSort::CreatedAt);
    }

    #[test]
    fn sort_columns_are_static_literals() {
        // 每个排序项映射到的都是本文件里写死的列名，不含任何外部输入
        for s in [
            UserSort::CreatedAt,
            UserSort::Username,
            UserSort::Bytes,
            UserSort::Records,
            UserSort::Sessions,
        ] {
            let c = s.column();
            assert!(!c.contains(' '), "列表达式不该含空格：{c}");
            assert!(!c.contains(';'), "列表达式不该含分号：{c}");
        }
        assert_eq!(direction(true), "DESC");
        assert_eq!(direction(false), "ASC");
    }
}
