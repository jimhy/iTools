//! 给插件开放**按插件隔离**的 SQLite 数据库，弥补 `db`/`data`（KV）存储在数据量大时的短板。
//!
//! ## 隔离模型
//! 每个插件会话（[`ActiveSession`]：插件 id + 是否调试）通过 [`session_files_dir`] 定位到
//! 自己的沙盒文件根（正式 `plugin-data/<id>/files`，调试走 dev 沙盒同构路径），本模块取其
//! 父目录（即该会话专属的 `<...>/<id>/` 目录）后接 `sqlite/`，库文件落在
//! `plugin-data/<插件id>/sqlite/<name>.db`（调试会话落在 dev 沙盒同构路径下）——
//! 复用宿主已经做好的 dev/prod 分流逻辑，不在本文件里重新拼一遍 `data_root()`，
//! 避免两处路径逻辑长期漂移不一致。
//!
//! ## 句柄归属
//! `plugin_sqlite_open` 返回的句柄只登记在 [`SqliteState`] 的一张全局表里，键是**发起调用的
//! 窗口当前会话**（[`caller_session`] 取到的 [`ActiveSession`]，比对用 `same_as`——同时比 id
//! 与 `dev` 标志，不能只比 id：调试插件与同名正式插件共用一个 id 时，只比 id 会让调试窗
//! 打开的句柄被正式窗当作自己的用，或反过来）。任何 exec/query/batch/close 都先按这套身份
//! 校验句柄归属，越权访问一律拒绝。
//!
//! ## 为什么必须禁用 `ATTACH DATABASE`（本模块最核心的红线）
//! SQLite 的 `ATTACH DATABASE '<path>' AS <alias>` 可以把**任意其它文件**（包括其它插件的
//! `plugin-data/<别的插件id>/sqlite/*.db`，甚至沙盒外的任意路径）挂到当前连接上，之后就能用
//! 普通 `SELECT`/`INSERT` 跨库读写——这会彻底打穿"插件数据互相隔离"这条项目红线：A 插件只要
//! 猜得到/枚举得到别的插件 id，就能读走别人的数据。**因此 ATTACH（以及配套的 DETACH）必须
//! 被彻底挡住，而且要挡得足够可靠。**
//!
//! ## 双层防御：为什么 authorizer 和文本闸门都要留
//! - **核心层**（[`authorize`]，装在 [`Connection::authorizer`] 上）：SQLite 在**每条语句
//!   prepare 时**为其中每个动作单独回调这个函数，直接由 SQLite 自己判定"这条语句到底在做
//!   什么"，不依赖我们自己对 SQL 文本的解析是否到位——是最权威、最难绕过的一道关卡，
//!   `ATTACH`/`DETACH` 在这一层被硬性 `Deny`。`rusqlite` 的 `authorizer` API 挂在
//!   `#[cfg(feature = "hooks")]` 后面，`Cargo.toml` 已加上 `hooks` 特性（主控加的，本文件
//!   未改 `Cargo.toml`）。
//! - **文本层**（[`check_sql_allowed`] + [`require_single_statement`]）：在语句交给 SQLite
//!   之前，对去除注释/空白后的首个关键字做拒绝名单校验，并强制"一次调用只喂一条语句"。
//!   **不能删**，原因不是"怕 authorizer 装错"，而是有真实的**authorizer 覆盖不到的空子**：
//!   `VACUUM INTO '<库外路径>'` 会把整份数据库复制到任意文件，但 SQLite 的授权回调**根本
//!   没有 `VACUUM` 这个动作码**（翻过 rusqlite `src/hooks/mod.rs` 里 `AuthAction` 的完整变体
//!   列表，没有 `Vacuum` 或类似条目——`VACUUM`/`VACUUM INTO` 在执行期间只触发普通的
//!   `Read`/`Insert` 之类回调，authorizer 拿不到"这是一次 VACUUM"这个信号，也就无法单独
//!   拦截它）。所以 `VACUUM` 的拦截**只能靠文本层**，这也是两层缺一不可的直接证据。
//!   反过来，文本层给出的是更友好的中文报错，且能在语句连 prepare 都不用走就直接拒绝。
//!
//! ## authorizer 具体挡了什么（见 [`authorize`] 的实现，逐条对应 `AuthAction` 变体）
//! - `Attach`/`Detach`：硬性拒绝——本模块的核心红线。
//! - `CreateVtable`/`DropVtable`：硬性拒绝（项目没开 `vtab`/`csvtab`/`fts5` 等 rusqlite 特性，
//!   插件也用不上虚拟表；显式拒绝而不是依赖"这些模块反正没编译进去"的运气）。
//! - `Pragma`：白名单放行只读自省类（`table_info`/`table_xinfo`/`index_list`/`index_info`/
//!   `index_xinfo`/`foreign_key_list`/`database_list`/`collation_list`），其余一律拒绝——
//!   包括能影响文件落点/库行为的 `journal_mode`/`temp_store_directory`/`data_store_directory`/
//!   `writable_schema`/`legacy_file_format` 等，以及任何未在白名单里的、我没考虑到的 PRAGMA
//!   （默认拒绝，不是默认放行）。宿主自己在 [`open_and_init`] 里设 `journal_mode`/
//!   `synchronous`/`foreign_keys` 用的是 rusqlite 类型化 API，**在 authorizer 装上之前**
//!   执行，不受这道插件闸门约束（见该函数注释）。
//! - `Function`：一般 SQL 函数放行，但 `load_extension`/`fts3_tokenizer` 这两个历史上出过
//!   安全问题的函数名显式拒绝——即便项目没开 `load_extension` cargo 特性、`sqlite3_load_extension`
//!   相关代码理论上没编译进这个二进制、调用会在更底层直接失败，这里仍多做一层显式拦截，
//!   不依赖"某个可选特性没开"这个前提是否永远成立。
//! - 其余常规 DML/DDL/事务/自省读取（`Select`/`Read`/`Insert`/`Update`/`Delete`/建表建索引
//!   建视图建触发器及其 Drop、`AlterTable`/`Reindex`/`Analyze`/`Transaction`/`Savepoint`/
//!   `Recursive`）：插件在**自己的库内**合法需要的操作，放行。
//! - **任何未识别的/未来 SQLite 新增的动作码**（含 `Unknown` 变体）：`match` 落到通配分支
//!   一律 `Deny`——fail-safe 默认拒绝，不是"不认识就放行"。
//!
//! ## 如实说明：authorizer 挡不住 / 挡不干净的地方
//! - **`VACUUM INTO`**：如上，authorizer 没有对应动作码，完全依赖文本层的 `VACUUM` 拒绝；
//!   如果文本层被绕过（目前没找到绕过路径，但如实指出这是单点），VACUUM 就没有第二道防线。
//! - **`ATTACH` 是否 100% 无法以任何形式生效**：`authorize` 对 `Attach{..}` 一律 `Deny`，
//!   这是 SQLite 官方文档明确保证的"authorizer 返回 DENY 时该动作导致 prepare 直接失败"的
//!   标准行为；但本模块没有、也没办法在不引入额外测试基础设施的前提下针对"SQLite 自身实现
//!   缺陷"做验证——这里的可靠性边界是"信任 SQLite 官方对 authorizer 语义的实现"，这一点
//!   如实标注。
//! - **`.backup` / `.dump` 等 sqlite3 CLI 点命令**：不是 SQL 语法，rusqlite 的 `prepare`/
//!   `execute` 根本不认识这种文本，插件没有任何 API 能碰到真正的 `sqlite3_backup_init` C 接口，
//!   两层闸门都用不上、也不需要。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tauri::State;

use super::commands::{caller_session, session_files_dir};
use super::{ActiveSession, PluginRegistry};

// ==================== 运行期状态：句柄表 ====================

/// 一个已打开的插件 SQLite 连接：记住是谁打开的（归属校验用），连接本身用 `Arc<Mutex<_>>`
/// 包一层，方便把克隆的引用移进 `spawn_blocking` 的阻塞线程而不必持有整张句柄表的锁。
struct SqliteHandle {
    /// 打开这个句柄的会话（插件 id + dev 标志），后续每次访问都要与调用者当前会话比对。
    owner: ActiveSession,
    conn: Arc<Mutex<Connection>>,
}

/// 插件 SQLite 句柄注册表（managed state）。全部插件共用一张表，靠 [`SqliteHandle::owner`]
/// 做归属隔离——句柄本身不含任何权限信息，纯粹是"哪个会话打开的哪个连接"。
#[derive(Default)]
pub struct SqliteState {
    handles: Mutex<HashMap<String, SqliteHandle>>,
    counter: AtomicU64,
}

impl SqliteState {
    /// 取某句柄的连接引用，同时校验调用者会话与打开者会话**完全一致**（id + dev）。
    fn conn_for(&self, session: &ActiveSession, handle: &str) -> Result<Arc<Mutex<Connection>>, String> {
        let handles = self
            .handles
            .lock()
            .map_err(|_| "句柄表锁获取失败".to_string())?;
        match handles.get(handle) {
            Some(h) if h.owner.same_as(session) => Ok(h.conn.clone()),
            Some(_) => Err("该数据库句柄不属于当前插件".to_string()),
            None => Err("数据库句柄不存在或已关闭".to_string()),
        }
    }
}

// ==================== 命令参数 ====================

/// [`plugin_sqlite_batch`] 里的一条语句：SQL 文本 + 可选的 `?` 占位参数。
#[derive(Debug, Deserialize)]
// 入参字段统一用 camelCase：这些结构体是**插件页直接传进来的 JSON**，JS 侧的自然写法就是
// camelCase，且 Tauri 对命令顶层参数本来就做 camelCase 转换。若这里不标，插件传 `maxDepth`
// 会因为对不上 `max_depth` 而被静默忽略、退回默认值——不报错、不生效，最难查的那类问题。
#[serde(rename_all = "camelCase")]
pub struct SqliteStatement {
    /// SQL 文本（必须是单条语句，用 `?` 占位符绑定参数，禁止插件自己拼接参数值进 SQL 字符串）。
    pub sql: String,
    /// 按顺序绑定到 `sql` 里 `?` 占位符的参数；省略视为无参数。
    #[serde(default)]
    pub params: Vec<JsonValue>,
}

// ==================== 库名校验与路径定位 ====================

/// Windows 保留设备名（不区分大小写、无视扩展名——`CON.db` 在 Windows 上仍指向 CON 设备）。
const RESERVED_WIN_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 严格校验插件传入的库名：只允许 `[A-Za-z0-9_-]`，长度 1~64，且不是 Windows 保留设备名。
///
/// 不允许出现 `/`、`\`、`:`、`.`（自然也就不可能拼出 `..` 或盘符），杜绝插件借库名跳出
/// `plugin-data/<自己id>/sqlite/` 这个目录、把库建到别的插件目录或任意磁盘位置。
fn validate_db_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 64 {
        return Err("数据库名称不能为空，且不超过 64 个字符".to_string());
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(
            "数据库名称只能包含字母、数字、下划线、短横线（不允许路径分隔符/盘符/\"..\"）"
                .to_string(),
        );
    }
    if RESERVED_WIN_NAMES.iter().any(|r| r.eq_ignore_ascii_case(n)) {
        return Err(format!("\"{n}\" 是 Windows 保留设备名，不能用作数据库名称"));
    }
    Ok(())
}

/// 定位某会话专属的 SQLite 库目录：`plugin-data/<id>/sqlite/`（正式）或调试沙盒同构路径。
///
/// 直接复用 [`session_files_dir`] 已经做好的 dev/prod 分流（它给出的是同一 `<id>` 目录下的
/// `files` 子目录），取其父目录后接 `sqlite`，与宿主既有的隔离逻辑保持同一套真相来源。
fn sqlite_dir(session: &ActiveSession, registry: &PluginRegistry) -> Result<PathBuf, String> {
    let files_dir = session_files_dir(session, registry);
    let base = files_dir
        .parent()
        .ok_or_else(|| "插件数据目录解析失败".to_string())?;
    Ok(base.join("sqlite"))
}

// ==================== SQL 安全闸门 ====================

/// 跳过开头的空白与 SQL 注释（`--...` 行注释 / `/* ... */` 块注释），返回剩余部分。
/// 用于在做"首关键字"判定前，先排除"关键字前面挂了个注释"这种花招。
fn skip_ws_and_comments(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            match rest.find('\n') {
                Some(pos) => {
                    s = &rest[pos + 1..];
                    continue;
                }
                None => return "",
            }
        } else if let Some(rest) = t.strip_prefix("/*") {
            match rest.find("*/") {
                Some(pos) => {
                    s = &rest[pos + 2..];
                    continue;
                }
                None => return "",
            }
        } else {
            return t;
        }
    }
}

/// 把一段 SQL 文本按**顶层**（不在引号字符串/方括号标识符/注释内）的 `;` 切成多条语句，
/// 各段已 `trim`、空段被丢弃。用于识别"插件是不是想在一次调用里塞进多条语句"。
///
/// 只针对 ASCII 分隔符（`'`/`"`/`` ` ``/`[`/`]`/`-`/`/`/`;`）做状态判断，其余字节（含所有
/// UTF-8 多字节字符的后续字节）一律原样跳过——这些分隔符全部是单字节 ASCII，在其上切片
/// 不会切碎多字节 UTF-8 字符，字符串切片操作是安全的。
fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < n {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < n {
                    if bytes[i] == b'\'' {
                        if i + 1 < n && bytes[i + 1] == b'\'' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < n {
                    if bytes[i] == b'"' {
                        if i + 1 < n && bytes[i + 1] == b'"' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                while i < n && bytes[i] != b'`' {
                    i += 1;
                }
                i = (i + 1).min(n);
            }
            b'[' => {
                i += 1;
                while i < n && bytes[i] != b']' {
                    i += 1;
                }
                i = (i + 1).min(n);
            }
            b'-' if i + 1 < n && bytes[i + 1] == b'-' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            b';' => {
                let seg = sql[start..i].trim();
                // 用「剥掉前导空白与注释后还剩不剩东西」判空，而不是只 trim 空白：
                // 否则 `SELECT 1; -- 尾注释` 会被切成两段（第二段是纯注释），
                // 于是一条完全正常的语句被当成「多条语句」拒掉。
                if !skip_ws_and_comments(seg).is_empty() {
                    out.push(seg);
                }
                i += 1;
                start = i;
                continue;
            }
            _ => {
                i += 1;
            }
        }
    }
    let tail = sql[start..].trim();
    if !skip_ws_and_comments(tail).is_empty() {
        out.push(tail);
    }
    out
}

/// 要求 `sql` 恰好是一条语句，返回 trim 后的语句体；多条 / 空则报错。
///
/// 每次 exec/query 调用、以及 batch 里的每一项，都必须先过这一关：既防"用分号在一次调用里
/// 偷偷塞第二条语句"，也顺带给出比"SQL 语法错误"更明确的报错。
fn require_single_statement(sql: &str) -> Result<&str, String> {
    match split_statements(sql).as_slice() {
        [] => Err("SQL 语句不能为空".to_string()),
        [one] => Ok(*one),
        _ => Err("每次调用只允许一条 SQL 语句；多条请用 plugin_sqlite_batch".to_string()),
    }
}

/// 只读自省类 PRAGMA 白名单：不改变文件路径/库行为，只读取 schema 元信息。
/// 与 [`authorize`] 里 `AuthAction::Pragma` 分支共用同一份名单——文本层与 authorizer 核心层
/// 判定标准必须一致，否则"两层"就变成了两套互相矛盾的规则。
const PRAGMA_ALLOWLIST: &[&str] = &[
    "table_info",
    "table_xinfo",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "database_list",
    "collation_list",
];

/// 对单条语句做拒绝名单校验（文本层闸门，与 [`authorize`] 核心层构成双层防御，
/// 取舍与"为什么两层都要"见模块头注释）。
fn check_sql_allowed(sql: &str) -> Result<(), String> {
    let body = skip_ws_and_comments(sql);
    let mut words = body
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty());
    let first = words.next().unwrap_or_default().to_ascii_uppercase();
    // ATTACH/DETACH 这里其实 authorizer 核心层也会挡（双保险）；VACUUM 则是文本层的独有防线
    // ——SQLite 的授权回调没有专门的 VACUUM 动作码，authorizer 拦不住 `VACUUM INTO '<路径>'`
    // 把整份库复制到任意文件，细节见模块头注释。
    const DENY: &[&str] = &["ATTACH", "DETACH", "VACUUM"];
    if DENY.contains(&first.as_str()) {
        return Err(format!("插件 SQL 不允许使用 {first} 语句（数据库隔离限制）"));
    }
    if first == "PRAGMA" {
        // 简化提取：直接取 PRAGMA 后的下一个词。像 `PRAGMA main.table_info(t)` 这种带 schema
        // 前缀的写法，这里可能把 "main" 当成待校验的名字而误判——这只是"文本层"这道友好的
        // 前置提示，判断不精确时最坏情况是提前给出一个不够准确的拒绝理由；真正兜底、说了算的
        // 是 authorizer 核心层（见 [`authorize`]），它拿到的是 SQLite 解析后的真实 pragma 名，
        // 不受这种简化提取影响，所以不存在"文本层判断错就等于挡不住"的安全缺口。
        let name = words.next().unwrap_or_default();
        if !PRAGMA_ALLOWLIST.iter().any(|p| p.eq_ignore_ascii_case(name)) {
            return Err(format!(
                "插件 SQL 不允许使用 PRAGMA {name}（仅允许只读自省类 PRAGMA，如 table_info/index_list）"
            ));
        }
    }
    if first == "CREATE" {
        if let Some(second) = words.next() {
            if second.eq_ignore_ascii_case("VIRTUAL") {
                return Err("插件 SQL 不允许创建虚拟表（CREATE VIRTUAL TABLE）".to_string());
            }
        }
    }
    Ok(())
}

// ==================== authorizer 核心层（SQLite 原生防线） ====================

/// 装在每个插件连接上的 SQLite 授权回调：语句 prepare 时，SQLite 对其中每个动作单独回调这里，
/// 返回 `Deny` 会让 prepare 直接失败。这是本模块里唯一由 **SQLite 自己**（而非我们对 SQL 文本
/// 的解析）判定"这条语句在做什么"的一层，具体挡了什么、为什么这样分类见模块头注释。
fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        // 红线：跨库访问，硬性拒绝。
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => Authorization::Deny,
        // 虚拟表：项目未编译相关特性，插件也用不上，显式拒绝而非依赖"反正没编译进去"。
        AuthAction::CreateVtable { .. } | AuthAction::DropVtable { .. } => Authorization::Deny,
        // PRAGMA：与文本层共用同一份白名单，未在名单里的（含未知 pragma）一律拒绝。
        AuthAction::Pragma { pragma_name, .. } => {
            if PRAGMA_ALLOWLIST.iter().any(|p| p.eq_ignore_ascii_case(pragma_name)) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        // 历史上出过安全问题的 SQL 函数：即便加载扩展的能力理论上已因特性未开而不可用，
        // 这里仍显式拒绝，不依赖"某个可选特性没开"这个前提永远成立。
        AuthAction::Function { function_name }
            if function_name.eq_ignore_ascii_case("load_extension")
                || function_name.eq_ignore_ascii_case("fts3_tokenizer") =>
        {
            Authorization::Deny
        }
        // 插件在自己库内合法需要的常规 DML/DDL/事务/自省读取：放行。
        AuthAction::CreateIndex { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::CreateTempIndex { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::CreateTempTrigger { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::CreateTrigger { .. }
        | AuthAction::CreateView { .. }
        | AuthAction::Delete { .. }
        | AuthAction::DropIndex { .. }
        | AuthAction::DropTable { .. }
        | AuthAction::DropTempIndex { .. }
        | AuthAction::DropTempTable { .. }
        | AuthAction::DropTempTrigger { .. }
        | AuthAction::DropTempView { .. }
        | AuthAction::DropTrigger { .. }
        | AuthAction::DropView { .. }
        | AuthAction::Insert { .. }
        | AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Transaction { .. }
        | AuthAction::Update { .. }
        | AuthAction::AlterTable { .. }
        | AuthAction::Reindex { .. }
        | AuthAction::Analyze { .. }
        | AuthAction::Function { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Recursive => Authorization::Allow,
        // 任何未识别的（`Unknown`）/未来 SQLite 新增的动作码：fail-safe 默认拒绝，不是默认放行。
        _ => Authorization::Deny,
    }
}

// ==================== JSON <-> SQLite 值互转 ====================

/// 把插件传入的 JSON 参数值转成 SQLite 绑定值。
///
/// 约定：`{"$blob": "<base64>"}` 这种恰好只有 `$blob` 一个键的对象表示 BLOB；
/// 其余 `null`/`bool`/数字/字符串按自然语义映射；其它 JSON 类型（数组、多键对象）不支持。
fn json_to_sql(v: &JsonValue) -> Result<rusqlite::types::Value, String> {
    use rusqlite::types::Value as SqlValue;
    match v {
        JsonValue::Null => Ok(SqlValue::Null),
        JsonValue::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(SqlValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(SqlValue::Real(f))
            } else {
                Err("参数数字超出支持范围".to_string())
            }
        }
        JsonValue::String(s) => Ok(SqlValue::Text(s.clone())),
        JsonValue::Object(map) if map.len() == 1 && map.contains_key("$blob") => {
            let b64 = map
                .get("$blob")
                .and_then(|x| x.as_str())
                .ok_or_else(|| "\"$blob\" 字段必须是 base64 字符串".to_string())?;
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("blob 参数 base64 解码失败: {e}"))?;
            Ok(SqlValue::Blob(bytes))
        }
        other => Err(format!("不支持的参数类型: {other}")),
    }
}

/// 把查询结果的一个单元格转成 JSON 值；BLOB 编成 `{"$blob": "<base64>"}`（与 [`json_to_sql`] 的
/// 约定对称，供插件回读二进制列）。
fn sql_value_to_json(v: rusqlite::types::ValueRef<'_>) -> JsonValue {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(i) => JsonValue::from(i),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        ValueRef::Text(t) => JsonValue::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => {
            use base64::Engine as _;
            let mut obj = serde_json::Map::with_capacity(1);
            obj.insert(
                "$blob".to_string(),
                JsonValue::String(base64::engine::general_purpose::STANDARD.encode(b)),
            );
            JsonValue::Object(obj)
        }
    }
}

// ==================== 阻塞线程池里跑的纯逻辑（不依赖 Tauri State） ====================

/// 打开（或创建）库文件、设好并发/崩溃安全相关的 PRAGMA，再装上 [`authorize`] 授权回调。
///
/// **顺序很关键，不能颠倒**：宿主自己这几条 `pragma_update`（`journal_mode`/`synchronous`/
/// `foreign_keys`）必须在装 authorizer **之前**执行——authorizer 是装在整个连接上的钩子，
/// 装上之后连宿主自己发的语句也会经过它；而 `journal_mode`/`foreign_keys` 又不在
/// [`PRAGMA_ALLOWLIST`] 里（这些属于"能影响库行为，不给插件直接用"的那一类），如果先装
/// authorizer 再调用这几个 `pragma_update`，会被自己刚装上的闸门拒绝，库反而初始化不完整。
/// 装上之后，插件之后发来的每一条语句才会经过它——这几条初始化 PRAGMA 不经过
/// [`check_sql_allowed`] 那道插件 SQL 文本闸门（那道闸门只检查插件自己发来的 SQL 文本）。
fn open_and_init(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建插件数据目录失败: {e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("打开 SQLite 数据库失败: {e}"))?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    conn.authorizer(Some(authorize));
    Ok(conn)
}

fn exec_blocking(conn: &Mutex<Connection>, sql: &str, params: &[JsonValue]) -> Result<u64, String> {
    let one = require_single_statement(sql)?;
    check_sql_allowed(one)?;
    let values = params
        .iter()
        .map(json_to_sql)
        .collect::<Result<Vec<_>, _>>()?;
    let conn = conn.lock().map_err(|_| "数据库连接锁获取失败".to_string())?;
    let n = conn
        .execute(one, rusqlite::params_from_iter(values))
        .map_err(|e| format!("执行失败: {e}"))?;
    Ok(n as u64)
}

fn query_blocking(
    conn: &Mutex<Connection>,
    sql: &str,
    params: &[JsonValue],
) -> Result<Vec<JsonValue>, String> {
    let one = require_single_statement(sql)?;
    check_sql_allowed(one)?;
    let values = params
        .iter()
        .map(json_to_sql)
        .collect::<Result<Vec<_>, _>>()?;
    let conn = conn.lock().map_err(|_| "数据库连接锁获取失败".to_string())?;
    let mut stmt = conn.prepare(one).map_err(|e| format!("SQL 准备失败: {e}"))?;
    let col_names: Vec<String> = stmt.column_names().into_iter().map(|s| s.to_string()).collect();
    let rows_iter = stmt
        .query_map(rusqlite::params_from_iter(values), |row| {
            let mut obj = serde_json::Map::with_capacity(col_names.len());
            for (i, name) in col_names.iter().enumerate() {
                obj.insert(name.clone(), sql_value_to_json(row.get_ref(i)?));
            }
            Ok(JsonValue::Object(obj))
        })
        .map_err(|e| format!("查询失败: {e}"))?;
    let mut out = Vec::new();
    for r in rows_iter {
        out.push(r.map_err(|e| format!("读取结果行失败: {e}"))?);
    }
    Ok(out)
}

fn batch_blocking(conn: &Mutex<Connection>, statements: &[SqliteStatement]) -> Result<u64, String> {
    if statements.is_empty() {
        return Err("批量语句不能为空".to_string());
    }
    // 先把每一条都校验一遍再开事务：任何一条不合法就整体不执行，不留半开事务。
    for st in statements {
        let one = require_single_statement(&st.sql)?;
        check_sql_allowed(one)?;
    }
    let mut conn = conn.lock().map_err(|_| "数据库连接锁获取失败".to_string())?;
    let tx = conn.transaction().map_err(|e| format!("开启事务失败: {e}"))?;
    let mut total: u64 = 0;
    for st in statements {
        // 上面已校验过，这里重取一次单语句体（不可能失败，`require_single_statement` 是纯函数）。
        let one = require_single_statement(&st.sql)?;
        let values = st
            .params
            .iter()
            .map(json_to_sql)
            .collect::<Result<Vec<_>, _>>()?;
        let n = tx
            .execute(one, rusqlite::params_from_iter(values))
            .map_err(|e| format!("批量执行失败（已整体回滚）: {e}"))?;
        total += n as u64;
    }
    tx.commit().map_err(|e| format!("提交事务失败: {e}"))?;
    Ok(total)
}

// ==================== Tauri 命令 ====================

/// 打开（不存在则创建）一个按当前插件隔离的 SQLite 数据库，返回句柄 id。
///
/// 库文件落在 `plugin-data/<插件id>/sqlite/<name>.db`（调试会话落调试沙盒同构路径），
/// `name` 经过严格校验（见 [`validate_db_name`]），不可能借它跳出该目录。
/// 每次调用都会开一条新的独立连接（SQLite 原生支持同文件多连接 + WAL 并发），互不影响。
///
/// # Errors
/// - `name` 为空 / 超长 / 含非法字符 / 是 Windows 保留设备名
/// - 当前窗口没有正在运行的插件会话
/// - 创建目录失败或打开数据库文件失败（磁盘只读、路径被占用等）
#[tauri::command]
pub async fn plugin_sqlite_open(
    name: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    sqlite: State<'_, SqliteState>,
) -> Result<String, String> {
    let session = caller_session(&webview, &registry)?;
    validate_db_name(&name)?;
    let path = sqlite_dir(&session, &registry)?.join(format!("{name}.db"));
    let conn = tauri::async_runtime::spawn_blocking(move || open_and_init(&path))
        .await
        .map_err(|e| format!("打开数据库失败（线程异常）: {e}"))??;
    let id = sqlite.counter.fetch_add(1, Ordering::Relaxed).to_string();
    let mut handles = sqlite
        .handles
        .lock()
        .map_err(|_| "句柄表锁获取失败".to_string())?;
    handles.insert(
        id.clone(),
        SqliteHandle {
            owner: session,
            conn: Arc::new(Mutex::new(conn)),
        },
    );
    Ok(id)
}

/// 在指定句柄上执行一条写语句（`?` 占位符 + JSON 参数绑定，禁止插件自己拼 SQL 字符串），
/// 返回受影响行数。
///
/// # Errors
/// - `handle` 不存在 / 已关闭 / 不属于当前插件会话
/// - `sql` 不是单条语句，或命中 ATTACH/DETACH/PRAGMA/VACUUM/CREATE VIRTUAL TABLE 拒绝名单
/// - 参数类型不受支持，或 SQL 执行失败（语法错误、约束冲突等，原始 SQLite 错误信息透传）
#[tauri::command]
pub async fn plugin_sqlite_exec(
    handle: String,
    sql: String,
    params: Option<Vec<JsonValue>>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    sqlite: State<'_, SqliteState>,
) -> Result<u64, String> {
    let session = caller_session(&webview, &registry)?;
    let conn = sqlite.conn_for(&session, &handle)?;
    let params = params.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || exec_blocking(&conn, &sql, &params))
        .await
        .map_err(|e| format!("执行失败（线程异常）: {e}"))?
}

/// 在指定句柄上执行一条查询语句，返回行数组，每行是 `{列名: 值}` 的 JSON 对象。
///
/// BLOB 列以 `{"$blob": "<base64>"}` 形式返回（与参数绑定的 blob 约定对称）。
///
/// # Errors
/// 同 [`plugin_sqlite_exec`]。
#[tauri::command]
pub async fn plugin_sqlite_query(
    handle: String,
    sql: String,
    params: Option<Vec<JsonValue>>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    sqlite: State<'_, SqliteState>,
) -> Result<Vec<JsonValue>, String> {
    let session = caller_session(&webview, &registry)?;
    let conn = sqlite.conn_for(&session, &handle)?;
    let params = params.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || query_blocking(&conn, &sql, &params))
        .await
        .map_err(|e| format!("查询失败（线程异常）: {e}"))?
}

/// 在指定句柄上按顺序执行多条语句，整体包在一个事务里：任意一条失败则**全部回滚**，
/// 成功时返回全部语句的受影响行数之和。
///
/// # Errors
/// - `statements` 为空
/// - 其中任意一条不满足单语句 / 拒绝名单校验（此时**不会**开启事务，等同整体拒绝）
/// - 事务执行/提交失败（此时已执行的部分随事务回滚，不落地）
#[tauri::command]
pub async fn plugin_sqlite_batch(
    handle: String,
    statements: Vec<SqliteStatement>,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    sqlite: State<'_, SqliteState>,
) -> Result<u64, String> {
    let session = caller_session(&webview, &registry)?;
    let conn = sqlite.conn_for(&session, &handle)?;
    tauri::async_runtime::spawn_blocking(move || batch_blocking(&conn, &statements))
        .await
        .map_err(|e| format!("批量执行失败（线程异常）: {e}"))?
}

/// 关闭指定句柄，释放连接。句柄必须属于当前插件会话；关闭后该 id 立即失效，
/// 后续 exec/query/batch 会收到"句柄不存在或已关闭"。
///
/// 实际关闭（可能触发末次 WAL checkpoint / fsync）挪到阻塞线程池执行，避免卡住 IPC 执行器。
///
/// # Errors
/// - `handle` 不存在 / 已关闭 / 不属于当前插件会话
#[tauri::command]
pub async fn plugin_sqlite_close(
    handle: String,
    webview: tauri::Webview,
    registry: State<'_, PluginRegistry>,
    sqlite: State<'_, SqliteState>,
) -> Result<(), String> {
    let session = caller_session(&webview, &registry)?;
    let removed = {
        let mut handles = sqlite
            .handles
            .lock()
            .map_err(|_| "句柄表锁获取失败".to_string())?;
        match handles.get(&handle) {
            Some(h) if h.owner.same_as(&session) => handles.remove(&handle),
            Some(_) => return Err("该数据库句柄不属于当前插件".to_string()),
            None => return Err("数据库句柄不存在或已关闭".to_string()),
        }
    };
    if let Some(h) = removed {
        let _ = tauri::async_runtime::spawn_blocking(move || drop(h)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- 库名校验 --------

    #[test]
    fn validate_db_name_accepts_normal_names() {
        assert!(validate_db_name("notes").is_ok());
        assert!(validate_db_name("my-db_01").is_ok());
    }

    #[test]
    fn validate_db_name_rejects_path_tricks() {
        assert!(validate_db_name("").is_err());
        assert!(validate_db_name("..").is_err());
        assert!(validate_db_name("../evil").is_err());
        assert!(validate_db_name("a/b").is_err());
        assert!(validate_db_name("a\\b").is_err());
        assert!(validate_db_name("C:\\evil").is_err());
        assert!(validate_db_name("a.b").is_err());
        assert!(validate_db_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn validate_db_name_rejects_windows_reserved() {
        assert!(validate_db_name("CON").is_err());
        assert!(validate_db_name("con").is_err());
        assert!(validate_db_name("LPT1").is_err());
    }

    // -------- 单语句切分 --------

    #[test]
    fn split_statements_ignores_semicolons_in_strings_and_comments() {
        let sql = "INSERT INTO t(v) VALUES('a;b'); -- comment; still one\nSELECT 1";
        // 分号在字符串字面量里、以及注释里，都不应被当成语句边界。
        let one = require_single_statement("INSERT INTO t(v) VALUES('a;b')").unwrap();
        assert_eq!(one, "INSERT INTO t(v) VALUES('a;b')");
        // 但真正顶层的两条语句应该被识别为「多条」而拒绝。
        assert!(require_single_statement(sql).is_err());
    }

    #[test]
    fn require_single_statement_rejects_stacked_statements() {
        let err = require_single_statement("SELECT 1; ATTACH DATABASE 'x' AS y").unwrap_err();
        assert!(err.contains("只允许一条"));
    }

    #[test]
    fn require_single_statement_rejects_empty() {
        assert!(require_single_statement("   ").is_err());
        assert!(require_single_statement("-- just a comment").is_err());
    }

    // -------- 拒绝名单 --------

    #[test]
    fn check_sql_allowed_blocks_attach_and_friends() {
        assert!(check_sql_allowed("ATTACH DATABASE 'x' AS y").is_err());
        assert!(check_sql_allowed("  attach database 'x' as y").is_err());
        assert!(check_sql_allowed("DETACH y").is_err());
        assert!(check_sql_allowed("PRAGMA journal_mode=WAL").is_err());
        assert!(check_sql_allowed("VACUUM").is_err());
        assert!(check_sql_allowed("VACUUM INTO 'out.db'").is_err());
        assert!(check_sql_allowed("CREATE VIRTUAL TABLE t USING fts5(x)").is_err());
        assert!(check_sql_allowed("/* c */ ATTACH DATABASE 'x' AS y").is_err());
    }

    #[test]
    fn check_sql_allowed_permits_normal_dml_ddl() {
        assert!(check_sql_allowed("SELECT * FROM t WHERE id=?").is_ok());
        assert!(check_sql_allowed("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)").is_ok());
        assert!(check_sql_allowed("INSERT INTO t(v) VALUES(?)").is_ok());
        assert!(check_sql_allowed("UPDATE t SET v=? WHERE id=?").is_ok());
        assert!(check_sql_allowed("DELETE FROM t WHERE id=?").is_ok());
    }

    #[test]
    fn check_sql_allowed_pragma_allowlist() {
        // 白名单内：放行。
        assert!(check_sql_allowed("PRAGMA table_info(t)").is_ok());
        assert!(check_sql_allowed("pragma index_list(t)").is_ok());
        // 白名单外：拒绝，包括能影响文件落点/库行为的项。
        assert!(check_sql_allowed("PRAGMA journal_mode=WAL").is_err());
        assert!(check_sql_allowed("PRAGMA temp_store_directory='C:\\evil'").is_err());
        assert!(check_sql_allowed("PRAGMA writable_schema=ON").is_err());
    }

    // -------- authorizer 核心层：绕过文本层，直接验证 SQLite 授权回调本身 --------

    /// 内存库 + 装上 [`authorize`]，但**不经过**本模块的 exec/query 包装——用来单独证明
    /// authorizer 这一层自己就能拦住东西，不依赖文本层配合。
    fn authed_mem_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("打开内存库失败");
        conn.authorizer(Some(authorize));
        conn
    }

    #[test]
    fn authorizer_denies_attach_and_detach_standalone() {
        let conn = authed_mem_conn();
        assert!(conn.execute("ATTACH DATABASE 'other.db' AS evil", []).is_err());
        assert!(conn.execute("DETACH DATABASE main", []).is_err());
    }

    #[test]
    fn authorizer_pragma_allowlist_standalone() {
        let conn = authed_mem_conn();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", []).unwrap();
        assert!(conn.prepare("PRAGMA table_info(t)").is_ok());
        assert!(conn.execute("PRAGMA journal_mode=WAL", []).is_err());
        assert!(conn.execute("PRAGMA writable_schema=ON", []).is_err());
    }

    #[test]
    fn authorizer_allows_normal_dml_ddl_and_transactions() {
        let conn = authed_mem_conn();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO t(id, v) VALUES(1, 'a')", []).unwrap();
        conn.execute("UPDATE t SET v='b' WHERE id=1", []).unwrap();
        let v: String = conn
            .query_row("SELECT v FROM t WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "b");
        conn.execute("DELETE FROM t WHERE id=1", []).unwrap();
    }

    #[test]
    fn authorizer_denies_dangerous_functions_by_name() {
        let conn = authed_mem_conn();
        // 无论 `load_extension` 这个 SQL 函数在当前编译的 SQLite 里是否真的注册了，
        // 我们的 authorizer 一旦被问到 `AuthAction::Function{ function_name: "load_extension" }`
        // 就会 Deny；即使它压根没注册，prepare 也会因"函数不存在"直接失败——两种情况下
        // 净结果都是调用失败，这正是我们要验证的效果（不对具体错误原因下过度精确的断言）。
        assert!(conn.execute("SELECT load_extension('nope')", []).is_err());
    }

    // -------- exec/query/batch 真实跑在内存库上（文本层，不装 authorizer，隔离验证） --------

    fn mem_conn() -> Mutex<Connection> {
        Mutex::new(Connection::open_in_memory().expect("打开内存库失败"))
    }

    #[test]
    fn exec_and_query_roundtrip_including_blob() {
        let conn = mem_conn();
        exec_blocking(
            &conn,
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, data BLOB)",
            &[],
        )
        .unwrap();
        let affected = exec_blocking(
            &conn,
            "INSERT INTO t(id, name, data) VALUES(?, ?, ?)",
            &[
                JsonValue::from(1),
                JsonValue::String("hello".to_string()),
                serde_json::json!({"$blob": "aGVsbG8="}), // "hello" 的 base64
            ],
        )
        .unwrap();
        assert_eq!(affected, 1);

        let rows = query_blocking(&conn, "SELECT id, name, data FROM t WHERE id=?", &[JsonValue::from(1)])
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = rows[0].as_object().unwrap();
        assert_eq!(row["name"], JsonValue::String("hello".to_string()));
        assert_eq!(row["data"], serde_json::json!({"$blob": "aGVsbG8="}));
    }

    #[test]
    fn exec_rejects_attach_before_touching_sqlite() {
        let conn = mem_conn();
        let err = exec_blocking(&conn, "ATTACH DATABASE 'other.db' AS evil", &[]).unwrap_err();
        assert!(err.contains("ATTACH"));
    }

    #[test]
    fn batch_rolls_back_entirely_on_failure() {
        let conn = mem_conn();
        exec_blocking(&conn, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT NOT NULL)", &[]).unwrap();
        let statements = vec![
            SqliteStatement {
                sql: "INSERT INTO t(id, v) VALUES(?, ?)".to_string(),
                params: vec![JsonValue::from(1), JsonValue::String("a".to_string())],
            },
            SqliteStatement {
                // v 是 NOT NULL，这里故意插入 NULL 触发失败，整个批次应回滚（含第一条也不落地）。
                sql: "INSERT INTO t(id, v) VALUES(?, ?)".to_string(),
                params: vec![JsonValue::from(2), JsonValue::Null],
            },
        ];
        assert!(batch_blocking(&conn, &statements).is_err());
        let rows = query_blocking(&conn, "SELECT id FROM t", &[]).unwrap();
        assert!(rows.is_empty(), "任一条失败必须整体回滚，第一条也不应落地");
    }

    #[test]
    fn batch_commits_all_on_success() {
        let conn = mem_conn();
        exec_blocking(&conn, "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", &[]).unwrap();
        let statements = vec![
            SqliteStatement {
                sql: "INSERT INTO t(id, v) VALUES(?, ?)".to_string(),
                params: vec![JsonValue::from(1), JsonValue::String("a".to_string())],
            },
            SqliteStatement {
                sql: "INSERT INTO t(id, v) VALUES(?, ?)".to_string(),
                params: vec![JsonValue::from(2), JsonValue::String("b".to_string())],
            },
        ];
        let total = batch_blocking(&conn, &statements).unwrap();
        assert_eq!(total, 2);
        let rows = query_blocking(&conn, "SELECT id FROM t ORDER BY id", &[]).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn batch_rejects_forbidden_statement_without_running_any() {
        let conn = mem_conn();
        exec_blocking(&conn, "CREATE TABLE t(id INTEGER PRIMARY KEY)", &[]).unwrap();
        let statements = vec![
            SqliteStatement {
                sql: "INSERT INTO t(id) VALUES(?)".to_string(),
                params: vec![JsonValue::from(1)],
            },
            SqliteStatement {
                sql: "ATTACH DATABASE 'other.db' AS evil".to_string(),
                params: vec![],
            },
        ];
        assert!(batch_blocking(&conn, &statements).is_err());
        let rows = query_blocking(&conn, "SELECT id FROM t", &[]).unwrap();
        assert!(rows.is_empty(), "预校验阶段就应整体拒绝，第一条合法语句也不应执行");
    }

    // -------- 句柄归属隔离 --------

    fn session(id: &str, dev: bool) -> ActiveSession {
        ActiveSession {
            id: id.to_string(),
            dev,
        }
    }

    #[test]
    fn conn_for_isolates_by_full_session_identity() {
        let state = SqliteState::default();
        let conn = Connection::open_in_memory().unwrap();
        state.handles.lock().unwrap().insert(
            "h1".to_string(),
            SqliteHandle {
                owner: session("plugin-a", false),
                conn: Arc::new(Mutex::new(conn)),
            },
        );
        // 同 id 同 dev：允许。
        assert!(state.conn_for(&session("plugin-a", false), "h1").is_ok());
        // 同 id 但 dev 不同（调试窗 vs 正式窗）：必须拒绝。
        assert!(state.conn_for(&session("plugin-a", true), "h1").is_err());
        // 不同 id：必须拒绝。
        assert!(state.conn_for(&session("plugin-b", false), "h1").is_err());
        // 不存在的句柄：必须拒绝。
        assert!(state.conn_for(&session("plugin-a", false), "no-such-handle").is_err());
    }
}
