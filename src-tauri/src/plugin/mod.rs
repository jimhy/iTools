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
#[cfg(windows)]
pub mod native_overlay;
pub mod ocr;
pub mod pin;
pub mod record;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

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
    pub name: String,
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

fn load_one(dir: &Path, manifest_path: &Path) -> Result<LoadedPlugin, String> {
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
        let subtitle = format!("{} · 插件", p.manifest.name);
        for f in &p.manifest.features {
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
                                Err(e) => ilog!(
                                    "[iTools] 插件 {} 的 regex 无效 {src:?}: {e}",
                                    p.manifest.name
                                ),
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
                    "[iTools] 插件 {} 的 feature {:?} 无可用触发方式已跳过（cmds 只支持裸字符串关键字、{{\"type\":\"regex\"}}、{{\"type\":\"text\"}}；{{\"type\":\"keyword\"}} 对象形态/files/img 不会被搜到）",
                    p.manifest.name,
                    f.code
                );
                continue;
            }
            let title = if f.explain.trim().is_empty() {
                p.manifest.description.clone()
            } else {
                f.explain.clone()
            };
            out.push(PluginCommand {
                plugin_id: p.manifest.name.clone(),
                code: f.code.clone(),
                title,
                subtitle: subtitle.clone(),
                keywords,
                regexes,
                any_text,
                icon: logo.clone(),
            });
        }
    }
    out
}

/// 读插件 logo（png/jpg）为 base64 data URL；失败或不存在返回 None。
fn read_logo(dir: &Path, icon: &str) -> Option<String> {
    use base64::Engine as _;
    let path = dir.join(icon);
    let bytes = std::fs::read(&path).ok()?;
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        _ => "image/png",
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
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
}

/// 插件运行期注册表（managed state）。plugins 用 RwLock 以支持热重载运行时替换。
pub struct PluginRegistry {
    /// 插件根目录（项目内 `plugins/`），热重载（reload）时重扫它。
    pub root: PathBuf,
    pub plugins: RwLock<Vec<LoadedPlugin>>,
    /// 本次进入信息，供 `plugin_take_enter` 拉取（规避事件时序竞态）。
    pub pending_enter: Mutex<Option<EnterInfo>>,
    /// 当前插件窗展示的插件 id，供 db/文件命令按插件隔离作用域。
    pub current: Mutex<Option<String>>,
}

impl PluginRegistry {
    pub fn new(root: PathBuf, plugins: Vec<LoadedPlugin>) -> Self {
        Self {
            root,
            plugins: RwLock::new(plugins),
            pending_enter: Mutex::new(None),
            current: Mutex::new(None),
        }
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

    /// 判定 query 的触发类型："regex" / "keyword" / "text"。
    pub fn trigger_kind(&self, id: &str, code: &str, query: &str) -> String {
        if let Ok(plugins) = self.plugins.read() {
            if let Some(p) = plugins.iter().find(|p| p.manifest.name == id) {
                if let Some(f) = p.manifest.features.iter().find(|f| f.code == code) {
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
                    let kw_hit = f.cmds.iter().any(|c| {
                        matches!(c, Cmd::Keyword(k) if matcher.fuzzy_match(k, query).is_some())
                    });
                    if kw_hit {
                        return "keyword".to_string();
                    }
                    if has_text && !query.is_empty() {
                        return "text".to_string();
                    }
                }
            }
        }
        "keyword".to_string()
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
                    description: m.description.clone(),
                    version: m.version.clone(),
                    author: m.author.clone(),
                    feature_count: m.features.len(),
                    cmds,
                    logo: read_logo(&p.dir, &m.icon),
                    enabled: !disabled.iter().any(|d| d == &m.name),
                    permissions: m.permissions.clone(),
                    granted: granted_map.get(&m.name).cloned().unwrap_or_default(),
                }
            })
            .collect()
    }
}

/// 「插件管理」页展示的一条插件信息（与前端 `PluginInfo` 对齐）。
#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub name: String,
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
    // 打包：可写目录，首启从随包资源播种（内置示例插件随安装包分发）。
    // 幂等闸门用 marker(.seed_version) 而非「目录是否存在」：仅【完整】播种成功才落 marker，
    // 失败/半拷贝不落 → 下次启动重试补齐；构建版本变化 → 补种新增/更新的内置插件。
    let writable = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("itools")
        .join("plugins");
    let marker = writable.join(".seed_version");
    let cur_ver = env!("CARGO_PKG_VERSION");
    let need_seed = std::fs::read_to_string(&marker)
        .map(|v| v.trim() != cur_ver)
        .unwrap_or(true);
    if need_seed {
        if let Ok(res) = app.path().resource_dir() {
            let seed = res.join("plugins");
            if seed.is_dir() {
                match copy_dir_merge(&seed, &writable) {
                    Ok(()) => {
                        let _ = std::fs::write(&marker, cur_ver);
                    }
                    Err(e) => ilog!("[iTools] 播种失败（下次启动将重试）: {e}"),
                }
            }
        }
    }
    writable
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
    let status = |code: u16| {
        tauri::http::Response::builder()
            .status(code)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap()
    };

    let path = request.uri().path();
    let rel = path.trim_start_matches('/');
    let mut segs = rel.splitn(2, '/');
    let plugin_id = segs.next().unwrap_or("");
    if plugin_id.is_empty() {
        return status(404);
    }
    let sub = match segs.next() {
        Some(s) if !s.is_empty() => s,
        _ => "index.html",
    };

    let base = root.join(plugin_id);
    let target = base.join(sub);
    let (canon_base, canon_target) = match (base.canonicalize(), target.canonicalize()) {
        (Ok(b), Ok(t)) => (b, t),
        _ => return status(404),
    };
    if !canon_target.starts_with(&canon_base) {
        ilog!("[iTools] 插件资源越界访问被拒: {path}");
        return status(403);
    }
    let bytes = match std::fs::read(&canon_target) {
        Ok(b) => b,
        Err(_) => return status(404),
    };
    let mime = mime_for(&canon_target);
    // 插件页统一【严格 CSP】：允许内联脚本/样式，但掐断一切外联(connect/img)与被框入(frame-ancestors)。
    // 联网【不经 CSP 放开】——所有插件共享同一源(http://itplugin.localhost)，同源下 per-document CSP 不是隔离边界，
    // 会被同源 iframe 借道绕过；改由原生 itools.fetch 代理按【当前活动插件的 network 授权】放行（见 plugin_fetch）。
    // img/media 放开 blob:——插件用 URL.createObjectURL 显示原生截图/贴图/录屏结果（blob: 同源、页面自建，安全）。
    const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; media-src 'self' blob:; font-src 'self' data:; connect-src 'self'; form-action 'self'; base-uri 'self'; frame-ancestors 'none'";
    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", mime)
        .header("Content-Security-Policy", CSP)
        .header("Access-Control-Allow-Origin", "*")
        .body(bytes)
        .unwrap()
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
}
