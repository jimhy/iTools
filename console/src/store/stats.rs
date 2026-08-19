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

/// 流量时间序列的一个点。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrafficPoint {
    /// 桶起点（Unix 秒）
    pub bucket: i64,
    pub reqs: i64,
    /// 4xx + 5xx
    pub errs: i64,
    /// 仅 5xx
    pub server_errs: i64,
    pub bytes_in: i64,
    pub bytes_out: i64,
    /// 耗时总和。平均值 = dur_sum_ms / reqs，**精确**，不是估算的百分位。
    pub dur_sum_ms: i64,
    pub dur_max_ms: i64,
}

/// 按路由汇总的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStat {
    pub route: String,
    pub method: String,
    pub reqs: i64,
    pub errs: i64,
    pub server_errs: i64,
    pub bytes_out: i64,
    pub dur_sum_ms: i64,
    pub dur_max_ms: i64,
    /// 延迟分布：<50ms / <200ms / <1s / ≥1s
    pub b_fast: i64,
    pub b_mid: i64,
    pub b_slow: i64,
    pub b_vslow: i64,
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

    // ---------- 请求指标（来自主服务的 traffic_hourly / plugin_downloads_hourly）----------

    /// 流量时间序列。`bucket_sec` 只接受 3600（小时）或 86400（天）。
    ///
    /// 主服务把指标按**小时**落库，所以小时是最细粒度——再细的曲线画不出来，
    /// 也不会去插值伪造。按天则是把小时桶聚起来。
    pub async fn traffic_series(
        &self,
        bucket_sec: i64,
        points: i64,
        now_ms: i64,
        tz_offset_min: i64,
    ) -> StoreResult<Vec<TrafficPoint>> {
        if !self.caps().traffic {
            return Ok(Vec::new());
        }
        let points = points.clamp(1, 400);
        let bucket_sec = if bucket_sec >= DAY { DAY } else { HOUR };
        let offset_sec = tz_offset_min * 60;
        let since = now_ms / 1000 - points * bucket_sec;

        // 按天聚合时要带时区偏移，否则「今天」的边界跟着数据库所在时区跑。
        // 按小时聚合时 hour_ts 本身就是桶起点，直接用。
        let bucket_expr = if bucket_sec == DAY {
            "CAST(FLOOR((hour_ts + ?) / ?) * ? - ? AS SIGNED)"
        } else {
            "CAST(hour_ts AS SIGNED)"
        };

        let sql = format!(
            "SELECT {bucket_expr} AS bkt,
                    CAST(COALESCE(SUM(reqs), 0) AS SIGNED) AS reqs,
                    CAST(COALESCE(SUM(CASE WHEN status_class IN (4,5) THEN reqs ELSE 0 END), 0) AS SIGNED) AS errs,
                    CAST(COALESCE(SUM(CASE WHEN status_class = 5 THEN reqs ELSE 0 END), 0) AS SIGNED) AS server_errs,
                    CAST(COALESCE(SUM(bytes_in), 0) AS SIGNED) AS bytes_in,
                    CAST(COALESCE(SUM(bytes_out), 0) AS SIGNED) AS bytes_out,
                    CAST(COALESCE(SUM(dur_sum_ms), 0) AS SIGNED) AS dur_sum_ms,
                    CAST(COALESCE(MAX(dur_max_ms), 0) AS SIGNED) AS dur_max_ms
             FROM traffic_hourly WHERE hour_ts >= ?
             GROUP BY bkt ORDER BY bkt ASC"
        );

        let mut q = sqlx::query(&sql);
        if bucket_sec == DAY {
            q = q.bind(offset_sec).bind(bucket_sec).bind(bucket_sec).bind(offset_sec);
        }
        let rows = q.bind(since).fetch_all(self.pool()).await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(TrafficPoint {
                bucket: r.try_get("bkt")?,
                reqs: r.try_get("reqs")?,
                errs: r.try_get("errs")?,
                server_errs: r.try_get("server_errs")?,
                bytes_in: r.try_get("bytes_in")?,
                bytes_out: r.try_get("bytes_out")?,
                dur_sum_ms: r.try_get("dur_sum_ms")?,
                dur_max_ms: r.try_get("dur_max_ms")?,
            });
        }
        Ok(out)
    }

    /// 按路由汇总（Top N，按请求数倒序）。
    pub async fn traffic_routes(
        &self,
        hours: i64,
        limit: i64,
        now_ms: i64,
    ) -> StoreResult<Vec<RouteStat>> {
        if !self.caps().traffic {
            return Ok(Vec::new());
        }
        let since = now_ms / 1000 - hours.clamp(1, 24 * 365) * HOUR;
        let rows = sqlx::query(
            "SELECT route, method,
                    CAST(SUM(reqs) AS SIGNED) AS reqs,
                    CAST(SUM(CASE WHEN status_class IN (4,5) THEN reqs ELSE 0 END) AS SIGNED) AS errs,
                    CAST(SUM(CASE WHEN status_class = 5 THEN reqs ELSE 0 END) AS SIGNED) AS server_errs,
                    CAST(SUM(bytes_out) AS SIGNED) AS bytes_out,
                    CAST(SUM(dur_sum_ms) AS SIGNED) AS dur_sum_ms,
                    CAST(MAX(dur_max_ms) AS SIGNED) AS dur_max_ms,
                    CAST(SUM(b_fast) AS SIGNED) AS b_fast,
                    CAST(SUM(b_mid) AS SIGNED) AS b_mid,
                    CAST(SUM(b_slow) AS SIGNED) AS b_slow,
                    CAST(SUM(b_vslow) AS SIGNED) AS b_vslow
             FROM traffic_hourly WHERE hour_ts >= ?
             GROUP BY route, method ORDER BY reqs DESC LIMIT ?",
        )
        .bind(since)
        .bind(limit.clamp(1, 100))
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(RouteStat {
                route: r.try_get("route")?,
                method: r.try_get("method")?,
                reqs: r.try_get("reqs")?,
                errs: r.try_get("errs")?,
                server_errs: r.try_get("server_errs")?,
                bytes_out: r.try_get("bytes_out")?,
                dur_sum_ms: r.try_get("dur_sum_ms")?,
                dur_max_ms: r.try_get("dur_max_ms")?,
                b_fast: r.try_get("b_fast")?,
                b_mid: r.try_get("b_mid")?,
                b_slow: r.try_get("b_slow")?,
                b_vslow: r.try_get("b_vslow")?,
            });
        }
        Ok(out)
    }

    /// 状态码大类分布（近 N 小时）。
    pub async fn traffic_status_mix(&self, hours: i64, now_ms: i64) -> StoreResult<Vec<(u8, i64)>> {
        if !self.caps().traffic {
            return Ok(Vec::new());
        }
        let since = now_ms / 1000 - hours.clamp(1, 24 * 365) * HOUR;
        let rows = sqlx::query(
            "SELECT status_class, CAST(SUM(reqs) AS SIGNED) AS reqs
             FROM traffic_hourly WHERE hour_ts >= ?
             GROUP BY status_class ORDER BY status_class",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push((r.try_get::<i8, _>("status_class")? as u8, r.try_get("reqs")?));
        }
        Ok(out)
    }

    /// 全局延迟分布（近 N 小时）。四个桶的精确计数，**不是估算的百分位**。
    pub async fn traffic_latency_mix(&self, hours: i64, now_ms: i64) -> StoreResult<[i64; 4]> {
        if !self.caps().traffic {
            return Ok([0; 4]);
        }
        let since = now_ms / 1000 - hours.clamp(1, 24 * 365) * HOUR;
        let row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(b_fast),0) AS SIGNED) AS a,
                    CAST(COALESCE(SUM(b_mid),0) AS SIGNED) AS b,
                    CAST(COALESCE(SUM(b_slow),0) AS SIGNED) AS c,
                    CAST(COALESCE(SUM(b_vslow),0) AS SIGNED) AS d
             FROM traffic_hourly WHERE hour_ts >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        Ok([
            row.try_get("a")?,
            row.try_get("b")?,
            row.try_get("c")?,
            row.try_get("d")?,
        ])
    }

    /// 插件下载排行（近 N 天）。
    pub async fn plugin_downloads(
        &self,
        days: i64,
        limit: i64,
        now_ms: i64,
    ) -> StoreResult<Vec<(String, i64)>> {
        if !self.caps().traffic {
            return Ok(Vec::new());
        }
        let since = now_ms / 1000 - days.clamp(1, 3650) * DAY;
        let rows = sqlx::query(
            "SELECT name, CAST(SUM(downloads) AS SIGNED) AS n
             FROM plugin_downloads_hourly WHERE hour_ts >= ?
             GROUP BY name ORDER BY n DESC LIMIT ?",
        )
        .bind(since)
        .bind(limit.clamp(1, 100))
        .fetch_all(self.pool())
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push((r.try_get("name")?, r.try_get("n")?));
        }
        Ok(out)
    }

    /// 指标覆盖的时间范围（最早/最晚的小时桶）。
    ///
    /// 面板用它如实标注「数据从什么时候开始有」——主服务是某次发版才开始采集的，
    /// 在那之前的时间段是**没有数据**而不是**没有流量**，两者必须区分开。
    pub async fn traffic_coverage(&self) -> StoreResult<Option<(i64, i64)>> {
        if !self.caps().traffic {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT CAST(COALESCE(MIN(hour_ts), 0) AS SIGNED) AS lo,
                    CAST(COALESCE(MAX(hour_ts), 0) AS SIGNED) AS hi
             FROM traffic_hourly",
        )
        .fetch_one(self.pool())
        .await?;
        let lo: i64 = row.try_get("lo")?;
        let hi: i64 = row.try_get("hi")?;
        if lo == 0 && hi == 0 {
            return Ok(None);
        }
        Ok(Some((lo, hi)))
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
