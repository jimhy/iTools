//! 开发者中心的**插件提审**：把调试目录打成 zip 上传到服务端，并查询审核状态。
//!
//! # 这条链路的两端
//!
//! 客户端只做三件事：打包、上传、查状态。审核本身（机械校验 + 大模型读代码）全在服务端，
//! 见 `server/src/pkg.rs` 与 `server/src/llm.rs`。**客户端不做任何「预判通过」的事**——
//! 本地看着没问题不代表能过审，把本地校验结果说成审核结论就是假信息。
//!
//! # 提审前的本地自检是「拦下必然失败的提交」，不是「预测审核结果」
//!
//! [`preflight`] 只拦服务端**一定**会拒的那几条（缺 index.html、name 不合法、features 为空、
//! 版本号没升……）。拦它们是为了省一次上传与一次模型调用，文案上必须说清楚
//! 「通过自检 ≠ 会过审」。
//!
//! # 诚信约束（doc/开发准则.md）
//!
//! - 状态**全部来自服务端**：审核中 / 已上线 / 已驳回 / 待人工处理，客户端一个都不自己推断；
//! - 「已上线版本号」来自市场索引（服务端的真相源），不是本地记的；
//! - 未登录 / 未配置服务器时，提审按钮**禁用并写明原因**，不做「点了没反应」的控件。

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::logging::ilog;

/// 上传超时（秒）。包最大 32MB，慢网络下留足余量。
const UPLOAD_TIMEOUT_SECS: u64 = 180;
/// 查询类请求超时（秒）。
const QUERY_TIMEOUT_SECS: u64 = 15;
/// 与服务端 `SYNC_MAX_UPLOAD_MB` 默认值对齐。本地先拦一道，省得传完 32MB 才被拒。
const MAX_PACKAGE_BYTES: u64 = 32 * 1024 * 1024;
/// 打包时跳过的目录（开发期产物，不属于插件内容，带上去只会撑爆体积或直接触发拒收）。
const SKIP_DIRS: &[&str] = &[".git", "node_modules", ".vscode", ".idea", "__pycache__"];

/// 一条提审记录（与服务端 `submission_json` 对齐）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Submission {
    pub id: String,
    pub name: String,
    pub version: String,
    /// `reviewing` 审核中 | `approved` 已上线 | `rejected` 已驳回 | `manual` 待人工处理。
    pub status: String,
    /// 给作者看的一句话结论 / 驳回原因（服务端原文，前端原样展示）。
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub file_count: i64,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// 模型裁决原文（只有查单条详情时才有）。
    #[serde(default)]
    pub review: serde_json::Value,
}

/// 一个调试插件的「发布状态」快照：本地版本 + 线上版本 + 最近一次提审。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishStatus {
    /// 提审用的插件名（`plugin.json` 的 name）。
    pub name: String,
    /// 本地调试目录里的版本。
    pub local_version: String,
    /// 市场上已上线的版本；`None` = 这个插件还没上线过。
    pub online_version: Option<String>,
    /// 线上条目是否已被下架，以及原因。
    pub revoked: bool,
    #[serde(default)]
    pub revoked_reason: String,
    /// 本地版本是否高于线上（可以提交新版本）。
    pub can_submit_new_version: bool,
    /// 该插件最近一次提审记录；`None` = 从没提交过。
    pub latest: Option<Submission>,
    /// 该插件的历史提审记录（新→旧，含 latest）。
    pub history: Vec<Submission>,
    /// 查询过程中遇到的真实错误（如未登录、服务器不可达）。
    /// **非空时上面那些字段可能是不完整的**，前端必须如实展示这条，不能渲染成「没有记录」。
    pub error: Option<String>,
}

/// 提审前的本地自检结论。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    /// 阻断项：任一非空则不允许提交（服务端也一定会拒）。
    pub blockers: Vec<String>,
    /// 提醒项：不阻断提交，但值得先看一眼。
    pub warnings: Vec<String>,
}

impl Preflight {
    pub fn ok(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// 当前服务器地址（与云同步共用）。
fn endpoint() -> Result<String, String> {
    crate::account::cloud_endpoint()
        .map(|e| e.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            "还没有配置服务器地址，无法提交审核。请到「设置 → 网络 → 服务器地址」填写后重试。"
                .to_string()
        })
}

/// 提审必须登录——审核结果与插件归属都挂在账号上。
///
/// 令牌由命令层从 `AccountStore` 取好传进来（本模块不持有账号状态）。
fn require_token(token: &str) -> Result<&str, String> {
    if token.trim().is_empty() {
        return Err(
            "提交审核需要先登录云账号（插件的归属与审核结果都挂在账号上）。请到「我的账号」页登录。"
                .to_string(),
        );
    }
    Ok(token)
}

// ==================== 打包 ====================

/// 把插件目录打成 zip（目录内容直接放在 zip 根，不套一层目录名）。
///
/// 跳过 [`SKIP_DIRS`] 与 `.` 开头的条目：`.git` / `node_modules` 动辄几百 MB，
/// 带上去要么超体积上限，要么让审核模型去读一堆无关代码。
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut total: u64 = 0;
        let mut count = 0usize;

        for entry in walkdir::WalkDir::new(dir).into_iter().filter_entry(|e| {
            // 根目录本身不参与过滤（它的 file_name 可能就叫 .something）
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
        }) {
            let entry = entry.map_err(|e| format!("遍历插件目录失败: {e}"))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(dir)
                .map_err(|e| format!("计算相对路径失败: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(entry.path()).map_err(|e| format!("读取 {rel} 失败: {e}"))?;
            total += bytes.len() as u64;
            count += 1;
            if total > MAX_PACKAGE_BYTES {
                return Err(format!(
                    "插件目录内容超过 {} MB 上限（已读到 {rel}）。请清理构建产物、大素材与依赖目录后重试。",
                    MAX_PACKAGE_BYTES / 1024 / 1024
                ));
            }
            zw.start_file(&rel, opts)
                .map_err(|e| format!("打包 {rel} 失败: {e}"))?;
            zw.write_all(&bytes).map_err(|e| format!("打包 {rel} 失败: {e}"))?;
        }
        if count == 0 {
            return Err("插件目录里没有任何可打包的文件".to_string());
        }
        zw.finish().map_err(|e| format!("生成插件包失败: {e}"))?;
    }
    Ok(buf)
}

// ==================== 本地自检 ====================

/// 提审前的本地自检。
///
/// 只判服务端**必然**会拒的那几条 —— 通过它**不代表**会过审（代码审核在服务端）。
/// `online_version` 是市场上已上线的版本，用于挡住「版本号没升」这种一定会被拒的提交。
pub fn preflight(
    dir: &Path,
    manifest_name: &str,
    version: &str,
    issues: &[super::DevIssue],
    online_version: Option<&str>,
) -> Preflight {
    let mut blockers: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    if !dir.join("index.html").is_file() {
        blockers.push("缺少 index.html —— 插件没有入口页面，装上也打不开".to_string());
    }
    if !crate::plugin::install::is_valid_plugin_name(manifest_name) {
        blockers.push(format!(
            "plugin.json 的 name「{manifest_name}」不合法：须以小写字母或数字开头，只含小写字母、数字与 . _ -，最长 64"
        ));
    }
    if version.trim().is_empty() {
        blockers.push("plugin.json 缺少 version —— 没有版本号就无法做更新检查".to_string());
    }
    if let Some(online) = online_version {
        if !crate::plugin::install::version_gt(version, online) {
            blockers.push(format!(
                "版本号 {version} 不高于已上线的 {online}。改了代码就升 version（客户端的更新检查只比这个值）。"
            ));
        }
    }

    // 调试环境的清单校验结论：error 一定过不了服务端，warn 只提醒
    for i in issues {
        if i.level == "error" {
            blockers.push(format!("{}：{}", i.field, i.message));
        } else {
            warnings.push(format!("{}：{}", i.field, i.message));
        }
    }

    Preflight { blockers, warnings }
}

// ==================== 与服务端交互 ====================

/// 上传插件包，返回服务端建立的提审单。
pub fn submit(token: &str, bytes: Vec<u8>) -> Result<Submission, String> {
    let ep = endpoint()?;
    let tk = require_token(token)?;
    let url = format!("{ep}/api/plugins/submit");
    let size = bytes.len();
    let resp = crate::http::post(&url)
        .timeout(std::time::Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .set("Authorization", &format!("Bearer {tk}"))
        .set("Content-Type", "application/zip")
        .send_bytes(&bytes)
        .map_err(|e| describe("提交审核", e))?;
    let text = resp
        .into_string()
        .map_err(|e| format!("读取服务端响应失败: {e}"))?;
    let sub: Submission = serde_json::from_str(&text)
        .map_err(|e| format!("服务端响应解析失败: {e}（原文前 200 字：{}）", head(&text)))?;
    ilog!("[iTools] 已提交审核 {} v{}（{size} 字节），提审单 {}", sub.name, sub.version, sub.id);
    Ok(sub)
}

/// 我的全部提审记录（新→旧）。
pub fn list_submissions(token: &str) -> Result<Vec<Submission>, String> {
    let ep = endpoint()?;
    let tk = require_token(token)?;
    let url = format!("{ep}/api/plugins/submissions");
    let text = crate::http::get(&url)
        .timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS))
        .set("Authorization", &format!("Bearer {tk}"))
        .call()
        .map_err(|e| describe("查询提审记录", e))?
        .into_string()
        .map_err(|e| format!("读取服务端响应失败: {e}"))?;

    #[derive(Deserialize)]
    struct Wrap {
        #[serde(default)]
        submissions: Vec<Submission>,
    }
    let wrap: Wrap = serde_json::from_str(&text)
        .map_err(|e| format!("提审记录解析失败: {e}（原文前 200 字：{}）", head(&text)))?;
    Ok(wrap.submissions)
}

/// 单条提审单的详情（含模型裁决原文）。
pub fn get_submission(token: &str, id: &str) -> Result<Submission, String> {
    let ep = endpoint()?;
    let tk = require_token(token)?;
    let url = format!("{ep}/api/plugins/submissions/{id}");
    let text = crate::http::get(&url)
        .timeout(std::time::Duration::from_secs(QUERY_TIMEOUT_SECS))
        .set("Authorization", &format!("Bearer {tk}"))
        .call()
        .map_err(|e| describe("查询提审详情", e))?
        .into_string()
        .map_err(|e| format!("读取服务端响应失败: {e}"))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("提审详情解析失败: {e}（原文前 200 字：{}）", head(&text)))
}

fn head(s: &str) -> String {
    s.chars().take(200).collect()
}

/// 把 ureq 错误翻成能照做的中文（并保留服务端原文）。
fn describe(what: &str, e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            // 服务端的错误体形如 {"error":"…"}，那句话是写给作者看的，必须原样带出来
            let body = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| head(&body));
            match code {
                401 => format!("{what}失败：登录已失效，请重新登录后再试"),
                403 => format!("{what}被拒绝：{msg}"),
                413 => format!("{what}失败：插件包超过服务端允许的体积上限"),
                429 => format!("{what}失败：{msg}"),
                _ if msg.is_empty() => format!("{what}失败：HTTP {code}"),
                _ => format!("{what}失败：{msg}"),
            }
        }
        ureq::Error::Transport(t) => {
            format!("{what}失败：连接不上服务器（{t}）。请检查「设置 → 网络」里的服务器地址与网络。")
        }
    }
}

/// 读一个 zip 的条目数（仅测试与诊断用）。
#[cfg(test)]
fn zip_entry_names(bytes: &[u8]) -> Vec<String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("应是合法 zip");
    (0..z.len())
        .map(|i| z.by_index(i).expect("条目可读").name().to_string())
        .collect()
}

/// 读取 zip 里某个文件的内容（仅测试用）。
#[cfg(test)]
fn zip_read(bytes: &[u8], name: &str) -> Option<String> {
    use std::io::Read as _;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).ok()?;
    let mut f = z.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("itools-submit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn pack_puts_plugin_json_at_zip_root() {
        let d = tmpdir("root");
        write(&d, "plugin.json", r#"{"name":"demo"}"#);
        write(&d, "index.html", "<html></html>");
        write(&d, "assets/app.js", "console.log(1)");
        let zip = pack_dir(&d).unwrap();
        let names = zip_entry_names(&zip);
        assert!(names.contains(&"plugin.json".to_string()), "{names:?}");
        assert!(names.contains(&"assets/app.js".to_string()), "{names:?}");
        assert_eq!(zip_read(&zip, "plugin.json").unwrap(), r#"{"name":"demo"}"#);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pack_skips_dev_junk() {
        let d = tmpdir("skip");
        write(&d, "plugin.json", "{}");
        write(&d, "node_modules/left-pad/index.js", "x");
        write(&d, ".git/config", "y");
        write(&d, ".env", "SECRET=1");
        let zip = pack_dir(&d).unwrap();
        let names = zip_entry_names(&zip);
        assert_eq!(names, vec!["plugin.json".to_string()], "开发期垃圾与点文件都不该进包");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_dir_is_an_error_not_an_empty_zip() {
        let d = tmpdir("empty");
        assert!(pack_dir(&d).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn preflight_blocks_what_server_would_reject() {
        let d = tmpdir("pre");
        write(&d, "plugin.json", "{}");
        // 没有 index.html + name 不合法 + 版本没升
        let p = preflight(&d, "Bad Name", "1.0.0", &[], Some("1.0.0"));
        assert!(!p.ok());
        assert!(p.blockers.iter().any(|b| b.contains("index.html")));
        assert!(p.blockers.iter().any(|b| b.contains("Bad Name")));
        assert!(p.blockers.iter().any(|b| b.contains("不高于已上线")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn preflight_passes_a_clean_plugin() {
        let d = tmpdir("clean");
        write(&d, "plugin.json", "{}");
        write(&d, "index.html", "<html></html>");
        let p = preflight(&d, "demo", "1.0.1", &[], Some("1.0.0"));
        assert!(p.ok(), "{:?}", p.blockers);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dev_issues_split_into_blockers_and_warnings() {
        let d = tmpdir("issues");
        write(&d, "plugin.json", "{}");
        write(&d, "index.html", "<html></html>");
        let issues = vec![
            super::super::DevIssue {
                level: "error".into(),
                field: "plugin.json".into(),
                message: "解析失败".into(),
            },
            super::super::DevIssue {
                level: "warn".into(),
                field: "description".into(),
                message: "描述为空".into(),
            },
        ];
        let p = preflight(&d, "demo", "1.0.1", &issues, None);
        assert_eq!(p.blockers.len(), 1);
        assert_eq!(p.warnings.len(), 1);
        assert!(p.blockers[0].contains("解析失败"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn first_submission_has_no_version_floor() {
        let d = tmpdir("first");
        write(&d, "plugin.json", "{}");
        write(&d, "index.html", "<html></html>");
        // 没上线过时，任何版本号都可以提交
        let p = preflight(&d, "demo", "0.0.1", &[], None);
        assert!(p.ok(), "{:?}", p.blockers);
        let _ = std::fs::remove_dir_all(&d);
    }
}
