//! 插件热更新（零新依赖）：后台线程轮询 `plugins/` 目录、按**每个插件**算内容指纹，
//! 变化时自动重扫插件清单 + 刷新搜索索引；并**仅当正在打开的那个插件自身变化时**
//! 才重载它的窗口——改插件 JS/清单后无需重启、无需手动「重新加载插件」。
//!
//! 设计要点：
//! - **按插件隔离**：指纹是「插件 id → 该目录内容 hash」的映射。改 / 删插件 B 不会打断
//!   正开着的插件 A（只更新搜索索引）；只有当前插件 A 自己被改才重载 A 的窗口。
//! - **指纹纳入相对路径**：纯重命名（内容/大小/mtime 不变）也能被检出。
//! - **跳过 node_modules/.git**：避免对无关大目录做无谓遍历。
//! - 为何用轮询而非 OS 文件事件（notify/ReadDirectoryChangesW）：零新依赖、对「先写临时文件
//!   再改名」等各种保存姿势都稳；插件目录很小，一次遍历微秒级，1.5s 一轮开销可忽略；
//!   仅在**文件真的变化**时才动作，正常使用（不改插件文件）永不打扰。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Emitter, Manager};

use crate::logging::ilog;
use crate::search::SearchIndex;
use crate::settings::SettingsStore;

use super::PluginRegistry;

/// 轮询间隔：兼顾「改完很快生效」与「几乎零开销」。
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// 插件 id → 该插件目录的内容指纹。
type Fingerprints = HashMap<String, u64>;

/// 启动后台轮询监听（线程随进程存活，app 退出即随之结束）。
pub fn start(app: &AppHandle, root: PathBuf) {
    let handle = app.clone();
    let watch_root = root.clone();
    let spawned = std::thread::Builder::new()
        .name("plugin-watch".into())
        .spawn(move || {
            let mut last = plugin_fingerprints(&watch_root);
            loop {
                std::thread::sleep(POLL_INTERVAL);
                let now = plugin_fingerprints(&watch_root);
                if now != last {
                    let changed = changed_plugins(&last, &now);
                    on_change(&handle, &changed);
                    last = now;
                }
            }
        });
    match spawned {
        Ok(_) => ilog!(
            "[iTools] 插件热更新已启用（轮询 {:?}）：{}",
            POLL_INTERVAL,
            root.display()
        ),
        Err(e) => ilog!("[iTools] 插件热更新线程启动失败（仍可手动重载）：{e}"),
    }
}

/// 扫描插件根，对每个**顶层插件目录**算一个内容指纹。
fn plugin_fingerprints(root: &Path) -> Fingerprints {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out; // 目录还不存在（没放插件）——正常
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        out.insert(id, dir_hash(&dir));
    }
    out
}

/// 单个插件目录的内容指纹：对每个文件的 (相对路径, 大小, mtime 纳秒) 做**顺序无关**的异或累加。
/// - 纳入相对路径 → 纯重命名也会改变指纹；
/// - 大小 / mtime 纳秒 → 内容改动被检出（含「同秒内、同大小」的编辑）；
/// - 增 / 删文件 → 参与异或的项集合变化，指纹随之变化；
/// - 跳过 `node_modules` / `.git`：无关的大目录不必遍历。
fn dir_hash(dir: &Path) -> u64 {
    let mut acc: u64 = 0;
    let walker = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !matches!(e.file_name().to_str(), Some("node_modules") | Some(".git")));
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let mut h = DefaultHasher::new();
        if let Ok(rel) = entry.path().strip_prefix(dir) {
            rel.to_string_lossy().hash(&mut h);
        }
        if let Ok(meta) = entry.metadata() {
            meta.len().hash(&mut h);
            if let Ok(mt) = meta.modified() {
                if let Ok(d) = mt.duration_since(SystemTime::UNIX_EPOCH) {
                    d.as_nanos().hash(&mut h);
                }
            }
        }
        acc ^= h.finish(); // 异或：顺序无关；任一文件任一属性变化都翻转其贡献
    }
    acc
}

/// 对比新旧指纹，返回发生变化（新增 / 删除 / 内容变 / 重命名）的插件 id。
fn changed_plugins(old: &Fingerprints, new: &Fingerprints) -> Vec<String> {
    let mut ids = Vec::new();
    for (id, h) in new {
        if old.get(id) != Some(h) {
            ids.push(id.clone()); // 新增或内容变化
        }
    }
    for id in old.keys() {
        if !new.contains_key(id) {
            ids.push(id.clone()); // 被删除
        }
    }
    ids
}

/// 目录变更处理：重扫插件 + 刷新搜索索引；仅当**当前打开的插件自身**变化时才重载其窗口。
fn on_change(app: &AppHandle, changed: &[String]) {
    let (Some(registry), Some(index), Some(settings)) = (
        app.try_state::<PluginRegistry>(),
        app.try_state::<SearchIndex>(),
        app.try_state::<SettingsStore>(),
    ) else {
        return;
    };

    // 1) 重扫清单 + 刷新可搜索命令（任一插件的新增 / 删除 / 改关键词 / 改版本都要即时反映）
    let cmds = registry.reload(&settings.get().disabled_plugins);
    let n = cmds.len();
    index.set_plugin_commands(cmds);
    ilog!(
        "[iTools] 插件目录变更 {:?} → 自动重载：{n} 条可搜索命令",
        changed
    );

    // 2) 只有「当前正在展示的插件」自己发生变化，才重载它的窗口：
    //    避免改 / 删别的插件时打断用户正在用的这个窗口（消除跨插件放大）。
    //    注：当前插件自身被改属开发者主动改代码，重载即其预期；重载会重置该页未持久化的输入
    //    （热更新固有行为），重载前重放 onEnter 让 code/query 恢复。
    let current = registry.current.lock().ok().and_then(|g| g.clone());
    if let Some(cur) = current {
        if changed.iter().any(|id| id == &cur) {
            if let Some(win) = app.get_webview_window("plugin") {
                registry.reseed_enter();
                if let Err(e) = win.eval("location.reload()") {
                    ilog!("[iTools] 刷新插件窗口失败：{e}");
                }
            }
        }
    }

    // 3) 通知管理中心「插件管理」页刷新（前端如已监听则列表 / 版本即时更新；未监听则无副作用）
    let _ = app.emit("plugins-changed", ());
}
