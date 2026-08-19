//! 统计聚合。
//!
//! ## 这里的数字都是从库里**真算**出来的
//!
//! 一条也没有估算、补零或推算。凡是库里没有的东西（HTTP 请求量、带宽、响应时间、
//! 插件下载次数），这个模块里就**没有对应的函数**——前端因此只能如实显示
//! 「服务端尚未采集」，而不会有一条看着像真的、其实是编的曲线。
//!
//! ## 分桶的时区
//!
//! 按天/按小时分桶**不依赖数据库时区**：`FROM_UNIXTIME` 会跟着 MariaDB 的
//! `time_zone` 变，同一份数据在不同机器上能画出不同的曲线。这里改成在 SQL 里
//! 手算桶号 `FLOOR((ts/1000 + 偏移秒) / 桶长)`，偏移由 `CONSOLE_TZ_OFFSET_MIN`
//! 给定（默认 +480 分钟 = 东八区），行为完全确定、可测试。

use sqlx::Row;

use super::{Store, StoreResult};

/// 一个时间桶。`bucket` 是桶起点的 Unix 秒（已还原成 UTC 语义的绝对时刻）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bucket {
    pub bucket: i64,
    pub count: i64,
    /// 该桶内的字节量。不适用的指标恒为 0。
    pub bytes: i64,
}

/// 存储占用排行的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRow {
    pub username: String,
    pub count: i64,
    pub bytes: i64,
}

/// 命名空间维度的全局分布。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NsRow {
    pub ns: String,
    pub users: i64,
    pub count: i64,
    pub bytes: i64,
}

/// 概览页的汇总数字。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overview {
    pub users_total: i64,
    pub users_new_1d: i64,
    pub users_new_7d: i64,
    pub users_new_30d: i64,
    pub sessions_total: i64,
    pub sessions_new_1d: i64,
    pub records_total: i64,
    pub bytes_total: i64,
    pub records_updated_1d: i64,
    pub market_total: i64,
    pub market_revoked: i64,
    pub submissions_total: i64,
    /// 数据库中这几张表占用的磁盘字节（`information_schema` 的估算值，
    /// InnoDB 下本身就是近似——UI 上要标明「近似」）。
    pub db_bytes: i64,
}

/// 一天的秒数。
const DAY: i64 = 86_400;
/// 一小时的秒数。
const HOUR: i64 = 3_600;

impl Store {
    /// 概览汇总。`now_ms` 为当前毫秒时间戳。
    pub async fn overview(&self, now_ms: i64) -> StoreResult<Overview> {
        let d1 = now_ms - DAY * 1000;
        let d7 = now_ms - 7 * DAY * 1000;
        let d30 = now_ms - 30 * DAY * 1000;

        let users_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(self.pool())
            .await?;
        let users_new_1d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE created_at >= ?")
            .bind(d1)
            .fetch_one(self.pool())
            .await?;
        let users_new_7d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE created_at >= ?")
            .bind(d7)
            .fetch_one(self.pool())
            .await?;
        let users_new_30d: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE created_at >= ?")
            .bind(d30)
            .fetch_one(self.pool())
            .await?;

        let sessions_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(self.pool())
            .await?;
        let sessions_new_1d: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE created_at >= ?")
                .bind(d1)
                .fetch_one(self.pool())
                .await?;

        let records_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM data_records")
            .fetch_one(self.pool())
            .await?;
        let bytes_total: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) FROM data_records",
        )
        .fetch_one(self.pool())
        .await?;
        let records_updated_1d: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM data_records WHERE updated_at >= ?")
                .bind(d1)
                .fetch_one(self.pool())
                .await?;

        let (market_total, market_revoked) = self.count_market().await?;
        let submissions_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plugin_submissions")
            .fetch_one(self.pool())
            .await?;

        let db_bytes: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(data_length + index_length), 0) AS SIGNED)
             FROM information_schema.tables WHERE table_schema = DATABASE()",
        )
        .fetch_one(self.pool())
        .await
        // information_schema 在某些权限配置下读不到 —— 这不该让整个概览页 500，
        // 但也不能显示成 0 让人以为库是空的，所以用 -1 表示「读不到」，前端据此标注。
        .unwrap_or(-1);

        Ok(Overview {
            users_total,
            users_new_1d,
            users_new_7d,
            users_new_30d,
            sessions_total,
            sessions_new_1d,
            records_total,
            bytes_total,
            records_updated_1d,
            market_total,
            market_revoked,
            submissions_total,
            db_bytes,
        })
    }

    /// 用户注册按天分桶。
    pub async fn users_by_day(&self, days: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("users", "created_at", DAY, days, now_ms, tz_offset_min)
            .await
    }

    /// 现存会话的创建时间按天分桶。
    ///
    /// ⚠ 语义提醒：主服务在用户登出时会**删掉**会话行，因此这条曲线是
    /// 「当前还存在的会话是什么时候建的」，**不等于历史登录量**。
    /// 前端必须原样把这句话显示出来，否则就是拿一个近似值冒充登录趋势。
    pub async fn sessions_by_day(&self, days: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("sessions", "created_at", DAY, days, now_ms, tz_offset_min)
            .await
    }

    /// 现存会话的创建时间按小时分桶（最近 N 小时）。
    pub async fn sessions_by_hour(&self, hours: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("sessions", "created_at", HOUR, hours, now_ms, tz_offset_min)
            .await
    }

    /// 同步数据的写入活跃度按天分桶（按 `updated_at`）。
    ///
    /// ⚠ 语义提醒：`data_records` 的主键是 `(username, ns, k)`，同一条记录被反复
    /// 覆盖时只更新 `updated_at`。所以这是「当前有多少条记录最后一次更新落在那一天」，
    /// **不是那一天发生了多少次写入**。同样必须如实标注。
    pub async fn records_by_day(&self, days: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("data_records", "updated_at", DAY, days, now_ms, tz_offset_min)
            .await
    }

    /// 插件上线时间按天分桶。
    pub async fn market_by_day(&self, days: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("market_entries", "published_at", DAY, days, now_ms, tz_offset_min)
            .await
    }

    /// 提审量按天分桶。这张表**一次提审一行、永不覆盖**，
    /// 所以它是本库里唯一真正意义上的「事件流」，趋势最可信。
    pub async fn submissions_by_day(&self, days: i64, now_ms: i64, tz_offset_min: i64) -> StoreResult<Vec<Bucket>> {
        self.bucket_count("plugin_submissions", "created_at", DAY, days, now_ms, tz_offset_min)
            .await
    }

    /// 存储占用排行（按字节倒序）。
    pub async fn storage_ranking(&self, limit: i64) -> StoreResult<Vec<StorageRow>> {
        let rows = sqlx::query(
            "SELECT username, COUNT(*) AS c,
                    CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) AS b
             FROM data_records GROUP BY username ORDER BY b DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 100))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| StorageRow {
                username: r.get("username"),
                count: r.get("c"),
                bytes: r.get("b"),
            })
            .collect())
    }

    /// 命名空间维度的全局分布：每个 ns 有多少用户在用、多少条、多少字节。
    pub async fn ns_distribution(&self) -> StoreResult<Vec<NsRow>> {
        let rows = sqlx::query(
            "SELECT ns,
                    COUNT(DISTINCT username) AS u,
                    COUNT(*) AS c,
                    CAST(COALESCE(SUM(LENGTH(v)), 0) AS SIGNED) AS b
             FROM data_records GROUP BY ns ORDER BY b DESC LIMIT 50",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| NsRow {
                ns: r.get("ns"),
                users: r.get("u"),
                count: r.get("c"),
                bytes: r.get("b"),
            })
            .collect())
    }

    /// 通用分桶计数。
    ///
    /// `table` 与 `column` **只能来自本文件的字面量**——它们直接拼进 SQL，
    /// 绝不可以接受任何外部输入。所有调用点都在上面，全是硬编码。
    async fn bucket_count(
        &self,
        table: &'static str,
        column: &'static str,
        bucket_sec: i64,
        buckets: i64,
        now_ms: i64,
        tz_offset_min: i64,
    ) -> StoreResult<Vec<Bucket>> {
        let buckets = buckets.clamp(1, 400);
        let offset_sec = tz_offset_min * 60;
        let since_ms = now_ms - buckets * bucket_sec * 1000;

        // `FLOOR()` 在 MariaDB 里返回 DECIMAL 而不是整数，必须显式 CAST 成 SIGNED——
        // 否则解码成 i64 时会失败。这和 `SUM()` 是同一个坑，两处都踩过。
        let sql = format!(
            "SELECT CAST(FLOOR(({column} / 1000 + ?) / ?) AS SIGNED) AS bkt, COUNT(*) AS c
             FROM {table} WHERE {column} >= ?
             GROUP BY bkt ORDER BY bkt ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(offset_sec)
            .bind(bucket_sec)
            .bind(since_ms)
            .fetch_all(self.pool())
            .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            // 用 try_get 而不是 get：后者在类型对不上时直接 panic，
            // 那会变成「连接被重置」这种毫无线索的故障，而不是一条可读的 500。
            let bkt: i64 = r.try_get("bkt")?;
            let count: i64 = r.try_get("c")?;
            out.push(Bucket {
                // 桶号还原成桶起点的绝对时刻（Unix 秒）：先乘回桶长，再减掉时区偏移
                bucket: bkt * bucket_sec - offset_sec,
                count,
                bytes: 0,
            });
        }
        Ok(out)
    }
}

/// 把稀疏的分桶结果补齐成连续序列（缺的桶填 0）。
///
/// 这**不是**编数据：某一天没有新注册，真实值就是 0，画成断线反而看不出来。
/// 与之相对，「服务端根本没采集」的指标不会走到这里——那种情况前端显示的是
/// 「未采集」而不是一条零线。
pub fn fill_buckets(rows: &[Bucket], bucket_sec: i64, buckets: i64, now_sec: i64, tz_offset_min: i64) -> Vec<Bucket> {
    let offset_sec = tz_offset_min * 60;
    // 当前所处桶的起点
    let current = ((now_sec + offset_sec) / bucket_sec) * bucket_sec - offset_sec;
    let start = current - (buckets - 1) * bucket_sec;
    let mut out = Vec::with_capacity(buckets as usize);
    for i in 0..buckets {
        let b = start + i * bucket_sec;
        let found = rows.iter().find(|r| r.bucket == b);
        out.push(Bucket {
            bucket: b,
            count: found.map(|r| r.count).unwrap_or(0),
            bytes: found.map(|r| r.bytes).unwrap_or(0),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_produces_contiguous_series_with_zeros() {
        // 2026-08-19 02:00:00 UTC = 1787104800
        let now = 1_787_104_800;
        let rows = vec![Bucket {
            // 东八区当天的起点：UTC 前一天 16:00
            bucket: ((now + 480 * 60) / DAY) * DAY - 480 * 60,
            count: 3,
            bytes: 0,
        }];
        let filled = fill_buckets(&rows, DAY, 5, now, 480);
        assert_eq!(filled.len(), 5, "补齐到请求的桶数");
        assert_eq!(filled[4].count, 3, "有数据的那天保留原值");
        assert!(filled[..4].iter().all(|b| b.count == 0), "没数据的天补 0");
        // 桶必须严格等距且递增
        for w in filled.windows(2) {
            assert_eq!(w[1].bucket - w[0].bucket, DAY);
        }
    }

    #[test]
    fn timezone_offset_shifts_day_boundary() {
        let now = 1_787_104_800; // UTC 02:00
        let utc = fill_buckets(&[], DAY, 1, now, 0);
        let cst = fill_buckets(&[], DAY, 1, now, 480);
        assert_ne!(
            utc[0].bucket, cst[0].bucket,
            "UTC 02:00 时东八区已是次日，天桶起点必须不同"
        );
        // 东八区的桶起点应当是 UTC 的前一天 16:00
        assert_eq!(cst[0].bucket % DAY, DAY - 480 * 60);
    }

    #[test]
    fn hour_buckets_are_contiguous() {
        let now = 1_787_104_800;
        let filled = fill_buckets(&[], HOUR, 24, now, 480);
        assert_eq!(filled.len(), 24);
        for w in filled.windows(2) {
            assert_eq!(w[1].bucket - w[0].bucket, HOUR);
        }
        assert!(filled.last().unwrap().bucket <= now, "最后一个桶不能落在未来");
    }
}
