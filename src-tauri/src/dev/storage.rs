//! 调试环境的存储层：**独立测试库**的读写 + 存储查看器命令。
//!
//! 两套存储 API 在调试环境里各自对应测试库的一张表，与正式库同名同结构、**不同文件**：
//! - `itools.db.*`（纯本地 KV）→ `plugin_kv`
//! - `itools.data.*`（正式环境参与云同步）→ `plugin_data`
//!
//! 「调试数据永不上云」是物理保证：正式库的 `pd_namespaces()` 只扫正式库文件，
//! 天生看不到这里的任何一行，所以 `sync_now` 的全量遍历根本遍历不到调试数据。

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;

use super::DevRuntime;

/// 调试插件的数据命名空间：与正式环境**同一套命名**（`plugin:<id>`，直接复用同一个函数），
/// 隔离靠的是「另一个库文件」，不是靠改名字——这样插件代码在两边行为完全一致，
/// 且查看器看到的一定就是插件写进去的那一份。
pub fn dev_ns(id: &str) -> String {
    crate::plugin::commands::plugin_ns(id)
}

/// 存储查看器里的一条记录。
#[derive(Debug, Clone, Serialize)]
pub struct DevKv {
    pub key: String,
    /// 值的原始文本（`data` 类是 JSON 文本，`db` 类是插件自己 stringify 后的字符串）。
    pub value: String,
    /// 值占用的字节数（UTF-8），供 UI 展示体积。
    pub bytes: usize,
    /// 最后更新时间（RFC3339）。`db` 类不记录时间 → None（如实为空，不编造）。
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

/// 导出 / 导入的文件结构（人可读、可手改后再导回）。
#[derive(Serialize, Deserialize)]
struct DevDump {
    /// 结构版本，便于以后演进时兼容旧文件。
    #[serde(default = "one")]
    version: u32,
    #[serde(rename = "pluginId", default)]
    plugin_id: String,
    #[serde(rename = "exportedAt", default)]
    exported_at: String,
    /// `itools.db.*` 的键值（值是字符串）。
    #[serde(default)]
    db: serde_json::Map<String, serde_json::Value>,
    /// `itools.data.*` 的键值（值是任意 JSON）。
    #[serde(default)]
    data: serde_json::Map<String, serde_json::Value>,
}

fn one() -> u32 {
    1
}

/// 校验 `kind` 参数（前端只会传这两种，非法值必须报错而不是静默当成某一种）。
fn check_kind(kind: &str) -> Result<&str, String> {
    match kind {
        "db" | "data" => Ok(kind),
        other => Err(format!("未知存储类型「{other}」（只支持 db / data）")),
    }
}

/// 校验调试插件 id：`dev_storage_*` 的 `id` 是**命令参数**，而它会一路参与
/// [`DevRuntime::files_dir`] 的路径拼接，最终被 `dev_storage_clear` 交给 `remove_dir_all`。
/// `Path::join` 对 `..` 一视同仁，传 `id=".."` 会让删除目标落到调试根之外
/// （`<dev_home>/plugin-data/../files` → `<dev_home>/files`）。所以必须在入口收口，
/// 不能指望「前端只传真实目录名」——同文件另一侧（沙盒读写）早就有 `sandbox_relative` 兜底，
/// 这里不该是唯一一条没有防线的路径。
///
/// 为什么**不**复用正式插件名白名单（`^[a-z0-9][a-z0-9._-]{0,63}$`）：调试插件的 id 取的是
/// **目录名**，开发者的工作目录完全可能叫 `MyPlugin` / `插件-demo` / `dist`，
/// 套正式规则会把一堆合法的调试目录挡在门外（而正式环境的严格是因为那个名字还要当
/// URL 路径段与 SQLite 命名空间用）。这里只要求它是**一个普通路径段**：
/// 非空、无首尾空白、不含路径分隔符 / 盘符冒号 / NUL、不是 `.` `..` 这类纯点目录、
/// 不以点或空格结尾（Windows 会静默剥掉，`demo.` 与 `demo` 会指向同一个目录）。
///
/// 也不用「必须在当前注册表里」做判据：调试目录随时可能正在重建（`dist/` 一时不存在），
/// 那会让「清空这个插件的测试数据」在最需要用的时候失败。形态校验已经足以关掉穿越这条路。
fn check_id(id: &str) -> Result<&str, String> {
    let bad = id.is_empty()
        || id.trim() != id
        || id.contains(['/', '\\', ':', '\0'])
        || id.chars().all(|c| c == '.')
        || id.ends_with('.');
    if bad {
        return Err(format!(
            "非法的调试插件 id「{id}」：必须是单个目录名（不能含 / \\ : 或以点结尾，也不能是 . / ..）"
        ));
    }
    Ok(id)
}

/// Unix 秒 → 本地 RFC3339。
fn ts(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_default()
}

// ==================== 调试沙盒文件 ====================

/// 删掉某调试插件的沙盒文件目录（`dev_storage_clear` 一并清理，避免残留脏文件）。
fn remove_files_dir(dev: &DevRuntime, id: &str) -> Result<(), String> {
    let dir = dev.files_dir(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("清理调试沙盒失败: {e}")),
    }
}

/// 供 `plugin_*_file` 在调试会话下取沙盒根（父目录的创建由写入方负责）。
///
/// 这里的 `id` 来自**当前会话**（`ActiveSession.id`），而会话只可能由 `open_dev_window`
/// 建立，那条路径上 `dev.dir_of(&id)` 必须命中才放行——所以它一定是本次扫描到的真实目录名，
/// 不是插件能随手编造的字符串（沙盒内的相对路径另有 `sandbox_relative` 把关）。
/// 命令参数直传的那条路见 [`check_id`]。
pub fn sandbox_root(dev: &DevRuntime, id: &str) -> PathBuf {
    dev.files_dir(id)
}

// ==================== 命令：存储查看器（管理中心） ====================

/// 列出某调试插件在测试库里的全部记录。
#[tauri::command]
pub fn dev_storage_list(
    id: String,
    kind: String,
    dev: State<'_, Arc<DevRuntime>>,
) -> Result<Vec<DevKv>, String> {
    check_id(&id)?;
    let kind = check_kind(&kind)?;
    let out = if kind == "db" {
        dev.db
            .pkv_entries(&id)
            .into_iter()
            .map(|(key, value)| DevKv {
                bytes: value.len(),
                key,
                value,
                updated_at: None,
            })
            .collect()
    } else {
        dev.db
            .pd_entries(&dev_ns(&id))
            .into_iter()
            .map(|(key, value, updated)| DevKv {
                bytes: value.len(),
                key,
                value,
                updated_at: Some(ts(updated)),
            })
            .collect()
    };
    Ok(out)
}

/// 写 / 覆盖一条记录（开发者直接在查看器里造数据）。
///
/// `data` 类的 value 必须是 JSON 文本；解析不了就按纯字符串存——与
/// `plugin_data_set` 的容错完全一致，保证「查看器里造的数据」和「插件自己写的数据」同构。
#[tauri::command]
pub fn dev_storage_set(
    id: String,
    kind: String,
    key: String,
    value: String,
    dev: State<'_, Arc<DevRuntime>>,
) -> Result<(), String> {
    check_id(&id)?;
    let kind = check_kind(&kind)?;
    if key.trim().is_empty() {
        return Err("key 不能为空".to_string());
    }
    if kind == "db" {
        dev.db.pkv_set(&id, &key, &value)
    } else {
        let parsed: serde_json::Value =
            serde_json::from_str(&value).unwrap_or(serde_json::Value::String(value));
        dev.db
            .pd_set(&dev_ns(&id), &key, &parsed.to_string(), now_secs(), true)
    }
}

/// 删一条记录。
#[tauri::command]
pub fn dev_storage_remove(
    id: String,
    kind: String,
    key: String,
    dev: State<'_, Arc<DevRuntime>>,
) -> Result<(), String> {
    check_id(&id)?;
    let kind = check_kind(&kind)?;
    if kind == "db" {
        dev.db.pkv_remove(&id, &key)
    } else {
        dev.db.pd_remove(&dev_ns(&id), &key)
    }
}

/// 清空该插件在**测试库**里的全部数据（两类都清）并删掉调试沙盒文件目录。
/// 只作用于测试库与调试沙盒，正式数据一行都不会动。
///
/// `id` 先过 [`check_id`]：本命令是全模块唯一会 `remove_dir_all` 的入口，
/// 拼接前不校验就等于把删除目标交给调用方随便指。
#[tauri::command]
pub fn dev_storage_clear(id: String, dev: State<'_, Arc<DevRuntime>>) -> Result<(), String> {
    check_id(&id)?;
    dev.db.pkv_clear(&id)?;
    dev.db.pd_clear_ns(&dev_ns(&id))?;
    remove_files_dir(&dev, &id)
}

/// 导出该插件的测试数据为 JSON 文本（前端另存为文件）。
#[tauri::command]
pub fn dev_storage_export(id: String, dev: State<'_, Arc<DevRuntime>>) -> Result<String, String> {
    check_id(&id)?;
    let mut dump = DevDump {
        version: 1,
        plugin_id: id.clone(),
        exported_at: crate::plugin::install::now_rfc3339(),
        db: serde_json::Map::new(),
        data: serde_json::Map::new(),
    };
    for (k, v) in dev.db.pkv_entries(&id) {
        dump.db.insert(k, serde_json::Value::String(v));
    }
    for (k, v, _) in dev.db.pd_entries(&dev_ns(&id)) {
        let parsed = serde_json::from_str(&v).unwrap_or(serde_json::Value::String(v));
        dump.data.insert(k, parsed);
    }
    serde_json::to_string_pretty(&dump).map_err(|e| format!("导出失败: {e}"))
}

/// 从导出的 JSON 文本导入（**合并**，同名键覆盖；不清空原有数据）。
#[tauri::command]
pub fn dev_storage_import(
    id: String,
    json: String,
    dev: State<'_, Arc<DevRuntime>>,
) -> Result<(), String> {
    check_id(&id)?;
    let dump: DevDump =
        serde_json::from_str(&json).map_err(|e| format!("导入文件不是合法的导出结构: {e}"))?;
    for (k, v) in &dump.db {
        // db 类的值在正式实现里就是字符串；对象/数字一律转成其 JSON 文本，不丢数据
        let text = v
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| v.to_string());
        dev.db.pkv_set(&id, k, &text)?;
    }
    let ns = dev_ns(&id);
    for (k, v) in &dump.data {
        dev.db.pd_set(&ns, k, &v.to_string(), now_secs(), true)?;
    }
    Ok(())
}

/// 当前 Unix 秒。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(tag: &str) -> DevRuntime {
        let base = std::env::temp_dir().join(format!(
            "itools-devstore-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        DevRuntime::new(base.clone(), base.join("dev-plugins"))
    }

    /// 导出 → 清空 → 导入 能完整还原两类数据。
    #[test]
    fn export_clear_import_roundtrip() {
        let rt = runtime("dump");
        let ns = dev_ns("demo");
        rt.db.pkv_set("demo", "token", "\"abc\"").unwrap();
        rt.db.pd_set(&ns, "note", r#"{"t":"hi"}"#, 100, true).unwrap();

        let dumped = {
            let mut out = DevDump {
                version: 1,
                plugin_id: "demo".into(),
                exported_at: String::new(),
                db: serde_json::Map::new(),
                data: serde_json::Map::new(),
            };
            for (k, v) in rt.db.pkv_entries("demo") {
                out.db.insert(k, serde_json::Value::String(v));
            }
            for (k, v, _) in rt.db.pd_entries(&ns) {
                out.data.insert(k, serde_json::from_str(&v).unwrap());
            }
            serde_json::to_string(&out).unwrap()
        };

        rt.db.pkv_clear("demo").unwrap();
        rt.db.pd_clear_ns(&ns).unwrap();
        assert!(rt.db.pkv_entries("demo").is_empty());
        assert!(rt.db.pd_entries(&ns).is_empty());

        let back: DevDump = serde_json::from_str(&dumped).unwrap();
        for (k, v) in &back.db {
            rt.db.pkv_set("demo", k, v.as_str().unwrap()).unwrap();
        }
        for (k, v) in &back.data {
            rt.db.pd_set(&ns, k, &v.to_string(), 100, true).unwrap();
        }
        assert_eq!(rt.db.pkv_get("demo", "token").as_deref(), Some("\"abc\""));
        assert_eq!(rt.db.pd_entries(&ns).len(), 1);
        let _ = std::fs::remove_dir_all(rt.db_path.parent().unwrap());
    }

    /// 非法 kind 必须报错，不能悄悄按某一种处理（否则「查看 data 却看到 db」）。
    #[test]
    fn unknown_kind_is_rejected() {
        assert!(check_kind("db").is_ok());
        assert!(check_kind("data").is_ok());
        assert!(check_kind("kv").is_err());
    }

    /// id 直接参与路径拼接（`files_dir` → `remove_dir_all`），穿越形态必须在入口被拒。
    #[test]
    fn traversal_ids_are_rejected() {
        // 调试插件的 id 取目录名，比正式插件宽松：大写 / 中文 / 点分隔都得放行
        for ok in ["demo", "MyPlugin", "插件-demo", "my.plugin_v2-x", "dist"] {
            assert!(check_id(ok).is_ok(), "本应合法: {ok}");
        }
        for bad in [
            "",            // 空
            ".",           // 当前目录
            "..",          // 上级目录：会把 remove_dir_all 打到调试根之外
            "...",         // 纯点
            "../demo",     //
            "..\\demo",    // Windows 分隔符
            "a/b",         //
            "a\\b",        //
            "C:\\Windows", // 盘符（join 遇绝对路径会**丢弃**前缀直接用它）
            "demo:stream", // NTFS 备用数据流
            "demo.",       // 尾点会被 Windows 静默剥掉 → 与 demo 撞同一个目录
            " demo",       // 首尾空白同理
            "demo ",
        ] {
            assert!(check_id(bad).is_err(), "本应非法: {bad:?}");
        }
    }

    /// 校验必须真的挡住删除：`files_dir` 是纯 `join`，`..` 会**原样**留在删除目标里
    /// （`remove_dir_all` 按 OS 语义解析，真正删掉的是 `<dev_home>/files`）。
    #[test]
    fn clear_never_escapes_dev_root() {
        let rt = runtime("escape");
        let escaped = rt.files_dir("..");
        assert!(
            escaped
                .components()
                .any(|c| c == std::path::Component::ParentDir),
            "files_dir 不做任何归一化，所以 `..` 必须在 check_id 这一层就被拒：{}",
            escaped.display()
        );
        assert!(check_id("..").is_err(), "唯一的防线就在这里");
        // 正常 id 老老实实待在沙盒根下
        let normal = rt.files_dir("demo");
        assert!(normal.starts_with(&rt.files_root));
        assert!(!normal
            .components()
            .any(|c| c == std::path::Component::ParentDir));
        let _ = std::fs::remove_dir_all(rt.db_path.parent().unwrap());
    }

    /// 调试沙盒根与正式沙盒根**不同**（隔离的第三条腿）。
    ///
    /// 这里的「正式」指的是**同一个构建里的正式插件环境**（`<数据根>\plugin-data\<id>\files`），
    /// 不是「release 版的数据」——本测试只比路径、不读任何文件。所以它必须跟着
    /// [`crate::paths::data_root`] 走：测试跑在 debug 下，正式插件沙盒此时就在 `itools-dev`
    /// 里，若这里还写死 `itools`，断言会因为「两个根本不相干的目录当然不相等」而**假通过**，
    /// 隔离到底有没有真做到就再也测不出来了。
    #[test]
    fn dev_sandbox_is_not_the_production_one() {
        let rt = runtime("sandbox");
        let dev_files = sandbox_root(&rt, "demo");
        let prod_files = crate::paths::data_root()
            .join("plugin-data")
            .join("demo")
            .join("files");
        assert_ne!(dev_files, prod_files);
        assert!(
            !dev_files.starts_with(&prod_files),
            "调试沙盒绝不能落在正式沙盒里面"
        );
        let _ = std::fs::remove_dir_all(rt.db_path.parent().unwrap());
    }
}
