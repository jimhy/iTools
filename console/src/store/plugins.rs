//! 插件市场与提审单的查询。**本模块全部只读。**
//!
//! ## 为什么第一版不做下架 / 改判
//!
//! 主服务的市场索引是**进程内 `RwLock` 缓存**（`server/src/market.rs:69`），
//! 只在启动 bootstrap 与 publish/set_revoked 时重建；插件包也是直接读磁盘返回。
//! 控制台直接 `UPDATE market_entries SET revoked = 1` 只会改到库，
//! **客户端拿到的索引与下载链路一个都不会变**——直到主服务重启为止。
//!
//! 那就是典型的「按钮点了不生效」，踩项目诚信红线。所以这里一行写操作都没有，
//! 前端对应的按钮一律禁用并标注「需服务端支持」。等主服务补上下架端点/索引刷新钩子，
//! 再由控制台调它的 HTTP 接口，而不是绕过它改库。

use sqlx::Row;

use super::{escape_like, Page, Store, StoreResult};

/// 一条市场条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketRow {
    pub name: String,
    pub owner: String,
    pub version: String,
    pub content_hash: String,
    pub package_file: String,
    pub revoked: bool,
    pub revoked_reason: String,
    /// `""` 未下架 | `owner` 作者自助下架 | `admin` 维护者处置。
    /// 主服务尚未有这一列时恒为空串（见 `Caps::market_revoked_by`）。
    pub revoked_by: String,
    pub published_at: i64,
    pub updated_at: i64,
    /// 从 `entry` JSON 里取出的展示字段。取不到就留空，不编造。
    pub title: String,
    pub description: String,
}

/// 一条提审单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    /// `reviewing` 审核中 | `approved` 已上线 | `rejected` 已驳回
    /// | `manual` 待人工处理 | `failed` 校验未通过
    pub status: String,
    pub content_hash: String,
    pub file_count: i64,
    pub size_bytes: i64,
    pub message: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 提审单详情：在列表行之上多带 manifest 与模型裁决原文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionDetail {
    pub row: SubmissionRow,
    pub manifest: String,
    pub review: String,
}

/// 各状态的提审单计数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCount {
    pub status: String,
    pub count: i64,
}

impl Store {
    /// 市场条目列表。`include_revoked = false` 时只看在架的。
    pub async fn list_market(
        &self,
        query: Option<&str>,
        include_revoked: bool,
        page: Page,
    ) -> StoreResult<(Vec<MarketRow>, i64)> {
        let like = query.map(|q| format!("%{}%", escape_like(q)));
        // 主服务旧版本没有 revoked_by 列，探测不到就用常量空串占位，
        // 而不是让整条 SQL 报 Unknown column 把页面打崩。
        let revoked_by_expr = if self.caps().market_revoked_by {
            "revoked_by"
        } else {
            "''"
        };

        let total: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*) FROM market_entries
              WHERE (? IS NULL OR name LIKE ? ESCAPE '\\' OR owner LIKE ? ESCAPE '\\')
                AND (? = 1 OR revoked = 0)",
        )
        .bind(like.as_deref())
        .bind(like.as_deref())
        .bind(like.as_deref())
        .bind(i8::from(include_revoked))
        .fetch_one(self.pool())
        .await?;

        let sql = format!(
            r"SELECT name, owner, version, content_hash, package_file,
                     revoked, revoked_reason, {revoked_by_expr} AS revoked_by,
                     published_at, updated_at, entry
              FROM market_entries
              WHERE (? IS NULL OR name LIKE ? ESCAPE '\\' OR owner LIKE ? ESCAPE '\\')
                AND (? = 1 OR revoked = 0)
              ORDER BY updated_at DESC
              LIMIT ? OFFSET ?"
        );

        let rows = sqlx::query(&sql)
            .bind(like.as_deref())
            .bind(like.as_deref())
            .bind(like.as_deref())
            .bind(i8::from(include_revoked))
            .bind(page.size)
            .bind(page.offset)
            .fetch_all(self.pool())
            .await?;

        let list = rows.into_iter().map(map_market_row).collect();
        Ok((list, total))
    }

    pub async fn get_market_entry(&self, name: &str) -> StoreResult<Option<MarketRow>> {
        let revoked_by_expr = if self.caps().market_revoked_by {
            "revoked_by"
        } else {
            "''"
        };
        let sql = format!(
            "SELECT name, owner, version, content_hash, package_file,
                    revoked, revoked_reason, {revoked_by_expr} AS revoked_by,
                    published_at, updated_at, entry
             FROM market_entries WHERE name = ?"
        );
        let row = sqlx::query(&sql)
            .bind(name)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(map_market_row))
    }

    pub async fn count_market(&self) -> StoreResult<(i64, i64)> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM market_entries")
            .fetch_one(self.pool())
            .await?;
        let revoked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM market_entries WHERE revoked = 1")
            .fetch_one(self.pool())
            .await?;
        Ok((total, revoked))
    }

    /// 提审单列表。可按状态与作者/插件名筛选。
    pub async fn list_submissions(
        &self,
        status: Option<&str>,
        query: Option<&str>,
        page: Page,
    ) -> StoreResult<(Vec<SubmissionRow>, i64)> {
        let like = query.map(|q| format!("%{}%", escape_like(q)));

        let total: i64 = sqlx::query_scalar(
            r"SELECT COUNT(*) FROM plugin_submissions
              WHERE (? IS NULL OR status = ?)
                AND (? IS NULL OR name LIKE ? ESCAPE '\\' OR author LIKE ? ESCAPE '\\')",
        )
        .bind(status)
        .bind(status)
        .bind(like.as_deref())
        .bind(like.as_deref())
        .bind(like.as_deref())
        .fetch_one(self.pool())
        .await?;

        let rows = sqlx::query(
            r"SELECT id, name, version, author, status, content_hash,
                     file_count, size_bytes, message, created_at, updated_at
              FROM plugin_submissions
              WHERE (? IS NULL OR status = ?)
                AND (? IS NULL OR name LIKE ? ESCAPE '\\' OR author LIKE ? ESCAPE '\\')
              ORDER BY created_at DESC
              LIMIT ? OFFSET ?",
        )
        .bind(status)
        .bind(status)
        .bind(like.as_deref())
        .bind(like.as_deref())
        .bind(like.as_deref())
        .bind(page.size)
        .bind(page.offset)
        .fetch_all(self.pool())
        .await?;

        let list = rows.into_iter().map(map_submission_row).collect();
        Ok((list, total))
    }

    pub async fn get_submission(&self, id: &str) -> StoreResult<Option<SubmissionDetail>> {
        let row = sqlx::query(
            "SELECT id, name, version, author, status, content_hash,
                    file_count, size_bytes, message, created_at, updated_at,
                    manifest, review
             FROM plugin_submissions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| SubmissionDetail {
            manifest: r.get("manifest"),
            review: r.get("review"),
            row: map_submission_row(r),
        }))
    }

    /// 提审单按状态分组计数（概览页用）。
    pub async fn submission_status_counts(&self) -> StoreResult<Vec<StatusCount>> {
        let rows = sqlx::query(
            "SELECT status, COUNT(*) AS c FROM plugin_submissions GROUP BY status ORDER BY c DESC",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StatusCount {
                status: r.get("status"),
                count: r.get("c"),
            })
            .collect())
    }
}

fn map_market_row(r: sqlx::mysql::MySqlRow) -> MarketRow {
    let entry: String = r.get("entry");
    // `entry` 是客户端 MarketEntry 形状的 JSON 文本。解析失败不是致命错误——
    // 展示字段留空即可，绝不为了「好看」编一个标题出来。
    let (title, description) = match serde_json::from_str::<serde_json::Value>(&entry) {
        Ok(v) => (
            v.get("title")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
            v.get("description")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        ),
        Err(_) => (String::new(), String::new()),
    };
    MarketRow {
        name: r.get("name"),
        owner: r.get("owner"),
        version: r.get("version"),
        content_hash: r.get("content_hash"),
        package_file: r.get("package_file"),
        revoked: r.get::<i8, _>("revoked") != 0,
        revoked_reason: r.get("revoked_reason"),
        revoked_by: r.get("revoked_by"),
        published_at: r.get("published_at"),
        updated_at: r.get("updated_at"),
        title,
        description,
    }
}

fn map_submission_row(r: sqlx::mysql::MySqlRow) -> SubmissionRow {
    SubmissionRow {
        id: r.get("id"),
        name: r.get("name"),
        version: r.get("version"),
        author: r.get("author"),
        status: r.get("status"),
        content_hash: r.get("content_hash"),
        file_count: r.get("file_count"),
        size_bytes: r.get("size_bytes"),
        message: r.get("message"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}
