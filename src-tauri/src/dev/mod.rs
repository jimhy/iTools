//! **插件调试环境**（开发者中心）：与正式插件**完全物理隔离**的一套运行环境。
//!
//! 为什么要单独一套：正在开发的插件如果和正式插件共用环境，调试产生的垃圾数据会写进正式库、
//! 甚至被 `sync_now` 真的推到云端；正式插件的硬约束（`name` 必须等于目录名、缺 `index.html`
//! 直接跳过）又会让开发者「改错一个字插件就凭空消失」，无从排查。
//!
//! 隔离面（四条，缺一条这套环境就不成立）：
//! 1. **加载源**：固定的 `dev-plugins/` + 用户手动添加的任意本地目录（见 [`DevRuntime::rescan`]），
//!    与正式插件根互不相干；
//! 2. **存储**：独立的 SQLite 文件（[`DevRuntime::db`]）。因为是**另一个库文件**，正式库的
//!    `pd_namespaces()` 天生扫不到调试数据——「调试数据永不上云」是物理保证而非过滤条件；
//! 3. **文件沙盒**：`<localAppData>/itools/dev/plugin-data/<id>/files/`，与正式沙盒不共目录；
//! 4. **授权 / 同步**：独立的 `dev_plugin_permissions` 授权表 + 本地同步 Mock（绝不发网络请求）。
//!
//! 校验策略与正式环境**相反**：正式环境「有问题就跳过」，调试环境「有问题也加载，但把问题
//! 精确到字段如实报给 UI」（见 [`DevIssue`]）——这正是「清单实时校验」的价值所在。
//!
//! 窗口 / 协议：调试插件跑在 label 为 [`DEV_WINDOW_LABEL`] 的独立窗口里，资源经
//! [`DEV_SCHEME`] 协议提供（按 id→目录映射表解析，因为调试插件分散在多个根）。

pub mod commands;
pub mod logs;
pub mod mock;
pub mod storage;
pub mod submit;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use serde::Serialize;

use crate::db::Db;
use crate::logging::ilog;
use crate::plugin::{read_logo, Cmd, PluginCommand, PluginManifest};

use logs::DevLogs;
use mock::DevMockConfig;

/// 调试插件窗口的 label（**独立于正式插件窗 `plugin`**）：
/// 二者可同时开着，且 `capabilities/plugin-dev.json` 能单独给它放行调试专用命令。
pub const DEV_WINDOW_LABEL: &str = "plugin-dev";

/// 调试插件资源协议名。**不带连字符**——Windows 上自定义协议会变成
/// `http://<scheme>.localhost` 的主机名，连字符在主机名里易出问题。
pub const DEV_SCHEME: &str = "itplugindev";

/// 调试插件进入主搜索时，`SearchItem.target` 的 id 前缀（`dev:<id>#<code>`）。
///
/// 之所以要前缀：主搜索的回车路径统一走 `open_plugin_window(target)`，只有带上这个前缀
/// 后端才能把它路由到调试窗口而不是正式窗口。Windows 目录名不允许含 `:`，所以它
/// 不可能与真实插件 id 撞车。
pub const DEV_ID_PREFIX: &str = "dev:";

/// 已知的高危能力清单（用于清单校验时提示「写了个不存在的 permission」）。
const KNOWN_PERMISSIONS: &[&str] = &[
    "runCommand",
    "network",
    "screen-capture",
    "audio-capture",
    "hotkey",
];

// ==================== 对前端的数据结构（camelCase） ====================

/// 清单校验发现的一个问题。
///
/// `level="error"` 表示**致命**（该插件 `runnable=false`，点「运行」也没意义）；
/// `level="warn"` 表示可运行但有隐患（例如 `name` 与目录名不一致——调试能跑，装到正式环境会被跳过）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DevIssue {
    /// `"error"`（致命）/ `"warn"`（可运行但有问题）。
    pub level: String,
    /// 出问题的**字段路径**，精确到下标，如 `name` / `features[0].code` / `features[1].cmds[2]` /
    /// `index.html`。开发者要能一眼看出是哪一处写错了。
    pub field: String,
    /// 人类可读的说明（含「正式环境会怎样」的后果提示）。
    pub message: String,
}

impl DevIssue {
    fn error(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: "error".to_string(),
            field: field.into(),
            message: message.into(),
        }
    }
    fn warn(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: "warn".to_string(),
            field: field.into(),
            message: message.into(),
        }
    }
}

/// 一个 feature 的触发方式摘要，供「触发模拟器」渲染可点的触发入口。
#[derive(Debug, Clone, Serialize)]
pub struct DevFeature {
    pub code: String,
    pub explain: String,
    /// 关键字触发词（裸字符串形态的 cmds）。
    pub keywords: Vec<String>,
    /// 是否有 `{"type":"regex"}` 触发（模拟器需让开发者自己输入待匹配文本）。
    #[serde(rename = "hasRegex")]
    pub has_regex: bool,
    /// 是否有 `{"type":"text"}` 触发（任意输入命中）。
    #[serde(rename = "hasText")]
    pub has_text: bool,
}

/// 一个调试目录（加载源）。
#[derive(Debug, Clone, Serialize)]
pub struct DevDirInfo {
    pub path: String,
    /// 是否为固定的 `dev-plugins/` 根（不可移除）。
    pub fixed: bool,
    /// 目录当前是否存在（用户添加后被删/改名时如实告知，而不是装作没事）。
    pub exists: bool,
    /// 本次扫描在该目录下**实际加载到**的调试插件数。
    #[serde(rename = "pluginCount")]
    pub plugin_count: usize,
}

/// 一个调试插件的完整信息（清单 + 校验结论）。
#[derive(Debug, Clone, Serialize)]
pub struct DevPluginInfo {
    /// 调试环境里的插件 id：**取目录名**（不取 `plugin.json.name`——名字写错时目录名才是可靠锚点）。
    pub id: String,
    /// 插件目录绝对路径。
    pub dir: String,
    /// 它来自哪个调试根目录。
    pub root: String,
    /// `plugin.json` 里声明的 name（可能与 `id` 不一致，此时 issues 里会有一条 warn）。
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    /// logo 的 base64 data URL（无 / 读取失败为 None，前端用兜底字形）。
    pub logo: Option<String>,
    #[serde(rename = "featureCount")]
    pub feature_count: usize,
    /// 关键字预览（所有 feature 的裸字符串 cmds）。
    pub cmds: Vec<String>,
    /// 清单声明的高危能力。
    pub permissions: Vec<String>,
    /// **调试环境**里已授权的能力（来自 `AppSettings::dev_plugin_permissions`，与正式授权表无关）。
    pub granted: Vec<String>,
    /// 是否可运行（无致命问题）。false 时前端应禁用「运行」入口并展示 issues。
    pub runnable: bool,
    pub issues: Vec<DevIssue>,
    pub features: Vec<DevFeature>,
}

/// 开发者中心的整体配置快照。
#[derive(Debug, Clone, Serialize)]
pub struct DevConfig {
    /// 调试插件是否并入主搜索（默认 false：调试插件不该污染日常搜索）。
    #[serde(rename = "searchVisible")]
    pub search_visible: bool,
    pub dirs: Vec<DevDirInfo>,
    pub mock: DevMockConfig,
    /// 独立测试库的真实路径（UI 直接展示，让开发者确认「写的不是正式库」）。
    #[serde(rename = "dbPath")]
    pub db_path: String,
    /// 调试沙盒文件根的真实路径。
    #[serde(rename = "filesRoot")]
    pub files_root: String,
}

// ==================== 扫描结果 ====================

/// 一个已扫描的调试插件：对外信息 + 解析成功时的清单（供触发类型判定 / 权限声明校验）。
pub struct DevPlugin {
    /// 不含 `granted`（授权随时会变，列表时才从设置里填）。
    info: DevPluginInfo,
    /// `plugin.json` 解析成功才有；解析失败时为 None（此时 `info.runnable=false`）。
    manifest: Option<PluginManifest>,
}

// ==================== 运行时状态（managed state） ====================

/// 插件调试运行时：调试插件注册表 + 独立测试库 + 日志环形缓冲 + 同步 Mock 配置。
///
/// 同一个 `Arc` 有两条访问路径：作为 managed state 供 `dev_*` 命令使用，
/// 同时挂在 [`crate::plugin::PluginRegistry::dev`] 上，供插件运行时命令按会话分流
/// （否则每个 `plugin_*` 命令都要多传一个 State，改动面无谓地扩大）。
pub struct DevRuntime {
    /// 固定调试根（`dev-plugins/`），不可被用户移除。
    pub fixed_root: PathBuf,
    /// 本次扫描出的调试插件（`dev_rescan` 整体替换）。
    plugins: RwLock<Vec<DevPlugin>>,
    /// 各调试目录的扫描统计（与 `plugins` 同一次扫描产生，保证计数与列表一致）。
    dirs: RwLock<Vec<DevDirInfo>>,
    /// **独立的**测试库（另一个 SQLite 文件，与正式库物理隔离）。
    pub db: Arc<Db>,
    /// 测试库文件路径（UI 展示用）。
    pub db_path: PathBuf,
    /// 调试沙盒文件根：`<dev_home>/plugin-data`。
    pub files_root: PathBuf,
    /// API 调用 / console / 异常日志环形缓冲（由 `dev_log_push` 探针与后端共同写入）。
    pub logs: DevLogs,
    /// 云同步 Mock 配置（持久化在**测试库**的 `app_kv` 里，重启保留）。
    mock: Mutex<DevMockConfig>,
}

impl DevRuntime {
    /// 建立调试运行时：确保 `dev_home` 与固定调试根存在，打开独立测试库，载入 Mock 配置。
    ///
    /// `dev_home` 必须**先建目录**再开库：`Db::open` 在路径不可用时会静默回退内存库，
    /// 那样调试数据一关就没，且开发者完全看不出来。
    pub fn new(dev_home: PathBuf, fixed_root: PathBuf) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dev_home) {
            ilog!("[iTools] 调试环境目录创建失败 {}: {e}", dev_home.display());
        }
        if let Err(e) = std::fs::create_dir_all(&fixed_root) {
            ilog!("[iTools] 固定调试目录创建失败 {}: {e}", fixed_root.display());
        }
        let db_path = dev_home.join("plugin_data_dev.sqlite");
        let db = Arc::new(Db::open(&db_path));
        let mock = mock::load(&db);
        Self {
            fixed_root,
            plugins: RwLock::new(Vec::new()),
            dirs: RwLock::new(Vec::new()),
            db,
            db_path,
            files_root: dev_home.join("plugin-data"),
            logs: DevLogs::default(),
            mock: Mutex::new(mock),
        }
    }

    /// 某调试插件的沙盒文件目录：`<dev_home>/plugin-data/<id>/files/`。
    /// 与正式沙盒（`<localAppData>/itools/plugin-data/<id>/files/`）**不同根**，互不可见。
    pub fn files_dir(&self, id: &str) -> PathBuf {
        self.files_root.join(id).join("files")
    }

    /// 重扫所有调试目录（固定根 + 用户添加的目录），整体替换注册表，返回加载到的插件数。
    ///
    /// 宽松校验：**任何问题都不跳过**，一律加载并把问题写进 `issues`——开发者才有的排查。
    pub fn rescan(&self, extra_dirs: &[String]) -> usize {
        let (plugins, dirs) = scan_all(&self.fixed_root, extra_dirs);
        let n = plugins.len();
        if let Ok(mut g) = self.plugins.write() {
            *g = plugins;
        }
        if let Ok(mut g) = self.dirs.write() {
            *g = dirs;
        }
        n
    }

    /// 列出调试插件信息，`granted` 现填（授权表随时可改，不该缓存进扫描结果）。
    pub fn list(&self, granted: &HashMap<String, Vec<String>>) -> Vec<DevPluginInfo> {
        let Ok(g) = self.plugins.read() else {
            return Vec::new();
        };
        g.iter()
            .map(|p| {
                let mut info = p.info.clone();
                info.granted = granted.get(&info.id).cloned().unwrap_or_default();
                info
            })
            .collect()
    }

    /// 本次扫描各调试目录的统计。
    pub fn dirs(&self) -> Vec<DevDirInfo> {
        self.dirs.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// 某调试插件的目录（id 冲突时取扫描顺序中的第一个——与 `runnable` 的判定一致）。
    pub fn dir_of(&self, id: &str) -> Option<PathBuf> {
        let g = self.plugins.read().ok()?;
        g.iter()
            .find(|p| p.info.id == id && p.info.runnable)
            .or_else(|| g.iter().find(|p| p.info.id == id))
            .map(|p| PathBuf::from(&p.info.dir))
    }

    /// 该调试插件**是否还在本次扫描结果里**。
    ///
    /// 返回 `None` 表示读不出注册表（锁中毒），调用方须保守处理——与
    /// [`crate::plugin::PluginRegistry::has_plugin`] 同一约定（别把「读不到」当成「已删除」）。
    pub fn has_plugin(&self, id: &str) -> Option<bool> {
        Some(self.plugins.read().ok()?.iter().any(|p| p.info.id == id))
    }

    /// 该调试插件是否可运行（缺 `index.html` / 清单解析失败等致命问题时为 false）。
    pub fn runnable(&self, id: &str) -> bool {
        self.plugins
            .read()
            .is_ok_and(|g| g.iter().any(|p| p.info.id == id && p.info.runnable))
    }

    /// 该调试插件**当前清单**是否声明了某高危能力（授权判定的第二个条件，与正式环境同构）。
    pub fn declares_permission(&self, id: &str, perm: &str) -> bool {
        self.plugins.read().is_ok_and(|g| {
            g.iter().any(|p| {
                p.info.id == id && p.info.runnable && p.info.permissions.iter().any(|x| x == perm)
            })
        })
    }

    /// 判定一次触发的类型（regex / keyword / text）——与正式环境同一套规则。
    pub fn trigger_kind(&self, id: &str, code: &str, query: &str) -> String {
        let Ok(g) = self.plugins.read() else {
            return "keyword".to_string();
        };
        g.iter()
            .find(|p| p.info.id == id)
            .and_then(|p| p.manifest.as_ref())
            .map(|m| crate::plugin::trigger_kind_of(m, code, query))
            .unwrap_or_else(|| "keyword".to_string())
    }

    /// 调试插件展开成的可搜索命令（id 带 [`DEV_ID_PREFIX`]，副标题标注「调试」）。
    /// 只有在 `dev_set_search_visible(true)` 时才会被并进搜索索引。
    pub fn search_commands(&self) -> Vec<PluginCommand> {
        let Ok(g) = self.plugins.read() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for p in g.iter() {
            let (Some(m), true) = (p.manifest.as_ref(), p.info.runnable) else {
                continue; // 跑不起来的插件不该出现在搜索里（点了也打不开 = 欺骗）
            };
            out.extend(crate::plugin::expand_one(
                &format!("{DEV_ID_PREFIX}{}", p.info.id),
                m,
                &format!("{} · 调试插件", p.info.id),
                p.info.logo.clone(),
            ));
        }
        out
    }

    /// 读当前 Mock 配置。
    pub fn mock(&self) -> DevMockConfig {
        self.mock.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 写 Mock 配置（已 clamp 到可执行范围）并持久化到测试库。
    pub fn set_mock(&self, cfg: DevMockConfig) {
        let cfg = cfg.normalized();
        if let Ok(mut g) = self.mock.lock() {
            *g = cfg.clone();
        }
        mock::save(&self.db, &cfg);
    }
}

// ==================== 扫描与校验 ====================

/// 扫描全部调试根，返回（插件列表, 目录统计）。
///
/// 每个根支持两种形态：
/// - 根目录本身就是一个插件（`<root>/plugin.json` 存在）——直接把 `<插件源码>/dist`
///   这类构建产物目录加进来时就是这种；
/// - 根目录是插件的容器（扫描其一级子目录）——固定的 `dev-plugins/` 是这种。
fn scan_all(fixed_root: &Path, extra_dirs: &[String]) -> (Vec<DevPlugin>, Vec<DevDirInfo>) {
    let mut plugins: Vec<DevPlugin> = Vec::new();
    let mut dirs: Vec<DevDirInfo> = Vec::new();
    let mut seen_roots: Vec<PathBuf> = Vec::new();

    let mut roots: Vec<(PathBuf, bool)> = vec![(fixed_root.to_path_buf(), true)];
    for d in extra_dirs {
        let p = PathBuf::from(d.trim());
        if !d.trim().is_empty() {
            roots.push((p, false));
        }
    }

    for (root, fixed) in roots {
        // 同一目录被添加两次（或与固定根重复）时只扫一次，避免凭空多出一份「id 冲突」
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen_roots.contains(&key) {
            continue;
        }
        seen_roots.push(key);

        let exists = root.is_dir();
        let before = plugins.len();
        if exists {
            if root.join("plugin.json").exists() {
                plugins.push(inspect(&root, &root));
            } else {
                let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
                    .map(|it| it.flatten().map(|e| e.path()).collect())
                    .unwrap_or_default();
                entries.sort(); // 扫描顺序确定 → id 冲突时「谁胜出」也确定
                for dir in entries {
                    if !dir.is_dir() {
                        continue;
                    }
                    let name = dir
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    // `.` 开头的内部目录与 node_modules 不是插件
                    if name.starts_with('.') || name == "node_modules" {
                        continue;
                    }
                    if !dir.join("plugin.json").exists() {
                        continue; // 没有清单 = 不是插件目录（不报错，避免噪音）
                    }
                    plugins.push(inspect(&root, &dir));
                }
            }
        }
        dirs.push(DevDirInfo {
            path: root.to_string_lossy().into_owned(),
            fixed,
            exists,
            plugin_count: plugins.len() - before,
        });
    }

    mark_id_conflicts(&mut plugins);
    (plugins, dirs)
}

/// id（目录名）冲突处理：**不静默取其一**——先扫到的胜出并保留可运行，
/// 后来的标 error 且 `runnable=false`，两边都能在 UI 上看到冲突对象的路径。
fn mark_id_conflicts(plugins: &mut [DevPlugin]) {
    let ids: Vec<String> = plugins.iter().map(|p| p.info.id.clone()).collect();
    let dirs: Vec<String> = plugins.iter().map(|p| p.info.dir.clone()).collect();
    for i in 0..plugins.len() {
        let first = ids.iter().position(|x| x == &ids[i]).unwrap_or(i);
        if first == i {
            // 胜出方：若后面还有同名，给一条 warn 让开发者知道存在冲突（自身仍可运行）
            if let Some(j) = ids.iter().skip(i + 1).position(|x| x == &ids[i]) {
                let other = dirs[i + 1 + j].clone();
                plugins[i].info.issues.push(DevIssue::warn(
                    "id",
                    format!("存在同名调试插件：{other}（本条先被扫到，实际生效的是本条）"),
                ));
            }
        } else {
            let winner = dirs[first].clone();
            plugins[i].info.issues.push(DevIssue::error(
                "id",
                format!("插件 id（目录名）与 {winner} 冲突，本条不会被加载；请改目录名"),
            ));
            plugins[i].info.runnable = false;
        }
    }
}

/// 校验单个调试插件目录：**不因任何问题跳过**，把问题精确到字段收进 `issues`。
fn inspect(root: &Path, dir: &Path) -> DevPlugin {
    let id = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut issues: Vec<DevIssue> = Vec::new();

    // ---- index.html：**唯一**的「致命且与清单无关」的问题 ----
    if !dir.join("index.html").exists() {
        issues.push(DevIssue::error(
            "index.html",
            "缺少 index.html（插件页面入口），插件无法运行",
        ));
    }

    // ---- plugin.json ----
    let manifest_path = dir.join("plugin.json");
    let raw = std::fs::read_to_string(&manifest_path);
    let manifest: Option<PluginManifest> = match &raw {
        Err(e) => {
            issues.push(DevIssue::error("plugin.json", format!("读取失败: {e}")));
            None
        }
        Ok(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Err(e) => {
                issues.push(DevIssue::error(
                    "plugin.json",
                    format!("不是合法 JSON: {e}（注意 JSON 不支持注释与尾逗号）"),
                ));
                None
            }
            Ok(value) => match serde_json::from_value::<PluginManifest>(value) {
                Err(e) => {
                    issues.push(DevIssue::error(
                        "plugin.json",
                        format!("清单结构不符合规范: {e}"),
                    ));
                    None
                }
                Ok(m) => Some(m),
            },
        },
    };

    let mut info = DevPluginInfo {
        id: id.clone(),
        dir: dir.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
        name: String::new(),
        version: String::new(),
        description: String::new(),
        author: String::new(),
        logo: None,
        feature_count: 0,
        cmds: Vec::new(),
        permissions: Vec::new(),
        granted: Vec::new(),
        runnable: false,
        issues: Vec::new(),
        features: Vec::new(),
    };

    if let Some(m) = &manifest {
        info.name = m.name.clone();
        info.version = m.version.clone();
        info.description = m.description.clone();
        info.author = m.author.clone();
        info.permissions = m.permissions.clone();
        info.feature_count = m.features.len();
        info.logo = read_logo(dir, &m.icon);
        // 把「图标是否可用」的结论传进去，避免为了产出一条 warn 再读一次图并重新 base64
        check_manifest(m, &id, info.logo.is_some(), &mut issues);
        info.features = m
            .features
            .iter()
            .map(|f| {
                let mut keywords = Vec::new();
                let mut has_regex = false;
                let mut has_text = false;
                for c in &f.cmds {
                    match c {
                        Cmd::Keyword(k) => keywords.push(k.clone()),
                        Cmd::Typed(t) if t.kind == "regex" => has_regex = true,
                        Cmd::Typed(t) if t.kind == "text" => has_text = true,
                        Cmd::Typed(_) => {}
                    }
                }
                info.cmds.extend(keywords.iter().cloned());
                DevFeature {
                    code: f.code.clone(),
                    explain: f.explain.clone(),
                    keywords,
                    has_regex,
                    has_text,
                }
            })
            .collect();
    }

    // 致命问题（level=error）⇒ 不可运行
    info.runnable = !issues.iter().any(|i| i.level == "error");
    info.issues = issues;
    DevPlugin { info, manifest }
}

/// 清单字段级校验（全部只产生 warn——正式环境会因这些问题拒装，但调试环境要能跑起来看效果）。
fn check_manifest(m: &PluginManifest, dir_name: &str, logo_ok: bool, issues: &mut Vec<DevIssue>) {
    if m.name.trim().is_empty() {
        issues.push(DevIssue::warn("name", "name 为空（正式环境会拒绝加载）"));
    } else if m.name != dir_name {
        issues.push(DevIssue::warn(
            "name",
            // ⚠ 这些 message 会被前端原样塞进 textContent 显示，**不过任何 Markdown 渲染**：
            // 写 `**加粗**` 只会让星号原样露在界面上。要强调就用「」引号或直白措辞。
            format!(
                "name（{}）与目录名（{dir_name}）不一致：调试环境仍按目录名加载，\
                 但正式环境会直接跳过该插件（表现为「插件凭空消失」）",
                m.name
            ),
        ));
    }
    if m.description.trim().is_empty() {
        issues.push(DevIssue::warn("description", "description 为空（搜索结果里没有说明文字）"));
    }
    if !logo_ok {
        issues.push(DevIssue::warn(
            "icon",
            format!("图标 {} 不可用（缺失 / 非 png,jpg,jpeg,svg,webp / 超出大小上限），列表将用兜底字形", m.icon),
        ));
    }
    for (i, p) in m.permissions.iter().enumerate() {
        if !KNOWN_PERMISSIONS.contains(&p.as_str()) {
            issues.push(DevIssue::warn(
                format!("permissions[{i}]"),
                format!("未知能力「{p}」：可用的是 {}", KNOWN_PERMISSIONS.join(" / ")),
            ));
        }
    }
    if m.features.is_empty() {
        issues.push(DevIssue::warn(
            "features",
            "features 为空：插件搜不到也进不去（正式环境会拒绝加载）",
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, f) in m.features.iter().enumerate() {
        if f.code.trim().is_empty() {
            issues.push(DevIssue::warn(
                format!("features[{i}].code"),
                "code 为空（正式环境会拒绝加载）",
            ));
        } else if seen.contains(&f.code.as_str()) {
            issues.push(DevIssue::warn(
                format!("features[{i}].code"),
                format!("code「{}」重复（正式环境会拒绝加载）", f.code),
            ));
        }
        seen.push(&f.code);

        let (mut kw, mut re, mut text) = (0usize, 0usize, false);
        for (j, c) in f.cmds.iter().enumerate() {
            match c {
                Cmd::Keyword(k) => {
                    if k.trim().is_empty() {
                        issues.push(DevIssue::warn(
                            format!("features[{i}].cmds[{j}]"),
                            "空关键字不会被搜到",
                        ));
                    } else {
                        kw += 1;
                    }
                }
                Cmd::Typed(t) if t.kind == "regex" => match t.pattern.as_deref() {
                    None | Some("") => issues.push(DevIssue::warn(
                        format!("features[{i}].cmds[{j}]"),
                        "regex 触发缺少 match 字段，该条不会生效",
                    )),
                    Some(src) => match regex::Regex::new(src) {
                        Ok(_) => re += 1,
                        Err(e) => issues.push(DevIssue::warn(
                            format!("features[{i}].cmds[{j}]"),
                            format!("正则不合法：{e}"),
                        )),
                    },
                },
                Cmd::Typed(t) if t.kind == "text" => text = true,
                Cmd::Typed(t) => issues.push(DevIssue::warn(
                    format!("features[{i}].cmds[{j}]"),
                    format!(
                        "触发类型「{}」不参与匹配（当前只支持：裸字符串关键字、{{\"type\":\"regex\"}}、{{\"type\":\"text\"}}）",
                        t.kind
                    ),
                )),
            }
        }
        if kw == 0 && re == 0 && !text {
            issues.push(DevIssue::warn(
                format!("features[{i}].cmds"),
                "没有任何可用触发方式，该 feature 搜不到（仍可用「触发模拟器」手动进入）",
            ));
        }
    }
}

// ==================== 固定调试根 ====================

/// 解析固定调试根：
/// 1) 环境变量 `ITOOLS_DEV_PLUGINS_DIR`（测试 / 特殊布局用）；
/// 2) dev 构建：从 exe 上溯到含 `src-tauri` 的项目根，用其 `dev-plugins/`；
/// 3) 打包：`<数据根>\dev\dev-plugins`（即 [`dev_home`] 之下；数据根见 [`crate::paths::data_root`]）。
///
/// 与正式插件根**永不重合**——否则「调试插件」会被正式环境一并加载，隔离就白做了。
pub fn resolve_fixed_root() -> PathBuf {
    if let Ok(p) = std::env::var("ITOOLS_DEV_PLUGINS_DIR") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for anc in exe.ancestors() {
            if anc.join("src-tauri").is_dir() {
                return anc.join("dev-plugins");
            }
        }
    }
    dev_home().join("dev-plugins")
}

/// 调试环境的数据家目录：`<数据根>\dev`（测试库 + 调试沙盒都在这里）。
///
/// 挂在 [`crate::paths::data_root`] 之下而不是自己拼路径：数据根本身按构建类型分家
/// （release `itools` / debug `itools-dev`），调试家目录必须跟着一起分，
/// 否则 debug 与 release 的开发者中心又会共用同一个测试库。
/// 与「正式插件数据」的隔离靠的是这一层 `dev` 子目录，两边都成立。
pub fn dev_home() -> PathBuf {
    crate::paths::data_root().join("dev")
}

// ==================== 搜索索引接入 ====================

/// 把「正式插件命令 +（`dev_search_visible` 开启时的）调试插件命令」一起写进搜索索引。
///
/// 所有刷新插件搜索的入口都必须走这里，否则某条路径（热更新 / 托盘重载 / 安装完成）
/// 会把调试命令悄悄冲掉，开关就成了「时灵时不灵」。
pub fn apply_plugin_search(app: &tauri::AppHandle, mut cmds: Vec<PluginCommand>) {
    use tauri::Manager;
    let (Some(index), Some(settings), Some(dev)) = (
        app.try_state::<crate::search::SearchIndex>(),
        app.try_state::<crate::settings::SettingsStore>(),
        app.try_state::<Arc<DevRuntime>>(),
    ) else {
        return;
    };
    if settings.get().dev_search_visible {
        cmds.extend(dev.search_commands());
    }
    index.set_plugin_commands(cmds);
}

/// 重建插件搜索索引：正式命令从**内存清单**重新展开（不重扫磁盘），再按开关并入调试命令。
pub fn refresh_search(app: &tauri::AppHandle) {
    use tauri::Manager;
    let (Some(registry), Some(settings)) = (
        app.try_state::<crate::plugin::PluginRegistry>(),
        app.try_state::<crate::settings::SettingsStore>(),
    ) else {
        return;
    };
    let cmds = registry.commands(&settings.get().disabled_plugins);
    apply_plugin_search(app, cmds);
}

// ==================== 自定义协议 ====================

/// `itplugindev://localhost/<id>/<path>`（Windows 上为 `http://itplugindev.localhost/...`）。
///
/// 与正式协议的唯一区别：调试插件分散在多个根，所以 id → 目录要**查映射表**而不是拼根路径。
/// 路径穿越校验、CSP、`Cache-Control: no-store` 全部沿用正式实现（调试更需要 no-store：
/// 改完代码重开窗口必须拿到最新文件）。
pub fn serve(
    dev: &DevRuntime,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let path = request.uri().path();
    let rel = path.trim_start_matches('/');
    let mut segs = rel.splitn(2, '/');
    let id = segs.next().unwrap_or("");
    let sub = match segs.next() {
        Some(s) if !s.is_empty() => s,
        _ => "index.html",
    };
    let Some(base) = dev.dir_of(id) else {
        return crate::plugin::serve_status(404);
    };
    crate::plugin::serve_file(&base, sub)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一个临时调试插件目录，返回其根。
    fn temp_root(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "itools-dev-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_plugin(root: &Path, dir: &str, manifest: &str, with_index: bool) -> PathBuf {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.json"), manifest).unwrap();
        if with_index {
            std::fs::write(d.join("index.html"), "<html></html>").unwrap();
        }
        d
    }

    fn issue<'a>(info: &'a DevPluginInfo, field: &str) -> Option<&'a DevIssue> {
        info.issues.iter().find(|i| i.field == field)
    }

    /// 宽松校验：name 与目录名不一致**不跳过**，仍加载并给出 warn（正式环境是直接跳过）。
    #[test]
    fn lenient_name_mismatch_is_loaded_with_warning() {
        let root = temp_root("name");
        write_plugin(
            &root,
            "my-plugin",
            r#"{"name":"wrong-name","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["x"]}]}"#,
            true,
        );
        let (plugins, dirs) = scan_all(&root, &[]);
        assert_eq!(plugins.len(), 1, "name 不一致也必须加载");
        let info = &plugins[0].info;
        assert_eq!(info.id, "my-plugin", "id 取目录名（name 写错时目录名才可靠）");
        assert_eq!(info.name, "wrong-name");
        assert!(info.runnable, "name 不一致只是 warn，仍可运行");
        let i = issue(info, "name").expect("必须给出 name 字段的问题");
        assert_eq!(i.level, "warn");
        // 文案经前端 textContent 原样显示：写 Markdown 星号只会让 `**` 露在界面上
        assert!(
            !i.message.contains("**"),
            "issue 文案不做 Markdown 渲染，不能写 `**`：{}",
            i.message
        );
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].plugin_count, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 缺 index.html 是**唯一**的非清单类致命问题：加载但 runnable=false。
    #[test]
    fn missing_index_html_is_fatal_but_still_listed() {
        let root = temp_root("noindex");
        write_plugin(
            &root,
            "demo",
            r#"{"name":"demo","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["x"]}]}"#,
            false,
        );
        let (plugins, _) = scan_all(&root, &[]);
        assert_eq!(plugins.len(), 1);
        assert!(!plugins[0].info.runnable);
        assert_eq!(issue(&plugins[0].info, "index.html").unwrap().level, "error");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清单坏了（非法 JSON）也要列出来——这正是开发者最需要看到的一条。
    #[test]
    fn broken_manifest_is_reported_not_hidden() {
        let root = temp_root("broken");
        write_plugin(&root, "demo", "{ not json", true);
        let (plugins, _) = scan_all(&root, &[]);
        assert_eq!(plugins.len(), 1, "清单坏掉的插件必须出现在列表里");
        assert!(!plugins[0].info.runnable);
        assert_eq!(issue(&plugins[0].info, "plugin.json").unwrap().level, "error");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 字段级问题要精确到下标：features[0].cmds[1] 的非法正则、未知 permission、空 features。
    #[test]
    fn field_level_issues_point_at_exact_path() {
        let root = temp_root("fields");
        write_plugin(
            &root,
            "demo",
            r#"{"name":"demo","version":"1.0.0","description":"d","permissions":["netwrok"],
                "features":[{"code":"main","cmds":["ok",{"type":"regex","match":"([a-z"}]},
                            {"code":"main","cmds":[{"type":"files"}]}]}"#,
            true,
        );
        let (plugins, _) = scan_all(&root, &[]);
        let info = &plugins[0].info;
        assert!(info.runnable, "这些都只是 warn");
        assert!(issue(info, "features[0].cmds[1]").is_some(), "非法正则要指到具体下标");
        assert!(issue(info, "permissions[0]").is_some(), "拼错的 permission 要提示");
        assert!(issue(info, "features[1].code").is_some(), "重复 code 要提示");
        assert!(issue(info, "features[1].cmds").is_some(), "无可用触发要提示");
        assert!(info.issues.iter().all(|i| i.level == "warn"));
        // 所有 issue 文案都进 textContent，一律不许带 Markdown 星号
        for i in &info.issues {
            assert!(!i.message.contains("**"), "issue 文案不做 Markdown 渲染：{}", i.message);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// id 冲突（两个调试目录里同名）不静默取其一：后者标 error 且不可运行。
    #[test]
    fn duplicate_id_across_roots_is_an_error() {
        let a = temp_root("dupa");
        let b = temp_root("dupb");
        let m = r#"{"name":"demo","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["x"]}]}"#;
        write_plugin(&a, "demo", m, true);
        write_plugin(&b, "demo", m, true);
        let (plugins, _) = scan_all(&a, &[b.to_string_lossy().into_owned()]);
        assert_eq!(plugins.len(), 2, "冲突双方都要列出来");
        assert!(plugins[0].info.runnable, "先扫到的胜出");
        assert!(!plugins[1].info.runnable, "后来的不可运行");
        assert_eq!(issue(&plugins[1].info, "id").unwrap().level, "error");
        assert_eq!(issue(&plugins[0].info, "id").unwrap().level, "warn");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// 用户添加的目录**本身就是插件**（`<插件源码>/dist` 这类构建产物）也要能加载。
    #[test]
    fn extra_dir_can_be_the_plugin_itself() {
        let base = temp_root("dist");
        let plugin_dir = write_plugin(
            &base,
            "dist",
            r#"{"name":"dist","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["x"]}]}"#,
            true,
        );
        let fixed = temp_root("fixed-empty");
        let (plugins, dirs) = scan_all(&fixed, &[plugin_dir.to_string_lossy().into_owned()]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].info.id, "dist");
        assert_eq!(dirs.len(), 2, "固定根 + 用户目录");
        assert!(dirs[0].fixed && !dirs[1].fixed);
        assert_eq!(dirs[1].plugin_count, 1);
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&fixed);
    }

    /// 同一目录被重复添加只扫一次（否则会凭空造出一份「id 冲突」）。
    #[test]
    fn duplicate_root_is_scanned_once() {
        let root = temp_root("dupe-root");
        write_plugin(
            &root,
            "demo",
            r#"{"name":"demo","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["x"]}]}"#,
            true,
        );
        let p = root.to_string_lossy().into_owned();
        let (plugins, dirs) = scan_all(&root, &[p.clone(), p]);
        assert_eq!(plugins.len(), 1);
        assert_eq!(dirs.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 不存在的目录如实标 `exists=false`（不是装作没事，也不是报错弹窗）。
    #[test]
    fn missing_dir_is_reported_as_not_exists() {
        let root = temp_root("missing");
        let gone = root.join("not-there");
        let (plugins, dirs) = scan_all(&root, &[gone.to_string_lossy().into_owned()]);
        assert!(plugins.is_empty());
        assert_eq!(dirs.len(), 2);
        assert!(!dirs[1].exists);
        assert_eq!(dirs[1].plugin_count, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 搜索隔离：跑不起来的插件不进搜索；进搜索的 id 一律带 `dev:` 前缀（据此路由到调试窗）。
    #[test]
    fn search_commands_are_prefixed_and_skip_unrunnable() {
        let root = temp_root("search");
        write_plugin(
            &root,
            "good",
            r#"{"name":"good","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["gogo"]}]}"#,
            true,
        );
        write_plugin(
            &root,
            "bad",
            r#"{"name":"bad","version":"1.0.0","description":"d","features":[{"code":"main","cmds":["bad"]}]}"#,
            false, // 缺 index.html
        );
        let rt = DevRuntime::new(temp_root("search-home"), root.clone());
        assert_eq!(rt.rescan(&[]), 2);
        let cmds = rt.search_commands();
        assert_eq!(cmds.len(), 1, "缺 index.html 的插件不该进搜索");
        assert_eq!(cmds[0].plugin_id, "dev:good");
        assert_eq!(cmds[0].to_item().target, "dev:good#main");
        let _ = std::fs::remove_dir_all(&root);
    }
}
