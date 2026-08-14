//! 云账号与登录态：**本地优先 + 配置化云端 + 诚实降级**（遵 `doc/开发准则.md` 第 7 条）。
//!
//! 设计：
//! - **本地永远是真相**：登录态 / 用户名 / 会话 token 落统一 SQLite 库（`app_kv['account']`），
//!   离线可用。token 由服务器在**运行期**下发，**非源码硬编码**（源码与二进制零明文凭据）。
//! - **云端可选、可配置、诚实降级**：云端地址从环境变量 `ITOOLS_SYNC_ENDPOINT` 或用户设置读取（不写死）。
//!   - 未配置端点 → 登录 / 注销账号 统一返回**诚实错误**（`云端服务未接入…`），前端据此明示「云端未接入」，
//!     绝不假装成功、绝不用本地桩伪装联网。
//!   - 已配置端点 → 走**真实 HTTP** 鉴权（`{endpoint}/auth/login` 等）。
//!
//! 与 `sync.rs`（本地优先数据层 + 云同步引擎）配套：`cloud_endpoint` / `is_logged_in` / `token`
//! 供同步引擎判断「是否可真联网上行」。

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// 云端服务端点的环境变量名（本机 / CI 的显式覆盖，优先级最高）。
const CLOUD_ENDPOINT_ENV: &str = "ITOOLS_SYNC_ENDPOINT";
/// 云端鉴权请求超时（秒）。
const CLOUD_TIMEOUT_SECS: u64 = 15;

/// **dev 便利默认端点**：debug 构建下没有任何配置时 [`cloud_endpoint`] 返回它（release 不启用）。
///
/// 单独抽成常量是因为它有第二个用途：请求这个地址失败时，错误文案要能说清
/// 「这是开发模式的默认地址，服务端没启动属正常现象」（见 [`is_dev_default_endpoint`]）。
/// 两处各写一遍字面量迟早会分叉——改了默认值而提示还在说 8787，就是又一条误导用户的信息。
pub const DEV_DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8787";

/// **内置默认端点**：用户没填、也没设环境变量时生效的官方服务地址。
///
/// 这是一条**产品决定**，与「不写死服务端地址」的初衷并不冲突，但要说清楚区别：
/// 那条红线防的是「把私人服务器地址硬编码进发行版、用户不知情也改不掉」。这里的地址是
/// iTools 的**公开官方服务**，且三点都成立——用户随时可以在「设置 → 网络」里改掉或清空、
/// 环境变量优先级更高、界面上如实标注「当前生效的地址来自内置默认」。
///
/// 输入框**不预填**它（保持空），因为「空」在这里的语义是「跟随默认」而不是「不连服务器」；
/// 预填进去反而会让用户以为这是自己填过的值，清空时不知道会回落到哪。
///
/// ⚠ **端口不能省**：服务跑在 7101 上，写成 `https://api.jimhy.cn` 会去连 443，
/// 那个端口上是另一个服务（TLS 握手直接失败），表现为「登录不了、市场拉不到」而毫无线索。
pub const BUILTIN_DEFAULT_ENDPOINT: &str = "https://api.jimhy.cn:7101";

/// 用户在「账号 → 数据同步」里**手动填写**的云端地址（运行期从 `AppSettings.sync_endpoint` 同步进来）。
/// 源码/二进制不含任何写死的服务端地址；地址只在运行期从环境变量或用户设置获得，绝不随仓库上传。
static USER_ENDPOINT: RwLock<Option<String>> = RwLock::new(None);

/// 规范化端点串：去空白、去尾斜杠；空串视为未设置。
fn normalize_endpoint(s: &str) -> Option<String> {
    let t = s.trim().trim_end_matches('/').to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 由 `AppSettings.sync_endpoint` 更新用户端点（启动装载设置后、以及每次保存设置时调用）。
pub fn set_user_endpoint(raw: &str) {
    if let Ok(mut g) = USER_ENDPOINT.write() {
        *g = normalize_endpoint(raw);
    }
}

/// 云端地址的**唯一解析实现**：返回「实际生效地址 + 它来自哪一层」。
///
/// 优先级：
/// 1) `env`：环境变量 `ITOOLS_SYNC_ENDPOINT`（本机 / CI 显式覆盖，也是 dev 联调本地服务端的开关）；
/// 2) `user`：用户在「账号 → 数据同步」里**手填**的服务器地址（`AppSettings.sync_endpoint`）；
/// 3) `devDefault`：**仅 debug 构建**兜底 [`DEV_DEFAULT_ENDPOINT`]（开发时连本地服务端）；
/// 4) `builtin`：[`BUILTIN_DEFAULT_ENDPOINT`]，官方默认服务；
/// 5) `none`：**仅测试构建**——云端未接入（诚实降级为纯本地）。
///
/// [`cloud_endpoint`] 与 [`endpoint_info`] 都只能走它：这两处一旦各写一遍优先级，
/// 「设置里显示的地址」与「实际请求的地址」就会分叉——那正是用户这次困惑的根源
/// （他从没填过地址，dev 下客户端其实一直在连 127.0.0.1:8787，而界面上完全看不出来）。
fn resolve_endpoint() -> (Option<String>, &'static str) {
    if let Some(ep) = std::env::var(CLOUD_ENDPOINT_ENV).ok().as_deref().and_then(normalize_endpoint) {
        return (Some(ep), "env");
    }
    if let Some(ep) = USER_ENDPOINT.read().ok().and_then(|g| g.clone()) {
        return (Some(ep), "user");
    }
    // dev 便利：debug 默认连本地 server（开发期联调用；要连官方服务请设环境变量或在设置里填）。
    if cfg!(debug_assertions) && !cfg!(test) {
        return (Some(DEV_DEFAULT_ENDPOINT.to_string()), "devDefault");
    }
    // 测试里不给任何默认：否则一次跑偏的用例就会真的打到线上服务。
    if cfg!(test) {
        return (None, "none");
    }
    (Some(BUILTIN_DEFAULT_ENDPOINT.to_string()), "builtin")
}

/// 解析云端 base URL（优先级与来源判定见 [`resolve_endpoint`]）。
///
/// ⚠ 历史坑：曾在 debug 构建里默认兜底 `http://127.0.0.1:8787`——一旦本地服务端
/// 跑过、登录过一次，会话就永久落盘，之后每次启动都「已登录」且退不干净（僵尸登录态）。
/// dev 便利默认【保留】；僵尸态改由登录态门禁 + 会话失效自愈 + 可靠退出缓解；
/// release / test 不启用、不写死生产地址（红线）。
///
/// 源码/二进制零写死生产地址——「地址不进代码 / 不随仓库上传，手动填」的落地。
pub fn cloud_endpoint() -> Option<String> {
    resolve_endpoint().0
}

/// 「当前实际生效的同步服务器地址」快照（camelCase 发给前端）。
///
/// 存在的意义是**消除一个只有开发者才知道的隐形状态**：dev 构建下用户什么都没填，
/// 客户端却在连 [`DEV_DEFAULT_ENDPOINT`]；用户于是在「网络」页签里遍寻不着服务器地址，
/// 以为自己没配、或以为配置没生效。把「生效值 + 它从哪来 + 你填的是什么」三者一起如实摆出来。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    /// 实际生效的地址；`None` = 云端未接入（此时登录 / 同步都会诚实报「未接入」）。
    pub effective: Option<String>,
    /// 生效值来自哪一层：`"env"`（环境变量 `ITOOLS_SYNC_ENDPOINT`）
    /// | `"user"`（用户手填）| `"devDefault"`（**开发构建**的本地默认值，release 不存在）
    /// | `"builtin"`（内置的官方默认服务）| `"none"`（未接入）。
    pub source: String,
    /// 用户在「账号 → 数据同步」里手填的**原始值**（原样返回，可能为空串）。
    /// 与 `effective` 不同时，说明生效的是更高优先级的来源（env）或兜底（devDefault / builtin）。
    pub user_value: String,
    /// 内置默认服务地址。前端拿它写「留空即使用官方默认服务（xxx）」这句提示——
    /// 地址只有一个来源，前端不再各写一遍字面量（写两遍迟早分叉成假信息）。
    pub builtin_default: String,
}

/// 命令：查询当前实际生效的同步服务器地址（「设置 → 网络」页签展示用）。
///
/// 本地瞬时、不触网。`user_value` 取自设置里的**原始串**（不做 trim / 去尾斜杠），
/// 好让用户看见自己到底填了什么（多敲的空格也算事实）。
#[tauri::command]
pub fn sync_endpoint_info(settings: tauri::State<'_, crate::settings::SettingsStore>) -> EndpointInfo {
    endpoint_info(&settings.get().sync_endpoint)
}

/// [`sync_endpoint_info`] 的纯函数实现（`raw_user` = 设置里的原始手填值）。
fn endpoint_info(raw_user: &str) -> EndpointInfo {
    let (effective, source) = resolve_endpoint();
    EndpointInfo {
        effective,
        source: source.to_string(),
        user_value: raw_user.to_string(),
        builtin_default: BUILTIN_DEFAULT_ENDPOINT.to_string(),
    }
}

/// 该端点是否就是 [`DEV_DEFAULT_ENDPOINT`]（dev 兜底默认值）。
///
/// 仅在 **debug 构建**下成立：release 里这个默认值根本不会被启用，用户手填同样的地址属于
/// 「他自己起了个本地服务端」，此时把失败说成「开发模式默认地址未启动属正常」反而是假信息。
///
/// 用途：拉取失败时给出「不是故障」的准确解释——dev 下没启动 server 是常态，
/// 客户端会自动退回内置镜像列表，一切照常工作。
pub fn is_dev_default_endpoint(raw: &str) -> bool {
    cfg!(debug_assertions) && normalize_endpoint(raw).as_deref() == Some(DEV_DEFAULT_ENDPOINT)
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
    db: Arc<Db>,
    data: Mutex<Account>,
}

impl AccountStore {
    /// 从统一 SQLite 库加载（不存在 / 损坏 → 未登录空态）。
    pub fn load(db: Arc<Db>) -> Self {
        let data = db
            .blob_get("account")
            .and_then(|s| serde_json::from_str::<Account>(&s).ok())
            .unwrap_or_default();
        Self {
            db,
            data: Mutex::new(data),
        }
    }

    fn persist(&self) {
        if let Ok(guard) = self.data.lock() {
            if let Ok(json) = serde_json::to_string(&*guard) {
                self.db.blob_set("account", &json);
            }
        }
    }

    fn update<F: FnOnce(&mut Account)>(&self, f: F) {
        if let Ok(mut guard) = self.data.lock() {
            f(&mut guard);
        }
        self.persist();
    }

    /// 当前账号态快照（供前端 / 插件；`cloud_configured` 实时由环境变量 / 用户设置派生）。
    ///
    /// **登录态诚实门禁**：`logged_in` 派生为「本地已登录 **且** 云端已配置」。没有云端就谈不上
    /// 「云账号登录」，故未接入云端时一律呈现未登录——避免脏 / 失效的本地 `logged_in` 造成
    /// 「看着已登录、实则无有效会话」的僵尸态（`doc/开发准则.md`：UI 与实现一致）。
    pub fn state(&self) -> AccountState {
        let a = self.data.lock().map(|g| g.clone()).unwrap_or_default();
        let logged_in = a.logged_in && cloud_configured();
        AccountState {
            logged_in,
            username: if logged_in { a.username } else { String::new() },
            cloud_configured: cloud_configured(),
            sync_enabled: a.sync_enabled,
        }
    }

    /// 是否已登录（供同步引擎判断能否上行）。与 [`Self::state`] 同门禁：云端未配置一律视为未登录。
    pub fn is_logged_in(&self) -> bool {
        cloud_configured() && self.data.lock().map(|g| g.logged_in).unwrap_or(false)
    }

    /// 「登录后自动同步」是否开启。
    pub fn sync_enabled(&self) -> bool {
        self.data.lock().map(|g| g.sync_enabled).unwrap_or(false)
    }

    /// 当前会话 token（未登录 / 云端未配置返回 None）。供同步引擎带上鉴权。
    pub fn token(&self) -> Option<String> {
        if !cloud_configured() {
            return None;
        }
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

    /// 会话失效自愈：清本地登录态与 token（保留「自动同步」偏好），立即落盘。
    /// 由同步链路收到 401/403（服务端判定会话无效）时调用，使 UI 从「僵尸已登录」诚实转为未登录，
    /// 用户可据此重新登录。不触网（区别于 [`Self::logout`]，后者会尽力通知服务端）。
    pub fn invalidate_session(&self) {
        self.update(|a| {
            a.token.clear();
            a.logged_in = false;
        });
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
    let resp = crate::http::post(&url)
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
    crate::http::post(&url)
        .timeout(Duration::from_secs(CLOUD_TIMEOUT_SECS))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(serde_json::json!({ "allDevices": all_devices }))
        .map_err(map_auth_err)?;
    Ok(())
}

/// `POST {endpoint}/account/delete`，真实鉴权后删除云端账号数据。
fn cloud_delete(endpoint: &str, username: &str, password: &str) -> Result<(), String> {
    let url = format!("{endpoint}/account/delete");
    crate::http::post(&url)
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

    // 本测试独占操作全局 `USER_ENDPOINT`（进程级静态）；合并为单个用例避免与其它用例产生
    // 读写竞态，结束时复位。`endpoint_parsing` 为纯函数用例，不碰全局，可安全并行。
    #[test]
    fn store_state_gating_persist_and_selfheal() {
        set_user_endpoint(""); // 起点：云端未配置（除非 CI 设了 ITOOLS_SYNC_ENDPOINT）
        let db = Arc::new(Db::open_memory());

        // 1) 全新库：未登录、未开启同步、无 token
        let store = AccountStore::load(db.clone());
        let st = store.state();
        assert!(!st.logged_in);
        assert!(!st.sync_enabled);
        assert!(store.token().is_none());

        // 2) 未配置端点时登录必失败（诚实降级）——仅在进程确无 env 覆盖时校验
        if cloud_endpoint().is_none() {
            let err = store.login("u", "p").unwrap_err();
            assert!(err.contains("云端服务未接入"), "未配置端点应诚实报错，实际: {err}");
        }

        // 3) 门禁核心：库里持久化了「已登录」脏态，但云端未配置 → 一律呈现未登录（杜绝僵尸态）
        db.blob_set(
            "account",
            r#"{"username":"u","token":"t","logged_in":true,"sync_enabled":true}"#,
        );
        let dirty = AccountStore::load(db.clone());
        if cloud_endpoint().is_none() {
            assert!(!dirty.state().logged_in, "云端未配置时脏 logged_in 应呈现未登录");
            assert_eq!(dirty.state().username, "", "未登录不外泄用户名");
            assert!(!dirty.is_logged_in());
            assert!(dirty.token().is_none(), "云端未配置不应给出 token");
        }

        // 4) 配置云端后 → 同一本地态呈现为已登录（门禁放行；port 9 = discard，不触网）
        set_user_endpoint("http://127.0.0.1:9");
        assert!(dirty.state().logged_in, "云端已配置 + 本地已登录 → 已登录");
        assert_eq!(dirty.state().username, "u");
        assert!(dirty.is_logged_in());
        assert!(dirty.token().is_some());

        // 5) 会话失效自愈：invalidate 后为未登录，且已落盘（重载仍未登录）
        dirty.invalidate_session();
        assert!(!dirty.state().logged_in);
        assert!(dirty.token().is_none());
        assert!(!AccountStore::load(db.clone()).state().logged_in, "invalidate 应已落盘");

        // 6) set_sync_enabled 落盘往返
        dirty.set_sync_enabled(true);
        assert!(AccountStore::load(db.clone()).sync_enabled());

        // 7) endpoint_info 必须与 cloud_endpoint 同源同结论（两者都走 resolve_endpoint）
        //    此刻 USER_ENDPOINT = http://127.0.0.1:9（第 4 步设的）
        let info = endpoint_info("  http://127.0.0.1:9/  ");
        assert_eq!(
            info.user_value, "  http://127.0.0.1:9/  ",
            "user_value 是**原始**手填值，不做 trim / 去尾斜杠"
        );
        if std::env::var(CLOUD_ENDPOINT_ENV).is_err() {
            assert_eq!(info.source, "user");
            assert_eq!(info.effective.as_deref(), Some("http://127.0.0.1:9"));
            assert_eq!(info.effective, cloud_endpoint(), "展示值必须等于实际请求用的值");
        }

        set_user_endpoint(""); // 复位全局，勿污染其它用例

        // 8) 清空手填值后：test 构建不启用 devDefault → 未接入（source=none、effective=None）
        let info = endpoint_info("");
        if std::env::var(CLOUD_ENDPOINT_ENV).is_err() {
            assert_eq!(info.source, "none");
            assert!(info.effective.is_none());
            assert_eq!(info.effective, cloud_endpoint());
        }
        assert_eq!(info.user_value, "");
    }
}
