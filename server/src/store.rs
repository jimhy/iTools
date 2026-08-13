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

        for ddl in [CREATE_USERS, CREATE_SESSIONS, CREATE_DATA] {
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
}
