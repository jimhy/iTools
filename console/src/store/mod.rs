//! 数据访问层。**与云同步服务端共用同一个 MariaDB 库**，但读写边界划得很清：
//!
//! | 表 | 归属 | 控制台的权限 |
//! |---|---|---|
//! | `users` / `sessions` / `data_records` | 主服务 | **只读**，外加两个运营动作（踢下线、删号），见下 |
//! | `plugin_submissions` / `market_entries` | 主服务 | **只读** |
//! | `console_admins` / `console_sessions` / `console_audit_log` | 本服务 | 读写 |
//!
//! 「只读为主」不是洁癖，是安全边界：控制台**不建、不改、不删主服务的任何表结构**，
//! 主服务升级时两边不会互相踩。唯二的例外是删会话与删账号——那是运营动作本身的定义，
//! 且都是纯 DELETE，不改表结构、不改语义（主服务每次鉴权都实时查 `sessions`，
//! 删掉即刻生效，这一点已在 `server/src/routes.rs` 的 `authenticate` 里验证过）。
//!
//! ## 为什么控制台自己的表加 `console_` 前缀
//!
//! 同库共存，前缀是唯一的防撞护栏。主服务的建表是 `CREATE TABLE IF NOT EXISTS`，
//! 一旦重名，谁先启动谁定表结构，另一方会在运行时才炸——那种故障极难排查。

use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
use sqlx::{Executor, MySqlPool};

use crate::config::DbConfig;

pub mod admins;
pub mod audit;
pub mod plugins;
pub mod stats;
pub mod users;

/// 控制台管理员。口令哈希与盐不出这一层，序列化给前端时必须剔除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminRecord {
    pub username: String,
    pub password_hash: String,
    pub salt: String,
    /// [`role::SUPER`] | [`role::ADMIN`] | [`role::VIEWER`]
    pub role: String,
    /// `active` | `disabled`
    pub status: String,
    pub must_change_password: bool,
    pub created_at: i64,
    pub last_login_at: i64,
    pub last_login_ip: String,
}

/// `console_admins.role` 的取值。
pub mod role {
    /// 超级管理员：可管理其他管理员账号。引导账号即此角色。
    pub const SUPER: &str = "super";
    /// 普通管理员：可做全部运营动作，但不能增删管理员。
    pub const ADMIN: &str = "admin";
    /// 只读：只能看，任何写操作都会被拒。
    pub const VIEWER: &str = "viewer";

    /// 这个角色能否执行写操作（踢下线、删号等）。
    pub fn can_write(r: &str) -> bool {
        matches!(r, SUPER | ADMIN)
    }

    /// 这个角色能否管理其他管理员账号。
    pub fn can_manage_admins(r: &str) -> bool {
        r == SUPER
    }

    /// 是否是已知角色。库里出现未知值时一律按最小权限处理。
    pub fn is_known(r: &str) -> bool {
        matches!(r, SUPER | ADMIN | VIEWER)
    }
}

const CREATE_CONSOLE_ADMINS: &str = "\
  CREATE TABLE IF NOT EXISTS console_admins (
    username VARCHAR(190) PRIMARY KEY,
    password_hash TEXT NOT NULL,
    salt VARCHAR(64) NOT NULL,
    role VARCHAR(16) NOT NULL DEFAULT 'admin',
    status VARCHAR(16) NOT NULL DEFAULT 'active',
    must_change_password TINYINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    last_login_at BIGINT NOT NULL DEFAULT 0,
    last_login_ip VARCHAR(64) NOT NULL DEFAULT ''
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 控制台会话。与主服务的 `sessions` 有两处**有意的不同**：
/// ① 主键存的是令牌的 sha256 而非明文；② 有 `expires_at`，且真的会被校验和回收。
const CREATE_CONSOLE_SESSIONS: &str = "\
  CREATE TABLE IF NOT EXISTS console_sessions (
    token_hash VARCHAR(64) PRIMARY KEY,
    username VARCHAR(190) NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    ip VARCHAR(64) NOT NULL DEFAULT '',
    user_agent VARCHAR(255) NOT NULL DEFAULT '',
    INDEX idx_console_sessions_user (username),
    INDEX idx_console_sessions_exp (expires_at)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 审计日志。**每一个写操作都必须留痕**，包括失败的尝试（`ok=0`）——
/// 「谁在什么时候试图禁用谁但被拒了」和「谁真的删了谁」同样重要。
const CREATE_CONSOLE_AUDIT: &str = "\
  CREATE TABLE IF NOT EXISTS console_audit_log (
    id BIGINT AUTO_INCREMENT PRIMARY KEY,
    at BIGINT NOT NULL,
    actor VARCHAR(190) NOT NULL,
    action VARCHAR(64) NOT NULL,
    target VARCHAR(190) NOT NULL DEFAULT '',
    detail TEXT NOT NULL,
    ip VARCHAR(64) NOT NULL DEFAULT '',
    ok TINYINT NOT NULL DEFAULT 1,
    INDEX idx_console_audit_at (at),
    INDEX idx_console_audit_actor (actor),
    INDEX idx_console_audit_action (action)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";

/// 主服务库的「能力探测」结果。
///
/// 控制台与主服务是**独立发版**的两个二进制，NAS 上完全可能出现
/// 「控制台已是新版、主服务还是旧镜像」的组合。凡是主服务某个版本才有的列，
/// 都在这里探一次，查询按探测结果降级，而不是直接 SQL 报错整页崩掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    /// `market_entries.revoked_by`：区分「作者自助下架」与「维护者处置下架」。
    pub market_revoked_by: bool,
    /// `users.status`：禁用位。**第二步给主服务打补丁后才会有**，
    /// 在此之前控制台如实显示「服务端尚未支持」，不做假开关。
    pub users_status: bool,
}

/// 存储层错误。对外一律折叠成「数据库错误」，细节只进日志——
/// 错误信息里可能带库结构甚至数据片段，不该出现在浏览器里。
pub type StoreResult<T> = Result<T, sqlx::Error>;

pub struct Store {
    pool: MySqlPool,
    caps: Caps,
}

fn base_options(cfg: &DbConfig) -> MySqlConnectOptions {
    MySqlConnectOptions::new()
        .host(&cfg.host)
        .port(cfg.port)
        .username(&cfg.user)
        .password(&cfg.password)
        .charset("utf8mb4")
}

impl Store {
    /// 连接目标库并建好控制台自己的三张表（幂等），然后探测主服务的可选列。
    ///
    /// **不会** `CREATE DATABASE`：库是主服务建的，控制台连不上就该报错退出，
    /// 而不是自己造一个空库出来让人以为「后台好了、只是没数据」。
    pub async fn connect(cfg: &DbConfig) -> StoreResult<Self> {
        let pool = match &cfg.url {
            Some(url) => {
                MySqlPoolOptions::new()
                    .max_connections(cfg.conn_limit.max(1))
                    .connect(url)
                    .await?
            }
            None => {
                MySqlPoolOptions::new()
                    .max_connections(cfg.conn_limit.max(1))
                    .connect_with(base_options(cfg).database(&cfg.name))
                    .await?
            }
        };

        for ddl in [
            CREATE_CONSOLE_ADMINS,
            CREATE_CONSOLE_SESSIONS,
            CREATE_CONSOLE_AUDIT,
        ] {
            pool.execute(ddl).await?;
        }

        let caps = Caps {
            market_revoked_by: column_exists(&pool, "market_entries", "revoked_by").await?,
            users_status: column_exists(&pool, "users", "status").await?,
        };
        tracing::info!(
            "[store] 主服务库能力探测：market_entries.revoked_by={} users.status={}",
            caps.market_revoked_by,
            caps.users_status
        );

        Ok(Self { pool, caps })
    }

    /// 主服务库的能力探测结果（用于让前端如实标注哪些功能尚未就绪）。
    pub fn caps(&self) -> Caps {
        self.caps
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    /// 确认主服务的核心表都在。缺表说明连错库了——早报错好过让运营看着空列表猜。
    pub async fn verify_upstream_tables(&self) -> StoreResult<Vec<String>> {
        let mut missing = Vec::new();
        for t in [
            "users",
            "sessions",
            "data_records",
            "plugin_submissions",
            "market_entries",
        ] {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM information_schema.tables
                 WHERE table_schema = DATABASE() AND table_name = ?",
            )
            .bind(t)
            .fetch_one(&self.pool)
            .await?;
            if exists == 0 {
                missing.push(t.to_string());
            }
        }
        Ok(missing)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

async fn column_exists(pool: &MySqlPool, table: &str, column: &str) -> StoreResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns
         WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// 把用户输入的模糊搜索词转义成可安全用于 `LIKE` 的形式。
///
/// 不转义的话，运营搜一个 `%` 就等于全表扫描、搜 `_` 会匹配任意单字符——
/// 结果对不上还以为是数据错了。反斜杠必须**第一个**替换，否则会把后面
/// 刚插入的转义符再转义一遍。
pub fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 分页参数。上限刻意压得比较死：运营列表页不需要一次拉一万行，
/// 而一个手滑的 `size=999999` 能把 NAS 那点内存吃穿。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub offset: i64,
    pub size: i64,
}

impl Page {
    pub fn new(page: i64, size: i64) -> Self {
        let size = size.clamp(1, 200);
        let page = page.max(1);
        Self {
            offset: (page - 1) * size,
            size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escaping_neutralizes_wildcards() {
        assert_eq!(escape_like("a%b"), "a\\%b");
        assert_eq!(escape_like("a_b"), "a\\_b");
        // 反斜杠先转义，避免把随后插入的转义符再转一遍
        assert_eq!(escape_like("a\\%"), "a\\\\\\%");
        assert_eq!(escape_like("正常搜索词"), "正常搜索词");
    }

    #[test]
    fn page_clamps_out_of_range_input() {
        assert_eq!(Page::new(1, 20), Page { offset: 0, size: 20 });
        assert_eq!(Page::new(3, 20), Page { offset: 40, size: 20 });
        assert_eq!(Page::new(0, 20).offset, 0, "页码从 1 起，0 与负数都归到第一页");
        assert_eq!(Page::new(-5, 20).offset, 0);
        assert_eq!(Page::new(1, 999_999).size, 200, "单页上限 200");
        assert_eq!(Page::new(1, 0).size, 1);
    }

    #[test]
    fn role_permissions() {
        assert!(role::can_write(role::SUPER));
        assert!(role::can_write(role::ADMIN));
        assert!(!role::can_write(role::VIEWER), "只读角色不能写");
        assert!(!role::can_write("未知角色"), "未知角色按最小权限处理");
        assert!(role::can_manage_admins(role::SUPER));
        assert!(!role::can_manage_admins(role::ADMIN), "普通管理员不能增删管理员");
        assert!(role::is_known(role::VIEWER));
        assert!(!role::is_known("root"));
    }
}
