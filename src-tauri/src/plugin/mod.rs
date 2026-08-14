//! 插件系统（页面插件：关键词触发 → 打开 HTML 面板，由 AI 外部生成放进项目 `plugins/` 目录）。
//!
//! 组成：
//! - 清单解析与校验（[`PluginManifest`]）：宽松、缺字段用默认补齐，解析失败只告警跳过。
//! - 扫描与展开（[`scan_plugins`] / [`expand_commands`]）：目录 → 可搜索的 [`PluginCommand`]。
//! - 自定义协议服务（[`serve`]）：`itplugin://` 把插件目录下的 HTML/资源喂给插件窗口。
//! - 运行期状态（[`PluginRegistry`]）：插件清单 + 「本次进入信息」+「当前插件」。
//!
//! 命令与窗口在 `plugin::commands`；注入 `window.itools` 的桥接脚本在 `plugin::commands::BRIDGE_JS`。

pub mod audio;
pub mod capture;
pub mod commands;
pub mod hotkey;
pub mod install;
pub mod market;
pub mod mirror;
#[cfg(windows)]
pub mod native_overlay;
pub mod ocr;
pub mod pin;
pub mod record;
pub mod settings;
pub mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};

use crate::logging::ilog;
use crate::search::SearchItem;

/// 正则命中给一个较高的基础分（高于多数模糊匹配），确保精确规则优先。
const REGEX_SCORE: i64 = 200;
/// text 类型（任意输入命中）给一个很低的分，排在关键字/应用之后、不喧宾夺主。
const TEXT_SCORE: i64 = 5;

// ==================== 清单 ====================

/// `plugin.json` 结构。必填仅 name/version/description/features，其余靠默认补齐。
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    /// 插件唯一 id（小写字母数字连字符），同时是目录名与协议路径段。
    ///
    /// ⚠ 这是**机器标识**，不是给用户看的名字：它要当目录名、进 `itplugin://` 的路径段、
    /// 做市场索引的主键，所以只能是 ASCII。用户可见的名字请用 [`Self::display_name`]。
    pub name: String,
    /// 给用户看的名字（如「云端笔记」）。缺省时回落到 [`Self::name`]。
    ///
    /// 分成两个字段是因为它们的约束天然冲突：id 要稳定且 ASCII（改了等于换插件），
    /// 展示名要好懂、可以是中文、可以随时改而不影响已安装用户的数据归属。
    #[serde(default, alias = "displayName")]
    pub display_name: String,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// 作者（首期仅解析保留，后续插件详情页用）
    #[serde(default)]
    #[allow(dead_code)]
    pub author: String,
    /// 图标文件名（相对插件目录），缺省 `logo.png`。
    #[serde(default = "default_logo")]
    pub icon: String,
    /// 声明所需的高危能力（用户在「插件管理」按插件授权后才可用）：如 ["runCommand","network"]
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub features: Vec<Feature>,
    /// 「我的数据」页按什么口径数这个插件的数据（见 [`DataSet`]）。
    ///
    /// 不声明就只能按**存储键**计数——那是给开发者看的数字，对用户毫无意义：
    /// 用户建了 2 篇笔记、2 条待办、1 条密码，存储层却是 8 个键（整份待办清单只占 1 个键，
    /// 而每篇笔记的正文各占 1 个），界面显示「8 条」纯属答非所问。
    #[serde(default, alias = "dataSets")]
    pub data_sets: Vec<DataSet>,
}

/// 一个**用户视角**的数据集：把某个存储键翻译成「N 条 XX」。
///
/// 宿主不可能自己猜出插件的业务语义（`todos` 那个数组里是 2 条待办还是 2 个分组？
/// 换个插件同样的结构可能完全是别的意思），所以只能由插件声明。
#[derive(Debug, Clone, Deserialize)]
pub struct DataSet {
    /// 存储键（`itools.data.*` 的 key）。支持结尾 `*` 前缀匹配，
    /// 用于「每条记录各占一个键」的形态（如 `notes.body.*`）。
    pub key: String,
    /// 给用户看的名字，如「笔记」「待办」。
    pub label: String,
    /// 计数方式：
    /// - `length`（默认）：值是数组 → 取长度；值是对象 → 取键数；其它 → 1。
    /// - `one`：整个键算 1 条（用于 `notes.body.*` 这种一记录一键的形态）。
    #[serde(default = "default_count_by", alias = "countBy")]
    pub count_by: String,
}

fn default_count_by() -> String {
    "length".to_string()
}

impl PluginManifest {
    /// 用户可见的名字：声明了就用声明的，否则回落到 id。
    ///
    /// 所有对外展示（插件管理 / 搜索结果 / 我的数据 / 详情页）都必须走这里，
    /// 免得同一个插件在这处叫「云端笔记」、那处叫「deskbox」。
    pub fn display_label(&self) -> &str {
        let trimmed = self.display_name.trim();
        if trimmed.is_empty() {
            &self.name
        } else {
            trimmed
        }
    }
}

fn default_version() -> String {
    "1.0.0".to_string()
}
fn default_logo() -> String {
    "logo.png".to_string()
}

/// 一个功能命令。`code` 插件内唯一，进入插件时回传给页面。
#[derive(Debug, Clone, Deserialize)]
pub struct Feature {
    pub code: String,
    #[serde(default)]
    pub explain: String,
    #[serde(default)]
    pub cmds: Vec<Cmd>,
}

/// 触发方式：字符串即关键字；对象带 `type` 为其它类型。
/// 首期只实现【关键字】+【regex】触发；text/files/img 解析进清单但不参与匹配（向前兼容占位）。
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Cmd {
    /// 关键字直配，如 `"base64"`。
    Keyword(String),
    /// 带类型的触发规则。
    Typed(TypedCmd),
}

#[derive(Debug, Clone, Deserialize)]
pub struct TypedCmd {
    #[serde(rename = "type")]
    pub kind: String,
    /// regex 源串（不带 `/.../` 包裹）。
    #[serde(rename = "match", default)]
    pub pattern: Option<String>,
    /// files 类型的扩展名白名单（首期仅解析，files 触发后续再接）。
    #[serde(default)]
    #[allow(dead_code)]
    pub ext: Vec<String>,
}

// ==================== 加载 ====================

/// 已加载的一个插件（清单 + 所在目录）。
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
}

/// 一条可搜索的插件命令（由 feature 展开，携带已编译的触发规则与图标）。
#[derive(Clone)]
pub struct PluginCommand {
    pub plugin_id: String,
    pub code: String,
    pub title: String,
    pub subtitle: String,
    pub keywords: Vec<String>,
    pub regexes: Vec<regex::Regex>,
    /// text 类型：任意非空输入都命中（翻译/搜索类插件用），进入时 query 传给插件。
    pub any_text: bool,
    /// 插件 logo 的 base64 data URL；无 logo 则 None（前端用兜底字形）。
    pub icon: Option<String>,
}

impl PluginCommand {
    /// 查询是否命中：任一 regex 精确命中给高分；否则取关键字模糊匹配的最高分；
    /// text 类型对任意非空输入给一个很低的兜底分。均不中返回 None。
    pub fn match_score(&self, matcher: &SkimMatcherV2, query: &str) -> Option<i64> {
        for re in &self.regexes {
            if re.is_match(query) {
                return Some(REGEX_SCORE + query.len() as i64);
            }
        }
        let mut best: Option<i64> = None;
        for kw in &self.keywords {
            if let Some(s) = matcher.fuzzy_match(kw, query) {
                best = Some(best.map_or(s, |b| b.max(s)));
            }
        }
        if self.any_text && !query.is_empty() {
            best = Some(best.map_or(TEXT_SCORE, |b| b.max(TEXT_SCORE)));
        }
        best
    }

    pub fn to_item(&self) -> SearchItem {
        SearchItem {
            id: format!("plugin::{}#{}", self.plugin_id, self.code),
            title: self.title.clone(),
            subtitle: self.subtitle.clone(),
            kind: "plugin".to_string(),
            target: format!("{}#{}", self.plugin_id, self.code),
            icon: self.icon.clone(),
            action: "plugin".to_string(),
        }
    }
}

/// 扫描插件根目录，逐个解析 `plugin.json` 并校验；坏插件只告警跳过（AI 生成容错关键）。
pub fn scan_plugins(root: &Path) -> Vec<LoadedPlugin> {
    let mut out = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => {
            // 目录不存在（还没放插件）——正常，不报错
            return out;
        }
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // 跳过 `.` 开头的内部目录（`.staging` 安装暂存、`.git` 等）：
        // 安装过程会在 `.staging/` 里落半成品包，若被扫进来会造成「装到一半就被加载」。
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let manifest_path = dir.join("plugin.json");
        if !manifest_path.exists() {
            continue;
        }
        match load_one(&dir, &manifest_path) {
            Ok(plugin) => {
                let dir_name = dir.file_name().map(|s| s.to_string_lossy().into_owned());
                if Some(&plugin.manifest.name) != dir_name.as_ref() {
                    ilog!(
                        "[iTools] 插件 {} 的 name 与目录名 {:?} 不一致，跳过（name 必须等于目录名）",
                        plugin.manifest.name,
                        dir_name
                    );
                    continue;
                }
                if !seen_names.insert(plugin.manifest.name.clone()) {
                    ilog!("[iTools] 插件重名 {}，跳过后加载者", plugin.manifest.name);
                    continue;
                }
                ilog!(
                    "[iTools] 已加载插件 {} v{}（{} 个功能）",
                    plugin.manifest.name,
                    plugin.manifest.version,
                    plugin.manifest.features.len()
                );
                out.push(plugin);
            }
            Err(e) => ilog!("[iTools] 插件 {:?} 加载失败，跳过：{e}", dir.file_name()),
        }
    }
    out
}

/// 解析并校验单个插件目录（扫描与 Git 安装共用同一套校验，避免两处标准漂移）。
pub(crate) fn load_one(dir: &Path, manifest_path: &Path) -> Result<LoadedPlugin, String> {
    let text = std::fs::read_to_string(manifest_path).map_err(|e| format!("读 plugin.json 失败: {e}"))?;
    let manifest: PluginManifest =
        serde_json::from_str(&text).map_err(|e| format!("plugin.json 解析失败: {e}"))?;
    // 校验
    if manifest.name.trim().is_empty() {
        return Err("name 为空".into());
    }
    if !dir.join("index.html").exists() {
        return Err("缺少 index.html".into());
    }
    if manifest.features.is_empty() {
        return Err("features 为空".into());
    }
    let mut codes = std::collections::HashSet::new();
    for f in &manifest.features {
        if f.code.trim().is_empty() {
            return Err("存在空 feature.code".into());
        }
        if !codes.insert(&f.code) {
            return Err(format!("feature.code 重复: {}", f.code));
        }
    }
    Ok(LoadedPlugin {
        manifest,
        dir: dir.to_path_buf(),
    })
}

/// 把已加载插件展开成可搜索命令（每个 feature 一条，编译 regex、读 logo）。
pub fn expand_commands(plugins: &[LoadedPlugin]) -> Vec<PluginCommand> {
    let mut out = Vec::new();
    for p in plugins {
        let logo = read_logo(&p.dir, &p.manifest.icon);
        // 副标题给用户看，用展示名（未声明时它就等于 id，与原行为一致）
        let subtitle = format!("{} · 插件", p.manifest.display_label());
        out.extend(expand_one(&p.manifest.name, &p.manifest, &subtitle, logo));
    }
    out
}

/// 把**一个**插件清单展开成可搜索命令。
///
/// 单独抽出来是给调试环境复用的：调试插件的 id 取**目录名**（`plugin.json.name` 可能写错），
/// 副标题也要标成「调试插件」——除此之外的匹配规则必须与正式环境**完全一致**，
/// 否则「在调试里能搜到、装上去搜不到」，这套环境就失去了意义。
pub fn expand_one(
    id: &str,
    manifest: &PluginManifest,
    subtitle: &str,
    logo: Option<String>,
) -> Vec<PluginCommand> {
    let mut out = Vec::new();
    for f in &manifest.features {
        let mut keywords = Vec::new();
        let mut regexes = Vec::new();
        let mut any_text = false;
        for cmd in &f.cmds {
            match cmd {
                Cmd::Keyword(kw) => keywords.push(kw.clone()),
                Cmd::Typed(t) if t.kind == "regex" => {
                    if let Some(src) = &t.pattern {
                        match regex::Regex::new(src) {
                            Ok(re) => regexes.push(re),
                            Err(e) => ilog!("[iTools] 插件 {id} 的 regex 无效 {src:?}: {e}"),
                        }
                    }
                }
                // text：任意输入命中（翻译/搜索类）
                Cmd::Typed(t) if t.kind == "text" => any_text = true,
                // files/img 首期不参与匹配
                Cmd::Typed(_) => {}
            }
        }
        if keywords.is_empty() && regexes.is_empty() && !any_text {
            ilog!(
                "[iTools] 插件 {id} 的 feature {:?} 无可用触发方式已跳过（cmds 只支持裸字符串关键字、{{\"type\":\"regex\"}}、{{\"type\":\"text\"}}；{{\"type\":\"keyword\"}} 对象形态/files/img 不会被搜到）",
                f.code
            );
            continue;
        }
        let title = if f.explain.trim().is_empty() {
            manifest.description.clone()
        } else {
            f.explain.clone()
        };
        out.push(PluginCommand {
            plugin_id: id.to_string(),
            code: f.code.clone(),
            title,
            subtitle: subtitle.to_string(),
            keywords,
            regexes,
            any_text,
            icon: logo.clone(),
        });
    }
    out
}

/// 判定 query 对某 feature 的触发类型："regex" / "keyword" / "text"（正式与调试共用同一套规则）。
pub fn trigger_kind_of(manifest: &PluginManifest, code: &str, query: &str) -> String {
    let Some(f) = manifest.features.iter().find(|f| f.code == code) else {
        return "keyword".to_string();
    };
    let mut has_text = false;
    for cmd in &f.cmds {
        match cmd {
            Cmd::Typed(t) if t.kind == "regex" => {
                if let Some(src) = &t.pattern {
                    if regex::Regex::new(src)
                        .map(|re| re.is_match(query))
                        .unwrap_or(false)
                    {
                        return "regex".to_string();
                    }
                }
            }
            Cmd::Typed(t) if t.kind == "text" => has_text = true,
            _ => {}
        }
    }
    let matcher = SkimMatcherV2::default();
    let kw_hit = f
        .cmds
        .iter()
        .any(|c| matches!(c, Cmd::Keyword(k) if matcher.fuzzy_match(k, query).is_some()));
    if kw_hit {
        return "keyword".to_string();
    }
    if has_text && !query.is_empty() {
        return "text".to_string();
    }
    "keyword".to_string()
}

/// 校验 `plugin.json` 里的 `icon` 是否为**安全的插件目录内相对路径**，是则返回规范化的相对路径。
///
/// 为什么必须校验：`icon` 完全由插件作者（Git 安装时即**远端攻击者**）控制，
/// 而它会被 `dir.join(icon)` 直接拿去读文件。Windows 上 `Path::join` 遇到绝对路径会
/// **丢弃 dir 直接用参数**，于是 `"icon": "C:/Users/xxx/.ssh/id_rsa"` 能在**用户还没点安装**的
/// 预览阶段就把沙盒外的任意文件读出来、base64 成 data URL 送进 UI。
///
/// 规则：非空、无 NUL、无冒号（挡盘符与 scheme）、不以 `/` `\` 开头、每段不是纯点（`..`）、
/// 段不以点或空格结尾（Windows 会静默剥掉），且扩展名必须在图片白名单内。
fn safe_icon_rel(icon: &str) -> Option<PathBuf> {
    let icon = icon.trim();
    if icon.is_empty() || icon.contains('\0') || icon.contains(':') {
        return None;
    }
    if icon.starts_with('/') || icon.starts_with('\\') {
        return None;
    }
    let mut out = PathBuf::new();
    let mut segs = 0usize;
    for seg in icon.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg.chars().all(|c| c == '.') || seg.ends_with('.') || seg.ends_with(' ') {
            return None;
        }
        out.push(seg);
        segs += 1;
    }
    if segs == 0 {
        return None;
    }
    let ext = out.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "svg" | "webp") {
        return None;
    }
    Some(out)
}

/// 读插件 logo（png/jpg/svg/webp）为 base64 data URL；失败、越界或过大返回 None。
///
/// 三道闸，缺一不可（`list_infos`、搜索索引展开、Git 安装预览都走这一条路）：
/// 1. [`safe_icon_rel`]：路径形态与扩展名白名单（挡「绝对路径逃出插件目录」）；
/// 2. `canonicalize` 后必须仍在插件目录内（挡预先埋好的软链接 / 重解析点）；
/// 3. 限量读取 [`install::MAX_LOGO_BYTES`]：否则 `"icon": "C:/Windows/MEMORY.DMP"` 这类
///    几 GB 文件会被一次性读进内存再 base64（×1.33 再拷一份）直接 OOM，
///    而攻击成本只是让用户粘贴一条 URL 点「获取信息」。
pub(crate) fn read_logo(dir: &Path, icon: &str) -> Option<String> {
    use base64::Engine as _;
    use std::io::Read as _;

    let rel = safe_icon_rel(icon)?;
    let path = dir.join(&rel);
    let (canon_dir, canon_path) = (dir.canonicalize().ok()?, path.canonicalize().ok()?);
    if !canon_path.starts_with(&canon_dir) {
        ilog!("[iTools] 插件 icon 指向目录外的文件，已忽略：{icon}");
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&canon_path)
        .ok()?
        .take(install::MAX_LOGO_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > install::MAX_LOGO_BYTES {
        ilog!(
            "[iTools] 插件 icon 超过 {} MB 上限，已忽略：{icon}",
            install::MAX_LOGO_BYTES / 1024 / 1024
        );
        return None;
    }
    // mime 取自**已校验**的相对路径扩展名（不看 canonicalize 后的真实路径），大小写不敏感
    let mime = match rel
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        _ => "image/png",
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buf)
    ))
}

// ==================== 运行期状态 ====================

/// 进入插件时回传给页面的信息（前端 `itools.onEnter` 拿到）。
#[derive(Debug, Clone, Serialize)]
pub struct EnterInfo {
    pub code: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub query: String,
    /// 当前插件 id（桥接层内部用于 `settings.onChange` 过滤；不透给业务 `onEnter` 回调）。
    #[serde(rename = "pluginId")]
    pub plugin_id: String,
}

/// 一次插件运行**会话**：哪个插件 + 是不是调试会话。
///
/// 为什么不能只存 id：调试插件与正式插件可能同名（正在开发的就是已装的那个），
/// 存储 / 文件 / 授权 / 同步四类命令必须靠 `dev` 这个标志决定落到测试库还是正式库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSession {
    pub id: String,
    /// true = 调试会话（`plugin-dev` 窗口），落测试库与调试沙盒。
    pub dev: bool,
}

impl ActiveSession {
    /// 是否为**同一个会话**：插件 id 与 `dev` 标志都相等才算。
    ///
    /// 一切「归属校验」（录音 / 录屏会话的 owner、热键绑定的归属）都必须走这里，**不能只比 id**：
    /// 「正在开发的就是已装的那个」是最典型的调试场景，此时调试插件与正式插件共用同一个 id，
    /// 只比 id 会让调试窗里的 demo 通过校验、拿走正式窗里 demo 正在录的麦克风 / 屏幕内容
    /// （反向亦然）——那是一条实打实的 dev↔prod 越界。
    pub fn same_as(&self, other: &Self) -> bool {
        self.id == other.id && self.dev == other.dev
    }

    /// 用于「按插件存的宿主侧配置」（如窗口尺寸）的作用域键：调试会话加 `dev:` 前缀，
    /// 免得调试窗口的尺寸把同名正式插件的记忆盖掉。
    pub fn scope_key(&self) -> String {
        if self.dev {
            format!("{}{}", crate::dev::DEV_ID_PREFIX, self.id)
        } else {
            self.id.clone()
        }
    }
}

/// 插件运行期注册表（managed state）。plugins 用 RwLock 以支持热重载运行时替换。
pub struct PluginRegistry {
    /// 插件根目录（项目内 `plugins/`），热重载（reload）时重扫它。
    pub root: PathBuf,
    pub plugins: RwLock<Vec<LoadedPlugin>>,
    /// 插件调试运行时（同一个 Arc 也作为 managed state 供 `dev_*` 命令使用）。
    /// 挂在这里是为了让 `plugin_*` 命令在**不额外多传一个 State** 的前提下按会话分流。
    pub dev: Arc<crate::dev::DevRuntime>,
    /// 各插件窗口待取的进入信息（`plugin_take_enter` 取走即清）。
    /// **按窗口 label 隔离**：正式窗与调试窗可同时开着，共用一个槽会互相取走对方的上下文。
    pending_enter: Mutex<HashMap<String, EnterInfo>>,
    /// 上次进入信息的留存（热更新重载窗口时重放 onEnter），同样按窗口 label 隔离。
    last_enter: Mutex<HashMap<String, EnterInfo>>,
    /// 每个插件窗口当前加载的会话：窗口 label → 会话。**这是「当前是哪个插件」的唯一真相**。
    ///
    /// 为什么不是一个全局槽：正式插件窗（`plugin`）与调试插件窗（`plugin-dev`）可以同时开着，
    /// 全局槽会被后打开的那个覆盖——那时调试窗的存储写入会落进正式库、正式插件也可能反过来
    /// 读到测试库。按窗口存则两边各说各话，隔离才成立。
    sessions: Mutex<HashMap<String, ActiveSession>>,
}

impl PluginRegistry {
    pub fn new(root: PathBuf, plugins: Vec<LoadedPlugin>, dev: Arc<crate::dev::DevRuntime>) -> Self {
        Self {
            root,
            plugins: RwLock::new(plugins),
            dev,
            pending_enter: Mutex::new(HashMap::new()),
            last_enter: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// 登记「某窗口现在跑的是哪个会话」。
    pub fn set_session(&self, label: &str, session: ActiveSession) {
        if let Ok(mut g) = self.sessions.lock() {
            g.insert(label.to_string(), session);
        }
    }

    /// 某窗口当前的会话（**存储 / 文件 / 授权 / 同步类命令必须用这个**）。
    pub fn session_for(&self, label: &str) -> Option<ActiveSession> {
        self.sessions.lock().ok()?.get(label).cloned()
    }

    /// 存入某窗口的「本次进入信息」（同时留存一份供热更新重放）。
    pub fn set_enter(&self, label: &str, info: EnterInfo) {
        if let Ok(mut g) = self.pending_enter.lock() {
            g.insert(label.to_string(), info.clone());
        }
        if let Ok(mut g) = self.last_enter.lock() {
            g.insert(label.to_string(), info);
        }
    }

    /// 取走某窗口的「本次进入信息」。
    pub fn take_enter(&self, label: &str) -> Option<EnterInfo> {
        self.pending_enter.lock().ok()?.remove(label)
    }

    /// 某会话对应的插件目录（正式查正式注册表，调试查调试注册表）。
    pub fn session_dir(&self, session: &ActiveSession) -> Option<PathBuf> {
        if session.dev {
            self.dev.dir_of(&session.id)
        } else {
            self.plugin_dir(&session.id)
        }
    }

    /// 该插件**是否还装着**（按 `plugin.json` 的 name 匹配，与目录名一致）。
    ///
    /// 返回 `None` 表示**读不出注册表**（锁中毒），调用方必须保守处理：
    /// 「最近使用」的残留清理就靠它区分「确实已卸载」与「这次读不到」——
    /// 把后者当成前者会因为一次无关的 panic 把用户的记录整列删光。
    pub fn has_plugin(&self, id: &str) -> Option<bool> {
        Some(self.plugins.read().ok()?.iter().any(|p| p.manifest.name == id))
    }

    /// 某插件的目录（存在则返回其副本）。
    pub fn plugin_dir(&self, id: &str) -> Option<PathBuf> {
        self.plugins
            .read()
            .ok()?
            .iter()
            .find(|p| p.manifest.name == id)
            .map(|p| p.dir.clone())
    }

    /// 某插件**当前已加载的清单**是否声明了该高危能力。
    ///
    /// 这是授权判定的第二个条件（见 [`commands::plugin_granted`]）：授权表按插件**名**存，
    /// 光看「授权过没有」会让同名覆盖安装 / 手工换包的新代码继承旧插件的授权，
    /// 用上新清单里根本没声明的能力。插件未加载（被删/加载失败）时返回 false——
    /// 此时它本就不该拿到任何高危能力。
    pub fn declares_permission(&self, id: &str, perm: &str) -> bool {
        self.plugins.read().is_ok_and(|g| {
            g.iter()
                .any(|p| p.manifest.name == id && p.manifest.permissions.iter().any(|x| x == perm))
        })
    }

    /// 判定 query 的触发类型："regex" / "keyword" / "text"。
    pub fn trigger_kind(&self, id: &str, code: &str, query: &str) -> String {
        self.plugins
            .read()
            .ok()
            .and_then(|plugins| {
                plugins
                    .iter()
                    .find(|p| p.manifest.name == id)
                    .map(|p| trigger_kind_of(&p.manifest, code, query))
            })
            .unwrap_or_else(|| "keyword".to_string())
    }

    /// 由内存中的插件清单展开可搜索命令，过滤掉被禁用的插件（不重扫磁盘）。
    pub fn commands(&self, disabled: &[String]) -> Vec<PluginCommand> {
        let plugins = match self.plugins.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        expand_commands(&plugins)
            .into_iter()
            .filter(|c| !disabled.iter().any(|d| d == &c.plugin_id))
            .collect()
    }

    /// 热重载：重扫插件根目录、替换自身清单，返回过滤禁用后的可搜索命令（供刷新搜索索引）。
    pub fn reload(&self, disabled: &[String]) -> Vec<PluginCommand> {
        let loaded = scan_plugins(&self.root);
        if let Ok(mut g) = self.plugins.write() {
            *g = loaded;
        }
        self.commands(disabled)
    }

    /// 插件热更新自动重载窗口前调用：把该窗口「上次进入信息」重新放回待取队列，
    /// 使重载后的插件页 `onEnter` 无缝拿到原 code/query（否则 pending 已被取走，onEnter 收不到上下文）。
    /// 仅在当前无待取信息时重放，避免顶掉一次真实的新打开。
    pub fn reseed_enter(&self, label: &str) {
        if let (Ok(last), Ok(mut pending)) = (self.last_enter.lock(), self.pending_enter.lock()) {
            if !pending.contains_key(label) {
                if let Some(info) = last.get(label) {
                    pending.insert(label.to_string(), info.clone());
                }
            }
        }
    }

    /// 列出已装插件信息（供「插件管理」页），enabled 依据禁用清单、granted 依据授权表。
    pub fn list_infos(
        &self,
        disabled: &[String],
        granted_map: &std::collections::HashMap<String, Vec<String>>,
    ) -> Vec<PluginInfo> {
        let plugins = match self.plugins.read() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        // Git 安装来源来自插件根下的锁文件（`.installed.json`）：手工放入/内置的插件没有记录 → None，
        // 前端据此判断「能否检查更新 / 查看仓库」，不会把无来源的插件伪装成可更新。
        let lock = install::read_lock(&self.root);
        plugins
            .iter()
            .map(|p| {
                let m = &p.manifest;
                let mut cmds = Vec::new();
                for f in &m.features {
                    for c in &f.cmds {
                        if let Cmd::Keyword(k) = c {
                            cmds.push(k.clone());
                        }
                    }
                }
                PluginInfo {
                    name: m.name.clone(),
                    display_name: m.display_label().to_string(),
                    description: m.description.clone(),
                    version: m.version.clone(),
                    author: m.author.clone(),
                    feature_count: m.features.len(),
                    cmds,
                    logo: read_logo(&p.dir, &m.icon),
                    enabled: !disabled.iter().any(|d| d == &m.name),
                    permissions: m.permissions.clone(),
                    granted: granted_map.get(&m.name).cloned().unwrap_or_default(),
                    has_readme: p.dir.join("README.md").exists(),
                    has_settings: p.dir.join("settings.json").exists(),
                    source: lock.plugins.get(&m.name).map(|e| e.source.clone()),
                    builtin: install::is_builtin(&m.name),
                }
            })
            .collect()
    }
}

/// 「插件管理」页展示的一条插件信息（与前端 `PluginInfo` 对齐）。
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    /// 插件 id（机器标识，ASCII）。UI 展示请用 `display_name`。
    pub name: String,
    /// 给用户看的名字（清单未声明时等于 `name`，所以前端可以无脑用它）。
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub feature_count: usize,
    /// 关键字预览
    pub cmds: Vec<String>,
    /// logo 的 base64 data URL（无则 None）
    pub logo: Option<String>,
    pub enabled: bool,
    /// 声明所需的高危能力（如 runCommand/network）
    pub permissions: Vec<String>,
    /// 已授权的能力
    pub granted: Vec<String>,
    /// 是否有 README.md（详情页「说明」tab 是否可用）
    pub has_readme: bool,
    /// 是否有 settings.json（详情页「设置」tab 是否可用）
    pub has_settings: bool,
    /// Git 安装来源（来自 `<plugins_root>/.installed.json`）；手工放入或内置的插件为 None，
    /// 此时**没有**可用的更新来源，UI 不应展示「检查更新 / 查看仓库」。
    pub source: Option<install::GitSource>,
    /// 是否为随安装包分发的内置插件（不可被 Git 安装覆盖，随应用整体升级）。
    pub builtin: bool,
}

/// 解析插件根目录（可写）：
/// 1) 环境变量 `ITOOLS_PLUGINS_DIR`；
/// 2) dev：从 exe 上溯到含 `src-tauri` 的项目根，用其 `plugins/`（可写、git 管理）；
/// 3) 打包：可写的 `%LOCALAPPDATA%\itools\plugins`，**首启**从随包 `resource_dir/plugins` 播种内置示例。
pub fn resolve_plugins_root(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager;
    if let Ok(p) = std::env::var("ITOOLS_PLUGINS_DIR") {
        return PathBuf::from(p);
    }
    // dev：项目根（有 src-tauri）的 plugins
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            if anc.join("src-tauri").is_dir() {
                let cand = anc.join("plugins");
                if cand.is_dir() {
                    return cand;
                }
            }
        }
    }
    // 打包：可写目录，**每次启动都「缺啥补啥」合并播种**内置插件（随安装包分发）。
    // copy_dir_merge 只补缺失文件、不覆盖用户改动；不再用版本号 marker 作跳过闸门——否则同版本
    // 新增的内置插件（如 deskbox / pixshot）会因 marker 已存在而永远补不上（曾致其加载不了）。
    let writable = packaged_plugins_root();
    if let Ok(res) = app.path().resource_dir() {
        let seed = res.join("plugins");
        if seed.is_dir() {
            // 已从插件市场装过的同名插件**整个跳过播种**：市场版是经过审核的完整包，
            // 「缺啥补啥」会把内置版独有的旧文件补进去，让目录变成两个版本的混合物
            // （内容哈希对不上、旧 JS 还可能被 index.html 引到）。
            let skip = seed_skip_names(&writable);
            if !skip.is_empty() {
                ilog!("[iTools] 这些插件已从市场安装，跳过内置播种：{skip:?}");
            }
            if let Err(e) = copy_dir_merge_except(&seed, &writable, &skip) {
                ilog!("[iTools] 内置插件播种（缺啥补啥）部分失败: {e}");
            }
        }
    }
    writable
}

/// 播种时要跳过的插件名：**锁文件里记着市场来源的那些**。
///
/// 没有这一步，内置插件就没有真正的升级通道——用户从市场装了新版，下次启动内置版的文件
/// 又被补回去。读锁文件失败 / 没有记录时返回空集，退化成原来的全量播种（安全的默认）。
fn seed_skip_names(root: &Path) -> std::collections::HashSet<String> {
    install::read_lock(root)
        .plugins
        .iter()
        .filter(|(_, e)| e.source.is_market())
        .map(|(name, _)| name.clone())
        .collect()
}

/// 「打包分支」的可写插件根：`%LOCALAPPDATA%\itools\plugins`。
///
/// 单独抽出来是因为 [`install::init_builtins`] 要靠它判断「本次运行的插件根是不是**会被
/// 资源目录播种**的那个」——只有那种情况下同名插件才会被内置版本覆盖回来，
/// 才该被判为「内置、不可 Git 安装覆盖」。两处必须用同一份定义，否则会判反（见 init_builtins）。
pub fn packaged_plugins_root() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("itools")
        .join("plugins")
}

/// `root` 是否就是「会被随包资源播种的那个可写插件根」。
///
/// dev 分支（项目 `plugins/`）与 `ITOOLS_PLUGINS_DIR` 覆盖分支都**不播种**，此处返回 false。
/// 路径经 canonicalize 后比较（大小写 / 短名 / 符号链接都归一）；目录还不存在时退化为字面比较。
pub fn is_seeded_root(root: &Path) -> bool {
    let expect = packaged_plugins_root();
    match (expect.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => expect == root,
    }
}

/// 顶层「缺啥补啥」播种，但跳过 `skip` 里的插件目录（已从市场安装的那些，见 [`seed_skip_names`]）。
fn copy_dir_merge_except(
    src: &Path,
    dst: &Path,
    skip: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    if skip.is_empty() {
        return copy_dir_merge(src, dst);
    }
    std::fs::create_dir_all(dst)?;
    let mut had_err = false;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if skip.contains(&name) {
            continue;
        }
        let to = dst.join(entry.file_name());
        let res = if entry.file_type()?.is_dir() {
            copy_dir_merge(&entry.path(), &to)
        } else if to.exists() {
            Ok(())
        } else {
            std::fs::copy(entry.path(), &to).map(|_| ())
        };
        if let Err(e) = res {
            had_err = true;
            ilog!("[iTools] 播种失败 {}: {e}", to.display());
        }
    }
    if had_err {
        Err(std::io::Error::other("部分文件播种失败"))
    } else {
        Ok(())
    }
}

/// 递归「缺啥补啥」复制：已存在文件不覆盖（保留用户改动）；单文件失败不短路，
/// 有任一失败则整体返回 Err（使调用方不落 marker、下次启动重试）。
fn copy_dir_merge(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let mut had_err = false;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            if let Err(e) = copy_dir_merge(&entry.path(), &to) {
                had_err = true;
                ilog!("[iTools] 播种子目录失败 {}: {e}", to.display());
            }
        } else if !to.exists() {
            if let Err(e) = std::fs::copy(entry.path(), &to) {
                had_err = true;
                ilog!("[iTools] 播种文件失败 {}: {e}", to.display());
            }
        }
    }
    if had_err {
        Err(std::io::Error::other("部分文件播种失败"))
    } else {
        Ok(())
    }
}

// ==================== 自定义协议 ====================

/// `itplugin://localhost/<plugin_id>/<path>`（Windows 上表现为 `http://itplugin.localhost/...`）
/// → 读 `<root>/<plugin_id>/<path>`。canonicalize 后校验仍在该插件目录内，拒绝 `..` 穿越。
pub fn serve(root: &Path, request: &tauri::http::Request<Vec<u8>>) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path();
    let rel = path.trim_start_matches('/');
    let mut segs = rel.splitn(2, '/');
    let plugin_id = segs.next().unwrap_or("");
    if plugin_id.is_empty() {
        return serve_status(404);
    }
    let sub = match segs.next() {
        Some(s) if !s.is_empty() => s,
        _ => "index.html",
    };
    serve_file(&root.join(plugin_id), sub)
}

/// 只有状态码的响应（404 / 403）。
pub fn serve_status(code: u16) -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(code)
        .header("Access-Control-Allow-Origin", "*")
        .body(Vec::new())
        .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
}

/// 从 `base` 目录里取出 `sub` 并按插件页的统一策略响应（CSP / mime / no-store / 越界防护）。
///
/// 正式协议（`itplugin`）与调试协议（`itplugindev`）共用同一份实现——两边的安全策略
/// 必须逐字节一致，否则「调试能跑、正式被 CSP 拦」这类问题会在上线时才暴露。
pub fn serve_file(base: &Path, sub: &str) -> tauri::http::Response<Vec<u8>> {
    let target = base.join(sub);
    let (canon_base, canon_target) = match (base.canonicalize(), target.canonicalize()) {
        (Ok(b), Ok(t)) => (b, t),
        _ => return serve_status(404),
    };
    if !canon_target.starts_with(&canon_base) {
        ilog!("[iTools] 插件资源越界访问被拒: {}", target.display());
        return serve_status(403);
    }
    let bytes = match std::fs::read(&canon_target) {
        Ok(b) => b,
        Err(_) => return serve_status(404),
    };
    let mime = mime_for(&canon_target);
    // 插件页统一【严格 CSP】：允许内联脚本/样式，但掐断一切外联(connect/img)与被框入(frame-ancestors)。
    // 联网【不经 CSP 放开】——所有插件共享同一源(http://itplugin.localhost)，同源下 per-document CSP 不是隔离边界，
    // 会被同源 iframe 借道绕过；改由原生 itools.fetch 代理按【当前活动插件的 network 授权】放行（见 plugin_fetch）。
    // img/media 放开 blob:——插件用 URL.createObjectURL 显示原生截图/贴图/录屏结果（blob: 同源、页面自建，安全）。
    // connect-src 放行 http://ipc.localhost：这是 Tauri 的 IPC 端点（invoke 经它调后端命令）。不放行则被 CSP 拦、
    // IPC 退化为 postMessage——每次加载报 CSP 错，且 Vec<u8> 返回值退化成「数字数组」(4× 体积、极慢)。它只是本机
    // Tauri runtime 的受控 IPC 通道（非任意外联），不放宽联网攻击面（联网仍走 itools.fetch 原生代理 + network 授权门禁）。
    const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self' data:; connect-src 'self' http://ipc.localhost; form-action 'self'; base-uri 'self'; frame-ancestors 'none'";
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", mime)
        .header("Content-Security-Policy", CSP)
        .header("Access-Control-Allow-Origin", "*")
        // 插件资源【不缓存】：改了插件 JS/HTML 后，下次打开/重载窗口即读最新盘上文件，
        // 不被 WebView2 的启发式缓存卡住旧版本（配合插件热更新，真正「改完即生效」）。
        .header("Cache-Control", "no-store")
        .body(bytes)
        .unwrap_or_else(|_| serve_status(500))
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R4 回归：只有「打包分支那个会被资源目录播种的可写根」才算内置插件根。
    ///
    /// 曾经的写法是「资源目录里有 plugins/ 就登记内置名单」，而 `tauri.conf.json` 配了
    /// `"resources": { "../plugins": "plugins" }`，dev 下 tauri 同样会把它铺到 `target/debug/plugins`
    /// ——于是开发机上 5 个示例插件全被判成内置、更新与覆盖安装一律被拒。
    /// 注意也不能用「seed 目录 != 插件根」判定：dev 下这两个路径本来就是两份不同的拷贝，
    /// 那样判等于没判。真正的区别是有没有**播种关系**。
    #[test]
    fn market_installed_plugins_are_skipped_when_seeding() {
        // 「装了市场版、重启又被内置版盖回去」是内置插件升级通道的死穴：
        // copy_dir_merge 是「缺啥补啥」，内置版独有的旧文件会被补进市场版目录，
        // 让它变成两个版本的混合物（内容哈希对不上、旧 JS 还可能被 index.html 引到）。
        // 目录名必须**进程内唯一**：用 process::id() 会让同一进程里的两次执行撞进同一个目录、
        // 互相 remove_dir_all 掉对方的文件（这个坑本身就是写这条用例时踩到的）。
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("itools-seedskip-{uniq}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("deskbox")).unwrap();
        std::fs::create_dir_all(root.join("base64")).unwrap();

        // deskbox 来自市场，base64 来自 Git
        install::write_lock_for_test(
            &root,
            &[
                ("deskbox", "itools-market://deskbox"),
                ("base64", "https://github.com/u/itools-base64.git"),
            ],
        );

        let skip = seed_skip_names(&root);
        assert!(skip.contains("deskbox"), "市场装的插件必须跳过播种");
        assert!(!skip.contains("base64"), "Git 装的插件不该被跳过");

        // 播种：deskbox 目录不该被写入任何东西，base64 该照常补齐
        // seed 放在 root **外面**：放里面会被自己遍历到，测的就不是播种逻辑了
        let seed = std::env::temp_dir().join(format!("itools-seedsrc-{uniq}"));
        let _ = std::fs::remove_dir_all(&seed);
        std::fs::create_dir_all(seed.join("deskbox")).unwrap();
        std::fs::create_dir_all(seed.join("base64")).unwrap();
        std::fs::write(seed.join("deskbox").join("old.js"), b"builtin-only").unwrap();
        std::fs::write(seed.join("base64").join("index.html"), b"x").unwrap();
        copy_dir_merge_except(&seed, &root, &skip).unwrap();

        assert!(
            !root.join("deskbox").join("old.js").exists(),
            "内置版独有的文件绝不能被补进市场版目录"
        );
        assert!(root.join("base64").join("index.html").exists(), "非市场来源应照常播种");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&seed);
    }

    #[test]
    fn seeded_root_only_matches_packaged_dir() {
        let packaged = packaged_plugins_root();
        assert!(
            is_seeded_root(&packaged),
            "打包分支的可写插件根必须被认作「会被随包资源播种」"
        );
        // dev 分支用的是项目根的 plugins/（cargo test 的 cwd 是 src-tauri/）
        let dev = std::env::current_dir().unwrap().join("..").join("plugins");
        assert!(
            !is_seeded_root(&dev),
            "dev 的项目 plugins/ 不该被认作内置根（否则示例插件在开发机上全被锁死）"
        );
        // ITOOLS_PLUGINS_DIR 指到别处时同样不播种
        assert!(!is_seeded_root(&std::env::temp_dir().join("itools-custom-plugins")));
    }

    /// 归属校验必须**同时**看 id 与 dev 标志：同名的调试插件与正式插件不是同一个会话。
    ///
    /// 这条不变量被录音（`plugin_stop_audio_record`）、录屏（`plugin_stop_gif_record`）与
    /// 插件热键（注册 / 注销）共用——任何一处退化成「只比 id」，
    /// 调试窗就能接管同名正式插件的麦克风 / 屏幕会话或热键。
    #[test]
    fn same_session_requires_matching_dev_flag() {
        let prod = ActiveSession {
            id: "demo".into(),
            dev: false,
        };
        let dev = ActiveSession {
            id: "demo".into(),
            dev: true,
        };
        let other = ActiveSession {
            id: "other".into(),
            dev: false,
        };
        assert!(prod.same_as(&prod.clone()));
        assert!(dev.same_as(&dev.clone()));
        assert!(!prod.same_as(&dev), "同名的调试会话不是同一个会话");
        assert!(!dev.same_as(&prod), "反向同样不成立");
        assert!(!prod.same_as(&other));
        // 作用域键也必须能把两者分开（窗口尺寸等宿主侧配置按它存）
        assert_ne!(prod.scope_key(), dev.scope_key());
    }

    #[test]
    fn manifest_parse_flexible_cmds() {
        let json = r#"{
            "name": "base64", "version": "1.0.0", "description": "Base64 编解码",
            "features": [
                { "code": "main", "explain": "编解码", "cmds": ["base64", "b64", { "type": "regex", "match": "^[A-Za-z0-9+/=]+$" }] }
            ]
        }"#;
        let m: PluginManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "base64");
        assert_eq!(m.features.len(), 1);
        assert_eq!(m.features[0].cmds.len(), 3);
        // 前两个关键字、第三个 regex
        assert!(matches!(m.features[0].cmds[0], Cmd::Keyword(_)));
        assert!(matches!(m.features[0].cmds[2], Cmd::Typed(_)));
    }

    #[test]
    fn expand_and_match() {
        let m: PluginManifest = serde_json::from_str(
            r#"{"name":"base64","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["base64","编码"]}]}"#,
        )
        .unwrap();
        let plugins = vec![LoadedPlugin {
            manifest: m,
            dir: PathBuf::from("."),
        }];
        let cmds = expand_commands(&plugins);
        assert_eq!(cmds.len(), 1);
        let matcher = SkimMatcherV2::default();
        assert!(cmds[0].match_score(&matcher, "base64").is_some());
        assert!(cmds[0].match_score(&matcher, "编码").is_some());
        assert!(cmds[0].match_score(&matcher, "zzzz").is_none());
        let item = cmds[0].to_item();
        assert_eq!(item.kind, "plugin");
        assert_eq!(item.action, "plugin");
        assert_eq!(item.target, "base64#main");
    }

    #[test]
    fn icon_path_validation() {
        for ok in ["logo.png", "assets/logo.PNG", "./icon.svg", "a/b/c.webp"] {
            assert!(safe_icon_rel(ok).is_some(), "本应放行: {ok}");
        }
        for bad in [
            "",
            "   ",
            // 绝对路径 / 盘符：Windows 上 dir.join(它) 会**丢弃 dir**，直接读到插件目录外
            "C:/Users/xxx/.ssh/id_rsa",
            "C:\\Windows\\MEMORY.DMP",
            "/etc/passwd",
            "\\\\server\\share\\x.png",
            "file:///c:/x.png",
            // 上跳
            "../../secret.png",
            "a/../../b.png",
            // 非图片扩展名（含无扩展名）
            "logo",
            "config.json",
            "payload.exe",
            // 段以点/空格结尾（Windows 会静默剥掉，落点与校验不一致）
            "logo.png.",
            "assets /logo.png",
            "assets./logo.png",
        ] {
            assert!(safe_icon_rel(bad).is_none(), "本应拒绝: {bad:?}");
        }
        assert_eq!(
            safe_icon_rel("assets//logo.png").unwrap(),
            PathBuf::from("assets").join("logo.png")
        );
    }

    #[test]
    fn read_logo_stays_in_plugin_dir_and_is_capped() {
        let base = std::env::temp_dir().join(format!(
            "itools-logo-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let plugin_dir = base.join("demo");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        // 插件目录**外**的「敏感文件」
        std::fs::write(base.join("secret.png"), b"TOP-SECRET").unwrap();
        std::fs::write(plugin_dir.join("logo.png"), b"\x89PNG-ok").unwrap();

        // 正常相对路径可读
        assert!(read_logo(&plugin_dir, "logo.png").is_some());

        // 绝对路径 / 上跳一律读不到（Windows 上 join 绝对路径会丢弃 dir，这是真实逃逸路径）
        let outside = base.join("secret.png");
        assert!(read_logo(&plugin_dir, outside.to_str().unwrap()).is_none());
        assert!(read_logo(&plugin_dir, "../secret.png").is_none());

        // 超过上限的图片不读（否则远端指定一个几 GB 的本地文件即可 OOM）
        let big = vec![b'x'; (install::MAX_LOGO_BYTES + 1) as usize];
        std::fs::write(plugin_dir.join("big.png"), &big).unwrap();
        assert!(read_logo(&plugin_dir, "big.png").is_none());

        let _ = std::fs::remove_dir_all(&base);
    }
}
