//! 版本更新检查与安装：**Gitee Releases 单一源**（中国区）。
//!
//! 设计：
//! - 检查更新：读 `releases/latest`，semver 比对，返回是否有新版 + 下载页 + msi 直链。
//! - 半自动安装：`download_update` 下载 msi 到临时目录，`launch_installer_and_quit`
//!   调起 `msiexec /i`（交互式向导）并退出当前 app，让安装程序替换正在运行的 exe。
//! - 版本号按语义化比较（semver），忽略 tag 前缀 `v`（发版 tag 形如 `v0.1.0`）。
//! - 网络失败静默降级：失败仅返回 Err，由前端决定是否打扰用户。
//!
//! 安全：**访问令牌绝不写进源码/二进制**。Gitee token 在运行期从环境变量
//! `ITOOLS_GITEE_TOKEN` 读取——设置了就带上（可读私有仓库 / 防限流），未设置则匿名
//! 请求（公开仓库读取 release 无需鉴权）。发版所需的写权限 token 放在发版环境 /
//! CI secret 的同名变量里，不随客户端分发。
//!
//! 命令均为同步 `#[tauri::command]`：Tauri 在独立线程执行，阻塞式 ureq 请求/下载
//! 不会占用 async 运行时的工作线程。

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 更新源仓库的环境变量覆盖（形如 `owner/repo`）。满足准则「服务端地址可配置、不写死」：
/// 默认指向下方公开发布仓库，可用 `ITOOLS_UPDATE_REPO` 覆盖（换发布主体 / 私有仓库）。
const UPDATE_REPO_ENV: &str = "ITOOLS_UPDATE_REPO";
/// 默认发布仓库 owner/repo（公开仓库，客户端匿名即可读 release）。
const DEFAULT_OWNER: &str = "jimyliu";
const DEFAULT_REPO: &str = "itools_release";

/// 解析更新源 `(owner, repo)`：优先环境变量 `ITOOLS_UPDATE_REPO=owner/repo`，否则用默认。
fn update_repo() -> (String, String) {
    if let Ok(v) = std::env::var(UPDATE_REPO_ENV) {
        if let Some((o, r)) = v.trim().split_once('/') {
            if !o.is_empty() && !r.is_empty() {
                return (o.to_string(), r.to_string());
            }
        }
    }
    (DEFAULT_OWNER.to_string(), DEFAULT_REPO.to_string())
}

/// 环境变量名：可选的 Gitee 访问令牌。源码与二进制中均**不含**明文 token。
/// 公开仓库读取 release 无需设置；私有仓库或防限流时在运行/构建环境导出此变量。
const GITEE_TOKEN_ENV: &str = "ITOOLS_GITEE_TOKEN";

/// 检查更新请求超时（秒）。
const TIMEOUT_SECS: u64 = 8;
/// 下载安装包超时（秒）——安装包较大，给足时间。
const DOWNLOAD_TIMEOUT_SECS: u64 = 600;
/// 请求 User-Agent。
const USER_AGENT: &str = "itools-updater";

/// 远端最新版本信息（归一化后返回给前端）。字段以 camelCase 序列化，方便前端使用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// 最新版本号（已去除 `v` 前缀），如 `1.2.3`。
    pub latest_version: String,
    /// 当前应用版本（来自 `CARGO_PKG_VERSION`）。
    pub current_version: String,
    /// 是否有新版本可用。
    pub has_update: bool,
    /// 该版本的下载页 URL（release 页面，非直链）。
    pub release_url: String,
    /// release 说明正文（markdown）。
    pub release_notes: String,
    /// msi 安装包直链（release 附件里第一个 `.msi` 结尾者），无则 None。
    /// 前端据此决定是否提供「立即更新」（自动下载 + 调起安装）。
    pub msi_url: Option<String>,
}

/// 单个 release 的最小化字段（Gitee v5）。
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

/// release 附件（Gitee v5 只暴露下载直链）。
#[derive(Debug, Deserialize)]
struct Asset {
    #[serde(default)]
    browser_download_url: String,
}

/// 语义化版本比较：`a > b` 返回 true。缺失段按 0 补齐，解析失败按 0 兜底。
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.trim_start_matches('v')
            .split('.')
            .map(|x| x.trim().parse().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 拉取 Gitee 最新 release。若环境变量 `ITOOLS_GITEE_TOKEN` 存在则附带鉴权，
/// 否则匿名请求（公开仓库可读）。token 只从环境变量读，不落代码。
fn fetch_gitee() -> Result<Release, String> {
    let (owner, repo) = update_repo();
    let mut url = format!("https://gitee.com/api/v5/repos/{owner}/{repo}/releases/latest");
    if let Ok(tok) = std::env::var(GITEE_TOKEN_ENV) {
        let tok = tok.trim();
        if !tok.is_empty() {
            url.push_str("?access_token=");
            url.push_str(tok);
        }
    }
    let resp = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .call()
        .map_err(|e| format!("gitee 请求失败: {e}"))?;
    resp.into_json::<Release>()
        .map_err(|e| format!("gitee 解析失败: {e}"))
}

/// 命令：检查更新。前端在「关于」页点击调用。
#[tauri::command]
pub fn check_update() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let release = fetch_gitee()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    let has_update = version_gt(&latest, &current);
    // 从附件里挑第一个 .msi 直链
    let msi_url = release
        .assets
        .into_iter()
        .map(|a| a.browser_download_url)
        .find(|u| u.to_ascii_lowercase().ends_with(".msi"));
    Ok(UpdateInfo {
        latest_version: latest,
        current_version: current,
        has_update,
        release_url: release.html_url,
        release_notes: release.body,
        msi_url,
    })
}

/// 命令：在系统默认浏览器打开 release 下载页。
/// 前端拿到 `UpdateInfo.releaseUrl` 后调用，避免在应用 webview 内导航到外链。
#[tauri::command]
pub fn open_release_page(url: String) -> Result<(), String> {
    opener::open(&url).map_err(|e| format!("打开下载页失败: {e}"))
}

/// 命令：返回当前应用版本（`CARGO_PKG_VERSION`）。本地瞬时，无网络。
/// 供「关于」页进入即展示版本号，不必等更新检查。
#[tauri::command]
pub fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 校验文件头是否为合法 MSI（OLE2 / CFBF 复合文档魔数 `D0CF11E0A1B11AE1`）。
/// 防止错误响应（HTML 错误页、截断包等）被当成安装包调起。
fn is_valid_msi(path: &std::path::Path) -> bool {
    use std::io::Read;
    const MSI_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    let mut buf = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf).map(|_| buf))
        .map(|b| b == MSI_MAGIC)
        .unwrap_or(false)
}

/// 命令：下载 msi 安装包到临时目录，返回本地绝对路径。阻塞式（在独立线程执行）。
///
/// 仅接受 `.msi`（防止误下载/执行其他类型）；下载完校验非空且与 Content-Length 一致。
#[tauri::command]
pub fn download_update(url: String) -> Result<String, String> {
    // 从 URL 末段取文件名，回退默认名
    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("itools_update.msi")
        .to_string();
    if !filename.to_ascii_lowercase().ends_with(".msi") {
        return Err(format!("非预期的安装包类型：{filename}"));
    }

    let dir = std::env::temp_dir().join("itools_update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let dest = dir.join(&filename);

    let resp = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .call()
        .map_err(|e| format!("下载请求失败: {e}"))?;
    let expected: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&dest).map_err(|e| format!("创建文件失败: {e}"))?;
    let written =
        std::io::copy(&mut reader, &mut file).map_err(|e| format!("写入文件失败: {e}"))?;
    drop(file);

    if written == 0 {
        let _ = std::fs::remove_file(&dest);
        return Err("下载内容为空".into());
    }
    if let Some(exp) = expected {
        if exp != 0 && written != exp {
            let _ = std::fs::remove_file(&dest);
            return Err(format!("下载不完整：{written}/{exp} 字节"));
        }
    }
    // 校验文件头确为合法 MSI（OLE2 魔数）——防错误响应/HTML 错误页被当安装包调起。
    if !is_valid_msi(&dest) {
        let _ = std::fs::remove_file(&dest);
        return Err("下载的文件不是合法的 MSI 安装包".to_string());
    }
    Ok(dest.to_string_lossy().into_owned())
}

/// 命令：启动 msi 安装向导（交互式 `msiexec /i`）并退出当前 app。
///
/// 退出是必须的：msi 升级要替换正在运行的 `iTools.exe`，进程占用会导致文件锁。
/// 安装程序为独立进程，不受本 app 退出影响。安装完由用户手动打开新版本。
#[tauri::command]
pub fn launch_installer_and_quit(path: String, app: tauri::AppHandle) -> Result<(), String> {
    if !std::path::Path::new(&path).is_file() {
        return Err(format!("安装包不存在：{path}"));
    }
    std::process::Command::new("msiexec")
        .arg("/i")
        .arg(&path)
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {e}"))?;
    // 让出对旧 exe 的占用；安装程序已独立启动
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::version_gt;

    #[test]
    fn version_compare() {
        assert!(version_gt("1.2.3", "1.2.2"));
        assert!(version_gt("v1.3.0", "1.2.9"));
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(version_gt("0.1.1", "0.1.0"));
        assert!(!version_gt("1.2.3", "1.2.3"));
        assert!(!version_gt("1.2.2", "1.2.3"));
        // 缺失段按 0 补齐：1.2 == 1.2.0
        assert!(!version_gt("1.2", "1.2.0"));
        assert!(version_gt("1.2.1", "1.2"));
    }
}
