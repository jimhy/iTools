//! **把插件的能力暴露成 MCP 工具**，让 Claude Code / Cursor 这类外部 AI 直接调用。
//!
//! # 这是 iTools 独有的一条路
//!
//! 同类产品的做法是「插件把工具注册给自家的 AI 助手」——AI 在产品内部，工具也在内部。
//! iTools 本身已经是一个 MCP server（见 `src/mcp/`），所以方向反过来：**插件写好的能力，
//! 自动成为任何外部 AI agent 可调用的工具**。用户在 Claude Code 里说一句「把这个视频转成 gif」，
//! AI 就能调到用户装在 iTools 里的那个插件。
//!
//! # 调用是怎么走通的
//!
//! MCP 的 `tools/call` 是同步请求-响应，而插件跑在另一个 webview 里，只能异步往返。所以：
//!
//! 1. 插件在 `plugin.json` 里声明 `tools`，并在页面初始化时 `itools.registerTool(name, handler)`；
//! 2. 外部 AI 调 `tools/call` → 宿主找到该插件；**没在运行就先把它后台拉起来**
//!    （用户不必事先打开插件，否则「AI 能不能调到」取决于用户刚好开着哪个插件，形同虚设）；
//! 3. 宿主推 `tool-call` 事件给插件页，带一个 `requestId`；
//! 4. 插件执行完调 `plugin_tool_result(requestId, ...)` 回传；
//! 5. 宿主这边一直在等这个 `requestId`，拿到就返回给 AI；超时则如实报超时。
//!
//! # 为什么 registerTool 不能写在 onEnter 里
//!
//! AI 调用时插件是被**后台拉起**的，走的是 `type === "background"` 那条路径；如果注册代码写在
//! 某个特定触发分支里就不会执行，AI 拿到的永远是「工具未注册」。这一点必须在作者文档里讲清楚。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

use super::commands::caller_session;
use super::PluginRegistry;

/// 单次工具调用的最长等待。插件可能在做重活（转码、下载），给得宽一些；
/// 但不能没有上限——插件崩了或忘了回传结果，MCP 那头会一直挂着。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// 已注册的工具：`插件id` → (`工具名` → 该工具的声明)。
///
/// 声明来自 `plugin.json`，注册动作来自插件页调 `registerTool`——**两者都要有**才算数：
/// 只声明不注册，调用时没人处理；只注册不声明，外部 AI 根本看不见它。
type ToolTable = HashMap<String, HashMap<String, Value>>;

fn tools() -> &'static Mutex<ToolTable> {
    static T: OnceLock<Mutex<ToolTable>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 等待中的调用：`requestId` → 结果槽。
struct Pending {
    slot: Mutex<Option<Result<String, String>>>,
    cv: Condvar,
}

fn pendings() -> &'static Mutex<HashMap<String, Arc<Pending>>> {
    static P: OnceLock<Mutex<HashMap<String, Arc<Pending>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 插件页调它把某个工具标记为「已就绪」。
///
/// `name` 必须与 `plugin.json` 的 `tools` 里某个键一致，否则拒绝——避免插件注册一个
/// 外部根本看不见的工具，然后疑惑为什么 AI 不调它。
#[tauri::command]
pub fn plugin_register_tool(
    name: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    if session.dev {
        return Err("调试会话暂不支持注册 MCP 工具".to_string());
    }
    let declared = registry
        .tool_decl(&session.id, &name)
        .ok_or_else(|| format!("plugin.json 的 tools 里没有声明工具「{name}」，无法注册"))?;
    let mut g = tools().lock().map_err(|_| "工具表加锁失败".to_string())?;
    g.entry(session.id.clone()).or_default().insert(name, declared);
    Ok(())
}

/// 插件页执行完工具后回传结果。
#[tauri::command]
pub fn plugin_tool_result(
    request_id: String,
    result: Option<String>,
    error: Option<String>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
) -> Result<(), String> {
    // 校验调用者身份：不做的话任意插件都能替别人的调用塞一个假结果
    let _session = caller_session(&webview, &registry)?;
    let pending = {
        let g = pendings().lock().map_err(|_| "等待表加锁失败".to_string())?;
        g.get(&request_id).cloned()
    };
    let Some(pending) = pending else {
        // 多半是已经超时被清掉了。不当错误处理——插件晚回来一步不该收到一个吓人的报错。
        return Ok(());
    };
    if let Ok(mut slot) = pending.slot.lock() {
        *slot = Some(match error {
            Some(e) => Err(e),
            None => Ok(result.unwrap_or_default()),
        });
    }
    pending.cv.notify_all();
    Ok(())
}

/// 供 MCP 层列出所有插件贡献的工具（工具名加 `plugin_` 前缀避免与宿主自带工具重名）。
pub fn list_plugin_tools() -> Vec<Value> {
    let Ok(g) = tools().lock() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (plugin_id, map) in g.iter() {
        for (name, decl) in map.iter() {
            let desc = decl
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("（插件未提供说明）");
            out.push(json!({
                "name": format!("plugin_{plugin_id}_{name}"),
                "description": format!("[插件 {plugin_id}] {desc}"),
                "inputSchema": decl.get("inputSchema").cloned().unwrap_or(json!({"type":"object"})),
            }));
        }
    }
    out
}

/// MCP 层拿到 `plugin_<id>_<tool>` 形态的调用名时走这里。返回 `None` 表示不是插件工具。
pub fn try_call(app: &AppHandle, full_name: &str, args: &Value) -> Option<Result<String, String>> {
    let rest = full_name.strip_prefix("plugin_")?;
    // 插件 id 里允许连字符但不允许下划线（安装校验保证），所以按第一个 `_` 切分是安全的
    let (plugin_id, tool) = rest.split_once('_')?;
    let registered = tools()
        .lock()
        .ok()
        .is_some_and(|g| g.get(plugin_id).is_some_and(|m| m.contains_key(tool)));
    if !registered {
        return None;
    }
    Some(call_plugin_tool(app, plugin_id, tool, args))
}

/// 真正发起一次调用并同步等待结果。
fn call_plugin_tool(
    app: &AppHandle,
    plugin_id: &str,
    tool: &str,
    args: &Value,
) -> Result<String, String> {
    // 插件没在跑就先后台拉起来。不这么做的话，AI 能不能调到工具取决于用户此刻恰好开着哪个插件。
    let label = super::commands::bg_label(plugin_id);
    if app.get_webview_window(&label).is_none() {
        let handle = app.clone();
        let id = plugin_id.to_string();
        tauri::async_runtime::block_on(async move {
            let _ = super::commands::open_plugin_background(handle, id).await;
        });
        // 给页面一点加载并注册工具的时间；轮询而不是死等固定时长，就绪就走
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let ready = tools()
                .lock()
                .ok()
                .is_some_and(|g| g.get(plugin_id).is_some_and(|m| m.contains_key(tool)));
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let Some(win) = app.get_webview_window(&label) else {
        return Err(format!("插件 {plugin_id} 无法启动，工具不可用"));
    };

    let request_id = new_request_id();
    let pending = Arc::new(Pending {
        slot: Mutex::new(None),
        cv: Condvar::new(),
    });
    if let Ok(mut g) = pendings().lock() {
        g.insert(request_id.clone(), pending.clone());
    }

    let payload = json!({ "requestId": request_id, "name": tool, "params": args });
    let js = format!("window.__itoolsEmit && window.__itoolsEmit('tool-call', {payload})");
    if win.eval(&js).is_err() {
        if let Ok(mut g) = pendings().lock() {
            g.remove(&request_id);
        }
        return Err("无法把调用投递给插件页".to_string());
    }

    // 等结果。用 Condvar 而不是轮询：工具可能几毫秒就返回，也可能跑两分钟。
    let outcome = {
        let mut slot = pending
            .slot
            .lock()
            .map_err(|_| "结果槽加锁失败".to_string())?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        while slot.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let (s, _) = pending
                .cv
                .wait_timeout(slot, left)
                .map_err(|_| "等待结果失败".to_string())?;
            slot = s;
        }
        slot.take()
    };
    if let Ok(mut g) = pendings().lock() {
        g.remove(&request_id);
    }
    outcome.unwrap_or_else(|| {
        Err(format!(
            "插件 {plugin_id} 的工具「{tool}」在 {} 秒内没有返回结果",
            CALL_TIMEOUT.as_secs()
        ))
    })
}

/// 插件被禁用 / 卸载 / 后台实例关闭时，把它注册的工具摘掉。
///
/// 不摘的话外部 AI 仍能看到这些工具，调用时才发现插件根本不在——那是一次白白的往返和超时。
pub fn unregister_all(plugin_id: &str) {
    if let Ok(mut g) = tools().lock() {
        g.remove(plugin_id);
    }
}

/// 请求 id：进程内计数器 + 纳秒时间戳。只需在本进程内唯一，不作安全凭据用。
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{ns:x}-{n:x}")
}
