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
    /// [`user_status::ACTIVE`] | [`user_status::DISABLED`]。
    pub status: String,
    /// 被停用的时刻（毫秒）；未停用为 0。
    pub disabled_at: i64,
    /// 停用原因。会原样告诉被停用的用户。
    pub disabled_reason: String,
}

/// 会话校验的结果。三种情况必须分开，因为对外的响应完全不同：
/// 无效令牌是 401「重新登录」，账号停用是 403「你被停用了，原因是…」。
/// 把后者折叠成 401 会让被停用的用户以为是登录过期，反复重登也进不去还不知道为什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCheck {
    /// 令牌不存在 / 为空。
    Invalid,
    /// 令牌有效，但账号已被停用。
    Disabled { username: String, reason: String },
    /// 通过。
    Active(String),
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
    created_at BIGINT NOT NULL,
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    disabled_at BIGINT NOT NULL DEFAULT 0,
    -- 用 VARCHAR 而不是 TEXT：TEXT 在 MySQL 上不允许 DEFAULT，
    -- 于是 NOT NULL 的 TEXT 列会要求每条 INSERT 都显式给值 ——
    -- 而 create_user 只列了最初那四列，结果就是登录直接 500。
    -- 停用原因 500 字够用（与插件下架原因的上限一致）。
    disabled_reason VARCHAR(500) NOT NULL DEFAULT ''
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
    revoked_by VARCHAR(16) NOT NULL DEFAULT '',
    published_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 按小时聚合的请求指标。一小时每路由每方法每状态类一行。
///
/// 主键就是聚合键，落库走 `ON DUPLICATE KEY UPDATE ... + VALUES(...)` 累加——
/// 这样多次 flush 落到同一个小时桶时是**累加而不是覆盖**，进程重启也不会把
/// 这一小时前半段的数据抹掉。
const CREATE_TRAFFIC: &str = "\
  CREATE TABLE IF NOT EXISTS traffic_hourly (
    hour_ts BIGINT NOT NULL,
    route VARCHAR(190) NOT NULL,
    method VARCHAR(10) NOT NULL,
    status_class TINYINT NOT NULL,
    reqs BIGINT NOT NULL,
    bytes_in BIGINT NOT NULL,
    bytes_out BIGINT NOT NULL,
    dur_sum_ms BIGINT NOT NULL,
    dur_max_ms BIGINT NOT NULL,
    b_fast BIGINT NOT NULL,
    b_mid BIGINT NOT NULL,
    b_slow BIGINT NOT NULL,
    b_vslow BIGINT NOT NULL,
    PRIMARY KEY (hour_ts, route, method, status_class),
    INDEX idx_traffic_hour (hour_ts)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 按小时聚合的插件下载量。
const CREATE_PLUGIN_DOWNLOADS: &str = "\
  CREATE TABLE IF NOT EXISTS plugin_downloads_hourly (
    hour_ts BIGINT NOT NULL,
    name VARCHAR(190) NOT NULL,
    downloads BIGINT NOT NULL,
    PRIMARY KEY (hour_ts, name),
    INDEX idx_pdl_hour (hour_ts)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 老库补列：`revoked_by` 是后加的（作者自助下架时需要区分「谁下的架」）。
///
/// 用 information_schema 先判存在再 `ALTER`，而不是靠 `ADD COLUMN IF NOT EXISTS`：
/// 后者是 MariaDB 的扩展语法，MySQL 上直接是语法错误，一条兼容性差异就能让服务启动不了。
const MIGRATIONS: &[(&str, &str, &str)] = &[
    (
        "market_entries",
        "revoked_by",
        "ALTER TABLE market_entries ADD COLUMN revoked_by VARCHAR(16) NOT NULL DEFAULT ''",
    ),
    // 账号停用位。默认 'active' 保证老库补列后所有存量账号照常可用。
    (
        "users",
        "status",
        "ALTER TABLE users ADD COLUMN status VARCHAR(16) NOT NULL DEFAULT 'active'",
    ),
    (
        "users",
        "disabled_at",
        "ALTER TABLE users ADD COLUMN disabled_at BIGINT NOT NULL DEFAULT 0",
    ),
    // 停用原因会原样展示给被停用的用户 —— 他有权知道自己为什么进不去
    (
        "users",
        "disabled_reason",
        "ALTER TABLE users ADD COLUMN disabled_reason VARCHAR(500) NOT NULL DEFAULT ''",
    ),
];

/// `users.status` 的取值。
pub mod user_status {
    /// 正常。
    pub const ACTIVE: &str = "active";
    /// 已停用：登录被拒，已有会话立即失效。
    pub const DISABLED: &str = "disabled";

    /// 这个状态是否允许访问。
    ///
    /// **未知值一律按停用处理**：库里出现没见过的状态说明数据被外部改过，
    /// 这时放行比拒绝危险得多。
    pub fn is_active(s: &str) -> bool {
        s == ACTIVE
    }
}

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

/// `market_entries.revoked_by` 的取值。
pub mod revoked_by {
    /// 作者把自己的插件下架了（可自行恢复，也可提交新版本重新上线）。
    pub const OWNER: &str = "owner";
    /// 维护者下架（处置）。作者**不能**自行恢复，也不能靠提交新版本绕过。
    pub const ADMIN: &str = "admin";
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
    /// 谁下的架：`""` 未下架 | [`revoked_by::OWNER`] 作者自助下架 | [`revoked_by::ADMIN`] 维护者下架。
    ///
    /// 这一列是**安全字段**：维护者下架属于处置，作者不能自己恢复、也不能靠提交新版本绕过。
    pub revoked_by: String,
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

        for ddl in [
            CREATE_USERS,
            CREATE_SESSIONS,
            CREATE_DATA,
            CREATE_SUBMISSIONS,
            CREATE_MARKET,
            CREATE_TRAFFIC,
            CREATE_PLUGIN_DOWNLOADS,
        ] {
            pool.execute(ddl).await?;
        }
        for (table, column, ddl) in MIGRATIONS {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.columns
                 WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
            )
            .bind(table)
            .bind(column)
            .fetch_one(&pool)
            .await?;
            if exists == 0 {
                pool.execute(*ddl).await?;
                tracing::info!("[store] 已为 {table} 补上 {column} 列");
            }
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

    /// 底层连接池。
    ///
    /// 开放出来是给集成测试做断言用的（例如核对指标是否真的累加进了表），
    /// 业务代码一律走本类型的方法——直接拿池子写 SQL 会绕开这里的全部不变量。
    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    /// 关闭连接池（进程退出 / 测试收尾）。
    pub async fn close(&self) {
        self.pool.close().await;
    }

    // ---------- 用户 ----------

    pub async fn get_user(&self, username: &str) -> Result<Option<UserRecord>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT username, password_hash, salt, created_at, status, disabled_at, disabled_reason
             FROM users WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| UserRecord {
            username: r.get("username"),
            password_hash: r.get("password_hash"),
            salt: r.get("salt"),
            created_at: r.get("created_at"),
            status: r.get("status"),
            disabled_at: r.get("disabled_at"),
            disabled_reason: r.get("disabled_reason"),
        }))
    }

    /// set 语义（与旧实现一致）：同名覆盖，避免并发下的重复键报错。
    ///
    /// ⚠ `ON DUPLICATE KEY UPDATE` 里**刻意不含 `status`**：被停用的账号即便触发
    /// 这条 upsert 也不会被悄悄解禁。停用只能由 [`MariaDbStore::set_user_status`] 撤销。
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

    /// 只看会话是否存在，**不检查账号状态**。
    ///
    /// 仅供登出使用：被停用的账号也应该能清掉自己的会话——拒绝登出既没有安全收益，
    /// 又会让客户端卡在一个退不出去的状态里。需要拦截停用账号的地方一律用
    /// [`MariaDbStore::session_user_active`]。
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

    /// 校验会话并**同时检查账号是否被停用**。
    ///
    /// 一次 JOIN 拿到状态，而不是「先查会话再查用户」两次往返——鉴权在每个受保护
    /// 请求上都要走一遍，多一次 RTT 就是全站变慢。
    ///
    /// 停用为什么必须在这里查、而不能只靠「停用时删会话」：删会话只对**当时已存在**
    /// 的会话有效，停用后对方仍可以用原口令重新登录拿到新令牌。登录那边也拦了，
    /// 两处合起来才是完整的封停。
    pub async fn session_user_active(&self, token: &str) -> Result<SessionCheck, sqlx::Error> {
        if token.is_empty() {
            return Ok(SessionCheck::Invalid);
        }
        let row = sqlx::query(
            "SELECT s.username, u.status, u.disabled_reason
             FROM sessions s JOIN users u ON u.username = s.username
             WHERE s.token = ?",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = row else {
            return Ok(SessionCheck::Invalid);
        };
        let username: String = r.get("username");
        let status: String = r.get("status");
        if user_status::is_active(&status) {
            Ok(SessionCheck::Active(username))
        } else {
            Ok(SessionCheck::Disabled {
                username,
                reason: r.get("disabled_reason"),
            })
        }
    }

    /// 设置账号状态。停用时**同时清掉该用户的全部会话**，单事务。
    ///
    /// 两件事必须在一个事务里：先删会话再改状态的话，中间那一瞬间对方还能用旧令牌
    /// 正常访问；反过来先改状态再删会话，虽然鉴权已经拦得住，但留着一堆无效会话行没意义。
    ///
    /// 返回是否真的改到了行（用户不存在时为 `false`）。
    pub async fn set_user_status(
        &self,
        username: &str,
        status: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let disabling = !user_status::is_active(status);
        let mut tx = self.pool.begin().await?;
        let affected = sqlx::query(
            "UPDATE users SET status = ?, disabled_at = ?, disabled_reason = ? WHERE username = ?",
        )
        .bind(status)
        .bind(if disabling { now_ms } else { 0 })
        .bind(if disabling { reason } else { "" })
        .bind(username)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if disabling {
            sqlx::query("DELETE FROM sessions WHERE username = ?")
                .bind(username)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        // rows_affected 在「值没变」时也可能是 0（MySQL 的行为），所以再确认一次存在性
        if affected > 0 {
            return Ok(true);
        }
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(&self.pool)
            .await?;
        Ok(exists > 0)
    }

    // ---------- 请求指标 ----------

    /// 把一批聚合好的请求指标累加进 `traffic_hourly`。
    ///
    /// 用 `ON DUPLICATE KEY UPDATE col = col + VALUES(col)` **累加**而不是覆盖：
    /// 一个小时会被 flush 很多次（默认每分钟一次），覆盖的话这一小时只会剩最后一分钟的数。
    /// `dur_max_ms` 特殊，取 `GREATEST` 而不是相加。
    ///
    /// 整批走一个事务：要么全落要么全不落，避免一半数据落库另一半丢在内存里。
    pub async fn upsert_traffic(
        &self,
        rows: &[(crate::metrics::Key, crate::metrics::Agg)],
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        for (k, a) in rows {
            sqlx::query(
                "INSERT INTO traffic_hourly
                 (hour_ts, route, method, status_class, reqs, bytes_in, bytes_out,
                  dur_sum_ms, dur_max_ms, b_fast, b_mid, b_slow, b_vslow)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE
                   reqs       = reqs       + VALUES(reqs),
                   bytes_in   = bytes_in   + VALUES(bytes_in),
                   bytes_out  = bytes_out  + VALUES(bytes_out),
                   dur_sum_ms = dur_sum_ms + VALUES(dur_sum_ms),
                   dur_max_ms = GREATEST(dur_max_ms, VALUES(dur_max_ms)),
                   b_fast     = b_fast     + VALUES(b_fast),
                   b_mid      = b_mid      + VALUES(b_mid),
                   b_slow     = b_slow     + VALUES(b_slow),
                   b_vslow    = b_vslow    + VALUES(b_vslow)",
            )
            .bind(k.hour_ts)
            // 路由模板理论上不会超长，但真超了宁可截断也不要整批落库失败
            .bind(truncate(&k.route, 190))
            .bind(truncate(&k.method, 10))
            .bind(k.status_class as i8)
            .bind(a.reqs)
            .bind(a.bytes_in)
            .bind(a.bytes_out)
            .bind(a.dur_sum_ms)
            .bind(a.dur_max_ms)
            .bind(a.b_fast)
            .bind(a.b_mid)
            .bind(a.b_slow)
            .bind(a.b_vslow)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    /// 把一批插件下载计数累加进 `plugin_downloads_hourly`。
    pub async fn upsert_plugin_downloads(
        &self,
        rows: &[((i64, String), i64)],
    ) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        for ((hour_ts, name), n) in rows {
            sqlx::query(
                "INSERT INTO plugin_downloads_hourly (hour_ts, name, downloads) VALUES (?, ?, ?)
                 ON DUPLICATE KEY UPDATE downloads = downloads + VALUES(downloads)",
            )
            .bind(hour_ts)
            .bind(truncate(name, 190))
            .bind(n)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    /// 清理保留期之外的指标。返回删掉的行数。
    ///
    /// 指标是可再生的运营数据，过期即删没有争议——与审计日志不同，后者是安全资产，
    /// 什么时候清必须由人显式决定。
    pub async fn purge_metrics_before(&self, hour_ts: i64) -> Result<u64, sqlx::Error> {
        let a = sqlx::query("DELETE FROM traffic_hourly WHERE hour_ts < ?")
            .bind(hour_ts)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let b = sqlx::query("DELETE FROM plugin_downloads_hourly WHERE hour_ts < ?")
            .bind(hour_ts)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(a + b)
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
    ///
    /// 上新版本会清掉**作者自己下的架**（作者下架后再发一版即视为重新上线），
    /// 但**维护者下的架不会被清**：否则「被维护者下架」只要提交一版就能绕过，那道处置形同虚设。
    /// 三个 `IF(revoked_by = 'admin', …)` 里读到的都是旧行的值 —— `revoked_by` 自己排在最后赋值。
    pub async fn publish_entry(&self, e: &MarketRow) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO market_entries
               (name, owner, version, content_hash, package_file, entry, revoked, revoked_reason,
                revoked_by, published_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0, '', '', ?, ?)
             ON DUPLICATE KEY UPDATE
               version = VALUES(version),
               content_hash = VALUES(content_hash),
               package_file = VALUES(package_file),
               entry = VALUES(entry),
               revoked = IF(revoked_by = ?, revoked, 0),
               revoked_reason = IF(revoked_by = ?, revoked_reason, ''),
               revoked_by = IF(revoked_by = ?, revoked_by, ''),
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
        .bind(revoked_by::ADMIN)
        .bind(revoked_by::ADMIN)
        .bind(revoked_by::ADMIN)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_entry(&self, name: &str) -> Result<Option<MarketRow>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT name, owner, version, content_hash, package_file, entry, revoked, revoked_reason,
                    revoked_by, published_at, updated_at FROM market_entries WHERE name = ?",
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
                    revoked_by, published_at, updated_at FROM market_entries ORDER BY updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    /// 下架 / 恢复。下架原因会原样展示给已安装该插件的用户，所以调用方必须给出真实原因。
    ///
    /// `by` 是下架方（[`revoked_by::OWNER`] / [`revoked_by::ADMIN`]），恢复时一律清空。
    /// **鉴权不在这一层**：谁能对哪个插件做这件事，由 `market::MarketService::set_revoked` 判。
    pub async fn set_revoked(
        &self,
        name: &str,
        revoked: bool,
        reason: &str,
        by: &str,
        now_ms: i64,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE market_entries SET revoked = ?, revoked_reason = ?, revoked_by = ?, updated_at = ?
             WHERE name = ?",
        )
        .bind(if revoked { 1 } else { 0 })
        .bind(if revoked { reason } else { "" })
        .bind(if revoked { by } else { "" })
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
        revoked_by: r.get("revoked_by"),
        published_at: r.get("published_at"),
        updated_at: r.get("updated_at"),
    }
}

/// 按**字符**截断（不是字节），避免把 UTF-8 多字节序列切一半塞进库里。
///
/// 只用于指标落库这类「宁可截断也不要整批失败」的字段——业务数据一律不截断。
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate;

    #[test]
    fn never_splits_multibyte_chars() {
        assert_eq!(truncate("/api/health", 190), "/api/health");
        assert_eq!(truncate("abcdef", 3), "abc");
        let s = truncate(&"插件名".repeat(100), 4);
        assert_eq!(s.chars().count(), 4);
        assert_eq!(s, "插件名插");
    }
}
