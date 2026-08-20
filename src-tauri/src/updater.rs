//! 版本更新检查与安装。
//!
//! # 更新源按发行线分成两条（别再合回一条）
//!
//! - **自建服务版**（官网线，`scripts/publish.sh` 构建时注入了 `ITOOLS_DEFAULT_ENDPOINT`）
//!   → 查自己的服务器 `<endpoint>/api/download/latest`，装 `<endpoint>/download/…-setup.exe`。
//! - **开源版**（GitHub Release 线，刻意不注入端点）→ 查 `releases/latest`。
//!
//! 合成一条的后果实测过：自建版用户每点一次「立即更新」，装上的都是 GitHub 的开源包，
//! 编译期注入的服务端地址随之消失，表现为某天起「云端未接入」而用户自己什么都没改。
//! 判据只看**编译期常量**、不看用户填的同步地址——理由见 [`selfhost_endpoint`]。
//!
//! # 两条线的隔离是**四道**，不是一道
//!
//! 只在检查更新时分流是不够的：那只保证「正常点更新」不串线，串线的其它路子照样通着。
//! 四道各自独立、缺一道就有一条交叉路径：
//!
//! 1. **构建期**：产物自带发行线指纹 [`CHANNEL_MARK`]（有没有注入端点决定它是 `selfhost`
//!    还是 `oss`）。两条发布流水线各自 grep 它做**反向校验**：官网线发出去的必须是
//!    `selfhost`，GitHub 线发出去的必须是 `oss`。没有它，CI 上多一个环境变量就能把
//!    带着运维拓扑的包发到公开 release 上，而外观完全正常。
//! 2. **检查更新**：[`check_blocking`] 按 [`selfhost_endpoint`] 分流（本条最早就有）。
//! 3. **下载与跳转**：[`download_update`] / [`open_release_page`] 拿的 URL 由前端传入，
//!    必须再过一遍 [`ensure_channel_url`] ——只允许本发行线的地址。缺这道，任何能调到
//!    这两个命令的地方都可以让官网版去下 GitHub 的包并执行它。
//! 4. **安装后**：[`init_channel_record`] 把本次运行的发行线记在数据目录里，与上次比对。
//!    真被跨线覆盖了（多半是老版本客户端从 GitHub 装的），要让用户**当场看见**
//!    「云服务接入没了、怎么恢复」，而不是某天自己发现「云端未接入」。
//!
//! ⚠ 可达性（部署前必读）：GitHub API 在国内网络下可能超时或被重置。本模块的请求走
//! [`crate::http`]，会遵循用户配置的代理；没配代理时检查更新可能失败——失败是**静默降级**
//! （只返回 Err，前端不打扰用户），不会影响任何其它功能。原实现用的是 Gitee（中国区可达性
//! 更好），2026-08-13 按维护者要求统一到 GitHub。若日后国内用户反馈收不到更新提示，
//! 优先考虑给安装包直链套一层镜像（[`crate::plugin::mirror`] 已有现成的镜像竞速能力），
//! 而不是把源改回去——两个源并存会让「哪个才是最新版」变得没人说得清。
//!
//! # 安装包只认 NSIS `-setup.exe`（**别再加回 msi**）
//!
//! 早先这里挑的是 release 附件里的 `.msi`，而官网下载线（`scripts/publish.sh`）发的
//! 一直是 NSIS 的 `iTools_<版本>_x64-setup.exe`——**同一个应用被两种安装器各装了一份**。
//! 两者互不认识对方的卸载记录（NSIS 写 HKCU\\…\\Uninstall\\iTools，MSI 写 HKLM 的
//! ProductCode 键），各自建各自的桌面快捷方式（NSIS 落当前用户桌面、MSI 因 `ALLUSERS=1`
//! 落公共桌面），于是**桌面上出现两个同名 iTools 图标**，两套文件还往同一个目录里灌。
//! 2026-08-18 定位到这个根因后统一到 NSIS：`tauri.conf.json` 不再产出 msi，这里也只认
//! `-setup.exe`。要是哪天又在 release 里看到 msi 附件，那是打包配置被改回去了，先修那边。
//!
//! 设计：
//! - 检查更新：读 `releases/latest`，semver 比对，返回是否有新版 + 下载页 + 安装包直链。
//! - 半自动安装：`download_update` 下载 setup.exe 到临时目录，`launch_installer_and_quit`
//!   直接运行它（NSIS 交互式向导）并退出当前 app，让安装程序替换正在运行的 exe。
//! - 版本号按语义化比较（semver），忽略 tag 前缀 `v`（发版 tag 形如 `v0.1.0`）。
//! - 网络失败静默降级：失败仅返回 Err，由前端决定是否打扰用户。
//!
//! 安全：**访问令牌绝不写进源码/二进制**。GitHub token 在运行期从环境变量
//! `ITOOLS_GITHUB_TOKEN` 读取——设置了就带上（可读私有仓库 / 防限流），未设置则匿名
//! 请求（公开仓库读取 release 无需鉴权）。发版所需的写权限 token 放在发版环境 /
//! CI secret 的同名变量里，不随客户端分发。
//!
//! 令牌走 **`Authorization` 请求头**而不是 URL query（这是换 GitHub 后的一处实质改善：
//! Gitee 只能 `?access_token=`，一旦超时，ureq 的错误 Display 会把含明文令牌的完整 URL
//! 回显到 UI）。错误出口仍保留 [`crate::plugin::install::redact_token`] 兜底——
//! 防的是将来有人又把令牌拼回 URL。
//!
//! 线程模型（**曾经写反过，务必别再写回去**）：不带 `async` 的 `#[tauri::command]` 走
//! `ExecutionContext::Blocking`——tauri-macros 的 `body_blocking` 把函数体直接内联进 IPC
//! handler，而 Windows 上 IPC handler 由 WebView2 controller 所属的**主 UI 线程**调用。
//! 也就是说同步命令是**在主线程上跑**的，不是「Tauri 在独立线程执行」。
//! 本文件里带网络的命令因此一律 `async fn` + `spawn_blocking`：交给异步运行时还不够，
//! 「同步函数体 + `(async)`」只是把阻塞从 UI 线程搬到 tokio worker（worker 数 = CPU 核数，
//! 低核机器上占住一个就让别的异步命令排队），必须再用 `spawn_blocking` 挪进专用阻塞线程池。

use crate::logging::ilog;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::Duration;

// 令牌脱敏与插件安装模块共用一份实现（两处都把 access_token 拼在 URL 上，泄漏面完全相同）
use crate::plugin::install::redact_token;

// ==================== 发行线（编译期确定） ====================

/// 本二进制属于哪条发行线：`selfhost` = 官网（自建服务）版，`oss` = GitHub 开源版。
///
/// 判据与 [`selfhost_endpoint`] 完全一致（编译期有没有注入 `ITOOLS_DEFAULT_ENDPOINT`），
/// 只是把它显式命名出来——「发行线」这件事要能被日志、界面和发布脚本各自引用，
/// 靠每处再判一次 `option_env!` 迟早分叉。
pub const CHANNEL: &str = match option_env!("ITOOLS_DEFAULT_ENDPOINT") {
    Some(_) => "selfhost",
    None => "oss",
};

/// 产物里的**发行线指纹**，供两条发布流水线 `grep` 裸 exe 做反向校验。
///
/// 为什么要一个专门的字符串而不是搜端点地址：GitHub 的流水线**不知道**端点是什么
/// （那是 secret，刻意不给它），没法搜一个自己不知道的值来证明「我没带上它」。
/// 搜这个指纹就能给出绝对判据：开源包里必须是 `oss`，官网包里必须是 `selfhost`。
///
/// ⚠ 它必须真的出现在二进制里才有意义——[`log_channel`] 每次启动都会把它打进日志，
/// 这就是它不会被优化掉的原因。**别把那行日志删了**。
pub const CHANNEL_MARK: &str = match option_env!("ITOOLS_DEFAULT_ENDPOINT") {
    Some(_) => "ITOOLS_RELEASE_CHANNEL=selfhost",
    None => "ITOOLS_RELEASE_CHANNEL=oss",
};

/// 发行线的中文名（给用户看）。
pub fn channel_label() -> &'static str {
    if CHANNEL == "selfhost" { "官网版" } else { "开源版" }
}

/// 本发行线的更新来源，一句人话（界面直接展示，让用户随时知道自己在哪条线上）。
pub fn update_source_label() -> String {
    match selfhost_endpoint() {
        Some(ep) => format!("官网（{ep}）"),
        None => {
            let (owner, repo) = update_repo();
            format!("GitHub Release（{owner}/{repo}）")
        }
    }
}

/// 更新源仓库的环境变量覆盖（形如 `owner/repo`）。满足准则「服务端地址可配置、不写死」：
/// 默认指向下方公开发布仓库，可用 `ITOOLS_UPDATE_REPO` 覆盖（换发布主体 / 私有仓库）。
const UPDATE_REPO_ENV: &str = "ITOOLS_UPDATE_REPO";
/// 默认发布仓库 owner/repo（公开仓库，客户端匿名即可读 release）。
const DEFAULT_OWNER: &str = "jimhy";
const DEFAULT_REPO: &str = "iTools";

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

/// 环境变量名：可选的 GitHub 访问令牌。源码与二进制中均**不含**明文 token。
/// 公开仓库读取 release 无需设置；私有仓库或防限流时在运行/构建环境导出此变量。
const GITHUB_TOKEN_ENV: &str = "ITOOLS_GITHUB_TOKEN";

/// 检查更新请求超时（秒）。
const TIMEOUT_SECS: u64 = 8;
/// 下载安装包超时（秒）——安装包较大，给足时间。
const DOWNLOAD_TIMEOUT_SECS: u64 = 600;
/// 请求 User-Agent。
const USER_AGENT: &str = "itools-updater";

/// 安装包附件的文件名后缀。
///
/// NSIS 产物固定形如 `iTools_1.5.2_x64-setup.exe`（权威出处：`scripts/publish.sh`
/// 里的 `SETUP_SRC`）。**刻意匹配 `-setup.exe` 而不是裸 `.exe`**：release 里可能
/// 还挂着别的 exe 附件（调试符号、便携版…），裸后缀会把它们误当安装包下下来运行。
const INSTALLER_SUFFIX: &str = "-setup.exe";

/// 安装包旁边那个「可信哈希」附件的后缀：`<安装包 URL>.sha256`。
///
/// # 它是让更新走镜像的**唯一**前提
///
/// 安装包下下来是要**直接执行**的，而本模块此前的全部信任都压在一句「URL 主机必须是 github.com」
/// （[`ensure_channel_url`]）上——既没有哈希也没有签名，只验了是不是 PE 文件。在这个前提下
/// 让下载走任何第三方反代，等于允许对方给用户塞一个任意安装包并被执行。
///
/// 所以顺序不能颠倒：**先有可信哈希，才谈得上镜像**。而「可信」的含义是这个哈希文件
/// 本身必须从**官方源直连**取（见 [`fetch_trusted_sha256`]）——镜像若能同时替换包和哈希，
/// 校验就成了自欺欺人。哈希文件只有几十字节，即便直连很慢也是一次可以忍受的小请求。
const INSTALLER_HASH_SUFFIX: &str = ".sha256";

/// 哈希附件的体积上限：正常内容是「64 位十六进制 + 可选的两个空格和文件名」，一行而已。
/// 给到 4 KB 是为了容下 `sha256sum` 风格的多行清单，同时挡住「拿一个大文件当哈希喂进来」。
const HASH_FILE_MAX_BYTES: u64 = 4 * 1024;

/// 调起安装器时传的参数。**少了 `/UPDATE` 就会变成「先卸载再安装」**。
///
/// # 为什么必须带这几个（2026-08-19 从生成的 installer.nsi 里逐条核对）
///
/// 不带参数时安装器走完整交互向导，其中 `PageReinstall` 在「已装旧版」的场景下会让用户
/// 二选一，而**默认选中的是「安装前卸载」**——于是每次更新都要看一遍卸载流程。
///
/// - `/UPDATE`：模板里写着 `In update mode, always proceeds without uninstalling`，
///   直接**覆盖安装**，跳过那个页面，也跳过 WebView2 的重复检测。
///   注意它不会放过历史 MSI：`PageLeaveReinstall` 里 `$WixMode = 1` 的判断排在
///   `$UpdateMode` 前面，存量 MSI 该卸的照卸（那份必须卸，否则两条卸载记录）。
/// - `/P`（passive）：不渲染选择页，只留一个进度条并在装完自动关闭。
///   顺带补一件事：passive / silent 下模板会主动 `CreateOrUpdateDesktopShortcut`，
///   桌面图标不会在更新后消失。
/// - `/R`：装完由安装器把 iTools 重新拉起来（`.onInstSuccess` 里只在 passive/silent
///   下识别这个标志）。没有它，用户点完「立即更新」就只剩一个装完自动关掉的窗口，
///   得自己再去点一次图标。
///
/// ⚠ 改这几个参数前先重读 `src-tauri/target/release/nsis/x64/installer.nsi`——
/// 它们是 Tauri NSIS 模板的内部约定，不是稳定公开接口（同 `windows/hooks.nsh` 的告诫）。
const INSTALLER_ARGS: &[&str] = &["/P", "/UPDATE", "/R"];

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
    /// 安装包直链（release 附件里第一个 [`INSTALLER_SUFFIX`] 结尾者），无则 None。
    /// 前端据此决定是否提供「立即更新」（自动下载 + 调起安装）。
    pub installer_url: Option<String>,
    /// 这条结果来自哪个更新源（[`update_source_label`] 的原话，如「官网（https://…）」）。
    /// 界面直接展示：用户点「立即更新」前就该知道包是从哪来的，而不是装完才发现换了条线。
    pub source: String,
}

/// 最近一次更新检查的结果快照（供「上次检查了什么」展示）。
///
/// # 为什么必须有它
///
/// 自动检查对失败是**刻意静默**的（联网不通时反复弹提示纯属骚扰），而没有新版时角标也不显示——
/// 于是「查过了、已是最新」和「压根没查成」在界面上长得一模一样，用户只能反复手动点。
/// 把「什么时候查的、查出什么」如实存下来给界面用，这个歧义才消失。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// 上次检查完成的时刻（Unix 毫秒）；0 = 本次启动后还没检查过。
    pub checked_at: i64,
    /// 上次检查成功时的结果；失败或还没查过为 None。
    pub info: Option<UpdateInfo>,
    /// 上次检查失败的真实原因；成功或还没查过为 None。
    pub error: Option<String>,
    /// 当前应用版本（无论有没有查过都给，界面至少能显示自己的版本）。
    pub current_version: String,
    /// 发行线标识（[`CHANNEL`]）：`selfhost` = 官网版，`oss` = 开源版。
    pub channel: String,
    /// 发行线 + 更新源的人话（如「官网版 · 更新来自 官网（https://…）」）。
    pub channel_desc: String,
    /// 检测到「上次运行的是另一条线」时的提示；没换过为 None。
    /// 非空说明这台机器被跨线覆盖过，界面必须显著提示——云服务接入可能已经悄悄失效。
    pub channel_switch_note: Option<String>,
}

/// 最近一次检查的结果（进程内，重启即清空——它只是「本次运行期间查过什么」的备忘）。
static LAST_CHECK: std::sync::Mutex<Option<(i64, Result<UpdateInfo, String>)>> =
    std::sync::Mutex::new(None);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn remember(result: Result<UpdateInfo, String>) {
    if let Ok(mut g) = LAST_CHECK.lock() {
        *g = Some((now_ms(), result));
    }
}

/// 命令：上次更新检查的结果快照。**本地瞬时、不发任何请求**。
///
/// 界面进入时调它即可知道「自动检查跑没跑、结论是什么」，不必再手动点一次触发网络请求。
#[tauri::command]
pub fn update_status() -> UpdateStatus {
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    // 发行线三件套与「上次查了什么」无关，无论哪个分支都一样，先算好
    let channel = CHANNEL.to_string();
    let channel_desc = format!("{} · 更新来自 {}", channel_label(), update_source_label());
    let channel_switch_note = channel_switch_note();
    match LAST_CHECK.lock().ok().and_then(|g| g.clone()) {
        Some((at, Ok(info))) => UpdateStatus {
            checked_at: at,
            info: Some(info),
            error: None,
            current_version,
            channel,
            channel_desc,
            channel_switch_note,
        },
        Some((at, Err(e))) => UpdateStatus {
            checked_at: at,
            info: None,
            error: Some(e),
            current_version,
            channel,
            channel_desc,
            channel_switch_note,
        },
        None => UpdateStatus {
            checked_at: 0,
            info: None,
            error: None,
            current_version,
            channel,
            channel_desc,
            channel_switch_note,
        },
    }
}

/// 自建服务版的更新源：编译期注入过 `ITOOLS_DEFAULT_ENDPOINT` 时启用，否则为 None。
///
/// # 为什么两条发行线必须各更各的
///
/// 官网（`scripts/publish.sh`）发的是**注入了服务端地址**的自建服务版，GitHub Release
/// 发的是**刻意不注入**的开源版。而这个模块原先无条件从 GitHub 取更新——于是自建版用户
/// 每点一次「立即更新」，装上的都是开源包，编译期常量随之变成空：表现为某天起「云端未接入」，
/// 而用户自己什么都没改，排查方向会完全跑偏。2026-08-18 在维护者本机实测到了这个现场
/// （安装目录里的 exe 已不含端点）。
///
/// # 刻意用**编译期常量**，不是用户填的同步地址
///
/// 更新源是「这个包从哪条线发出来的」这一发行渠道属性，不是用户偏好。若跟着
/// `AppSettings.sync_endpoint` 走，任何人把同步地址指到自己的服务器，就能让这台机器
/// 下载并执行他提供的 exe——那是一条不该存在的攻击面。用户换同步服务器是换数据存放处，
/// 不该连带换掉「从哪里拿安装包」。
fn selfhost_endpoint() -> Option<String> {
    crate::account::builtin_default_endpoint().map(|s| s.trim_end_matches('/').to_string())
}

/// 自建服务端 `GET /api/download/latest` 的响应（见 `server/src/routes.rs::latest_download`）。
/// 免认证端点——官网未登录时就在读它。
#[derive(Debug, Deserialize)]
struct SelfhostLatest {
    /// 扫下载目录得出的最新版本号；扫不出时服务端给 null 并附 `reason`。
    #[serde(default)]
    version: Option<String>,
    /// 官网固定链的文件名，正常是 `iTools-latest-setup.exe`。
    #[serde(default)]
    file: Option<String>,
    /// version 为 null 时服务端给出的原因。
    #[serde(default)]
    reason: Option<String>,
}

/// 官网下载按钮固定链接的文件名；服务端给不出可信文件名时回落到它。
const FALLBACK_INSTALLER_FILE: &str = "iTools-latest-setup.exe";

/// 把服务端返回的文件名收敛成一个**可以安全拼进下载 URL** 的纯文件名。
///
/// 这个值最终会决定「下载哪个文件并执行它」，所以按白名单收：必须是不含任何路径
/// 分隔符、不含 `..`、且以 [`INSTALLER_SUFFIX`] 结尾的纯文件名；凡不满足的一律
/// 回落到 [`FALLBACK_INSTALLER_FILE`]（官网下载按钮链的就是它，是安全且正确的默认）。
///
/// 服务端是我们自己的没错，但「自己的服务器返回的字符串」不该直接拿去拼路径——
/// 它可能被改配置、被中间人替换，而这条路径的终点是 `Command::new(path).spawn()`。
fn safe_installer_filename(from_server: Option<&str>) -> String {
    from_server
        .map(str::trim)
        .filter(|f| {
            !f.is_empty()
                && !f.contains('/')
                && !f.contains('\\')
                && !f.contains("..")
                && f.to_ascii_lowercase().ends_with(INSTALLER_SUFFIX)
        })
        .unwrap_or(FALLBACK_INSTALLER_FILE)
        .to_string()
}

/// 从自建服务端取最新版信息。
fn fetch_selfhost(ep: &str) -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let url = format!("{ep}/api/download/latest");
    let resp = crate::http::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .call()
        .map_err(|e| redact_token(format!("更新检查请求失败: {e}")))?;
    let latest: SelfhostLatest = resp
        .into_json()
        .map_err(|e| redact_token(format!("更新检查解析失败: {e}")))?;

    let Some(version) = latest.version.filter(|v| !v.trim().is_empty()) else {
        // 服务端明确说不出版本号就如实报错，绝不静默当成「已是最新」
        return Err(format!(
            "更新源没有可用的安装包：{}",
            latest.reason.as_deref().unwrap_or("服务端未说明原因")
        ));
    };

    let file = safe_installer_filename(latest.file.as_deref());
    let version = version.trim().trim_start_matches('v').to_string();
    Ok(UpdateInfo {
        has_update: version_gt(&version, &current),
        latest_version: version,
        current_version: current,
        // 自建线没有 release 页，指向官网首页——那里就有下载按钮
        release_url: format!("{ep}/"),
        // 服务端这个端点只扫文件名，没有更新说明字段。留空，前端会如实显示
        // 「这个版本没有提供更新说明」，而不是编一段出来。
        release_notes: String::new(),
        installer_url: Some(format!("{ep}/download/{file}")),
        source: update_source_label(),
    })
}

/// 单个 release 的最小化字段（GitHub REST v3；Gitee v5 的同名字段完全一致）。
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

/// release 附件（只取下载直链）。
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

/// 拉取 GitHub 最新 release。若环境变量 `ITOOLS_GITHUB_TOKEN` 存在则附带鉴权，
/// 否则匿名请求（公开仓库可读）。token 只从环境变量读、只进请求头，不落代码也不进 URL。
///
/// `releases/latest` 不会返回 draft / prerelease——预发布版本不会推给普通用户。
fn fetch_release() -> Result<Release, String> {
    let (owner, repo) = update_repo();
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let mut req = crate::http::get(&url)
        .set("User-Agent", USER_AGENT) // GitHub 强制要求 UA，缺了直接 403
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(TIMEOUT_SECS));
    if let Ok(tok) = std::env::var(GITHUB_TOKEN_ENV) {
        let tok = tok.trim();
        if !tok.is_empty() {
            req = req.set("Authorization", &format!("Bearer {tok}"));
        }
    }
    // 令牌在请求头里，正常不会进错误串；redact 仍保留，防的是将来有人把它拼回 URL。
    let resp = req
        .call()
        .map_err(|e| redact_token(format!("GitHub 请求失败: {e}")))?;
    resp.into_json::<Release>()
        .map_err(|e| redact_token(format!("GitHub 解析失败: {e}")))
}

/// 命令：检查更新。前端在「关于」页点击调用。
///
/// `async fn` + `spawn_blocking`：同步命令跑在主 UI 线程上（见文件头「线程模型」）；
/// 而「同步函数体 + `(async)`」只是把 8 秒阻塞从 UI 线程搬到 tokio worker——
/// 1~2 vCPU 的机器上 worker 数就等于核数，占住一个就足以让别的异步命令排队。
/// 与本文件的 [`download_update`] 保持同一种写法，避免后来者照抄错的那个。
#[tauri::command(async)]
pub async fn check_update() -> Result<UpdateInfo, String> {
    tauri::async_runtime::spawn_blocking(check_blocking)
        .await
        .map_err(|e| format!("检查更新任务异常终止: {e}"))?
}

/// [`check_update`] 的阻塞实现（跑在 `spawn_blocking` 线程上）。
///
/// **每次检查都留一行日志**：自动检查是后台行为，前端对失败刻意静默（免得联网不通时反复骚扰），
/// 于是「查过了没有新版」与「压根没查成」在界面上长得一模一样。没有日志时，
/// 连开发者都无法判断自动检查到底跑没跑——本轮就为此绕了一圈。
fn check_blocking() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    // 分线：自建服务版查自己的服务器，开源版查 GitHub Release（见 selfhost_endpoint 的长注释）
    let (source, result) = match selfhost_endpoint() {
        Some(ep) => ("自建服务", fetch_selfhost(&ep)),
        None => ("GitHub", fetch_github(&current)),
    };
    let info = match result {
        Ok(i) => i,
        Err(e) => {
            ilog!("[iTools] 检查更新失败（源：{source}，当前 v{current}）：{e}");
            remember(Err(e.clone()));
            return Err(e);
        }
    };
    if info.has_update {
        ilog!(
            "[iTools] 检查更新：发现新版本 v{}（源：{source}，当前 v{current}）",
            info.latest_version
        );
    } else {
        ilog!(
            "[iTools] 检查更新：已是最新（源：{source}，当前 v{current}，远端 v{}）",
            info.latest_version
        );
    }
    remember(Ok(info.clone()));
    Ok(info)
}

/// 从 GitHub Release 取最新版信息（开源版走这条）。
fn fetch_github(current: &str) -> Result<UpdateInfo, String> {
    let release = fetch_release()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    // 从附件里挑第一个 NSIS 安装包直链
    let installer_url = release
        .assets
        .into_iter()
        .map(|a| a.browser_download_url)
        .find(|u| u.to_ascii_lowercase().ends_with(INSTALLER_SUFFIX));
    Ok(UpdateInfo {
        has_update: version_gt(&latest, current),
        latest_version: latest,
        current_version: current.to_string(),
        release_url: release.html_url,
        release_notes: release.body,
        installer_url,
        source: update_source_label(),
    })
}

// ==================== 跨线门禁 ====================

/// 本发行线允许的 URL 前缀（下载与跳转共用）。
///
/// - 官网线：只认内置端点自己（`https://…:端口/`），它既托管官网页也托管 `/download/`；
/// - 开源线：只认本发布仓库的 release 地址。
///
/// 刻意**不**把两条都列进去：这个函数的全部意义就是「另一条线的地址在这里必须匹配不上」。
fn allowed_url_prefixes() -> Vec<String> {
    match selfhost_endpoint() {
        Some(ep) => vec![format!("{ep}/")],
        None => {
            let (owner, repo) = update_repo();
            vec![format!("https://github.com/{owner}/{repo}/releases/")]
        }
    }
}

/// 门禁：`url` 必须属于**本发行线**，否则拒绝并说清楚为什么。
///
/// # 为什么这道必须有
///
/// [`download_update`] 与 [`open_release_page`] 的 url 都是前端传进来的。正常路径上它来自
/// [`check_blocking`] 挑好的那一条，可这两个命令本身并不知道这件事——只要有任何一处
/// （旧版前端、被改的页面、将来某个复用它的功能）传进另一条线的地址，官网版就会去下
/// GitHub 的开源包**并执行它**，装完端点就没了。这正是要根除的「交叉」。
fn ensure_channel_url(url: &str, what: &str) -> Result<(), String> {
    let u = url.trim();
    // 前缀匹配挡不住 `…/releases/../../别的仓库` 这种写法：前缀是对的，实际指向别处。
    // 合法的更新地址里不会出现 `..`，直接拒掉最省事。
    if !u.contains("..") && allowed_url_prefixes().iter().any(|p| u.starts_with(p.as_str())) {
        return Ok(());
    }
    let allowed = allowed_url_prefixes().join(" 或 ");
    // 日志留一行：真触发了说明有路径在串线，得能查
    ilog!("[iTools] 拒绝跨发行线的{what}：{u}（本机是{}，只接受 {allowed}）", channel_label());
    Err(format!(
        "拒绝{what}：这个地址不属于当前的{}更新源。
         本机允许的来源是 {allowed}。
         官网版与开源版是两条独立的发行线，装错线会让云服务接入失效——所以这里直接拦下。",
        channel_label()
    ))
}

// ==================== 发行线记录（跨线覆盖检测） ====================

/// 记录发行线的文件名（放在用户数据目录，两条线共用同一个数据目录，正好用来比对）。
const CHANNEL_FILE: &str = "release-channel";

/// 「上次运行的发行线与这次不同」时给用户的提示；相同或首次运行为 `None`。
static CHANNEL_SWITCH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// 启动时调用：把本次的发行线记下来，并与上次比对。
///
/// 被跨线覆盖是**静默**的——安装程序不会说什么，界面也一切正常，只是云端悄悄「未接入」了。
/// 所以这里必须留下痕迹：日志一行 + 一句能让用户自己恢复的提示（界面会显示它）。
pub fn init_channel_record() {
    let note = read_and_write_channel();
    if let Some(n) = &note {
        ilog!("[iTools] ⚠ 发行线发生变化：{n}");
    }
    let _ = CHANNEL_SWITCH.set(note);
}

/// 读上次的发行线、写入本次，返回需要提示用户的话。
fn read_and_write_channel() -> Option<String> {
    let root = crate::paths::data_root();
    let path = root.join(CHANNEL_FILE);
    let prev = std::fs::read_to_string(&path).ok().map(|s| s.trim().to_string());
    // 写在读之后：失败不影响功能，只是下次比对不上，不值得打断启动
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::write(&path, CHANNEL);

    match prev.as_deref() {
        // 首次运行 / 记录还没建立：没有可比对的历史，不编故事
        None | Some("") => None,
        Some(p) if p == CHANNEL => None,
        Some("selfhost") => Some(
            "这台机器上次运行的是**官网版**，现在这份是**开源版**。             开源版不含云服务地址，账号登录、数据同步与插件市场都会显示「云端未接入」。             要恢复请到官网重新下载安装（开源版的自动更新只会一直给你开源版）。"
                .to_string(),
        ),
        Some("oss") => Some(
            "这台机器上次运行的是开源版，现在这份是官网版，云服务已接入。".to_string(),
        ),
        Some(other) => Some(format!("上次运行的发行线记录是「{other}」，本次是「{CHANNEL}」。")),
    }
}

/// 供界面读取的「跨线覆盖」提示（[`init_channel_record`] 还没跑时为 `None`）。
pub fn channel_switch_note() -> Option<String> {
    CHANNEL_SWITCH.get().cloned().flatten()
}

/// 启动日志：把发行线与更新源如实写下来。
///
/// ⚠ 这行同时是 [`CHANNEL_MARK`] 进入二进制的唯一保证（发布脚本靠 grep 它做反向校验），
/// **别把它删了或改成不引用常量的写法**。
pub fn log_channel() {
    ilog!(
        "[iTools] 发行线：{}（{}），更新源：{}",
        channel_label(),
        CHANNEL_MARK,
        update_source_label()
    );
}

/// 命令：在系统默认浏览器打开 release 下载页。
/// 前端拿到 `UpdateInfo.releaseUrl` 后调用，避免在应用 webview 内导航到外链。
///
/// URL 必须属于本发行线（见 [`ensure_channel_url`]）——这个命令底层是 `opener::open`，
/// 不设门禁等于把「用默认程序打开任意 URL」开放给调用方。
#[tauri::command]
pub fn open_release_page(url: String) -> Result<(), String> {
    ensure_channel_url(&url, "打开下载页")?;
    opener::open(&url).map_err(|e| format!("打开下载页失败: {e}"))
}

/// 命令：返回当前应用版本（`CARGO_PKG_VERSION`）。本地瞬时，无网络。
/// 供「关于」页进入即展示版本号，不必等更新检查。
#[tauri::command]
pub fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// 校验文件确为 Windows 可执行文件（PE）。
/// 防止错误响应（HTML 错误页、截断包、代理返回的门户页等）被当成安装包**运行**。
///
/// 两段都查，不只看 `MZ`：`MZ` 只有两个字节，任何以这俩字符开头的文本都能蒙混过关，
/// 而这个文件接下来是要被直接执行的。所以再顺着 DOS 头 `0x3C` 处的 `e_lfanew`
/// 偏移跳过去，确认那里是 PE 签名 `PE\0\0`——这是「它真是个 exe」的最小充分证据。
fn is_valid_installer(path: &std::path::Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let check = |path: &std::path::Path| -> std::io::Result<bool> {
        let mut f = std::fs::File::open(path)?;
        let mut dos = [0u8; 2];
        f.read_exact(&mut dos)?;
        if &dos != b"MZ" {
            return Ok(false);
        }
        // e_lfanew：PE 头相对文件起始的偏移，小端 u32，固定位于 0x3C
        f.seek(SeekFrom::Start(0x3C))?;
        let mut off = [0u8; 4];
        f.read_exact(&mut off)?;
        f.seek(SeekFrom::Start(u32::from_le_bytes(off) as u64))?;
        let mut sig = [0u8; 4];
        f.read_exact(&mut sig)?;
        Ok(&sig == b"PE\0\0")
    };
    check(path).unwrap_or(false)
}

/// 命令：下载安装包到临时目录，返回本地绝对路径。
///
/// **必须异步**：同步命令的函数体被内联进 IPC handler，在 Windows 上就是主 UI 线程
/// （见文件头「线程模型」）——本命令超时高达 600 秒，同步版等于「点了更新，整个 app
/// 最长十分钟没反应」。这里再进一步用 `spawn_blocking` 把阻塞下载挪到专用阻塞线程池，
/// 避免长时间占住异步运行时的工作线程。
#[tauri::command]
pub async fn download_update(url: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || download_blocking(&url))
        .await
        .map_err(|e| format!("下载任务异常终止: {e}"))?
}

/// [`download_update`] 的阻塞实现（跑在 `spawn_blocking` 线程上）。
///
/// 仅接受 [`INSTALLER_SUFFIX`]（防止误下载/执行其他类型）；下载完校验非空且与
/// Content-Length 一致。这道后缀检查独立于 [`check_blocking`] 里的附件筛选——
/// 本命令的 url 由前端传入，不能假定它一定来自我们自己挑出来的那一条。
fn download_blocking(url: &str) -> Result<String, String> {
    // 发行线门禁排在最前：下错线的包会被紧接着的 launch_installer_and_quit 执行掉，
    // 装完就是另一条线的客户端了（官网版的端点随之消失）。
    //
    // 注意这道门禁校验的**必须**是官方地址：镜像地址（`https://gh-proxy.com/https://github.com/…`）
    // 只在本函数内部按官方地址现场推导，从不接受外部传入——否则「主机必须是 github.com」
    // 这条判据就等于形同虚设。
    ensure_channel_url(url, "下载安装包")?;

    // 后缀门禁只看 URL（剥掉 ?query / #fragment 之后），**不拿远端文件名落盘**。
    //
    // 曾经是 `url.rsplit('/').next()` 直接当文件名 join 到临时目录——那是错的：
    // Windows 上反斜杠同样是路径分隔符，URL 末段里含字面反斜杠或 `..`
    // （形如 `…/a\..\..\Startup\x-setup.exe`）就能把文件写到 itools_update 之外，
    // 而紧接着 [`launch_installer_and_quit`] 会**执行**这个路径——可控落点叠加执行。
    // 远端文件名对本地落盘没有任何信息价值，所以固定写死一个名字，问题从根上消失。
    let path_part = url.split(['?', '#']).next().unwrap_or(url);
    if !path_part.to_ascii_lowercase().ends_with(INSTALLER_SUFFIX) {
        return Err(format!("非预期的安装包类型：{path_part}"));
    }

    let dir = std::env::temp_dir().join("itools_update");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建临时目录失败: {e}"))?;
    let dest = dir.join("itools-setup.exe");

    // 先拿可信哈希——**它决定了这次能不能用镜像**，所以必须排在选源之前。
    let trusted = fetch_trusted_sha256(url);
    if trusted.is_none() {
        ilog!(
            "[iTools] 更新包没有可信哈希（{url}{INSTALLER_HASH_SUFFIX} 取不到），本次只走官方源、不使用镜像"
        );
    }
    let sources = candidate_sources(url, trusted.as_deref());

    let mut errors: Vec<String> = Vec::new();
    for (i, (label, candidate)) in sources.iter().enumerate() {
        ilog!("[iTools] 下载更新包：尝试源 {}/{} = {label}", i + 1, sources.len());
        match download_one(candidate, &dest, trusted.as_deref()) {
            Ok(()) => {
                ilog!("[iTools] 更新包下载完成，来源 {label}");
                return Ok(dest.to_string_lossy().into_owned());
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                // 哈希不符**不换源重试**：说明拿到的东西确实不是官方那一个。换个源再下一遍
                // 既不会让它变对，还会把「某个源正在投毒」掩盖成一句「网络问题」。
                if e.starts_with(HASH_MISMATCH_PREFIX) {
                    ilog!("[iTools] 来源 {label} 的更新包哈希不符，已删除且不再换源");
                    return Err(e);
                }
                ilog!("[iTools] 来源 {label} 下载失败：{e}");
                errors.push(format!("{label}: {e}"));
            }
        }
    }
    Err(format!("全部 {} 个下载源都失败——{}", sources.len(), errors.join("；")))
}

/// 本次下载可以用哪些源。**这是整套镜像加速的安全闸门**，单独拆出来是为了能直接测。
///
/// 规则只有一条：**没有可信哈希就只走官方**。安装包下下来是要被执行的，没有校验手段时
/// 用第三方反代等于把「用户装到什么」的决定权交给对方。有了哈希，镜像最多能让下载失败。
///
/// 老 release 没有 `.sha256` 附件，走的正是 `None` 这一支——行为与加镜像之前逐字节一致，
/// 所以这个改动对已发布的版本不构成任何退化。
fn candidate_sources(url: &str, trusted: Option<&str>) -> Vec<(String, String)> {
    let Some(_) = trusted else {
        return vec![(update_source_label(), url.to_string())];
    };
    let mut sources = crate::plugin::mirror::download_sources(url);
    // `mirror` 模块把官方源标成 `github.com`，那是它服务插件市场时的语境。对**官网线**来说
    // 官方源根本不是 GitHub（`download_sources` 也确实不会给非 GitHub 地址配任何镜像），
    // 标签照抄过来就是一条会把排查带偏的日志。标签只出现在日志里，但错的日志比没有更糟。
    for s in sources.iter_mut() {
        if s.1 == url {
            s.0 = update_source_label();
        }
    }
    sources
}

/// 哈希不符错误的固定前缀：靠它把「内容不对」与「网络失败」区分开，前者绝不换源重试。
const HASH_MISMATCH_PREFIX: &str = "安装包 SHA-256 校验失败";

/// 取这个安装包的**可信**哈希：`<官方安装包 URL>.sha256`，**只从官方源直连取，绝不经镜像**。
///
/// 取不到（老 release 没传这个附件 / 网络失败 / 内容不合规）一律返回 `None`，
/// 由调用方退化成「只走官方源」，而不是让更新整个失败——老版本升级不能因为这个新机制断掉。
///
/// 接受两种内容形态：裸的 64 位十六进制，或 `sha256sum` 风格的 `<hex>  <文件名>`（取首行首段）。
fn fetch_trusted_sha256(installer_url: &str) -> Option<String> {
    let url = format!("{installer_url}{INSTALLER_HASH_SUFFIX}");
    let resp = crate::http::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .call()
        .ok()?;
    let mut text = String::new();
    resp.into_reader()
        .take(HASH_FILE_MAX_BYTES)
        .read_to_string(&mut text)
        .ok()?;
    parse_sha256_file(&text)
}

/// 从哈希附件的内容里取出那个 64 位小写十六进制串。不合规返回 `None`（绝不「尽力猜一个」）。
fn parse_sha256_file(text: &str) -> Option<String> {
    let first = text.lines().find(|l| !l.trim().is_empty())?;
    let token = first.split_whitespace().next()?;
    let hex = token.trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex)
    } else {
        None
    }
}

/// 从**一个**源把安装包下到 `dest` 并逐项校验；任一项不过就返回错误（文件由调用方删）。
///
/// 校验顺序是刻意的：先看体积、再看哈希、最后看 PE。哈希对得上就说明内容与官方发布的
/// 逐字节相同，PE 检查此时只是多一道兜底；而哈希对不上时，早一步拦下比「先确认它是个 exe」有意义得多。
fn download_one(
    url: &str,
    dest: &std::path::Path,
    expect_sha256: Option<&str>,
) -> Result<(), String> {
    // 统一出口取 Agent；600 秒的超时**按请求**设置，代理只决定走哪条链路、不改超时语义
    let resp = crate::http::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .call()
        .map_err(|e| redact_token(format!("下载请求失败: {e}")))?;
    let expected_len: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());

    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut written: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("下载读取失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("写入文件失败: {e}"))?;
        written += n as u64;
    }
    drop(file);

    if written == 0 {
        return Err("下载内容为空".into());
    }
    if let Some(exp) = expected_len {
        if exp != 0 && written != exp {
            return Err(format!("下载不完整：{written}/{exp} 字节"));
        }
    }
    if let Some(expect) = expect_sha256 {
        let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expect) {
            return Err(format!(
                "{HASH_MISMATCH_PREFIX}（期望 {expect}，实际 {actual}），已删除，不会执行"
            ));
        }
    }
    // 校验确为 PE 可执行文件——防错误响应/HTML 错误页被当安装包直接运行。
    if !is_valid_installer(dest) {
        return Err("下载的文件不是合法的安装程序".to_string());
    }
    Ok(())
}

/// 起安装器之前，先请提权的全盘索引守护退出。
///
/// # 为什么非做不可
///
/// 守护是**同一个 `itools.exe`** 带 `--mft-daemon` 起的 High 完整性子进程
/// （[`crate::search::mft`]），Windows 会对运行中的可执行文件加映像锁——它活着，
/// 安装器就覆盖不了安装目录里的 `itools.exe`。而：
///
/// - [`MftSearch::shutdown`] 此前**只**接在「关闭全盘索引」那个按钮上，app 退出路径不调它，
///   `app.exit(0)` 关掉的只是主进程，守护会留下来；
/// - NSIS 没有 MSI 那套 Restart Manager 兜底（msiexec 遇到占用可以 `MoveFileEx`
///   排到重启后替换，NSIS 的 `File` 写不进去就是失败）——切到 NSIS 之后这条退路没了；
/// - NSIS 模板的 `CheckIfAppIsRunning` 按进程名杀，但 `installMode=currentUser` 的安装器
///   是 Medium 完整性，**无权终止 High 完整性进程**，指望不上。
///
/// 所以只能由我们自己在退出前收掉它。收不掉就**如实拒绝启动安装器**，
/// 而不是让用户眼睁睁看着安装失败——那时主程序已经退出，他连个能点的界面都没有。
fn shutdown_mft_daemon_before_install() -> Result<(), String> {
    use crate::search::mft::MftSearch;
    if !MftSearch::is_running() {
        return Ok(());
    }
    ilog!("[iTools] 更新前先关闭全盘索引守护，避免它占着 itools.exe 的映像锁");
    let _ = MftSearch::shutdown();
    // `shutdown()` 只代表 IPC 请求得到了 Ok，进程真正退出还要一点时间；
    // 急着起安装器会正好撞上映像锁。最多等 3 秒，够了。
    for _ in 0..30 {
        if !MftSearch::is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("全盘索引的提权守护进程仍在运行，它占用着安装目录里的程序文件，\
         现在安装会失败。请先在 /f 面板关闭全盘索引，或重启电脑后再更新。"
        .to_string())
}

/// 命令：调起 NSIS 安装器**覆盖安装**（[`INSTALLER_ARGS`]）并退出当前 app。
///
/// 退出是必须的：升级要替换正在运行的 `itools.exe`，进程占用会导致文件锁。
/// 安装程序为独立进程，不受本 app 退出影响；`/R` 会让它在装完后把 iTools 拉起来。
///
/// **不再走完整交互向导**：那条路的 `PageReinstall` 在升级场景下默认选中「安装前卸载」，
/// 于是每次更新都要看一遍卸载流程（用户实际反馈）。参数的逐条依据见 [`INSTALLER_ARGS`]。
/// 也不用 `/S` 全静默——那会让用户在毫无反馈的情况下干等，`/P` 保留了进度条。
///
/// `async fn` + `spawn_blocking`：本命令会等提权守护退出（最长 3 秒，见
/// [`shutdown_mft_daemon_before_install`]），而同步命令的函数体是内联进 IPC handler
/// 的——在 Windows 上那就是主 UI 线程（见文件头「线程模型」）。同步版等于让界面
/// 白卡三秒。与本文件其余带阻塞的命令保持同一种写法。
#[tauri::command]
pub async fn launch_installer_and_quit(path: String, app: tauri::AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || launch_blocking(&path))
        .await
        .map_err(|e| format!("启动安装任务异常终止: {e}"))??;
    // 让出对旧 exe 的占用；安装程序已独立启动。
    // 放在 spawn_blocking 之外：退出应用是主进程层面的事，且只有前面每一步
    // （校验、收守护、起安装器）都成功了才该走到这里——失败时必须让 app 活着，
    // 用户还要在界面上看到原因、点「前往下载页」。
    app.exit(0);
    Ok(())
}

/// [`launch_installer_and_quit`] 的阻塞实现（跑在 `spawn_blocking` 线程上）。
fn launch_blocking(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if !p.is_file() {
        return Err(format!("安装包不存在：{path}"));
    }
    // 再校验一次 PE：下载与调起之间隔着用户点击，路径由前端回传，
    // 这里不该盲信「下载时校验过了」就去执行它。
    if !is_valid_installer(p) {
        return Err(format!("不是合法的安装程序：{path}"));
    }
    shutdown_mft_daemon_before_install()?;
    std::process::Command::new(path)
        .args(INSTALLER_ARGS)
        .spawn()
        .map_err(|e| format!("启动安装程序失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        candidate_sources, ensure_channel_url, is_valid_installer, parse_sha256_file,
        safe_installer_filename, selfhost_endpoint, version_gt, CHANNEL, CHANNEL_MARK,
        FALLBACK_INSTALLER_FILE, INSTALLER_ARGS,
    };

    // ---------- 更新包的可信哈希（走镜像的前提） ----------

    /// 哈希附件的两种正常形态都要能解析出来；任何不合规的内容都必须解析成 `None`。
    ///
    /// `None` 的后果是「退化成只走官方源」——安全方向；所以这里宁可严格：
    /// 长度不对、含非十六进制字符、空文件，一律不认，绝不「尽力猜一个」。
    #[test]
    fn sha256_file_parsing_is_strict() {
        const HEX: &str = "cc4156d51387566ea8ba653fc3a04897bdf812fddf652428d9030bbf7ae24835";

        // 裸哈希；带换行；sha256sum 风格（哈希 + 两个空格 + 文件名）；大写要归一成小写
        assert_eq!(parse_sha256_file(HEX).as_deref(), Some(HEX));
        assert_eq!(parse_sha256_file(&format!("{HEX}\n")).as_deref(), Some(HEX));
        assert_eq!(
            parse_sha256_file(&format!("{HEX}  iTools_1.5.6_x64-setup.exe\n")).as_deref(),
            Some(HEX)
        );
        assert_eq!(parse_sha256_file(&HEX.to_ascii_uppercase()).as_deref(), Some(HEX));
        // 前面有空行也要能找到首个有效行
        assert_eq!(parse_sha256_file(&format!("\n\n{HEX}\n")).as_deref(), Some(HEX));

        for bad in [
            "",
            "   ",
            "not-a-hash",
            "cc4156d5",                   // 太短
            &format!("{HEX}ab"),          // 太长
            &"z".repeat(64),              // 非十六进制
            "<html>404 Not Found</html>", // 错误页被当成哈希文件
        ] {
            assert!(parse_sha256_file(bad).is_none(), "「{bad}」不该被当成合法哈希");
        }
    }

    /// **安全闸门**：拿不到可信哈希时，候选源里绝不能出现任何镜像。
    ///
    /// 这条是整个「更新走镜像」改动的底线。安装包下下来是直接执行的，没有哈希就没有任何
    /// 手段判断它是不是官方那一个——此时用第三方反代，等于让对方决定用户装到什么。
    /// 谁把这个 `None` 分支改成「也去问镜像」，这条测试当场变红。
    #[test]
    fn without_trusted_hash_only_official_source_is_used() {
        let url =
            "https://github.com/jimhy/iTools/releases/download/v1.0.0/iTools_1.0.0_x64-setup.exe";
        let out = candidate_sources(url, None);
        assert_eq!(out.len(), 1, "没有可信哈希时只能有官方一个候选：{out:?}");
        assert_eq!(out[0].1, url, "候选地址必须原样是官方地址，不得被改写");
        for bad in ["gh-proxy", "ghfast", "ghproxy"] {
            assert!(!out[0].1.contains(bad), "没有可信哈希却混进了镜像：{out:?}");
        }
    }

    /// 更新安装必须是**覆盖安装**：少了 `/UPDATE`，NSIS 的 PageReinstall 在升级场景下
    /// 默认选中「安装前卸载」，用户每次更新都要走一遍卸载流程。
    ///
    /// 这三个参数是 Tauri NSIS 模板的内部约定，改动前要重读生成的 installer.nsi。
    #[test]
    fn installer_is_launched_as_an_in_place_update() {
        assert!(INSTALLER_ARGS.contains(&"/UPDATE"), "少了它就会先卸载再安装");
        assert!(INSTALLER_ARGS.contains(&"/P"), "少了它会弹出完整交互向导");
        assert!(INSTALLER_ARGS.contains(&"/R"), "少了它更新完不会自动重启 iTools");
        // /S 是全静默：装的过程没有任何反馈，刻意不用
        assert!(!INSTALLER_ARGS.contains(&"/S"), "不做无反馈的全静默安装");
    }

    /// 发行线指纹必须与 [`CHANNEL`] 一致，且格式就是发布脚本 grep 的那个串。
    ///
    /// `scripts/publish.sh` 与 `.github/workflows/release.yml` 都在裸 exe 里搜它做反向校验，
    /// 格式一改两条流水线同时失效——而它们失效的表现是「照常发布」，不是报错。
    #[test]
    fn channel_mark_is_the_string_publish_scripts_grep() {
        assert_eq!(CHANNEL_MARK, format!("ITOOLS_RELEASE_CHANNEL={CHANNEL}"));
        assert!(CHANNEL == "selfhost" || CHANNEL == "oss", "只有这两条线：{CHANNEL}");
        // 发行线的判据必须与实际的更新源判据同源，否则界面说的和实际走的会分叉
        assert_eq!(CHANNEL == "selfhost", selfhost_endpoint().is_some());
    }

    /// **另一条线的地址必须下不动**——这是「两条发行线不交叉」最后也是最硬的一道。
    ///
    /// 用例对两条线都成立：本次构建是哪条线，就断言另一条线的地址被拒。
    #[test]
    fn cross_channel_urls_are_rejected() {
        const GH: &str =
            "https://github.com/jimhy/iTools/releases/download/v1.0.0/iTools_1.0.0_x64-setup.exe";
        const SELF: &str = "https://example.invalid:7101/download/iTools-latest-setup.exe";

        if CHANNEL == "oss" {
            assert!(ensure_channel_url(GH, "下载").is_ok(), "本线的地址不该被拦");
            assert!(ensure_channel_url(SELF, "下载").is_err(), "官网线的地址必须拒");
        } else {
            // 官网线：GitHub 的包一律拒（装上它端点就没了）
            assert!(ensure_channel_url(GH, "下载").is_err(), "开源线的地址必须拒");
        }

        // 两条线都必须拒的：空、他人域名、前缀伪装、路径穿越、本地文件
        for bad in [
            "",
            "https://evil.example.com/iTools-setup.exe",
            // 前缀伪装：github.com.evil.com 不是 github.com
            "https://github.com.evil.com/jimhy/iTools/releases/x-setup.exe",
            // 前缀对但穿到别处
            "https://github.com/jimhy/iTools/releases/../../evil/x-setup.exe",
            "file:///C:/Windows/Temp/x-setup.exe",
        ] {
            assert!(ensure_channel_url(bad, "下载").is_err(), "「{bad}」应被拒");
        }
    }

    /// 服务端给的文件名最终会被拼进下载 URL、下下来、然后**被执行**。
    /// 所以这里两个方向都钉住：正常名字要原样用，任何带路径意味的都必须退回固定名。
    #[test]
    fn server_supplied_filename_is_confined_to_a_plain_name() {
        // 正常：原样采用
        assert_eq!(
            safe_installer_filename(Some("iTools_1.5.2_x64-setup.exe")),
            "iTools_1.5.2_x64-setup.exe"
        );
        assert_eq!(
            safe_installer_filename(Some("  iTools-latest-setup.exe  ")),
            "iTools-latest-setup.exe",
            "两侧空白要剥掉"
        );

        // 任何带路径意味或后缀不对的，一律退回固定名
        for bad in [
            "../../../Windows/System32/evil-setup.exe", // 相对路径穿越
            "sub/dir/x-setup.exe",                      // 正斜杠
            "sub\\dir\\x-setup.exe",                    // 反斜杠（Windows 同样是分隔符）
            "..\\x-setup.exe",
            "payload.exe",     // 后缀不是 -setup.exe
            "readme.txt",      // 完全不相干
            "",                // 空串
            "   ",             // 全空白
        ] {
            assert_eq!(
                safe_installer_filename(Some(bad)),
                FALLBACK_INSTALLER_FILE,
                "「{bad}」必须被拒绝并退回固定名"
            );
        }

        // 服务端没给这个字段
        assert_eq!(safe_installer_filename(None), FALLBACK_INSTALLER_FILE);
    }

    /// 造一个最小合法 PE 头：`MZ` + 0x3C 处放 PE 头偏移 + 该处放 `PE\0\0`。
    /// 不必带真正的 exe，这个函数看的就只有这三处。
    fn minimal_pe() -> Vec<u8> {
        let mut v = vec![0u8; 0x40];
        v[0] = b'M';
        v[1] = b'Z';
        v[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        v.extend_from_slice(b"PE\0\0");
        v
    }

    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("itools_updater_test_{name}"));
        std::fs::write(&p, bytes).expect("写测试文件");
        p
    }

    /// 这个判定是「下载的文件不是合法的安装程序」这条用户可见错误的唯一来源，
    /// 而它两个方向都会伤人：过松 → 把 HTML 错误页当安装器**执行**；
    /// 过严 → 每个用户都卡在这条错误上更不了新。所以两个方向都钉住。
    #[test]
    fn installer_validation_accepts_real_pe_and_rejects_impostors() {
        let ok = write_tmp("ok.exe", &minimal_pe());
        assert!(is_valid_installer(&ok), "最小合法 PE 必须通过");

        // 只有 MZ、没有 PE 签名：光看前两个字节就放行是不够的
        let mut only_mz = minimal_pe();
        only_mz[0x40..0x44].copy_from_slice(b"XXXX");
        let bad1 = write_tmp("only_mz.exe", &only_mz);
        assert!(!is_valid_installer(&bad1), "缺 PE 签名必须拒绝");

        // 代理门户 / 错误页：最典型的「下到了一坨 HTML」
        let bad2 = write_tmp("page.html", b"<!DOCTYPE html><html>404 Not Found</html>");
        assert!(!is_valid_installer(&bad2), "HTML 错误页必须拒绝");

        // 截断到读不出 e_lfanew：不能 panic，必须老实返回 false
        let bad3 = write_tmp("trunc.exe", b"MZ\x90\x00");
        assert!(!is_valid_installer(&bad3), "截断文件必须拒绝且不 panic");

        // e_lfanew 指到文件外：同样不能 panic
        let mut wild = minimal_pe();
        wild[0x3C..0x40].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        let bad4 = write_tmp("wild_offset.exe", &wild);
        assert!(!is_valid_installer(&bad4), "越界偏移必须拒绝且不 panic");

        for p in [ok, bad1, bad2, bad3, bad4] {
            let _ = std::fs::remove_file(p);
        }
    }

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
