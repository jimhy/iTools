//! 持久化存储：MariaDB（sqlx 连接池）。服务端强制依赖 MariaDB。
//!
//! last-write-wins 由 `updated_at`（大者胜；相等以推送为准）决定，与旧 Node 版一致。

use std::collections::HashMap;

use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::{Connection, Executor, MySqlConnection, MySqlPool, Row};

use crate::config::DbConfig;

/// 一个用户账号（口令只存哈希 + 盐，绝不明文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub username: String,
    pub password_hash: String,
    pub salt: String,
    pub created_at: i64,
}

/// 一条同步数据记录（value 为任意 JSON；updated_at 由客户端提供，用于 last-write-wins）。
#[derive(Debug, Clone, PartialEq)]
pub struct DataRecord {
    pub value: Value,
    pub updated_at: i64,
}

/// 一条线上数据记录（与客户端 `WireRecord` 对齐）。
#[derive(Debug, Clone, PartialEq)]
pub struct WireRecord {
    pub key: String,
    pub value: Value,
    pub updated_at: i64,
}

const CREATE_USERS: &str = "\
  CREATE TABLE IF NOT EXISTS users (
    username VARCHAR(190) PRIMARY KEY,
    password_hash TEXT NOT NULL,
    salt VARCHAR(64) NOT NULL,
    created_at BIGINT NOT NULL
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

const CREATE_SESSIONS: &str = "\
  CREATE TABLE IF NOT EXISTS sessions (
    token VARCHAR(190) PRIMARY KEY,
    username VARCHAR(190) NOT NULL,
    created_at BIGINT NOT NULL,
    INDEX idx_sessions_username (username)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

const CREATE_DATA: &str = "\
  CREATE TABLE IF NOT EXISTS data_records (
    username VARCHAR(190) NOT NULL,
    ns VARCHAR(190) NOT NULL,
    k VARCHAR(255) NOT NULL,
    v LONGTEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (username, ns, k)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 提审单。一次提审 = 一行，**永不覆盖**：作者能看到自己每一次提交的结论，
/// 包括被驳回的那几次和驳回原因。
const CREATE_SUBMISSIONS: &str = "\
  CREATE TABLE IF NOT EXISTS plugin_submissions (
    id VARCHAR(40) PRIMARY KEY,
    name VARCHAR(190) NOT NULL,
    version VARCHAR(64) NOT NULL,
    author VARCHAR(190) NOT NULL,
    status VARCHAR(24) NOT NULL,
    content_hash VARCHAR(80) NOT NULL,
    file_count INT NOT NULL,
    size_bytes BIGINT NOT NULL,
    manifest LONGTEXT NOT NULL,
    review LONGTEXT NOT NULL,
    message TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    INDEX idx_sub_author (author),
    INDEX idx_sub_name (name)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 已上线的市场条目（一个插件一行，新版本覆盖同一行）。
///
/// `owner` 是**安全字段**：同名插件只能由首次上线它的那个账号更新。没有这一列，
/// 任何登录用户都能提交一个同名包把别人的插件顶掉——而客户端那边是按插件名归属
/// 授权与数据的，顶包等于直接继承受害插件的用户授权。
const CREATE_MARKET: &str = "\
  CREATE TABLE IF NOT EXISTS market_entries (
    name VARCHAR(190) PRIMARY KEY,
    owner VARCHAR(190) NOT NULL,
    version VARCHAR(64) NOT NULL,
    content_hash VARCHAR(80) NOT NULL,
    package_file VARCHAR(255) NOT NULL,
    entry LONGTEXT NOT NULL,
    revoked TINYINT NOT NULL DEFAULT 0,
    revoked_reason TEXT NOT NULL,
    published_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 一条提审单。
#[derive(Debug, Clone, PartialEq)]
pub struct Submission {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    /// `reviewing` 审核中 | `approved` 已上线 | `rejected` 已驳回 | `manual` 待人工处理 | `failed` 校验未通过
    pub status: String,
    pub content_hash: String,
    pub file_count: i64,
    pub size_bytes: i64,
    /// `plugin.json` 原文
    pub manifest: String,
    /// 模型裁决原文（JSON），未审 / 审失败时为空串
    pub review: String,
    /// 给作者看的一句话结论 / 驳回原因
    pub message: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 一条已上线的市场条目。
#[derive(Debug, Clone, PartialEq)]
pub struct MarketRow {
    pub name: String,
    pub owner: String,
    pub version: String,
    pub content_hash: String,
    pub package_file: String,
    /// 客户端 `MarketEntry` 形状的 JSON 文本
    pub entry: String,
    pub revoked: bool,
    pub revoked_reason: String,
    pub published_at: i64,
    pub updated_at: i64,
}

pub struct MariaDbStore {
    pool: MySqlPool,
}

fn base_options(cfg: &DbConfig) -> MySqlConnectOptions {
    MySqlConnectOptions::new()
        .host(&cfg.host)
        .port(cfg.port)
        .username(&cfg.user)
        .password(&cfg.password)
        .charset("utf8mb4")
}

impl MariaDbStore {
    /// 连接 + 建库建表（幂等）。连不上或建表失败会返回 Err（由调用方处理并退出）。
    pub async fn connect(cfg: &DbConfig) -> Result<Self, sqlx::Error> {
        // 先用一条不指定库的连接确保目标库存在（幂等），避免库未建时直接连库报错。
        let mut bootstrap = MySqlConnection::connect_with(&base_options(cfg)).await?;
        let safe = cfg.database.replace('`', "``");
        bootstrap
            .execute(format!("CREATE DATABASE IF NOT EXISTS `{safe}` CHARACTER SET utf8mb4").as_str())
            .await?;
        bootstrap.close().await?;

        let pool = MySqlPoolOptions::new()
            .max_connections(cfg.connection_limit.max(1))
            .connect_with(base_options(cfg).database(&cfg.database))
            .await?;

        for ddl in [CREATE_USERS, CREATE_SESSIONS, CREATE_DATA, CREATE_SUBMISSIONS, CREATE_MARKET] {
            pool.execute(ddl).await?;
        }
        Ok(Self { pool })
    }

    /// 构造一个**懒连接**的存储：直到真的执行 SQL 时才尝试连库。
    ///
    /// 只用于「不碰存储层的端点」的测试（如 `/api/mirrors`）——那些用例本就不该访问数据库，
    /// 一旦误访问会立刻报连接错误而不是静默通过。
    pub fn lazy(cfg: &DbConfig) -> Self {
        let pool = MySqlPoolOptions::new()
            .max_connections(cfg.connection_limit.max(1))
            .connect_lazy_with(base_options(cfg).database(&cfg.database));
        Self { pool }
    }

    /// 关闭连接池（进程退出 / 测试收尾）。
    pub async fn close(&self) {
        self.pool.close().await;
    }

    // ---------- 用户 ----------

    pub async fn get_user(&self, username: &str) -> Result<Option<UserRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT username, password_hash, salt, created_at FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| UserRecord {
            username: r.get("username"),
            password_hash: r.get("password_hash"),
            salt: r.get("salt"),
            created_at: r.get("created_at"),
        }))
    }

    /// set 语义（与旧实现一致）：同名覆盖，避免并发下的重复键报错。
    pub async fn create_user(&self, user: &UserRecord) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO users (username, password_hash, salt, created_at) VALUES (?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE password_hash = VALUES(password_hash), salt = VALUES(salt), created_at = VALUES(created_at)",
        )
        .bind(&user.username)
        .bind(&user.password_hash)
        .bind(&user.salt)
        .bind(user.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 删除用户及其全部数据与会话（账号注销），事务保证一致性。
    pub async fn delete_user(&self, username: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM data_records WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM users WHERE username = ?")
            .bind(username)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    // ---------- 会话 ----------

    pub async fn create_session(
        &self,
        token: &str,
        username: &str,
        now_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO sessions (token, username, created_at) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE username = VALUES(username), created_at = VALUES(created_at)",
        )
        .bind(token)
        .bind(username)
        .bind(now_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn session_user(&self, token: &str) -> Result<Option<String>, sqlx::Error> {
        if token.is_empty() {
            return Ok(None);
        }
        let row = sqlx::query("SELECT username FROM sessions WHERE token = ?")
            .bind(token)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get("username")))
    }

    pub async fn delete_session(&self, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE token = ?")
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_user_sessions(&self, username: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM sessions WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---------- 数据 ----------

    /// 取某用户某命名空间的全部记录（不存在 → 空表）。
    pub async fn get_data(
        &self,
        username: &str,
        ns: &str,
    ) -> Result<HashMap<String, DataRecord>, sqlx::Error> {
        let rows = sqlx::query("SELECT k, v, updated_at FROM data_records WHERE username = ? AND ns = ?")
            .bind(username)
            .bind(ns)
            .fetch_all(&self.pool)
            .await?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let k: String = r.get("k");
            let v: String = r.get("v");
            // 库里存的就是 JSON 文本；解析不了说明数据被外部改坏了，
            // 此时诚实报错（500）比塞个 null 蒙混过去要好——后者会让客户端把坏数据同步回本地。
            let value: Value = serde_json::from_str(&v).map_err(|e| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("data_records({username}, {ns}, {k}).v 不是合法 JSON: {e}"),
                )))
            })?;
            out.insert(k, DataRecord { value, updated_at: r.get("updated_at") });
        }
        Ok(out)
    }

    /// 用量统计：某用户各命名空间的记录条数与占用字节数（`LENGTH(v)` 为真实字节数，非估算）。
    pub async fn usage(&self, username: &str) -> Result<(HashMap<String, i64>, i64), sqlx::Error> {
        // SUM(...) 在 MySQL/MariaDB 里是 DECIMAL，显式 CAST 成 SIGNED 才能稳定解码成 i64。
        let rows = sqlx::query(
            "SELECT ns, COUNT(*) AS c, CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) AS b
             FROM data_records WHERE username = ? GROUP BY ns",
        )
        .bind(username)
        .fetch_all(&self.pool)
        .await?;
        let mut counts = HashMap::with_capacity(rows.len());
        let mut bytes = 0i64;
        for r in rows {
            counts.insert(r.get::<String, _>("ns"), r.get::<i64, _>("c"));
            bytes += r.get::<i64, _>("b");
        }
        Ok((counts, bytes))
    }

    /// 上行合并：对每条推送记录做 last-write-wins（updated_at 大者胜；相等以推送为准覆盖）。
    pub async fn upsert_data(
        &self,
        username: &str,
        ns: &str,
        records: &[WireRecord],
    ) -> Result<(), sqlx::Error> {
        if records.is_empty() {
            return Ok(());
        }
        let values = vec!["(?, ?, ?, ?, ?)"; records.len()].join(", ");
        let sql = format!(
            "INSERT INTO data_records (username, ns, k, v, updated_at) VALUES {values}
             ON DUPLICATE KEY UPDATE
               v = IF(VALUES(updated_at) >= updated_at, VALUES(v), v),
               updated_at = GREATEST(updated_at, VALUES(updated_at))"
        );
        let mut q = sqlx::query(&sql);
        for r in records {
            // 值恒可序列化（它本就是从请求体里解析出来的 JSON）。
            let v = serde_json::to_string(&r.value).unwrap_or_else(|_| "null".into());
            q = q.bind(username).bind(ns).bind(&r.key).bind(v).bind(r.updated_at);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    // ---------- 提审单 ----------

    pub async fn create_submission(&self, s: &Submission) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO plugin_submissions
               (id, name, version, author, status, content_hash, file_count, size_bytes,
                manifest, review, message, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(&s.name)
        .bind(&s.version)
        .bind(&s.author)
        .bind(&s.status)
        .bind(&s.content_hash)
        .bind(s.file_count)
        .bind(s.size_bytes)
        .bind(&s.manifest)
        .bind(&s.review)
        .bind(&s.message)
        .bind(s.created_at)
        .bind(s.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// 审核结束后回填结论。**只改结论字段**，不动提交时记录的包信息。
    pub async fn finish_submission(
        &self,
        id: &str,
        status: &str,
        message: &str,
        review: &str,
        now_ms: i64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE plugin_submissions SET status = ?, message = ?, review = ?, updated_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(message)
        .bind(review)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_submission(&self, id: &str) -> Result<Option<Submission>, sqlx::Error> {
        let row = sqlx::query(SUBMISSION_COLUMNS_SELECT.as_str())
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_submission))
    }

    /// 某作者的提审记录（新→旧）。
    pub async fn list_submissions(
        &self,
        author: &str,
        limit: u32,
    ) -> Result<Vec<Submission>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, name, version, author, status, content_hash, file_count, size_bytes,
                    manifest, review, message, created_at, updated_at
             FROM plugin_submissions WHERE author = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(author)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_submission).collect())
    }

    /// 仍停在「审核中」且创建于 `before_ms` 之前的提审单 id。
    ///
    /// 审核任务活在进程内存里，进程一没它就永远不会有结论——启动时必须把这些单子如实改判，
    /// 不能留一个永远转圈的状态给作者。
    pub async fn stale_reviewing(&self, before_ms: i64) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id FROM plugin_submissions WHERE status = 'reviewing' AND created_at <= ?",
        )
        .bind(before_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// 该作者最近一次提审的时间（做冷却判定用）。
    pub async fn last_submit_at(&self, author: &str) -> Result<Option<i64>, sqlx::Error> {
        let row = sqlx::query("SELECT MAX(created_at) AS t FROM plugin_submissions WHERE author = ?")
            .bind(author)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.get::<Option<i64>, _>("t")))
    }

    // ---------- 市场条目 ----------

    /// 发布 / 更新一个条目（同名覆盖，`owner` 与 `published_at` 保持首次值）。
    pub async fn publish_entry(&self, e: &MarketRow) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO market_entries
               (name, owner, version, content_hash, package_file, entry, revoked, revoked_reason,
                published_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0, '', ?, ?)
             ON DUPLICATE KEY UPDATE
               version = VALUES(version),
               content_hash = VALUES(content_hash),
               package_file = VALUES(package_file),
               entry = VALUES(entry),
               revoked = 0,
               revoked_reason = '',
               updated_at = VALUES(updated_at)",
        )
        .bind(&e.name)
        .bind(&e.owner)
        .bind(&e.version)
        .bind(&e.content_hash)
        .bind(&e.package_file)
        .bind(&e.entry)
        .bind(e.published_at)
        .bind(e.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_entry(&self, name: &str) -> Result<Option<MarketRow>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT name, owner, version, content_hash, package_file, entry, revoked, revoked_reason,
                    published_at, updated_at FROM market_entries WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_entry))
    }

    /// 全部条目（含已下架的——客户端要靠 `revoked` 提示已安装该插件的用户）。
    pub async fn list_entries(&self) -> Result<Vec<MarketRow>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT name, owner, version, content_hash, package_file, entry, revoked, revoked_reason,
                    published_at, updated_at FROM market_entries ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    /// 下架 / 恢复。下架原因会原样展示给已安装该插件的用户，所以调用方必须给出真实原因。
    pub async fn set_revoked(
        &self,
        name: &str,
        revoked: bool,
        reason: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE market_entries SET revoked = ?, revoked_reason = ?, updated_at = ? WHERE name = ?",
        )
        .bind(if revoked { 1 } else { 0 })
        .bind(reason)
        .bind(now_ms)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

/// 单条提审单的查询语句（按 id）。抽成常量避免与 [`row_to_submission`] 的列顺序分叉。
static SUBMISSION_COLUMNS_SELECT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    "SELECT id, name, version, author, status, content_hash, file_count, size_bytes,
            manifest, review, message, created_at, updated_at
     FROM plugin_submissions WHERE id = ?"
        .to_string()
});

fn row_to_submission(r: sqlx::mysql::MySqlRow) -> Submission {
    Submission {
        id: r.get("id"),
        name: r.get("name"),
        version: r.get("version"),
        author: r.get("author"),
        status: r.get("status"),
        content_hash: r.get("content_hash"),
        file_count: r.get::<i32, _>("file_count") as i64,
        size_bytes: r.get("size_bytes"),
        manifest: r.get("manifest"),
        review: r.get("review"),
        message: r.get("message"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn row_to_entry(r: sqlx::mysql::MySqlRow) -> MarketRow {
    MarketRow {
        name: r.get("name"),
        owner: r.get("owner"),
        version: r.get("version"),
        content_hash: r.get("content_hash"),
        package_file: r.get("package_file"),
        entry: r.get("entry"),
        revoked: r.get::<i8, _>("revoked") != 0,
        revoked_reason: r.get("revoked_reason"),
        published_at: r.get("published_at"),
        updated_at: r.get("updated_at"),
    }
}
