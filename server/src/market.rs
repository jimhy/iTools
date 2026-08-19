//! 插件提审与市场：收包 → 机械校验 → 大模型审核 → 发布索引。
//!
//! # 这条链路取代了什么
//!
//! 此前插件市场的真相源是 GitHub 仓库里的 `registry/index.json`，收录靠人工提 Issue/PR，
//! 客户端从 raw.githubusercontent.com 拉索引、从 GitHub 归档下载插件包。现在**索引与包都在这里**：
//! 作者在开发者中心点「提交审核」直接把打包好的插件上传上来，服务端审完就直接发布。
//!
//! # 审核是两段，不是一段
//!
//! 1. **机械校验**（[`crate::pkg`]，同步、必过）：格式、清单、可执行文件、路径穿越、体积。
//!    这一段失败就当场拒绝，连模型都不用调。
//! 2. **大模型审核**（[`crate::llm`]，异步）：读代码判断恶意行为、权限名副其实、描述相符。
//!    这一段耗时几十秒到几分钟，所以提交接口**立即返回**一个 `reviewing` 的提审单 id，
//!    作者在开发者中心轮询状态。
//!
//! # 诚信约束（doc/开发准则.md）
//!
//! - **模型没配 / 调用失败 / 裁决不可解析 → 落 `manual`（待人工处理），绝不自动上线。**
//!   这是全模块最重要的一条：把「没审成」当成「审过了」，等于市场上所有「已审核」的字样都是假的。
//! - 进程重启会中断审核，启动时把卡住的 `reviewing` 单子如实改判为 `manual` 并写明原因，
//!   不留一个永远转圈的状态。
//! - 索引里的 `reviewedBy` 如实写明是哪个模型审的，客户端据此告诉用户「这是自动审核，不是人工审计」。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::llm;
use crate::pkg::{self, StagedPackage};
use crate::store::{MarketRow, MariaDbStore, Submission};
use crate::Clock;

/// 提审单状态。
pub mod status {
    /// 已收下，正在审核。
    pub const REVIEWING: &str = "reviewing";
    /// 审核通过，已上线。
    pub const APPROVED: &str = "approved";
    /// 审核未通过。
    pub const REJECTED: &str = "rejected";
    /// 审核没能完成（模型未配置 / 调用失败 / 服务重启），需要维护者人工处理。
    pub const MANUAL: &str = "manual";
}

/// 进程重启后，`reviewing` 状态被视为「已中断」的年龄阈值。
///
/// 启动时一律改判——审核任务活在内存里，进程一没它就永远不会有结论。
/// 不设「等一等说不定还在跑」的宽限：那只会让作者盯着一个永远转圈的状态。
const STALE_REVIEW_MS: i64 = 0;

/// 市场索引的响应体（带 ETag，客户端可 304）。
#[derive(Clone)]
pub struct IndexBody {
    pub json: String,
    pub etag: String,
}

pub struct MarketService {
    store: Arc<MariaDbStore>,
    config: Arc<Config>,
    clock: Clock,
    /// 索引缓存：只在发布 / 下架时重建，平时直接吐（并让 ETag 稳定命中 304）。
    index: RwLock<IndexBody>,
}

impl MarketService {
    pub fn new(store: Arc<MariaDbStore>, config: Arc<Config>, clock: Clock) -> Arc<Self> {
        let empty = render_index(&[], &config);
        Arc::new(Self {
            store,
            config,
            clock,
            index: RwLock::new(empty),
        })
    }

    /// 启动时调用：装载索引缓存 + 把中断的审核如实改判。
    pub async fn bootstrap(self: &Arc<Self>) {
        if let Err(e) = self.rebuild_index().await {
            tracing::error!("[market] 装载市场索引失败：{e}（索引暂为空，发布后会自动重建）");
        }
        match self.recover_stale_reviews().await {
            Ok(0) => {}
            Ok(n) => tracing::warn!("[market] {n} 个提审单在上次运行中被中断，已改判为「待人工处理」"),
            Err(e) => tracing::error!("[market] 恢复中断的提审单失败：{e}"),
        }
    }

    /// 把进程重启前卡在 `reviewing` 的提审单改判为 `manual`。
    async fn recover_stale_reviews(&self) -> Result<usize, sqlx::Error> {
        let now = (self.clock)();
        let stale = self.store.stale_reviewing(now - STALE_REVIEW_MS).await?;
        for id in &stale {
            self.store
                .finish_submission(
                    id,
                    status::MANUAL,
                    "审核过程被服务重启中断，未得出结论。请重新提交审核。",
                    "",
                    now,
                )
                .await?;
        }
        Ok(stale.len())
    }

    pub async fn index(&self) -> IndexBody {
        self.index.read().await.clone()
    }

    /// 从数据库重建索引缓存。
    pub async fn rebuild_index(&self) -> Result<(), sqlx::Error> {
        let rows = self.store.list_entries().await?;
        let body = render_index(&rows, &self.config);
        *self.index.write().await = body;
        Ok(())
    }

    /// 已上线插件包的磁盘路径（供下载端点）。返回 `None` = 没这个插件或已下架。
    pub async fn package_path(&self, name: &str) -> Result<Option<(PathBuf, MarketRow)>, sqlx::Error> {
        let Some(row) = self.store.get_entry(name).await? else {
            return Ok(None);
        };
        if row.revoked {
            return Ok(None);
        }
        let p = self.config.market.packages_dir.join(&row.package_file);
        Ok(Some((p, row)))
    }

    /// 收一个提审包。**同步**做完机械校验与落盘，然后把大模型审核丢到后台。
    ///
    /// 返回提审单 id。任何机械校验失败都直接 `Err`，此时不会留下任何提审单——
    /// 那类问题作者当场就该看到原文，不需要事后去查记录。
    pub async fn submit(
        self: &Arc<Self>,
        author: &str,
        bytes: Vec<u8>,
    ) -> Result<Submission, SubmitError> {
        let now = (self.clock)();

        // 冷却：审核要花模型的钱，挡住连点与脚本刷提交
        let cooldown = self.config.market.submit_cooldown_sec * 1000;
        if cooldown > 0 {
            if let Ok(Some(last)) = self.store.last_submit_at(author).await {
                let wait = last + cooldown - now;
                if wait > 0 {
                    return Err(SubmitError::TooSoon((wait + 999) / 1000));
                }
            }
        }

        // 解压 + 机械校验（CPU/IO 密集，挪出异步线程）
        let tmp = tempfile::tempdir().map_err(|e| SubmitError::Server(format!("创建临时目录失败: {e}")))?;
        let tmp_path = tmp.path().to_path_buf();
        let bytes_for_stage = bytes.clone();
        let staged = tokio::task::spawn_blocking(move || pkg::stage(&bytes_for_stage, &tmp_path))
            .await
            .map_err(|e| SubmitError::Server(format!("校验任务异常终止: {e}")))?
            .map_err(SubmitError::Rejected)?;

        let name = staged.manifest.name.clone();
        let version = staged.manifest.version.trim().to_string();
        if !is_safe_version(&version) {
            return Err(SubmitError::Rejected(format!(
                "version「{version}」含非法字符：只允许字母、数字与 . - _"
            )));
        }

        // 归属校验：同名插件只能由首次上线它的账号更新。
        // 客户端按插件名归属授权与数据，顶包等于直接继承受害插件的用户授权——这条必须挡死。
        match self.store.get_entry(&name).await {
            // 被维护者下架的插件不能靠「发一个新版本」复活——那等于把处置绕过去了。
            // 作者自己下架的不在此列：再发一版就是重新上线，这正是作者想要的。
            Ok(Some(existing))
                if existing.revoked && existing.revoked_by == crate::store::revoked_by::ADMIN =>
            {
                return Err(SubmitError::Forbidden(format!(
                    "「{name}」已被平台维护者下架，暂不接受新版本。下架原因：{}。如有异议请联系维护者。",
                    if existing.revoked_reason.is_empty() {
                        "未给出原因"
                    } else {
                        &existing.revoked_reason
                    }
                )));
            }
            Ok(Some(existing)) if existing.owner != author => {
                return Err(SubmitError::Forbidden(format!(
                    "插件名「{name}」已被账号 {} 占用。请换一个 name —— 它同时是安装目录名与数据命名空间，不能重名。",
                    mask(&existing.owner)
                )));
            }
            Ok(Some(existing)) if !pkg::version_gt(&version, &existing.version) => {
                return Err(SubmitError::Rejected(format!(
                    "版本号 {version} 不高于线上的 {}。改了代码就升 version（客户端的更新检查只比这个值）。",
                    existing.version
                )));
            }
            Err(e) => return Err(SubmitError::Server(format!("查询市场条目失败: {e}"))),
            _ => {}
        }

        // 包先落盘（审核通过后直接发布，不用作者再传一次）
        let package_file = format!("{name}/{version}.zip");
        let dest = self.config.market.packages_dir.join(&package_file);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SubmitError::Server(format!("创建包目录失败: {e}")))?;
        }
        std::fs::write(&dest, &bytes).map_err(|e| SubmitError::Server(format!("保存插件包失败: {e}")))?;

        let sub = Submission {
            id: new_id(),
            name: name.clone(),
            version: version.clone(),
            author: author.to_string(),
            status: status::REVIEWING.to_string(),
            content_hash: staged.content_hash.clone(),
            file_count: staged.file_count as i64,
            size_bytes: staged.total_bytes as i64,
            manifest: serde_json::to_string(&staged.manifest).unwrap_or_default(),
            review: String::new(),
            message: if self.config.llm.enabled() {
                format!("机械校验已通过，正在由 {} 审阅代码…", self.config.llm.model)
            } else {
                "机械校验已通过。服务端未配置审核模型，本次提审需要维护者人工处理。".to_string()
            },
            created_at: now,
            updated_at: now,
        };
        if let Err(e) = self.store.create_submission(&sub).await {
            return Err(SubmitError::Server(format!("写入提审单失败: {e}")));
        }

        // 后台审核：不阻塞提交响应
        let me = Arc::clone(self);
        let id = sub.id.clone();
        tokio::spawn(async move {
            me.run_review(id, staged, package_file).await;
        });

        Ok(sub)
    }

    /// 后台审核任务：调模型 → 通过则发布。
    ///
    /// 这个函数**不返回错误**——它是 detached 任务，任何失败都必须落到提审单的状态上，
    /// 否则作者那边就是一个永远「审核中」的黑洞。
    async fn run_review(&self, id: String, staged: StagedPackage, package_file: String) {
        let outcome = llm::review(&self.config.llm, &staged).await;
        let now = (self.clock)();

        let (status_, message, review_raw) = match outcome {
            Err(e) => {
                // 审核没能完成 —— 如实落「待人工处理」，绝不放行
                tracing::warn!("[market] 提审单 {id} 审核未完成：{e}");
                (
                    status::MANUAL.to_string(),
                    format!("自动审核未能完成：{e}\n本次提审已转由维护者人工处理，插件尚未上线。"),
                    String::new(),
                )
            }
            Ok(o) if o.verdict.approved() => {
                (status::APPROVED.to_string(), llm::brief(&o.verdict), o.raw)
            }
            Ok(o) => (status::REJECTED.to_string(), o.verdict.reject_reason(), o.raw),
        };

        if status_ == status::APPROVED {
            if let Err(e) = self.publish(&staged, &package_file, &id, now).await {
                tracing::error!("[market] 提审单 {id} 审核通过但发布失败：{e}");
                let _ = self
                    .store
                    .finish_submission(
                        &id,
                        status::MANUAL,
                        &format!("审核通过，但发布到市场时失败：{e}。请联系维护者。"),
                        &review_raw,
                        now,
                    )
                    .await;
                return;
            }
        }

        // 被驳回的包不会有人再用它，留在磁盘上只是慢性泄漏（每次提审最多 32MB）。
        // **只删 rejected**：`manual` 是「自动审核没做成、等维护者人工看」——
        // 把待审的包删掉，人工审核就无从下手了。
        if status_ == status::REJECTED {
            self.discard_package(&package_file, &id);
        }

        if let Err(e) = self
            .store
            .finish_submission(&id, &status_, &message, &review_raw, now)
            .await
        {
            tracing::error!("[market] 回填提审单 {id} 结论失败：{e}");
        }
    }

    /// 删掉一个不会再被使用的提审包（连同空掉的插件目录）。
    ///
    /// 失败只记日志：包没删掉不影响任何功能，但把它当成致命错误会让一次磁盘权限问题
    /// 演变成「提审单永远没有结论」。
    fn discard_package(&self, package_file: &str, id: &str) {
        let path = self.config.market.packages_dir.join(package_file);
        if let Err(e) = std::fs::remove_file(&path) {
            // 文件本就不存在时不值得报警（重试、并发清理都可能走到这里）
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("[market] 提审单 {id} 的包清理失败（{}）：{e}", path.display());
            }
            return;
        }
        // 该插件目录若已空则一并删掉，不留一堆空目录。非空（有其它版本）时会失败，忽略即可。
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
        tracing::info!("[market] 提审单 {id} 被驳回，已清理其插件包");
    }

    /// 把审核通过的包写进市场条目并重建索引。
    async fn publish(
        &self,
        staged: &StagedPackage,
        package_file: &str,
        submission_id: &str,
        now: i64,
    ) -> Result<(), String> {
        let m = &staged.manifest;
        let author = self
            .store
            .get_submission(submission_id)
            .await
            .map_err(|e| format!("查询提审单失败: {e}"))?
            .map(|s| s.author)
            .unwrap_or_default();

        let published_at = match self.store.get_entry(&m.name).await {
            Ok(Some(prev)) => prev.published_at, // 更新不改首发时间
            _ => now,
        };

        let entry = json!({
            "name": m.name,
            "displayName": m.display_name,
            "description": m.description,
            // 作者一律用**服务端确认的提审账号**，不采信 plugin.json 里的 author：
            // 那个字段是作者自己填的，可以随便写成别人的名字（deskbox 就写着模板占位的 "you"）。
            // 市场页上「作者」这一栏必须是可追责的身份，否则它什么也证明不了。
            "author": author,
            // 作者自称的署名另存一份，供插件详情页展示「作者自己写的是什么」，不与上面混为一谈。
            "authorDeclared": m.author,
            "version": m.version,
            "contentHash": staged.content_hash,
            "fileCount": staged.file_count,
            "sizeBytes": staged.total_bytes,
            "permissions": m.permissions,
            "keywords": m.keywords(),
            "featureCount": m.features.len(),
            "homepage": m.homepage,
            "license": m.license,
            "readme": staged.readme,
            // 如实标注审核方式：客户端市场页据此告诉用户「自动审核，不是人工审计」
            "reviewedBy": self.config.llm.model,
            "reviewedAt": now,
            "publishedAt": published_at,
            "updatedAt": now,
            "revoked": false,
            "revokedReason": "",
            "revokedBy": "",
        });

        let row = MarketRow {
            name: m.name.clone(),
            owner: author,
            version: m.version.clone(),
            content_hash: staged.content_hash.clone(),
            package_file: package_file.to_string(),
            entry: serde_json::to_string(&entry).map_err(|e| format!("序列化条目失败: {e}"))?,
            revoked: false,
            revoked_reason: String::new(),
            revoked_by: String::new(),
            published_at,
            updated_at: now,
        };
        self.store.publish_entry(&row).await.map_err(|e| format!("写入市场条目失败: {e}"))?;
        self.rebuild_index().await.map_err(|e| format!("重建索引失败: {e}"))?;
        tracing::info!("[market] 已上线 {} v{}", m.name, m.version);
        Ok(())
    }

    /// 下架 / 恢复一个插件。
    ///
    /// # 谁能动谁
    ///
    /// - **作者**（`entry.owner == actor`）：可以下架自己的插件，也可以把**自己下架的**恢复回来。
    /// - **维护者**：可以下架 / 恢复任何插件。
    /// - 作者**不能**恢复维护者下架的插件 —— 那是处置，不是作者的自主选择。
    ///   （同一道防线在 [`Self::submit`] 与 `store::publish_entry` 里也拦了「发新版本复活」这条路。）
    ///
    /// 恢复时 `reason` 与 `revoked_by` 一并清空，不留一条过期的下架原因挂在条目上。
    pub async fn set_revoked(
        &self,
        actor: &str,
        name: &str,
        revoked: bool,
        reason: &str,
    ) -> Result<(), RevokeError> {
        let entry = self
            .store
            .get_entry(name)
            .await
            .map_err(|e| RevokeError::Server(format!("查询市场条目失败: {e}")))?
            .ok_or(RevokeError::NotFound)?;

        let by = decide_revoke(actor, &entry, self.is_admin(actor), revoked)
            .map_err(RevokeError::Forbidden)?;
        let now = (self.clock)();
        let ok = self
            .store
            .set_revoked(name, revoked, reason, by, now)
            .await
            .map_err(|e| RevokeError::Server(format!("更新市场条目失败: {e}")))?;
        if !ok {
            // get_entry 刚查到、这里却没有命中：条目在这两步之间被删了
            return Err(RevokeError::NotFound);
        }
        self.rebuild_index()
            .await
            .map_err(|e| RevokeError::Server(format!("重建索引失败: {e}")))?;
        tracing::info!(
            "[market] {name} 已{}（操作者身份：{by}）",
            if revoked { "下架" } else { "恢复上架" }
        );
        Ok(())
    }

    pub fn is_admin(&self, user: &str) -> bool {
        self.config.market.admins.iter().any(|a| a == user)
    }

    pub fn store(&self) -> &MariaDbStore {
        &self.store
    }
}

/// 提交失败的分类（决定 HTTP 状态码与文案）。
pub enum SubmitError {
    /// 包本身不合格（400）——原因是给作者看的中文原文。
    Rejected(String),
    /// 归属冲突等（403）。
    Forbidden(String),
    /// 提交过于频繁（429），带需要等待的秒数。
    TooSoon(i64),
    /// 服务端自身问题（500）——细节只进日志。
    Server(String),
}

/// 下架 / 恢复失败的分类。
pub enum RevokeError {
    /// 市场里没有这个插件（404）。
    NotFound,
    /// 不是作者、也不是维护者；或作者想恢复维护者下架的插件（403）——原文写给操作者看。
    Forbidden(String),
    /// 服务端自身问题（500）——细节只进日志。
    Server(String),
}

/// 下架 / 恢复的**鉴权判定**。抽成纯函数是为了能单测——这几条边界一旦写错，
/// 要么谁都能下架别人的插件，要么被处置的插件作者自己就能恢复上架。
///
/// 返回本次操作应记录的下架方（[`crate::store::revoked_by`] 的常量），
/// `Err` 里是给操作者看的中文原文。
fn decide_revoke(
    actor: &str,
    entry: &MarketRow,
    is_admin: bool,
    revoked: bool,
) -> Result<&'static str, String> {
    use crate::store::revoked_by;

    let is_owner = entry.owner == actor;
    if !is_owner && !is_admin {
        // 不暴露「这个插件属于谁」——只说你不是作者
        return Err("只有插件作者本人或平台维护者可以下架 / 恢复这个插件。".to_string());
    }
    // 维护者下的架，作者自己收不回来（同一道防线在 submit 与 publish_entry 里也拦了发新版本这条路）
    if !revoked && !is_admin && entry.revoked_by == revoked_by::ADMIN {
        return Err(format!(
            "「{}」是被平台维护者下架的，作者无法自行恢复。下架原因：{}。如有异议请联系维护者。",
            entry.name,
            if entry.revoked_reason.is_empty() {
                "未给出原因"
            } else {
                &entry.revoked_reason
            }
        ));
    }
    // 维护者操作自己的插件时按「作者」记：那是他的自主选择，理应能自己收回
    Ok(if is_owner { revoked_by::OWNER } else { revoked_by::ADMIN })
}

/// 渲染市场索引 JSON + ETag。
///
/// 形状与客户端 `src-tauri/src/plugin/market.rs::MarketIndex` 对齐。
/// **已下架的条目照样在索引里**（带 `revoked`、原因与下架方）——客户端要靠它提醒已安装的用户。
fn render_index(rows: &[MarketRow], config: &Config) -> IndexBody {
    let plugins: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v: Value = serde_json::from_str(&r.entry).unwrap_or_else(|_| json!({}));
            if let Some(o) = v.as_object_mut() {
                // 以行上的权威字段为准（下架状态可能在条目写好之后才改）
                o.insert("name".into(), json!(r.name));
                o.insert("version".into(), json!(r.version));
                o.insert("contentHash".into(), json!(r.content_hash));
                o.insert("revoked".into(), json!(r.revoked));
                o.insert("revokedReason".into(), json!(r.revoked_reason));
                // 下架方（owner / admin）：开发者中心据此决定作者能不能自己恢复上架，
                // 不给这个字段，前端就只能画一个「点了才知道行不行」的按钮。
                o.insert("revokedBy".into(), json!(r.revoked_by));
                // 下载地址是**相对路径**：客户端拼上自己配置的服务器地址即可，
                // 服务端不需要知道自己被从哪个域名访问（反代 / 内网 / 端口转发都不影响）。
                o.insert("package".into(), json!(format!("/api/market/package/{}", r.name)));
            }
            v
        })
        .collect();

    let doc = MarketIndexDoc {
        version: 1,
        review_mode: if config.llm.enabled() { "llm" } else { "manual" },
        review_model: if config.llm.enabled() { config.llm.model.clone() } else { String::new() },
        plugins,
    };
    let json = serde_json::to_string(&doc).unwrap_or_else(|_| r#"{"version":1,"plugins":[]}"#.to_string());
    let etag = format!("\"{}\"", hex::encode(&Sha256::digest(json.as_bytes())[..8]));
    IndexBody { json, etag }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MarketIndexDoc {
    version: u32,
    /// `llm` = 由大模型自动审核；`manual` = 审核模型未接入，条目由维护者人工放行。
    /// 客户端市场页据此如实措辞，不把「自动审核」说成「人工审计」。
    review_mode: &'static str,
    review_model: String,
    plugins: Vec<Value>,
}

/// 版本号只允许出现在路径里安全的字符（它会成为包文件名的一部分）。
fn is_safe_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        && !v.contains("..")
}

/// 提审单 id：32 位十六进制随机串。
fn new_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// 别人的账号名对外只露头尾（提示归属冲突时用，不泄露完整账号）。
fn mask(u: &str) -> String {
    let chars: Vec<char> = u.chars().collect();
    match chars.len() {
        0 => "（未知）".to_string(),
        1..=2 => format!("{}*", chars[0]),
        n => format!("{}***{}", chars[0], chars[n - 1]),
    }
}

/// 判断 If-None-Match 是否命中（与 `mirrors::matches_etag` 同口径）。
pub fn matches_etag(if_none_match: Option<&str>, etag: &str) -> bool {
    let Some(raw) = if_none_match else {
        return false;
    };
    raw.split(',').map(str::trim).any(|t| t == etag || t == "*")
}

/// 供路由层：包文件的磁盘读取（带存在性检查）。
pub fn read_package(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("读取插件包失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model_key: &str) -> Arc<Config> {
        let mut env = std::collections::HashMap::new();
        if !model_key.is_empty() {
            env.insert("ITOOLS_LLM_API_KEY".to_string(), model_key.to_string());
        }
        Arc::new(Config::from_map(&env).unwrap())
    }

    fn row(name: &str, revoked: bool) -> MarketRow {
        MarketRow {
            name: name.into(),
            owner: "u".into(),
            version: "1.0.0".into(),
            content_hash: "sha256:aa".into(),
            package_file: format!("{name}/1.0.0.zip"),
            entry: json!({"name": name, "description": "d"}).to_string(),
            revoked,
            revoked_reason: if revoked { "有问题".into() } else { String::new() },
            revoked_by: if revoked { "admin".into() } else { String::new() },
            published_at: 1,
            updated_at: 2,
        }
    }

    #[test]
    fn index_marks_review_mode_honestly() {
        let no_llm = render_index(&[], &cfg(""));
        assert!(no_llm.json.contains(r#""reviewMode":"manual""#), "{}", no_llm.json);
        let with_llm = render_index(&[], &cfg("sk-x"));
        assert!(with_llm.json.contains(r#""reviewMode":"llm""#));
        assert!(with_llm.json.contains("gpt-5.5"));
    }

    #[test]
    fn index_keeps_revoked_entries_with_reason() {
        let body = render_index(&[row("bad", true)], &cfg("sk-x"));
        let v: Value = serde_json::from_str(&body.json).unwrap();
        let p = &v["plugins"][0];
        assert_eq!(p["revoked"], json!(true));
        assert_eq!(p["revokedReason"], json!("有问题"));
        // 少了 revokedBy，开发者中心就无法区分「自己下的架」和「被维护者下架」
        assert_eq!(p["revokedBy"], json!("admin"));
    }

    #[test]
    fn index_injects_relative_package_url() {
        let body = render_index(&[row("demo", false)], &cfg("sk-x"));
        let v: Value = serde_json::from_str(&body.json).unwrap();
        assert_eq!(v["plugins"][0]["package"], json!("/api/market/package/demo"));
    }

    #[test]
    fn etag_stable_and_content_sensitive() {
        let a = render_index(&[row("demo", false)], &cfg("sk-x"));
        let b = render_index(&[row("demo", false)], &cfg("sk-x"));
        assert_eq!(a.etag, b.etag, "同样内容必须给同样 ETag，否则 304 永不命中");
        let c = render_index(&[row("demo", true)], &cfg("sk-x"));
        assert_ne!(a.etag, c.etag);
    }

    #[test]
    fn version_path_safety() {
        assert!(is_safe_version("1.0.0"));
        assert!(is_safe_version("2.1.0-rc1"));
        assert!(!is_safe_version("../../etc"));
        assert!(!is_safe_version("1.0/0"));
        assert!(!is_safe_version(""));
    }

    #[test]
    fn etag_match_rules() {
        assert!(matches_etag(Some("\"abc\""), "\"abc\""));
        assert!(matches_etag(Some("\"x\", \"abc\""), "\"abc\""));
        assert!(matches_etag(Some("*"), "\"abc\""));
        assert!(!matches_etag(None, "\"abc\""));
        assert!(!matches_etag(Some("\"zzz\""), "\"abc\""));
    }

    /// 下架 / 恢复的权限边界。这几条判错的后果分别是「谁都能下架别人的插件」
    /// 与「被平台处置的插件作者自己就能恢复上架」，都必须钉死。
    #[test]
    fn revoke_permission_boundaries() {
        use crate::store::revoked_by;
        let mut mine = row("demo", false);
        mine.owner = "alice".into();

        // 作者可以下架自己的插件，记作 owner
        assert_eq!(decide_revoke("alice", &mine, false, true).unwrap(), revoked_by::OWNER);
        // 陌生人不行
        assert!(decide_revoke("mallory", &mine, false, true).is_err());
        // 维护者可以下架别人的插件，记作 admin
        assert_eq!(decide_revoke("root", &mine, true, true).unwrap(), revoked_by::ADMIN);
        // 维护者动自己的插件按 owner 记（那是自主选择，他理应能自己收回）
        assert_eq!(decide_revoke("alice", &mine, true, true).unwrap(), revoked_by::OWNER);
    }

    #[test]
    fn author_cannot_undo_an_admin_takedown() {
        use crate::store::revoked_by;
        let mut punished = row("demo", true); // row() 造的正是 revoked_by = admin
        punished.owner = "alice".into();
        assert_eq!(punished.revoked_by, revoked_by::ADMIN);

        // 作者恢复不了，而且必须把下架原因原文带给他，不能只说一句「没权限」
        let err = decide_revoke("alice", &punished, false, false).unwrap_err();
        assert!(err.contains("有问题"), "{err}");
        // 维护者可以恢复
        assert!(decide_revoke("root", &punished, true, false).is_ok());
        // 作者自己下的架，作者可以自己收回来
        let mut self_revoked = punished.clone();
        self_revoked.revoked_by = revoked_by::OWNER.into();
        assert_eq!(
            decide_revoke("alice", &self_revoked, false, false).unwrap(),
            revoked_by::OWNER
        );
        // 旧数据没有下架方：不当成处置，作者可以恢复（能否恢复以这条判定为准）
        let mut legacy = punished.clone();
        legacy.revoked_by = String::new();
        assert!(decide_revoke("alice", &legacy, false, false).is_ok());
    }

    #[test]
    fn owner_masked() {
        assert_eq!(mask("jimhy"), "j***y");
        assert_eq!(mask("ab"), "a*");
    }

    /// 造一个包目录指向临时路径的服务实例（存储层懒连接，本用例不碰数据库）。
    fn svc_with_packages_dir(dir: &std::path::Path) -> Arc<MarketService> {
        let mut env = std::collections::HashMap::new();
        env.insert(
            "SYNC_PACKAGES_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        );
        let config = Arc::new(Config::from_map(&env).unwrap());
        let store = Arc::new(crate::store::MariaDbStore::lazy(&config.db));
        MarketService::new(store, config, Arc::new(|| 0))
    }

    // sqlx 的连接池构造要求 Tokio 上下文（即便是 lazy、不真连库），故用 tokio::test
    #[tokio::test]
    async fn discard_removes_package_and_empty_dir() {
        // 被驳回的包留在磁盘上是慢性泄漏（每次最多 32MB），必须真的删掉
        let tmp = tempfile::tempdir().unwrap();
        let svc = svc_with_packages_dir(tmp.path());
        let pkg = tmp.path().join("demo").join("1.0.0.zip");
        std::fs::create_dir_all(pkg.parent().unwrap()).unwrap();
        std::fs::write(&pkg, b"zip").unwrap();

        svc.discard_package("demo/1.0.0.zip", "sub1");
        assert!(!pkg.exists(), "包文件应被删除");
        assert!(!tmp.path().join("demo").exists(), "空掉的插件目录应一并删除");
    }

    // sqlx 的连接池构造要求 Tokio 上下文（即便是 lazy、不真连库），故用 tokio::test
    #[tokio::test]
    async fn discard_keeps_other_versions() {
        // 同一插件的其它版本还在时，只删这一个版本，目录要保留
        let tmp = tempfile::tempdir().unwrap();
        let svc = svc_with_packages_dir(tmp.path());
        let dir = tmp.path().join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("1.0.0.zip"), b"old").unwrap();
        std::fs::write(dir.join("1.0.1.zip"), b"new").unwrap();

        svc.discard_package("demo/1.0.1.zip", "sub2");
        assert!(!dir.join("1.0.1.zip").exists());
        assert!(dir.join("1.0.0.zip").exists(), "其它版本不该被牵连");
        assert!(dir.exists(), "目录非空时不该被删");
    }

    // sqlx 的连接池构造要求 Tokio 上下文（即便是 lazy、不真连库），故用 tokio::test
    #[tokio::test]
    async fn discard_is_idempotent_and_never_panics() {
        // 清理失败绝不能演变成「提审单永远没有结论」——重复调用、文件不存在都要安静收场
        let tmp = tempfile::tempdir().unwrap();
        let svc = svc_with_packages_dir(tmp.path());
        svc.discard_package("nope/9.9.9.zip", "sub3");
        svc.discard_package("nope/9.9.9.zip", "sub3");
    }
}
