pub mod apps;
pub mod apps_folder;
pub mod builtins;
pub mod files;
pub mod icon;
pub mod winsearch;

use std::collections::HashMap;
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

/// 内存搜索索引：应用启动扫描；文件优先走 Windows Search 秒搜，不可用则 walkdir 兜底
pub struct SearchIndex {
    apps: Arc<RwLock<Vec<apps::AppEntry>>>,
    /// 「本地启动」清单里的自定义文件/文件夹，一并参与默认搜索
    custom: Arc<RwLock<Vec<apps::AppEntry>>>,
    /// 插件命令（页面插件），一并参与默认搜索（kind=plugin，回车打开插件窗口）
    plugins: Arc<RwLock<Vec<crate::plugin::PluginCommand>>>,
    files: Arc<RwLock<Vec<files::FileEntry>>>,
    winsearch: winsearch::WinSearchWorker,
    icon_cache: IconCache,
}

impl SearchIndex {
    pub fn new(custom_apps: Vec<String>) -> Self {
        let apps = Arc::new(RwLock::new(Vec::new()));
        let custom = Arc::new(RwLock::new(Vec::new()));
        let plugins = Arc::new(RwLock::new(Vec::new()));
        let files = Arc::new(RwLock::new(Vec::new()));
        let winsearch = winsearch::WinSearchWorker::new();
        let icon_cache: IconCache = Arc::new(Mutex::new(HashMap::new()));

        // Windows Search 可用时走它全盘秒搜；不可用才用 walkdir 遍历用户目录兜底
        if !winsearch.available {
            let files_bg = Arc::clone(&files);
            std::thread::spawn(move || {
                let scanned = files::scan_files();
                if let Ok(mut guard) = files_bg.write() {
                    *guard = scanned;
                }
            });
        }

        spawn_app_scan(Arc::clone(&apps), Arc::clone(&icon_cache), custom_apps);

        Self {
            apps,
            custom,
            plugins,
            files,
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
    /// - `/f xxx`：文件搜索（Windows Search 全盘秒搜，或 walkdir 兜底）
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

    /// 文件搜索：优先 Windows Search 全盘秒搜，不可用则 walkdir 索引模糊兜底
    fn query_files(&self, query: &str, limit: usize) -> Vec<SearchItem> {
        if self.winsearch.available {
            return self
                .winsearch
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
                .collect();
        }

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

    /// 默认只搜应用；"/f xxx" 才搜文件
    #[test]
    fn file_prefix_smoke() {
        let index = SearchIndex::new(Vec::new());

        let default_results = index.query("png", 8);
        assert!(
            default_results.iter().all(|r| r.kind == "app" || r.kind == "command"),
            "默认模式不应出现文件结果"
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
