//! **搜索结果注入**：让插件把自己的结果直接推进主搜索框。
//!
//! # 为什么这条机制是启动器的分界线
//!
//! 在此之前，插件只能「被关键字唤起 → 打开一个面板」。用户得先想起插件叫什么、敲对关键字，
//! 才能用上它。有了注入，插件可以在用户**边打字**的时候就把结果摆到搜索框里——
//! 剪贴板历史、书签、待办、翻译，都能像内置功能一样直接出现在结果列表里。
//! 这是「一堆小面板」和「平台」的区别。
//!
//! # 怎么走通的
//!
//! 搜索是同步的（`search` 命令返回一个 Vec），而插件在别的 webview 里，只能异步往返。
//! 所以注入走一条**独立的旁路**：
//!
//! 1. feature 在 `plugin.json` 里声明 `mainPush: true`；
//! 2. 这类插件由宿主**自动后台常驻**（不需要用户开自启动开关）——它必须活着才能回应查询，
//!    要求用户先手动开一次开关等于让这个功能默认失效；
//! 3. 前端每次搜索时，除了照常调 `search`，再并发调一次 `search_push`；
//! 4. `search_push` 把 query 推给所有已注册的插件，**限时**收集回应，超时的直接丢掉；
//! 5. 前端把拿到的结果追加到列表尾部。
//!
//! # 为什么要限时而且时间给得很短
//!
//! 主搜索是逐键触发的，任何一个插件卡住都会让整个搜索框停顿。这里的取舍很明确：
//! **宁可丢掉慢插件的结果，也不能让搜索框卡一下**。所以超时只给 [`PUSH_TIMEOUT`]，
//! 到点就返回已经收到的部分——这不是"失败"，是设计上的取舍，插件文档里要讲清楚。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

use super::commands::caller_session;
use super::PluginRegistry;
use crate::search::SearchItem;

/// 收集插件回应的时限。逐键搜索容不下更长的等待——见模块文档。
const PUSH_TIMEOUT: Duration = Duration::from_millis(250);

/// 单个插件单次回应的条数上限。不限的话一个插件能把整个结果列表刷屏。
const MAX_ITEMS_PER_PLUGIN: usize = 8;

/// 已就绪的注入插件：插件 id 集合。插件页调 `registerMainPush` 后才进这个集合——
/// 声明了 `mainPush` 但页面还没加载完时不该被计入，否则每次搜索都白等一次超时。
fn ready() -> &'static Mutex<Vec<String>> {
    static R: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// 等待中的一轮收集。
struct Round {
    slots: Mutex<HashMap<String, Vec<SearchItem>>>,
    cv: Condvar,
    expect: usize,
}

fn rounds() -> &'static Mutex<HashMap<String, Arc<Round>>> {
    static P: OnceLock<Mutex<HashMap<String, Arc<Round>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 插件页声明「我准备好接收搜索查询了」。
#[tauri::command]
pub fn plugin_register_main_push(
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if session.dev {
        return Err("调试会话暂不支持搜索结果注入".to_string());
    }
    if !registry.declares_main_push(&session.id) {
        return Err(format!(
            "插件 {} 没有任何 feature 声明 mainPush，不能注册搜索结果注入",
            session.id
        ));
    }
    if let Ok(mut g) = ready().lock() {
        if !g.iter().any(|id| id == &session.id) {
            g.push(session.id.clone());
        }
    }
    Ok(())
}

/// 插件页回传本轮的结果。
#[tauri::command]
pub fn plugin_main_push_result(
    round_id: String,
    items: Vec<PushItem>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    let round = {
        let g = rounds().lock().map_err(|_| "收集表加锁失败".to_string())?;
        g.get(&round_id).cloned()
    };
    // 拿不到多半是这一轮已经超时结束了。晚到不算错——用户早已在看下一次查询的结果。
    let Some(round) = round else {
        return Ok(());
    };
    let converted: Vec<SearchItem> = items
        .into_iter()
        .take(MAX_ITEMS_PER_PLUGIN)
        .map(|it| it.into_search_item(&session.id))
        .collect();
    if let Ok(mut slots) = round.slots.lock() {
        slots.insert(session.id.clone(), converted);
    }
    round.cv.notify_all();
    Ok(())
}

/// 插件回传的一条结果。刻意做得比 `SearchItem` 窄：插件不能自己指定 `action`、`kind`，
/// 那些由宿主填死，免得插件伪装成「应用」或「文件」骗取用户点击。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushItem {
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    /// 选中这条时回传给插件的载荷（经 `onEnter` 的 `info.query`）。
    #[serde(default)]
    pub payload: String,
    /// 命中的 feature code；空则用插件的第一个 feature。
    #[serde(default)]
    pub code: String,
    /// base64 data URL 图标；不给则用插件 logo。
    pub icon: Option<String>,
}

impl PushItem {
    fn into_search_item(self, plugin_id: &str) -> SearchItem {
        SearchItem {
            id: format!("push::{plugin_id}::{}", self.title),
            title: self.title,
            // 副标题统一带上来源插件：用户必须能看出这条结果是谁给的，
            // 否则注入就成了「不知道从哪冒出来的条目」，既困惑也不安全。
            subtitle: if self.subtitle.is_empty() {
                format!("{plugin_id} · 插件")
            } else {
                format!("{} · {plugin_id}", self.subtitle)
            },
            kind: "plugin".to_string(),
            // payload 编码进 target：SearchItem 是被契约钉死的结构（见 contract.rs），
            // 为一条旁路给它加字段要动前后端一整圈。这里用 U+0001 作分隔符附在后面，
            // 由 open_plugin_inner 解出来当 query 交给插件——插件 id 与 code 的字符集
            // （安装校验保证：小写字母数字连字符）里不可能出现控制字符，不会撞。
            target: if self.payload.is_empty() {
                format!("{plugin_id}#{}", self.code)
            } else {
                format!("{plugin_id}#{}\u{1}{}", self.code, self.payload)
            },
            icon: self.icon,
            action: "plugin".to_string(),
        }
    }
}

/// 向所有已就绪的注入插件发起一轮查询，限时收集结果。
///
/// 前端每次搜索时与 `search` 并发调用；拿到多少算多少，不会因为某个插件慢而拖住搜索框。
#[tauri::command]
pub async fn search_push(query: String, app: AppHandle) -> Vec<SearchItem> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let targets: Vec<String> = ready().lock().map(|g| g.clone()).unwrap_or_default();
    if targets.is_empty() {
        return Vec::new();
    }
    let round_id = new_round_id();
    let round = Arc::new(Round {
        slots: Mutex::new(HashMap::new()),
        cv: Condvar::new(),
        expect: targets.len(),
    });
    if let Ok(mut g) = rounds().lock() {
        g.insert(round_id.clone(), round.clone());
    }

    // 推给每个插件的后台窗口
    let payload = json!({ "roundId": round_id, "query": query });
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('main-push', {payload})");
    let mut sent = 0usize;
    for id in &targets {
        if let Some(win) = app.get_webview_window(&super::commands::bg_label(id)) {
            if win.eval(&js).is_ok() {
                sent += 1;
            }
        }
    }
    if sent == 0 {
        if let Ok(mut g) = rounds().lock() {
            g.remove(&round_id);
        }
        return Vec::new();
    }

    // 限时收集：够数就提前返回，到点就带着已收到的部分走
    let collected = tauri::async_runtime::spawn_blocking({
        let round = round.clone();
        move || -> Vec<SearchItem> {
            let Ok(mut slots) = round.slots.lock() else {
                return Vec::new();
            };
            let deadline = Instant::now() + PUSH_TIMEOUT;
            while slots.len() < round.expect {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match round.cv.wait_timeout(slots, left) {
                    Ok((s, _)) => slots = s,
                    Err(_) => return Vec::new(),
                }
            }
            slots.values().flatten().cloned().collect()
        }
    })
    .await
    .unwrap_or_default();

    if let Ok(mut g) = rounds().lock() {
        g.remove(&round_id);
    }
    collected
}

/// 插件被禁用 / 卸载 / 后台实例关闭时，把它从就绪集合里摘掉。
pub fn unregister(plugin_id: &str) {
    if let Ok(mut g) = ready().lock() {
        g.retain(|id| id != plugin_id);
    }
}

fn new_round_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{ns:x}-{n:x}")
}

/// 供 `Value` 类型推导用（`json!` 宏需要它在作用域内）。
#[allow(dead_code)]
fn _unused(_: Value) {}
