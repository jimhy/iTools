//! MCP 工具集：把开发者中心的能力暴露给 AI。
//!
//! # 每个工具都调既有实现，没有一个是为 MCP 新造的桩
//!
//! `list_plugins` 走 `DevRuntime::list`（与面板同一份数据，含逐字段的清单校验结论）、
//! `run_plugin` 走 `dev::commands::open_dev_window`（与点「运行」完全同一条路）、
//! `submit` 走 `dev::submit`（与点「提交审核」同一条路，含发布前自检与版本号拦截）。
//! 面板上做不到的事，这里也做不到；面板会拒绝的，这里同样会拒绝并给出同样的原文。
//!
//! # 返回什么给模型
//!
//! - 数据类工具返回**格式化 JSON**：模型解析准确，字段名与面板/文档一致；
//! - 文档类返回 **Markdown 原文**（规范本来就是给人读的，转成 JSON 只会丢结构）；
//! - 失败返回**后端的原话**（清单问题、自检阻断项、模型驳回理由），不改写、不概括——
//!   模型要靠这些原文决定下一步改什么。

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

use crate::dev::DevRuntime;
use crate::settings::SettingsStore;

/// 连上来之后先给模型看的总说明。**直接影响 AI 会不会用对这套工具**，
/// 所以这里写的是「怎么干活」的流程，而不是罗列有哪些函数。
pub const SERVER_INSTRUCTIONS: &str = r#"这是 iTools 插件开发环境的 MCP 服务器。iTools 是 Windows 上的键盘驱动效率启动器，
插件是**纯前端页面**（HTML/CSS/JS），通过 window.itools.* 调用宿主能力。

开始写插件之前，**先调 read_docs 读规范**（至少读 "开发规范" 那份）——插件的目录结构、
plugin.json 字段、可用的 window.itools API、权限模型都在里面，凭猜写出来的插件装不上。

典型工作流：
1. read_docs("开发规范")               了解怎么写
2. 在用户的调试目录里创建插件目录       （用你的文件工具，路径见 list_plugins 返回的 dirs）
3. rescan → list_plugins               确认被扫到，并检查 issues 里有没有 error
4. run_plugin                          打开调试窗口实际跑一次
5. read_logs                           看插件发出的调用、console 输出与报错
6. 反复改 → rescan → run_plugin → read_logs
7. **改 plugin.json 的 version**       已上线过的插件必须先升版本号（规则见下）
8. preflight                           发布前自检（会现查线上版本并比对版本号）
9. submit                              提交审核（服务端会跑机械校验 + 大模型读代码）
10. publish_status                     查审核结论；被驳回时里面有模型给出的逐条理由

════════ 版本号规则（最容易卡住的一步，请主动做，别等被拦） ════════

**iTools 不会自动升版本号。** 它只来自 plugin.json 的 version 字段，得你用文件工具改。

何时必须升：
- 只要 preflight / publish_status 里的 onlineVersion 不是 null（= 已上线过），
  提交前就必须把 version 改到**严格高于**它 —— 哪怕只改了一行代码。
- 首次提审（onlineVersion 为 null）没有这个约束，填什么都行（惯例 1.0.0）。

升哪一段（语义化版本 MAJOR.MINOR.PATCH）：
- 修 bug、改文案、调样式         → 升 PATCH：1.0.0 → 1.0.1
- 加新功能、加新 feature/关键字   → 升 MINOR：1.0.1 → 1.1.0
- 破坏性改动（删功能、改数据格式导致老数据读不了）→ 升 MAJOR：1.1.0 → 2.0.0
- 拿不准就升 PATCH。preflight 返回里的 suggestedVersion 就是「末段 +1」，可直接用。

硬性约束（违反会被拒或造成用户收不到更新）：
- 比较方式是**按 `.` 分段的数值比较**，缺失段补 0（`1.2` 等价于 `1.2.0`）。
- ❌ **不要用预发布后缀**（`-beta` / `-rc`）：解析不出数字的段按 0 处理，
  于是 `1.2.0-beta` 被读成 `1.2.0`（与正式版判定相等、互相收不到更新），
  而 `1.2.0-beta.1` 反而被读成 `1.2.0.1`、**高于**正式版 `1.2.0`。行为与 SemVer 规范相反。
- ❌ **不要靠降版本号回滚**：客户端只认「远端更高」，把 1.2.0 改回 1.1.9 既不会提示更新、
  也不会把用户拉回旧版，用户会一直卡在坏版本上。正确做法是**继续往上升**（发 1.2.1）。
- ⚠ 不升版本号的后果不只是提交被拒：客户端的更新检查**只比这一个值**，
  版本号没变的话，已经装了这个插件的用户永远收不到你的新版。

════════════════════════════════════════════════════════

其它要点：
- issues 里 level=error 的插件跑不起来，必须先修；level=warn 不影响调试但发布前要清。
- 改了插件文件后要 rescan，否则 list_plugins 拿到的还是旧清单。
- 提交审核需要用户已登录 iTools 云账号。
- 审核由大模型读代码判断（恶意行为、权限是否名副其实、描述是否相符），不是走过场。"#;

// ==================== 嵌入的规范文档 ====================
//
// 用 include_str! 编进二进制而不是运行时读文件：打包后的安装目录里没有 doc/，
// 从磁盘读会在用户机器上直接失败——而这套工具的第一步就是让 AI 读规范。

const DOC_DEV: &str = include_str!("../../../doc/插件系统/插件开发规范.md");
const DOC_CENTER: &str = include_str!("../../../doc/插件系统/开发者中心.md");
const DOC_DIST: &str = include_str!("../../../doc/插件系统/插件分发规范.md");

/// 文档 id → (展示名, 正文)。id 用中文是刻意的：模型看参数说明就知道该要哪份。
fn doc_of(name: &str) -> Option<(&'static str, &'static str)> {
    match name.trim() {
        "开发规范" | "plugin-dev" => Some(("插件开发规范", DOC_DEV)),
        "开发者中心" | "dev-center" => Some(("开发者中心（调试与提审）", DOC_CENTER)),
        "分发规范" | "distribution" => Some(("插件分发规范", DOC_DIST)),
        _ => None,
    }
}

const DOC_NAMES: &[&str] = &["开发规范", "开发者中心", "分发规范"];

// ==================== 工具清单 ====================

/// `tools/list` 的返回。每个工具的 description 直接影响模型会不会在对的时机调它，
/// 所以写的是「什么时候用、返回什么」，不是「这个函数叫什么」。
pub fn list() -> Vec<Value> {
    vec![
        json!({
            "name": "read_docs",
            "description": "读 iTools 插件开发规范。**写插件之前必读**——目录结构、plugin.json 字段、\
                可用的 window.itools API、权限模型都在里面。返回 Markdown 原文。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "doc": {
                        "type": "string",
                        "enum": DOC_NAMES,
                        "description": "要读哪一份：「开发规范」= 怎么写插件（最常用）；\
                            「开发者中心」= 调试环境与提审流程；「分发规范」= 上架与更新策略。"
                    }
                },
                "required": ["doc"]
            }
        }),
        json!({
            "name": "list_plugins",
            "description": "列出开发者中心里所有调试插件：id、目录、版本、清单校验结论（issues）、\
                features 与触发方式、是否可运行。返回里还有调试目录列表（新建插件就放在那里面）。\
                改过插件文件后请先调 rescan，否则拿到的是旧清单。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "rescan",
            "description": "重新扫描全部调试目录。新建/修改/删除插件文件后调它，再 list_plugins。",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "run_plugin",
            "description": "打开调试窗口实际运行一个插件（等同于在开发者中心点「运行」）。\
                会按你给的 code/query/kind 模拟一次触发，插件的 onEnter 会原样收到。\
                运行后用 read_logs 看它发出的调用与报错。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "插件 id（= 目录名，见 list_plugins）" },
                    "code": { "type": "string", "description": "要触发的 feature code" },
                    "query": { "type": "string", "description": "模拟用户输入的那串字（插件从 onEnter 的 info.query 拿到）" },
                    "kind": {
                        "type": "string",
                        "enum": ["keyword", "regex", "text", ""],
                        "description": "触发方式；留空则按清单自动判定"
                    }
                },
                "required": ["id", "code"]
            }
        }),
        json!({
            "name": "read_logs",
            "description": "读调试插件的运行日志：每一次 window.itools.* 调用（入参/返回/耗时/成败）、\
                console 输出、未捕获异常与资源加载失败。**排查「点了没反应」的首选**——\
                被后端拒绝的调用会带上拒绝原因。日志不落盘，重启即清零。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "afterSeq": { "type": "integer", "description": "只取序号大于它的（增量拉取用），默认 0 = 全部" },
                    "limit": { "type": "integer", "description": "最多返回多少条（默认 200，从最新往回取）" }
                }
            }
        }),
        json!({
            "name": "preflight",
            "description": "提交审核前的自检：缺 index.html、name 不合法、features 为空、\
                **版本号不高于已上线版本**等「服务端一定会拒」的项。\
                返回里带 onlineVersion 与 suggestedVersion —— 版本号被拦时把 plugin.json 的 \n                version 改成 suggestedVersion（或按语义化规则自己定一个更高的）再重试即可。\n                通过自检不代表会过审——代码审核在服务端的大模型那一侧。",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "插件 id" } },
                "required": ["id"]
            }
        }),
        json!({
            "name": "submit",
            "description": "把插件打包上传、提交审核。服务端先做机械校验，再由大模型通读代码\
                （恶意行为、申请的权限是否名副其实、描述是否与实际功能相符），通过即自动上线到插件市场。\
                **前置条件两条**：① 用户已登录 iTools 云账号；② 若该插件已上线过\n                （publish_status 的 onlineVersion 不为 null），必须先把 plugin.json 的 version \n                升到高于线上版本 —— iTools 不会替你升，没升会在打包前就被拦下。\n                提交后用 publish_status 查结论（要几十秒到几分钟）。",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "插件 id" } },
                "required": ["id"]
            }
        }),
        json!({
            "name": "publish_status",
            "description": "查一个插件的发布状态：本地版本、已上线版本、最近一次提审的结论与历史。\
                被驳回时 message 里是模型给出的逐条理由（照着改就行）。\
                状态含义：reviewing 审核中 / approved 已上线 / rejected 已驳回 / \
                manual 自动审核没做成需人工处理（**不是通过**）。",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "插件 id" } },
                "required": ["id"]
            }
        }),
    ]
}

/// `resources/list`：把三份规范也挂成资源，方便支持资源的客户端直接引用。
pub fn resources() -> Vec<Value> {
    DOC_NAMES
        .iter()
        .filter_map(|n| doc_of(n).map(|(title, _)| (n, title)))
        .map(|(n, title)| {
            json!({
                "uri": format!("itools://docs/{n}"),
                "name": title,
                "description": "iTools 插件开发规范（Markdown）",
                "mimeType": "text/markdown"
            })
        })
        .collect()
}

pub fn read_resource(uri: &str) -> Result<String, String> {
    let name = uri.strip_prefix("itools://docs/").unwrap_or("");
    doc_of(name)
        .map(|(_, body)| body.to_string())
        .ok_or_else(|| format!("没有这份文档：{uri}（可用：{}）", DOC_NAMES.join(" / ")))
}

// ==================== 工具调用 ====================

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// 把结构体格式化成模型好读的 JSON。序列化失败时如实报错而不是给个空对象。
fn pretty<T: serde::Serialize>(v: &T) -> Result<String, String> {
    serde_json::to_string_pretty(v).map_err(|e| format!("结果序列化失败: {e}"))
}

/// 调用一个工具。`Ok` = 成功（内容给模型），`Err` = 业务失败（原文同样给模型看）。
pub fn call(app: &AppHandle, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "read_docs" => {
            let want = arg_str(args, "doc");
            match doc_of(&want) {
                Some((title, body)) => Ok(format!("# {title}\n\n{body}")),
                None => Err(format!(
                    "没有名为「{want}」的文档。可用的是：{}",
                    DOC_NAMES.join(" / ")
                )),
            }
        }
        "list_plugins" => list_plugins(app),
        "rescan" => {
            let dev = app.state::<Arc<DevRuntime>>();
            let settings = app.state::<SettingsStore>();
            let n = dev.rescan(&settings.get().dev_plugin_dirs);
            crate::dev::refresh_search(app);
            Ok(format!("已重新扫描，当前共 {n} 个调试插件。用 list_plugins 看详情。"))
        }
        "run_plugin" => run_plugin(app, args),
        "read_logs" => read_logs(app, args),
        "preflight" => preflight(app, args),
        "submit" => submit(app, args),
        "publish_status" => publish_status(app, args),
        other => Err(format!("没有这个工具：{other}")),
    }
}

fn list_plugins(app: &AppHandle) -> Result<String, String> {
    let dev = app.state::<Arc<DevRuntime>>();
    let settings = app.state::<SettingsStore>();
    let s = settings.get();
    let plugins = dev.list(&s.dev_plugin_permissions);
    // 调试目录一起给：AI 要新建插件时得知道往哪个目录里放
    let dirs = dev.dirs();
    pretty(&json!({
        "plugins": plugins,
        "dirs": dirs,
        "hint": "新建插件请在 fixed=true 的那个目录下建一个子目录（目录名 = plugin.json 的 name），\
                 建好后调 rescan。issues 里 level=error 的插件跑不起来，必须先修。",
    }))
}

fn run_plugin(app: &AppHandle, args: &Value) -> Result<String, String> {
    let id = arg_str(args, "id");
    let code = arg_str(args, "code");
    if id.is_empty() || code.is_empty() {
        return Err("id 与 code 都必须给（见 list_plugins 的返回）".to_string());
    }
    let query = arg_str(args, "query");
    let kind = arg_str(args, "kind");
    let app2 = app.clone();
    // 与面板点「运行」完全同一条路：清单校验、runnable 判定、窗口复用都在里面
    tauri::async_runtime::block_on(crate::dev::commands::open_dev_window(
        app2,
        id.clone(),
        code.clone(),
        query,
        kind,
    ))?;
    Ok(format!(
        "已在调试窗口打开 {id}（feature={code}）。\
         用 read_logs 看它这次会话发出的调用与报错。"
    ))
}

fn read_logs(app: &AppHandle, args: &Value) -> Result<String, String> {
    let dev = app.state::<Arc<DevRuntime>>();
    let after = args.get("afterSeq").and_then(Value::as_u64).unwrap_or(0);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(200) as usize;
    let mut entries = dev.logs.list_after(after);
    // 从最新往回取：日志上限 2000 条，一次全塞给模型既超上下文又埋没重点
    let total = entries.len();
    if entries.len() > limit {
        entries = entries.split_off(total - limit);
    }
    pretty(&json!({
        "entries": entries,
        "returned": entries.len(),
        "totalAfterSeq": total,
        "hint": if total > entries.len() {
            "只返回了最新的一批；要继续往前翻请调小 afterSeq 或调大 limit。"
        } else {
            "这是 afterSeq 之后的全部日志。日志不落盘，重启 iTools 即清零。"
        },
    }))
}

/// 定位插件（与 dev 命令层同一套「找不到就说清楚」的行为）。
fn find_plugin(app: &AppHandle, id: &str) -> Result<crate::dev::DevPluginInfo, String> {
    let dev = app.state::<Arc<DevRuntime>>();
    let settings = app.state::<SettingsStore>();
    dev.list(&settings.get().dev_plugin_permissions)
        .into_iter()
        .find(|p| p.id == id)
        .ok_or_else(|| format!("调试插件「{id}」不在列表里。先调 rescan，再用 list_plugins 确认 id。"))
}

fn preflight(app: &AppHandle, args: &Value) -> Result<String, String> {
    let id = arg_str(args, "id");
    let info = find_plugin(app, &id)?;
    let dir = std::path::PathBuf::from(&info.dir);
    // 现查线上版本：版本号比对必须用最新值，否则会出现「自检放行、提交被拒」
    let online = crate::plugin::market::latest_version(&info.name).unwrap_or(None);
    let pre = crate::dev::submit::preflight(&dir, &info.name, &info.version, &info.issues, online.as_deref());
    // 版本号被拦时把「该改成多少」直接算给 AI —— 只讲规则不给值，它还得自己解析版本号
    let suggested = online.as_deref().map(bump_patch);
    let version_blocked = online
        .as_deref()
        .map(|o| !crate::plugin::install::version_gt(&info.version, o))
        .unwrap_or(false);
    pretty(&json!({
        "ok": pre.ok(),
        "blockers": pre.blockers,
        "warnings": pre.warnings,
        "localVersion": info.version,
        "onlineVersion": online,
        "suggestedVersion": suggested,
        "versionBlocked": version_blocked,
        "hint": if pre.ok() {
            "自检通过，可以调 submit 提交。注意：**通过自检不代表会过审**——\
             代码审核在服务端的大模型那一侧，本地看不出结论。".to_string()
        } else if version_blocked {
            format!(
                "版本号过不了：本地 v{} 不高于已上线的 v{}。请用文件工具把这个插件 plugin.json 里的 version 改成 {}（修 bug 升末段 / 加功能升中段 / 破坏性改动升首段），改完再调一次 preflight。",
                info.version,
                online.as_deref().unwrap_or("?"),
                suggested.as_deref().unwrap_or("?")
            )
        } else {
            "blockers 里每一条都会让服务端拒绝，必须先改掉。".to_string()
        },
    }))
}

/// 「末段 +1」的版本号建议（`1.0.0` → `1.0.1`）。
///
/// 只做最保守的那一档：升 minor / major 是**语义**判断（加了功能？破坏了兼容？），
/// 只有写代码的人知道，这里不替他猜——猜错会让用户收到一个语义错误的版本号。
fn bump_patch(online: &str) -> String {
    let trimmed = online.trim().trim_start_matches(['v', 'V']);
    let mut parts: Vec<String> = trimmed.split('.').map(str::to_string).collect();
    match parts.last().and_then(|p| p.trim().parse::<u64>().ok()) {
        Some(n) => {
            let last = parts.len() - 1;
            parts[last] = (n + 1).to_string();
            parts.join(".")
        }
        // 末段不是数字（如带了预发布后缀）：追加一段而不是瞎改，
        // 至少保证结果在「按 . 分段数值比较」下确实更大
        None => format!("{trimmed}.1"),
    }
}

fn submit(app: &AppHandle, args: &Value) -> Result<String, String> {
    let id = arg_str(args, "id");
    let info = find_plugin(app, &id)?;
    if !info.runnable {
        let why: Vec<String> = info
            .issues
            .iter()
            .filter(|i| i.level == "error")
            .map(|i| format!("{}：{}", i.field, i.message))
            .collect();
        return Err(format!("这个插件当前跑不起来，不能提交审核：\n{}", why.join("\n")));
    }
    let account = app.state::<crate::account::AccountStore>();
    let token = account
        .token()
        .ok_or_else(|| "提交审核需要用户先登录 iTools 云账号（管理中心 →「我的账号」）。".to_string())?;

    let dir = std::path::PathBuf::from(&info.dir);
    let online = crate::plugin::market::latest_version(&info.name).unwrap_or(None);
    let pre = crate::dev::submit::preflight(&dir, &info.name, &info.version, &info.issues, online.as_deref());
    if !pre.ok() {
        return Err(format!("提交前自检未通过，已中止：\n{}", pre.blockers.join("\n")));
    }
    let bytes = crate::dev::submit::pack_dir(&dir)?;
    let sub = crate::dev::submit::submit(&token, bytes)?;
    pretty(&json!({
        "submission": sub,
        "hint": "已提交，服务端正在审核（几十秒到几分钟）。用 publish_status 查结论。\
                 注意 status=manual 表示自动审核没做成、需要维护者人工处理，**它不是通过**。",
    }))
}

fn publish_status(app: &AppHandle, args: &Value) -> Result<String, String> {
    let id = arg_str(args, "id");
    let info = find_plugin(app, &id)?;
    let account = app.state::<crate::account::AccountStore>();
    let token = account.token().unwrap_or_default();

    let mut errors: Vec<String> = Vec::new();
    let (online, revoked, revoked_reason) = match crate::plugin::market::fetch_index() {
        Ok(index) => match index.plugins.into_iter().find(|p| p.name == info.name) {
            Some(e) => (Some(e.version), e.revoked, e.revoked_reason),
            None => (None, false, String::new()),
        },
        Err(e) => {
            errors.push(format!("读取市场索引失败：{e}"));
            (None, false, String::new())
        }
    };
    let history = match crate::dev::submit::list_submissions(&token) {
        Ok(list) => list.into_iter().filter(|s| s.name == info.name).collect::<Vec<_>>(),
        Err(e) => {
            errors.push(e);
            Vec::new()
        }
    };
    pretty(&json!({
        "name": info.name,
        "localVersion": info.version,
        "onlineVersion": online,
        "revoked": revoked,
        "revokedReason": revoked_reason,
        "latest": history.first(),
        "history": history,
        // 查不到就说查不到，绝不用「没有记录」把失败盖过去
        "error": (!errors.is_empty()).then(|| errors.join("\n")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_are_embedded_not_read_from_disk() {
        // 打包后的安装目录没有 doc/，从磁盘读会在用户机器上直接失败
        for n in DOC_NAMES {
            let (title, body) = doc_of(n).expect("三份文档都该能取到");
            assert!(!title.is_empty());
            assert!(body.len() > 1000, "{n} 看起来没被正确嵌入（只有 {} 字节）", body.len());
        }
    }

    #[test]
    fn docs_accept_english_aliases() {
        assert!(doc_of("plugin-dev").is_some());
        assert!(doc_of("dev-center").is_some());
        assert!(doc_of("distribution").is_some());
        assert!(doc_of("不存在的文档").is_none());
    }

    #[test]
    fn resource_uri_roundtrip() {
        for r in resources() {
            let uri = r["uri"].as_str().unwrap();
            assert!(read_resource(uri).is_ok(), "{uri} 应该读得到");
        }
        assert!(read_resource("itools://docs/瞎写的").is_err());
    }

    #[test]
    fn tool_list_schemas_are_wellformed() {
        let tools = list();
        assert!(tools.len() >= 8, "工具数量不对");
        for t in &tools {
            assert!(t["name"].is_string(), "每个工具都要有 name");
            let d = t["description"].as_str().unwrap_or("");
            assert!(d.len() > 20, "{} 的说明太短，模型会用不对时机", t["name"]);
            assert!(t["inputSchema"]["type"] == json!("object"), "inputSchema 必须是 object");
        }
    }

    #[test]
    fn bump_patch_suggests_a_strictly_higher_version() {
        // 「只讲规则不给值」等于让 AI 自己解析版本号，多一个出错的地方
        assert_eq!(bump_patch("1.0.0"), "1.0.1");
        assert_eq!(bump_patch("v2.3.9"), "2.3.10");
        assert_eq!(bump_patch("1.2"), "1.3");
        assert_eq!(bump_patch("1.0.0-beta"), "1.0.0-beta.1");
        // 建议值必须真的高于线上，否则给了也白给
        for v in ["1.0.0", "v2.3.9", "1.2", "1.0.0-beta"] {
            assert!(
                crate::plugin::install::version_gt(&bump_patch(v), v),
                "{v} 的建议版本必须严格更高"
            );
        }
    }

    #[test]
    fn instructions_state_the_version_rules() {
        // 只说「会被拦下」不够，要写清楚**规则**：什么时候升、升哪一段、什么不能干
        for must in ["不会自动升版本号", "PATCH", "MINOR", "MAJOR", "预发布后缀", "降版本号回滚"] {
            assert!(SERVER_INSTRUCTIONS.contains(must), "版本号规则里缺了「{must}」");
        }
    }

    #[test]
    fn instructions_tell_ai_to_read_docs_first() {
        // 这句是整套工具能不能被用对的关键：不先读规范，AI 写出来的插件装不上
        assert!(SERVER_INSTRUCTIONS.contains("read_docs"));
        assert!(SERVER_INSTRUCTIONS.contains("规范"));
    }
}
