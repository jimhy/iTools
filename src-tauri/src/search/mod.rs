pub mod apps;
pub mod apps_folder;
pub mod builtins;
pub mod files;
pub mod icon;
/// 全盘文件名秒搜（直读 NTFS MFT，提权守护进程 + 命名管道）。
/// 存在的理由见 `mft/mod.rs`：Windows Search 的索引范围通常只有 `C:\Users\`，
/// 其它盘一条都搜不到，且 CONTAINS 只支持词前缀。
pub mod mft;
pub mod winsearch;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};

/// 图标缓存：路径 → base64(PNG)；值为 `None` 表示「提取失败」也缓存，避免重复重试
type IconCache = Arc<Mutex<HashMap<String, Option<String>>>>;

/// 一条搜索结果，序列化结构与前端 `SearchItem`（src/types.ts）保持一致
#[derive(Clone, Serialize, Deserialize)]
pub struct SearchItem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub kind: String,
    pub target: String,
    pub icon: Option<String>,
    /// 执行动作："open"（用 shell 打开 target）或 "copy"（复制 target 到剪贴板）
    pub action: String,
}

/// 内存搜索索引：应用启动扫描；文件搜索三级降级（MFT 全盘 → Windows Search → walkdir 兜底）
pub struct SearchIndex {
    apps: Arc<RwLock<Vec<apps::AppEntry>>>,
    /// 「本地启动」清单里的自定义文件/文件夹，一并参与默认搜索
    custom: Arc<RwLock<Vec<apps::AppEntry>>>,
    /// 插件命令（页面插件），一并参与默认搜索（kind=plugin，回车打开插件窗口）
    plugins: Arc<RwLock<Vec<crate::plugin::PluginCommand>>>,
    files: Arc<RwLock<Vec<files::FileEntry>>>,
    /// walkdir 兜底索引是否已安排过扫描（一次性闸门，见 `scan_walkdir_index`）
    walkdir_scan_started: Arc<AtomicBool>,
    winsearch: winsearch::WinSearchWorker,
    icon_cache: IconCache,
}

impl SearchIndex {
    pub fn new(custom_apps: Vec<String>) -> Self {
        let apps = Arc::new(RwLock::new(Vec::new()));
        let custom = Arc::new(RwLock::new(Vec::new()));
        let plugins = Arc::new(RwLock::new(Vec::new()));
        let files = Arc::new(RwLock::new(Vec::new()));
        let walkdir_scan_started = Arc::new(AtomicBool::new(false));
        let winsearch = winsearch::WinSearchWorker::new();
        let icon_cache: IconCache = Arc::new(Mutex::new(HashMap::new()));

        // walkdir 索引是 `/f` 的最后一级兜底，只有「MFT 守护在跑」时才真的多余。
        //
        // 这里刻意**不**看 `winsearch.available`：老写法是「Windows Search 可用就不建 walkdir 索引」，
        // 而实测本机 SystemIndex 的索引范围只有 C:\Users\ 与开始菜单——服务「可用」并不等于
        // 「搜得到」。于是「服务在跑但索引范围窄」这个最常见的情况下，第 3 级降级成了空壳，
        // 用户搜其它盘/搜不到词前缀时一条结果都拿不到。
        let files_bg = Arc::clone(&files);
        let started_bg = Arc::clone(&walkdir_scan_started);
        std::thread::spawn(move || {
            // is_running() 要走一次命名管道往返（守护缺席时 open 立刻失败，不会久等），
            // 但仍放在后台线程里做，保持 new() 不阻塞窗口启动这一特性。
            if mft::MftSearch::is_running() {
                return;
            }
            scan_walkdir_index(&files_bg, &started_bg);
        });

        spawn_app_scan(Arc::clone(&apps), Arc::clone(&icon_cache), custom_apps);

        Self {
            apps,
            custom,
            plugins,
            files,
            walkdir_scan_started,
            winsearch,
            icon_cache,
        }
    }

    /// 设置变更（手动添加程序等）后重建应用索引
    pub fn rescan_apps(&self, custom_apps: Vec<String>) {
        spawn_app_scan(
            Arc::clone(&self.apps),
            Arc::clone(&self.icon_cache),
            custom_apps,
        );
    }

    /// 用「本地启动」清单重建可搜索的自定义条目（启动时初始化 + 增删本地启动项后调用）。
    /// 与扫描到的应用一同参与默认搜索（kind=app，回车用 open 打开），
    /// 与「开机自动启动」开关无关——只要在清单里就能搜到。轻量、同步（无 COM/扫描）。
    pub fn set_custom_items(&self, items: Vec<crate::settings::LaunchItem>) {
        let entries: Vec<apps::AppEntry> = items
            .into_iter()
            .map(|it| {
                apps::AppEntry::new(it.name.clone(), it.name, std::path::PathBuf::from(it.path))
            })
            .collect();
        if let Ok(mut guard) = self.custom.write() {
            *guard = entries;
        }
    }

    /// 用扫描到的插件命令重建插件搜索池（启动时初始化；重扫插件后可再调）。
    /// 轻量、同步，与应用一同参与默认搜索（kind=plugin，回车走 open_plugin_window 打开插件页）。
    pub fn set_plugin_commands(&self, cmds: Vec<crate::plugin::PluginCommand>) {
        if let Ok(mut guard) = self.plugins.write() {
            *guard = cmds;
        }
    }

    /// 供 tauri 命令按需提取图标时复用同一份缓存
    pub fn icon_cache_handle(&self) -> IconCache {
        Arc::clone(&self.icon_cache)
    }

    /// 查询入口：
    /// - 默认：内置命令置顶 + 应用模糊匹配（支持中文名的拼音全拼/首字母）
    /// - `/f xxx`：文件搜索（MFT 全盘秒搜 → Windows Search → walkdir 三级降级，见 [`Self::query_files`]）
    pub fn query(&self, raw: &str, limit: usize) -> Vec<SearchItem> {
        let query = raw.trim();
        if query.is_empty() {
            return Vec::new();
        }

        // "/f xxx" → 文件搜索模式
        if let Some(file_query) = query.strip_prefix("/f") {
            let file_query = file_query.trim();
            if file_query.is_empty() {
                return Vec::new();
            }
            let mut out = self.query_files(file_query, limit);
            self.fill_cached_icons(&mut out);
            return out;
        }

        // 默认：内置即时命令优先
        let mut out = builtins::match_commands(query);
        if out.len() >= limit {
            out.truncate(limit);
            return out;
        }

        // 应用：模糊匹配（应用名 + 拼音键取最高分）
        let matcher = SkimMatcherV2::default();
        let mut app_scored: Vec<(i64, SearchItem)> = Vec::new();
        if let Ok(apps) = self.apps.read() {
            for app in apps.iter() {
                if let Some(score) = app.match_score(&matcher, query) {
                    app_scored.push((score, app.to_item()));
                }
            }
        }
        // 本地启动清单里的自定义文件/文件夹，一并参与默认搜索
        if let Ok(custom) = self.custom.read() {
            for item in custom.iter() {
                if let Some(score) = item.match_score(&matcher, query) {
                    app_scored.push((score, item.to_item()));
                }
            }
        }
        // 插件命令（页面插件）：关键字模糊 / regex 精确命中
        if let Ok(plugins) = self.plugins.read() {
            for cmd in plugins.iter() {
                if let Some(score) = cmd.match_score(&matcher, query) {
                    app_scored.push((score, cmd.to_item()));
                }
            }
        }
        app_scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        app_scored.truncate(limit - out.len());
        out.extend(app_scored.into_iter().map(|(_, item)| item));

        self.fill_cached_icons(&mut out);
        out
    }

    /// 文件搜索：三级降级，**前一级「用不了」才降到下一级**（而不是「没搜到就降级」）。
    ///
    /// 1. MFT 全盘索引（提权守护）——真子串 + 全盘覆盖，最优；
    /// 2. Windows Search 索引——词前缀，覆盖范围取决于系统索引配置；
    /// 3. walkdir 内存索引——只覆盖用户常用目录，但永远在。
    fn query_files(&self, query: &str, limit: usize) -> Vec<SearchItem> {
        // 第 1 级。`None` = 守护没起/超时（后端用不了）；`Some(vec![])` = 索引可用但确实没匹配。
        let mft_hit = mft::MftSearch::query(query, limit);
        let backend = pick_file_backend(mft_hit.is_some(), self.winsearch.available);
        if backend != FileBackend::Mft {
            // 守护没起来 → 顺手引导用户开启全盘索引（本次查询不等它，见函数注释）
            spawn_mft_bootstrap();
        }
        match backend {
            // unwrap_or_default 只是为了不 unwrap：pick_file_backend 返回 Mft 的前提就是 is_some()
            FileBackend::Mft => mft_hit.unwrap_or_default(),
            FileBackend::WinSearch => self.query_files_winsearch(query, limit),
            FileBackend::Walkdir => {
                // 启动时守护在跑 → 我们没建 walkdir 索引（省内存）；守护后来被杀/崩了就会落到这里，
                // 而索引是空的。补建一次（后台，一个进程内最多一次），下次按键兜底就有货了，
                // 而不是让用户面对一个「永远 0 条」的搜索框。
                if self.files.read().map(|f| f.is_empty()).unwrap_or(false) {
                    self.spawn_walkdir_scan();
                }
                self.query_files_walkdir(query, limit)
            }
        }
    }

    /// 后台补建 walkdir 兜底索引（`scan_walkdir_index` 内有「最多扫一次」的闸门）
    fn spawn_walkdir_scan(&self) {
        let files = Arc::clone(&self.files);
        let started = Arc::clone(&self.walkdir_scan_started);
        std::thread::spawn(move || scan_walkdir_index(&files, &started));
    }

    /// 第 2 级：Windows Search（SystemIndex）。
    ///
    /// 只是**降级项**：它的覆盖范围由系统索引配置决定（实测本机只有 C:\Users\ 与开始菜单），
    /// 且 CONTAINS 只支持词前缀（搜「报告」搜不到「季度报告.docx」）。
    fn query_files_winsearch(&self, query: &str, limit: usize) -> Vec<SearchItem> {
        self.winsearch
            .query(query, limit)
            .into_iter()
            .map(|(name, path, is_dir)| SearchItem {
                id: path.clone(),
                title: name,
                subtitle: path.clone(),
                kind: if is_dir { "folder" } else { "file" }.to_string(),
                target: path,
                icon: None,
                action: "open".to_string(),
            })
            .collect()
    }

    /// 第 3 级：walkdir 内存索引模糊匹配。范围只有桌面/文档/下载/图片（见 `files::scan_files`），
    /// 但不依赖任何系统服务或提权，是真正的最后兜底。
    fn query_files_walkdir(&self, query: &str, limit: usize) -> Vec<SearchItem> {
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(i64, SearchItem)> = Vec::new();
        if let Ok(files) = self.files.read() {
            for file in files.iter() {
                if let Some(score) = matcher.fuzzy_match(&file.name, query) {
                    scored.push((score, file.to_item()));
                }
            }
        }
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored.truncate(limit);
        scored.into_iter().map(|(_, item)| item).collect()
    }

    /// 从缓存回填已提取的图标（未命中留 None，由前端按需 load_icons 补齐）
    fn fill_cached_icons(&self, items: &mut [SearchItem]) {
        if let Ok(cache) = self.icon_cache.lock() {
            for item in items.iter_mut() {
                if item.icon.is_none() {
                    if let Some(Some(b64)) = cache.get(&item.target) {
                        item.icon = Some(b64.clone());
                    }
                }
            }
        }
    }
}

/// `/f` 这一次查询实际用哪个后端。抽成独立类型是为了能单测降级判定
/// （见 `tests::file_backend_fallback_semantics`），生产路径与测试走的是同一份判定代码。
#[derive(Debug, PartialEq, Eq)]
enum FileBackend {
    /// 用 MFT 的结果——**包括「零条命中」**：索引可用时「没匹配」就是最终结论
    Mft,
    /// MFT 用不上，退到 Windows Search
    WinSearch,
    /// 前两级都用不上，退到 walkdir 内存索引
    Walkdir,
}

/// 扫描用户常用目录，填充 walkdir 兜底索引。`started` 是这份索引的一次性闸门：
/// 用 `swap` 而不是「先读后写」，保证并发按键时最多只有一次扫描在跑
/// （`files::scan_files` 要遍历四个用户目录，重复做纯属浪费）。
fn scan_walkdir_index(files: &RwLock<Vec<files::FileEntry>>, started: &AtomicBool) {
    if started.swap(true, Ordering::SeqCst) {
        return; // 已经扫过或正在扫
    }
    let scanned = files::scan_files();
    if let Ok(mut guard) = files.write() {
        *guard = scanned;
    }
}

/// 降级判定。参数是两条「这一级能不能用」的事实，不是「有没有结果」。
///
/// 关键语义：`mft_usable` 来自 `MftSearch::query(..).is_some()`。
/// `Some(vec![])` 表示**索引可用但确实没有匹配**，此时绝不能继续降级——
/// 再降下去只会拿回一批覆盖更窄、只支持词前缀的结果，反倒让用户以为「全盘索引不准」。
/// 只有 `None`（守护没起 / 超时，后端用不了）才往下走。
fn pick_file_backend(mft_usable: bool, winsearch_available: bool) -> FileBackend {
    if mft_usable {
        FileBackend::Mft
    } else if winsearch_available {
        FileBackend::WinSearch
    } else {
        FileBackend::Walkdir
    }
}

/// 顺手把 MFT 守护拉起来：用户第一次用 `/f` 就被引导开启全盘索引。
///
/// 两点刻意的设计：
/// 1. **放后台线程**——`ensure_running` 内部是 `ShellExecuteW("runas")`，它会阻塞到用户在
///    UAC 对话框上作答；直接在查询线程里调，等于把搜索框卡在那儿等用户点按钮。
/// 2. **本次查询不等它**——索引要几十秒才建好，这次仍旧走降级后端返回结果。
///
/// 不会骚扰用户：`ensure_running(false)` 内部有 60s 冷却，且「用户在 UAC 上点过否」之后
/// 本次运行不再自动弹（改由设置里手动开启）。
#[cfg(not(test))]
fn spawn_mft_bootstrap() {
    std::thread::spawn(|| {
        mft::MftSearch::ensure_running(false);
    });
}

/// 测试构建里不做引导：`cargo test` 跑到 `/f` 用例时不该弹 UAC，
/// 更不该在测试机上真的起一个全盘索引守护。降级链本身照常被测到。
#[cfg(test)]
fn spawn_mft_bootstrap() {}

/// 应用扫描（本地化名需 COM + 数百次 shell 调用，较重）与图标预热
/// 串行放同一后台线程，不阻塞窗口启动/设置保存
fn spawn_app_scan(
    apps: Arc<RwLock<Vec<apps::AppEntry>>>,
    icon_cache: IconCache,
    custom_apps: Vec<String>,
) {
    std::thread::spawn(move || {
        icon::init_com_for_thread();
        let scanned = apps::scan_apps(&custom_apps);
        let paths: Vec<String> = scanned
            .iter()
            .map(|e| e.path.to_string_lossy().into_owned())
            .collect();
        if let Ok(mut guard) = apps.write() {
            *guard = scanned;
        }
        for p in paths {
            let already = icon_cache
                .lock()
                .map(|g| g.contains_key(&p))
                .unwrap_or(true);
            if already {
                continue;
            }
            let value = icon::icon_base64_png(std::path::Path::new(&p));
            if let Ok(mut g) = icon_cache.lock() {
                g.insert(p, value);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 等后台线程把应用索引填充完（上限 ~10s）
    fn wait_apps(index: &SearchIndex) {
        for _ in 0..200 {
            if index.apps.read().map(|a| !a.is_empty()).unwrap_or(false) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("应用索引 10s 内未就绪");
    }

    /// 端到端冒烟：三源扫描 + 本地化名 + 降权 + 模糊查询
    #[test]
    fn scan_and_query_smoke() {
        icon::init_com_for_thread();
        let apps = apps::scan_apps(&[]);
        println!("扫描到 {} 个应用（开始菜单 + App Paths + 系统命令）", apps.len());
        for a in apps.iter().take(8) {
            println!("  app: {} [{}] ({})", a.name, a.file_stem, a.path.display());
        }
        assert!(!apps.is_empty(), "应至少扫描到一个应用");

        let index = SearchIndex::new(Vec::new());
        wait_apps(&index);
        println!("winsearch available: {}", index.winsearch.available);

        // 「卸载」：降权但必须能搜到（至少命中内置的「卸载或更新程序」）
        let uninstall = index.query("卸载", 8);
        println!("query 卸载 -> {} 条", uninstall.len());
        for r in uninstall.iter().take(4) {
            println!("  [{}] {}", r.kind, r.title);
        }
        assert!(
            uninstall.iter().any(|r| r.title.contains("卸载")),
            "搜「卸载」应能命中卸载类条目"
        );

        // 「远程」：本地化显示名应让远程桌面连接可被中文搜到
        let remote = index.query("远程", 8);
        println!("query 远程 -> {} 条", remote.len());
        for r in remote.iter().take(6) {
            println!("  [{}] {}", r.kind, r.title);
        }

        for q in ["se", "co", "png"] {
            let results = index.query(q, 8);
            println!("query {q:?} -> {} 条", results.len());
            for r in results.iter().take(4) {
                println!("  [{}] {}", r.kind, r.title);
            }
        }
    }

    /// 内置命令断言：计算 / 进制 / 颜色 / URL
    #[test]
    fn builtins_smoke() {
        let calc = builtins::match_commands("1+2*3");
        assert!(
            calc.iter().any(|i| i.kind == "command" && i.title.contains("= 7")),
            "计算器应得 7"
        );

        let radix = builtins::match_commands("255");
        assert!(
            radix.iter().any(|i| i.title.contains("0xFF")),
            "进制应含 0xFF"
        );

        let color = builtins::match_commands("#ff8800");
        assert!(
            color.iter().any(|i| i.title.contains("#FF8800")),
            "颜色应规范化为 #FF8800"
        );

        let url = builtins::match_commands("github.com");
        assert!(
            url.iter()
                .any(|i| i.action == "open" && i.target == "https://github.com"),
            "URL 应打开 https://github.com"
        );

        // 普通词不应误触发任何命令
        assert!(builtins::match_commands("edge").is_empty(), "普通词不应命中命令");
    }

    /// 拼音键生成 + 拼音模糊匹配
    #[test]
    fn pinyin_smoke() {
        let keys = apps::pinyin_keys("微信");
        assert!(
            keys.iter().any(|(f, i)| f == "weixin" && i == "wx"),
            "微信 应生成 (weixin, wx)，实际: {keys:?}"
        );

        // 多音字：乐 → le/yue 都应展开
        let keys = apps::pinyin_keys("QQ音乐");
        assert!(
            keys.iter().any(|(f, _)| f == "qqyinyue"),
            "多音字应含 qqyinyue 变体，实际: {keys:?}"
        );

        // 纯英文名不生成拼音键
        assert!(apps::pinyin_keys("Chrome").is_empty());

        // 端到端匹配：本地化名「远程桌面连接」+ 英文文件名双键
        let entry = apps::AppEntry::new(
            "远程桌面连接".to_string(),
            "Remote Desktop Connection".to_string(),
            std::path::PathBuf::from(r"C:\x\Remote Desktop Connection.lnk"),
        );
        let matcher = SkimMatcherV2::default();
        assert!(entry.match_score(&matcher, "远程").is_some(), "中文应命中");
        assert!(
            entry.match_score(&matcher, "yuancheng").is_some(),
            "拼音全拼应命中"
        );
        assert!(entry.match_score(&matcher, "yczm").is_some(), "拼音首字母应命中");
        assert!(entry.match_score(&matcher, "remote").is_some(), "英文文件名应命中");
        assert!(entry.match_score(&matcher, "qqqq").is_none(), "无关词不应命中");

        // 降权：卸载类命中但分数低于同等匹配的正常条目
        let normal = apps::AppEntry::new(
            "微信".to_string(),
            "微信".to_string(),
            std::path::PathBuf::from(r"C:\x\微信.lnk"),
        );
        let demoted = apps::AppEntry::new(
            "卸载微信".to_string(),
            "卸载微信".to_string(),
            std::path::PathBuf::from(r"C:\x\卸载微信.lnk"),
        );
        let ns = normal.match_score(&matcher, "微信");
        let ds = demoted.match_score(&matcher, "微信");
        assert!(ns.is_some() && ds.is_some(), "两者都应命中「微信」");
        assert!(ns > ds, "卸载类条目应被降权");
    }

    /// 降级链的判定语义（第 1 级返回值怎么解读）：
    /// `Some(vec![])` = 索引可用但确实没匹配 → **不许降级**；`None` = 后端用不了 → 才降级。
    /// 这两者一旦合并，用户就会在「MFT 索引里真没这个文件」时拿到一批覆盖更窄的结果。
    #[test]
    fn file_backend_fallback_semantics() {
        let sample = SearchItem {
            id: r"D:\x\季度报告.docx".to_string(),
            title: "季度报告.docx".to_string(),
            subtitle: r"D:\x\季度报告.docx".to_string(),
            kind: "file".to_string(),
            target: r"D:\x\季度报告.docx".to_string(),
            icon: None,
            action: "open".to_string(),
        };

        // 有命中：显然用第 1 级
        let hit: Option<Vec<SearchItem>> = Some(vec![sample]);
        assert_eq!(pick_file_backend(hit.is_some(), true), FileBackend::Mft);

        // 零命中但索引可用：最容易写错的一格
        let empty: Option<Vec<SearchItem>> = Some(Vec::new());
        assert_eq!(
            pick_file_backend(empty.is_some(), true),
            FileBackend::Mft,
            "Some(vec![]) 是「索引可用但确实没匹配」，不该降级"
        );
        assert_eq!(
            pick_file_backend(empty.is_some(), false),
            FileBackend::Mft,
            "第 2/3 级可用与否都不影响：第 1 级已经给出了结论"
        );

        // 后端用不了才降级，降到哪一级看 Windows Search 在不在
        let unusable: Option<Vec<SearchItem>> = None;
        assert_eq!(
            pick_file_backend(unusable.is_some(), true),
            FileBackend::WinSearch,
            "MFT 用不了、Windows Search 可用 → 第 2 级"
        );
        assert_eq!(
            pick_file_backend(unusable.is_some(), false),
            FileBackend::Walkdir,
            "前两级都用不了 → walkdir 兜底"
        );
    }

    /// 只要 MFT 守护没在跑，walkdir 兜底索引就必须建起来。
    /// 老代码是「Windows Search 可用就不建」，而本机 SystemIndex 只覆盖 C:\Users\，
    /// 于是第 3 级降级成了空壳——这条用例守着那个坑。
    #[test]
    fn walkdir_fallback_index_is_built() {
        // 守护真在跑时按设计就不该建 walkdir 索引，如实跳过。
        //
        // 曾经这里还会被同进程的 `mft::ipc::tests::server_client_end_to_end` 误触发
        //（它起的服务端占着生产管道名，导致本用例随执行顺序时跑时跳）。那个用例现在
        // 监听的是独立管道名（见 `ipc::serve_on` 的文档），所以下面这一支只在**本机
        // 真有提权守护**时才成立。
        if mft::MftSearch::is_running() {
            println!("MFT 守护在跑，按设计不建 walkdir 索引，跳过该用例");
            return;
        }
        // 先直接扫一遍：这台机器的用户目录里确实有东西可索引，断言才有意义
        if files::scan_files().is_empty() {
            println!("用户目录（桌面/文档/下载/图片）为空，无法断言索引非空，跳过");
            return;
        }

        let index = SearchIndex::new(Vec::new());
        println!("winsearch available: {}", index.winsearch.available);
        let mut built = 0usize;
        for _ in 0..600 {
            built = index.files.read().map(|f| f.len()).unwrap_or(0);
            if built > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            built > 0,
            "walkdir 兜底索引 30s 内没建起来（winsearch available={}）",
            index.winsearch.available
        );
        println!("walkdir 索引条目数 = {built}");

        // 拿索引里真实存在的一个名字，验证第 3 级真的能出结果（不是建了个不被查的索引）
        let sample = index
            .files
            .read()
            .ok()
            .and_then(|f| f.first().map(|e| e.name.clone()));
        let name = sample.expect("索引非空时必能取到一条");
        let hits = index.query_files_walkdir(&name, 32);
        assert!(
            hits.iter().any(|h| h.title == name),
            "walkdir 兜底应能搜到索引里的「{name}」，实得 {} 条",
            hits.len()
        );
    }

    /// walkdir 索引的一次性闸门：第一次真扫，之后不再重复遍历用户目录。
    /// （降级到第 3 级发现索引为空时会补建一次，靠这个闸门不至于每次按键都扫盘。）
    #[test]
    fn walkdir_scan_gate_runs_once() {
        let files = RwLock::new(Vec::new());
        let started = AtomicBool::new(false);
        scan_walkdir_index(&files, &started);
        let first = files.read().map(|f| f.len()).unwrap_or(0);
        println!("首次扫描 {first} 条");
        if !files::scan_files().is_empty() {
            assert!(first > 0, "用户目录里有东西，首次调用就该真的把索引填上");
        }

        // 清空后再调一次：被闸门挡住 → 内容不会被重新填上
        if let Ok(mut g) = files.write() {
            g.clear();
        }
        scan_walkdir_index(&files, &started);
        assert_eq!(
            files.read().map(|f| f.len()).unwrap_or(0),
            0,
            "第二次调用应被闸门挡住，不该重复扫盘"
        );
    }

    /// 默认只搜应用；"/f xxx" 才搜文件
    #[test]
    fn file_prefix_smoke() {
        let index = SearchIndex::new(Vec::new());

        let default_results = index.query("png", 8);
        assert!(
            default_results.iter().all(|r| r.kind == "app" || r.kind == "command"),
            "默认模式不应出现文件结果"
        );

        // 三级降级里哪一级会服务这次查询（测试环境一般没有提权守护 → 第 2/3 级）
        println!(
            "/f 走的后端: {:?}",
            pick_file_backend(
                mft::MftSearch::query("png", 1).is_some(),
                index.winsearch.available
            )
        );

        let file_results = index.query("/f png", 8);
        println!("/f png -> {} 条", file_results.len());
        for r in file_results.iter().take(4) {
            println!("  [{}] {}", r.kind, r.title);
        }
        assert!(
            file_results.iter().all(|r| r.kind == "file" || r.kind == "folder"),
            "/f 模式只应出现文件/文件夹"
        );
    }

    /// 图标提取：explorer.exe 应产出合法 PNG 的 base64
    #[test]
    fn icon_smoke() {
        icon::init_com_for_thread();
        let b64 = icon::icon_base64_png(std::path::Path::new(r"C:\Windows\explorer.exe"));
        match &b64 {
            Some(s) => println!("explorer 图标 base64 长度 = {}", s.len()),
            None => println!("未取到图标（异常）"),
        }
        let b64 = b64.expect("explorer.exe 应能提取图标");
        // PNG 魔数 89 50 4E 47 的 base64 前缀是 iVBOR
        assert!(b64.starts_with("iVBOR"), "应是合法 PNG");
    }

    /// 「本地启动」清单里的项应能在默认搜索里按名/拼音搜到，移除后搜不到
    #[test]
    fn custom_launch_items_searchable() {
        let index = SearchIndex::new(Vec::new());
        let path = r"C:\proj\我的报告.docx";
        index.set_custom_items(vec![crate::settings::LaunchItem {
            id: path.to_string(),
            path: path.to_string(),
            name: "我的报告.docx".to_string(),
            is_dir: false,
        }]);

        assert!(
            index.query("报告", 20).iter().any(|i| i.target == path),
            "应能按中文名搜到本地启动项"
        );
        assert!(
            index.query("baogao", 20).iter().any(|i| i.target == path),
            "应能按拼音搜到本地启动项"
        );

        // 移除后不应再搜到
        index.set_custom_items(Vec::new());
        assert!(
            !index.query("报告", 20).iter().any(|i| i.target == path),
            "移除后不应再搜到"
        );
    }
}
