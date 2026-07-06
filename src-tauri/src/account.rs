//! 云账号与登录态：**本地优先 + 配置化云端 + 诚实降级**（遵 `doc/开发准则.md` 第 7 条）。
//!
//! 设计：
//! - **本地永远是真相**：登录态 / 用户名 / 会话 token 落 `%LOCALAPPDATA%\itools\account.json`，
//!   离线可用。token 由服务器在**运行期**下发，**非源码硬编码**（源码与二进制零明文凭据）。
//! - **云端可选、可配置、诚实降级**：云端地址只从环境变量 `ITOOLS_SYNC_ENDPOINT` 读取（不写死）。
//!   - 未配置端点 → 登录 / 注销账号 统一返回**诚实错误**（`云端服务未接入…`），前端据此明示「云端未接入」，
//!     绝不假装成功、绝不用本地桩伪装联网。
//!   - 已配置端点 → 走**真实 HTTP** 鉴权（`{endpoint}/auth/login` 等）。
//!
//! 与 `sync.rs`（本地优先数据层 + 云同步引擎）配套：`cloud_endpoint` / `is_logged_in` / `token`
//! 供同步引擎判断「是否可真联网上行」。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 云端服务端点的环境变量名。未设置 = 云端未接入（诚实降级为纯本地）。
const CLOUD_ENDPOINT_ENV: &str = "ITOOLS_SYNC_ENDPOINT";
/// 云端鉴权请求超时（秒）。
const CLOUD_TIMEOUT_SECS: u64 = 15;

/// 解析云端 base URL：从环境变量读取、去空白、去尾斜杠；空串视为未配置。
///
/// 配置化而非写死：本机 / CI / 发版环境各自 `export ITOOLS_SYNC_ENDPOINT=https://...`，
/// 源码不含任何服务端地址。
pub fn cloud_endpoint() -> Option<String> {
    std::env::var(CLOUD_ENDPOINT_ENV).ok().and_then(|s| {
        let t = s.trim().trim_end_matches('/').to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    })
}

/// 云端是否已配置（供 UI / 插件判断是否展示云能力、是否可真同步）。
pub fn cloud_configured() -> bool {
    cloud_endpoint().is_some()
}

/// 本地持久化的账号态。`token` 为服务器下发的会话令牌（运行期获得，非硬编码）。
#[derive(Clone, Default, Serialize, Deserialize)]
struct Account {
    #[serde(default)]
    username: String,
    /// 会话令牌（登录成功后由服务端下发）。未登录为空。
    #[serde(default)]
    token: String,
    #[serde(default)]
    logged_in: bool,
    /// 「登录后自动同步」开关。默认关闭（未登录时同步本就不发生）。
    #[serde(default)]
    sync_enabled: bool,
}

/// 给前端 / 插件的账号态快照（camelCase）。**不含 token**（不外泄凭据）。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    /// 是否已登录云账号。
    pub logged_in: bool,
    /// 已登录的用户名（未登录为空字符串）。
    pub username: String,
    /// 云端服务是否已配置（`ITOOLS_SYNC_ENDPOINT`）。false = 云端未接入（只能本地）。
    pub cloud_configured: bool,
    /// 是否开启「登录后自动同步」。
    pub sync_enabled: bool,
}

/// 线程安全的账号存储；每次变更立即落盘。
pub struct AccountStore {
    path: PathBuf,
    data: Mutex<Account>,
}

impl AccountStore {
    /// 从默认位置加载（不存在 / 损坏 → 未登录空态）。
    pub fn load() -> Self {
        let dir = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("itools");
        Self::load_from(dir.join("account.json"))
    }

    fn load_from(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Account>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            data: Mutex::new(data),
        }
    }

    fn persist(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(guard) = self.data.lock() {
            if let Ok(json) = serde_json::to_string_pretty(&*guard) {
                let _ = std::fs::write(&self.path, json);
            }
        }
    }

    fn update<F: FnOnce(&mut Account)>(&self, f: F) {
        if let Ok(mut guard) = self.data.lock() {
            f(&mut guard);
        }
        self.persist();
    }

    /// 当前账号态快照（供前端 / 插件；`cloud_configured` 实时由环境变量派生）。
    pub fn state(&self) -> AccountState {
        let a = self.data.lock().map(|g| g.clone()).unwrap_or_default();
        AccountState {
            logged_in: a.logged_in,
            username: a.username,
            cloud_configured: cloud_configured(),
            sync_enabled: a.sync_enabled,
        }
    }

    /// 是否已登录（供同步引擎判断能否上行）。
    pub fn is_logged_in(&self) -> bool {
        self.data.lock().map(|g| g.logged_in).unwrap_or(false)
    }

    /// 「登录后自动同步」是否开启。
    pub fn sync_enabled(&self) -> bool {
        self.data.lock().map(|g| g.sync_enabled).unwrap_or(false)
    }

    /// 当前会话 token（未登录返回 None）。供同步引擎带上鉴权。
    pub fn token(&self) -> Option<String> {
        self.data.lock().ok().and_then(|g| {
            if g.logged_in && !g.token.is_empty() {
                Some(g.token.clone())
            } else {
                None
            }
        })
    }

    /// 登录：**云端已配置才可能成功**，否则诚实报错（不假装登录）。
    pub fn login(&self, username: &str, password: &str) -> Result<AccountState, String> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            return Err("请输入用户名和密码".to_string());
        }
        let endpoint = cloud_endpoint().ok_or_else(|| {
            "云端服务未接入（未配置 ITOOLS_SYNC_ENDPOINT），当前仅支持本地使用".to_string()
        })?;
        let token = cloud_login(&endpoint, username, password)?;
        self.update(|a| {
            a.username = username.to_string();
            a.token = token;
            a.logged_in = true;
            a.sync_enabled = true; // 登录即默认开启自动同步（用户可再关）
        });
        Ok(self.state())
    }

    /// 退出登录：本地会话无条件清除；云端登出尽力而为（失败不阻断本地登出）。
    /// `all_devices` 会**真实传给云端**（吊销全部设备会话）——仅在云端已配置且已登录时有服务端效果。
    pub fn logout(&self, all_devices: bool) -> AccountState {
        let endpoint = cloud_endpoint();
        let token = self.token();
        if let (Some(ep), Some(tok)) = (endpoint, token) {
            // 尽力通知服务端；网络失败也要完成本地登出
            let _ = cloud_logout(&ep, &tok, all_devices);
        }
        self.update(|a| {
            a.token.clear();
            a.logged_in = false;
        });
        self.state()
    }

    /// 注销账号：**需云端已配置**，走真实鉴权 + 服务端删除；成功后清空本地账号态。
    /// 未配置端点时诚实报错（不本地伪装删除「服务器数据」）。
    pub fn delete_account(&self, username: &str, password: &str) -> Result<AccountState, String> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            return Err("请输入用户名和密码".to_string());
        }
        let endpoint = cloud_endpoint()
            .ok_or_else(|| "云端服务未接入，无法注销云端账号".to_string())?;
        cloud_delete(&endpoint, username, password)?;
        self.update(|a| *a = Account::default());
        Ok(self.state())
    }

    /// 设置「登录后自动同步」开关。
    pub fn set_sync_enabled(&self, enabled: bool) -> AccountState {
        self.update(|a| a.sync_enabled = enabled);
        self.state()
    }
}

// ==================== 云端 HTTP（真实鉴权；仅在端点已配置时调用） ====================

/// 登录服务端点的响应：至少含会话 token。
#[derive(Deserialize)]
struct LoginResp {
    #[serde(default)]
    token: String,
}

/// `POST {endpoint}/auth/login`，成功返回会话 token。
fn cloud_login(endpoint: &str, username: &str, password: &str) -> Result<String, String> {
    let url = format!("{endpoint}/auth/login");
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(CLOUD_TIMEOUT_SECS))
        .send_json(serde_json::json!({ "username": username, "password": password }))
        .map_err(map_auth_err)?;
    let parsed: LoginResp = resp
        .into_json()
        .map_err(|e| format!("登录响应解析失败: {e}"))?;
    if parsed.token.trim().is_empty() {
        return Err("登录失败：服务器未返回会话令牌".to_string());
    }
    Ok(parsed.token)
}

/// `POST {endpoint}/auth/logout`，通知服务端吊销会话（可选全设备）。
fn cloud_logout(endpoint: &str, token: &str, all_devices: bool) -> Result<(), String> {
    let url = format!("{endpoint}/auth/logout");
    ureq::post(&url)
        .timeout(Duration::from_secs(CLOUD_TIMEOUT_SECS))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "allDevices": all_devices }))
        .map_err(map_auth_err)?;
    Ok(())
}

/// `POST {endpoint}/account/delete`，真实鉴权后删除云端账号数据。
fn cloud_delete(endpoint: &str, username: &str, password: &str) -> Result<(), String> {
    let url = format!("{endpoint}/account/delete");
    ureq::post(&url)
        .timeout(Duration::from_secs(CLOUD_TIMEOUT_SECS))
        .send_json(serde_json::json!({ "username": username, "password": password }))
        .map_err(map_auth_err)?;
    Ok(())
}

/// 把 ureq 错误翻译成用户可读信息；4xx 鉴权失败给出明确提示。
fn map_auth_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => "用户名或密码错误".to_string(),
        ureq::Error::Status(404, _) => "账号不存在".to_string(),
        ureq::Error::Status(code, _) => format!("云端返回错误（HTTP {code}）"),
        ureq::Error::Transport(t) => format!("无法连接云端服务: {t}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_parsing() {
        // 该测试不依赖真实环境变量；仅验证空/去斜杠逻辑的纯函数部分。
        // 直接构造以避免污染进程环境。
        let norm = |s: &str| -> Option<String> {
            let t = s.trim().trim_end_matches('/').to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        };
        assert_eq!(norm("  https://a.com/  "), Some("https://a.com".to_string()));
        assert_eq!(norm(""), None);
        assert_eq!(norm("   "), None);
    }

    #[test]
    fn store_login_requires_cloud() {
        let path = std::env::temp_dir().join("itools-test-account.json");
        let _ = std::fs::remove_file(&path);
        let store = AccountStore::load_from(path.clone());
        // 默认未登录、未开启同步
        let st = store.state();
        assert!(!st.logged_in);
        assert!(!st.sync_enabled);
        assert!(store.token().is_none());
        // 未配置端点时登录必失败（诚实降级）——测试进程默认无 ITOOLS_SYNC_ENDPOINT
        if super::cloud_endpoint().is_none() {
            let err = store.login("u", "p").unwrap_err();
            assert!(err.contains("云端服务未接入"), "未配置端点应诚实报错，实际: {err}");
        }
        // set_sync_enabled 落盘往返
        store.set_sync_enabled(true);
        let store2 = AccountStore::load_from(path.clone());
        assert!(store2.sync_enabled());
        let _ = std::fs::remove_file(&path);
    }
}
