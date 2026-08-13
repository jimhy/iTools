//! **前后端契约的闸门（Rust 侧）**：把已纳管的跨 `invoke` 边界结构体的顶层字段名钉死，
//! 并把它们导出成一份机器可读清单，供前端侧的校验脚本消费。
//!
//! 覆盖面：插件安装 / 下载源、设置与搜索、账号与同步、网络（代理测试 / 生效端点）、
//! 插件设置 schema、以及开发者中心（插件调试环境）的全部结构，共 32 个。
//!
//! ⚠ 仍未纳管的是**没有独立 TS interface** 的零散返回值（如 `plugin_fetch` 的
//! `FetchResponse`、`plugin_account_state` 的 `PluginAccountState`、`plugin_take_enter` 的
//! `EnterInfo`——它们的消费方是 `bridge.js` 而非 `src/types.ts`，本闸门比对的是 TS 声明，
//! 对它们无从下手）。新增跨 `invoke` 边界且在 `src/types.ts` 有声明的结构时，
//! **必须**一并加进下面的 `freeze` 清单。
//!
//! # 为什么必须有这个文件
//!
//! 前端的 `invoke<T>()` **只是类型断言**——后端改名 / 增删字段，`tsc --noEmit` 一声不吭，
//! 前端只会静默读到 `undefined`。项目里已连续两轮栽在同一处：
//!
//! - 上上轮：Rust 的 `MirrorConfigView` 是扁平结构、`src/types.ts` 却声明成嵌套，
//!   于是「插件下载源」面板恒显示「没有任何镜像站」；
//! - 上一轮：为此加了字段名快照断言，但它**只守 Rust 一侧**——本轮 Rust 给 `MirrorEntry`
//!   加了 `host` / `unlisted`、快照也同步钉成 9 个字段，`src/types.ts` 仍是 7 个，
//!   Rust 测试全绿、`tsc` 全绿，功能照样断在前端。
//!
//! 所以这道闸必须是**双向**的：Rust 侧在测试里把「实际序列化后的字段名」写进
//! [`SNAPSHOT_PATH`]，前端侧的校验脚本解析 `src/types.ts` 的 interface 与它逐字段比对。
//!
//! # 改字段的正确姿势
//!
//! 1. 改 Rust 结构体；
//! 2. 跑 `cargo test --manifest-path src-tauri/Cargo.toml -j1`
//!    —— [`contract_field_names_frozen`] 会飘红，把期望值改成新的字段集合，
//!    同一次运行会**重新生成** `src-tauri/contract-snapshot.json`；
//! 3. 同步改 `src/types.ts` 里同名的 interface 及所有字段访问点；
//! 4. 跑前端的契约校验脚本确认两侧一致，并把重新生成的 json **一起提交**。
//!
//! # 命名规则（导出的是**实际序列化后的名字**，不要想当然转 camel）
//!
//! - `GitSource` / `InstallPreview` / `PluginUpdate` / `MirrorEntry` / `MirrorConfigView`
//!   / `MirrorTestResult` / `AccountState` / `SyncResult` / `CloudUsage` / `DataUsage`
//!   / `UpdateInfo` / 全部 `Dev*`：带 `#[serde(rename_all = "camelCase")]` 或逐字段 rename → camelCase；
//! - `PluginInfo` / `AppSettings` / `LaunchItem` / `SearchItem` / `HomeData` / `NsCount`
//!   / `ProfileView` / `Settings*`：**没有** `rename_all` → 原样 snake_case（多数字段本就是单词，看不出区别）。
//!
//! # 带 `skip_serializing_if` 的字段
//!
//! 导出的是**实际序列化结果**，所以 `Option` + `skip_serializing_if` 的字段必须构造成
//! `Some(...)` 才会出现在清单里（否则会钉出一份「少了几个字段」的假契约，
//! 前端照着它删字段，等到后端真的返回值时反而读不到）。

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 机器可读字段清单的落盘路径（仓库内固定位置，随代码提交）。
///
/// 格式：`{ "<TS interface 名>": ["<序列化后的字段名>", ...], ... }`，
/// 顶层键与字段数组**均按字典序**排序（BTreeMap + 排序后的 Vec），所以内容是确定性的
/// ——字段没变时重跑测试不会产生 diff。文件内**只有**这一个 map，没有任何元数据键，
/// 消费方可以直接 `Object.entries(json)` 遍历。
fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contract-snapshot.json")
}

/// 序列化后的**顶层字段名**（排序后便于比对）。
fn field_names<T: Serialize>(v: &T) -> Vec<String> {
    let mut names: Vec<String> = serde_json::to_value(v)
        .expect("契约结构必须能序列化")
        .as_object()
        .expect("契约结构必须序列化成 JSON 对象")
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

fn sorted(want: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// 断言某结构体的字段集合与钉死的期望一致，并把**实际**字段名收进导出清单。
///
/// 导出的永远是实际序列化结果而非期望值：期望值写错时测试先红，不可能导出一份假清单。
fn freeze<T: Serialize>(
    out: &mut BTreeMap<String, Vec<String>>,
    ts_name: &str,
    v: &T,
    want: &[&str],
    note: &str,
) {
    let got = field_names(v);
    assert_eq!(
        got,
        sorted(want),
        "{ts_name} 的字段集合变了 → 必须同步改 src/types.ts 里同名的 interface 及所有访问点。{note}"
    );
    out.insert(ts_name.to_string(), got);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountState, EndpointInfo};
    use crate::commands::HomeData;
    use crate::http::ProxyTestResult;
    use crate::dev::logs::DevLogEntry;
    use crate::dev::mock::DevMockConfig;
    use crate::dev::storage::DevKv;
    use crate::dev::{DevConfig, DevDirInfo, DevFeature, DevIssue, DevPluginInfo};
    use crate::plugin::install::{GitSource, InstallPreview, PluginUpdate};
    use crate::plugin::mirror::{MirrorConfigView, MirrorEntry, MirrorTestResult};
    use crate::plugin::settings::{SettingsGroup, SettingsItem, SettingsOption, SettingsSchema};
    use crate::plugin::PluginInfo;
    use crate::profile::{Profile, ProfileView};
    use crate::search::SearchItem;
    use crate::settings::{AppSettings, LaunchItem};
    use crate::sync::{CloudUsage, DataUsage, ItemCount, NsCount, SyncResult};
    use crate::updater::UpdateInfo;

    /// **前后端契约快照**：钉死字段名 + 导出机器可读清单。
    ///
    /// 这份清单（`src-tauri/contract-snapshot.json`）是**给前端契约校验脚本消费的**：
    /// 改了任何一个跨 `invoke` 边界的字段，都必须重跑本测试让脚本重新生成它，
    /// 否则前端校验会拿着过期的清单给出「一切正常」的假结论。
    #[test]
    fn contract_field_names_frozen() {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();

        // ---------- 镜像（mirror.rs） ----------
        let entry = MirrorEntry {
            id: "m".into(),
            label: "m".into(),
            raw: "https://m/{owner}/{repo}/{ref}/{path}".into(),
            archive: "https://m/{owner}/{repo}/{ref}.zip".into(),
            healthy: true,
            latency_ms: Some(1),
            last_ok_at: Some("2026-08-12T10:00:00+08:00".into()),
            host: "m".into(),
            unlisted: true,
        };
        freeze(
            &mut out,
            "MirrorEntry",
            &entry,
            &["id", "label", "raw", "archive", "healthy", "latencyMs", "lastOkAt", "host", "unlisted"],
            "它同时是 MirrorConfigView.mirrors 的数组元素（MirrorView 只是别名）；\
             host / unlisted 由客户端现场判定，UI 靠它们如实标注「这个源到底是谁、是不是内置清单外的新主机」。",
        );

        let view = MirrorConfigView {
            origin: "builtin".into(),
            server_url: None,
            fetched_at: None,
            last_error: None,
            mode: "auto".into(),
            version: 1,
            updated_at: String::new(),
            mirrors: vec![entry],
            official_label: crate::plugin::mirror::official_label(
                crate::plugin::mirror::OFFICIAL_HOST,
            ),
        };
        freeze(
            &mut out,
            "MirrorConfigView",
            &view,
            &["origin", "serverUrl", "fetchedAt", "lastError", "mode", "version", "updatedAt", "mirrors", "officialLabel"],
            "它是**扁平**结构（没有 config / source / error / winnerId）。",
        );

        let test = MirrorTestResult {
            id: "m".into(),
            label: "m".into(),
            target: "raw".into(),
            is_mirror: true,
            url: "https://m/x".into(),
            ok: true,
            latency_ms: Some(1),
            error: None,
            hash_checked: false,
            hash_ok: false,
        };
        freeze(
            &mut out,
            "MirrorTestResult",
            &test,
            &["id", "label", "target", "isMirror", "url", "ok", "latencyMs", "error", "hashChecked", "hashOk"],
            "每个候选源返回两条（target=raw / archive），字段名是 isMirror 不是 official。",
        );

        // ---------- 插件市场（market.rs） ----------
        let entry = crate::plugin::market::MarketEntry {
            name: "demo".into(),
            display_name: "示例".into(),
            description: "d".into(),
            author: "a".into(),
            repo: "https://github.com/o/r".into(),
            path: String::new(),
            version: "1.0.0".into(),
            revision: "0".repeat(40),
            content_hash: "sha256:00".into(),
            category: "效率".into(),
            tags: vec![],
            keywords: vec![],
            permissions: vec![],
            permission_reasons: Default::default(),
            min_app_version: String::new(),
            license: String::new(),
            homepage: String::new(),
            source_repo: String::new(),
            screenshots: vec![],
            revoked: false,
            revoked_reason: String::new(),
            added_at: String::new(),
            audit_report: String::new(),
            file_count: 3,
        };
        freeze(
            &mut out,
            "MarketEntry",
            &entry,
            &[
                "name", "displayName", "description", "author", "repo", "path", "version",
                "revision", "contentHash", "category", "tags", "keywords", "permissions",
                "permissionReasons", "minAppVersion", "license", "homepage", "sourceRepo",
                "screenshots", "revoked", "revokedReason", "addedAt", "auditReport", "fileCount",
            ],
            "字段与 registry/schema/entry.schema.json 对应；contentHash / fileCount 由 CI 回填。\
             改这里要同时改 schema 与 scripts/registry/check.mjs，否则 CI 生成的 index.json 客户端读不全。",
        );

        let view = crate::plugin::market::MarketView {
            plugins: vec![entry],
            origin: "remote".into(),
            error: None,
            installed: Default::default(),
            source: "o/r@main:registry/index.json".into(),
        };
        freeze(
            &mut out,
            "MarketView",
            &view,
            &["plugins", "origin", "error", "installed", "source"],
            "origin 区分 remote/cache/none —— 拿缓存与刚更新过对用户是不同的信息，不能只给 plugins。",
        );

        // ---------- 安装 / 更新（install.rs） ----------
        let source = GitSource {
            host: "github".into(),
            domain: "github.com".into(),
            owner: "o".into(),
            repo: "r".into(),
            sub_path: String::new(),
            revision: String::new(),
            url: "https://github.com/o/r.git".into(),
            page_url: "https://github.com/o/r".into(),
        };
        freeze(
            &mut out,
            "GitSource",
            &source,
            &["host", "domain", "owner", "repo", "subPath", "revision", "url", "pageUrl"],
            "它以 InstallPreview.source / PluginInfo.source 的形态发给前端（host 是**风格**、domain 才是域名）。",
        );

        let preview = InstallPreview {
            token: "t".into(),
            name: "demo".into(),
            version: "1.0.0".into(),
            description: String::new(),
            author: String::new(),
            permissions: Vec::new(),
            feature_count: 0,
            cmds: Vec::new(),
            logo: None,
            readme: None,
            source: source.clone(),
            already_installed: false,
            installed_version: None,
            same_source: false,
            is_builtin: false,
            file_count: 0,
            total_bytes: 0,
            download_source: crate::plugin::mirror::official_label(
                crate::plugin::mirror::OFFICIAL_HOST,
            ),
            via_mirror: false,
            hash_verified: false,
        };
        freeze(
            &mut out,
            "InstallPreview",
            &preview,
            &[
                "token", "name", "version", "description", "author", "permissions", "featureCount",
                "cmds", "logo", "readme", "source", "alreadyInstalled", "installedVersion",
                "sameSource", "isBuiltin", "fileCount", "totalBytes", "downloadSource", "viaMirror",
                "hashVerified",
            ],
            "sameSource / downloadSource / viaMirror / hashVerified 是安装弹窗必须如实展示的四项。",
        );

        let update = PluginUpdate {
            name: "demo".into(),
            current_version: "1.0.0".into(),
            latest_version: None,
            has_update: false,
            pinned: false,
            checked: false,
            error: None,
            download_source: None,
            via_mirror: false,
        };
        freeze(
            &mut out,
            "PluginUpdate",
            &update,
            &[
                "name", "currentVersion", "latestVersion", "hasUpdate", "pinned", "checked",
                "error", "downloadSource", "viaMirror",
            ],
            "checked 用来区分「已是最新」与「根本没检查」。",
        );

        // ---------- 插件列表（plugin/mod.rs）：**没有 rename_all，走 snake_case** ----------
        let info = PluginInfo {
            name: "demo".into(),
            display_name: "示例插件".into(),
            description: String::new(),
            version: "1.0.0".into(),
            author: String::new(),
            feature_count: 0,
            cmds: Vec::new(),
            logo: None,
            enabled: true,
            permissions: Vec::new(),
            granted: Vec::new(),
            has_readme: false,
            has_settings: false,
            source: Some(source),
            builtin: false,
        };
        freeze(
            &mut out,
            "PluginInfo",
            &info,
            &[
                "name", "display_name", "description", "version", "author", "feature_count",
                "cmds", "logo", "enabled", "permissions", "granted", "has_readme", "has_settings",
                "source", "builtin",
            ],
            "本结构**没有** #[serde(rename_all)]：字段名原样 snake_case（feature_count / has_readme / has_settings）。",
        );

        // ---------- 设置（settings.rs）：**没有 rename_all，走 snake_case** ----------
        let mut settings = AppSettings::default();
        settings
            .plugin_window_sizes
            .insert("demo".into(), [960.0, 660.0]);
        freeze(
            &mut out,
            "AppSettings",
            &settings,
            &[
                "opacity", "background_image", "hotkey", "custom_apps", "autostart", "theme",
                "background_enabled", "background_dim", "search_placeholder", "separate_hotkey",
                "auto_clear_seconds", "proxy_enabled", "proxy_address", "local_launch_items",
                "disabled_plugins", "plugin_permissions", "plugin_window_sizes",
                "plugin_mirror_mode", "screenshot_hotkey", "pin_hotkey", "dev_plugin_dirs",
                "dev_plugin_permissions", "dev_search_visible", "sync_endpoint",
            ],
            "整包保存（save_settings）必须逐字段对齐：TS 声明里漏掉的字段会在保存时被后端\
             按缺省值写回，等于静默清空用户数据（plugin_window_sizes 就是这么丢的）。\
             本结构**没有** #[serde(rename_all)]：字段名原样 snake_case。",
        );

        freeze(
            &mut out,
            "LaunchItem",
            &LaunchItem {
                id: r"C:\x\a.exe".into(),
                path: r"C:\x\a.exe".into(),
                name: "a.exe".into(),
                is_dir: false,
            },
            &["id", "path", "name", "is_dir"],
            "AppSettings.local_launch_items 的数组元素；同样是 snake_case（is_dir）。",
        );

        // ---------- 搜索 / 主面板 ----------
        freeze(
            &mut out,
            "SearchItem",
            &SearchItem {
                id: "app::x".into(),
                title: "x".into(),
                subtitle: String::new(),
                kind: "app".into(),
                target: "x".into(),
                icon: Some(String::new()),
                action: "open".into(),
            },
            &["id", "title", "subtitle", "kind", "target", "icon", "action"],
            "主搜索每一条结果；kind=plugin 时 target 形如 `<id>#<code>`，\
             调试插件多带一个 `dev:` 前缀（后端据此路由到调试窗口）。",
        );

        freeze(
            &mut out,
            "HomeData",
            &HomeData {
                user: "u".into(),
                recent: Vec::new(),
                pinned: Vec::new(),
            },
            &["user", "recent", "pinned"],
            "主面板首屏数据。",
        );

        // ---------- 账号 / 同步 ----------
        freeze(
            &mut out,
            "ProfileView",
            &ProfileView {
                profile: Profile {
                    nickname: String::new(),
                    avatar_path: None,
                    phone: String::new(),
                    first_use_ts: 0,
                },
                companion_days: 1,
            },
            &["nickname", "avatar_path", "phone", "first_use_ts", "companion_days"],
            "Profile 被 #[serde(flatten)] 摊平进来，所以顶层同时有 Profile 的字段；\
             TS 侧对应 `ProfileView extends Profile`。",
        );

        freeze(
            &mut out,
            "AccountState",
            &AccountState {
                logged_in: false,
                username: String::new(),
                cloud_configured: false,
                sync_enabled: false,
            },
            &["loggedIn", "username", "cloudConfigured", "syncEnabled"],
            "不含 token（凭据绝不外泄给前端）。",
        );

        freeze(
            &mut out,
            "SyncResult",
            &SyncResult {
                synced: false,
                reason: Some("offline".into()),
                pushed: 0,
                pulled: 0,
                message: Some(String::new()),
            },
            &["synced", "reason", "pushed", "pulled", "message"],
            "reason / message 带 skip_serializing_if，这里必须构造成 Some 才能钉住完整字段集；\
             调试环境的同步 Mock 复用的就是**同一个结构体**，形态必须一致（否则调试通了、上线崩）。",
        );

        freeze(
            &mut out,
            "NsCount",
            &NsCount {
                ns: "app".into(),
                count: 0,
                // items 带 skip_serializing_if：必须构造成非空才会出现在清单里，
                // 否则会钉出一份「少了 items」的假契约（见文件头说明）。
                items: vec![ItemCount { label: "笔记".into(), count: 2 }],
            },
            &["ns", "count", "items"],
            "DataUsage.local / CloudUsage.counts 的数组元素；items 带 skip_serializing_if（空则不出现）。",
        );

        freeze(
            &mut out,
            "ItemCount",
            &ItemCount { label: "笔记".into(), count: 2 },
            &["label", "count"],
            "NsCount.items 的数组元素：插件声明的业务条目口径。",
        );

        freeze(
            &mut out,
            "CloudUsage",
            &CloudUsage {
                counts: Vec::new(),
                total: 0,
                bytes: 0,
            },
            &["counts", "total", "bytes"],
            "只有真的从服务端取到才有值，取不到是 DataUsage.cloud=null。",
        );

        freeze(
            &mut out,
            "DataUsage",
            &DataUsage {
                local: Vec::new(),
                local_total: 0,
                local_only: Vec::new(),
                local_only_total: 0,
                cloud: None,
                cloud_reason: Some("offline".into()),
            },
            &[
                "local",
                "localTotal",
                "localOnly",
                "localOnlyTotal",
                "cloud",
                "cloudReason",
            ],
            "cloudReason 带 skip_serializing_if（云端可用时不出现），这里构造成 Some 钉全字段。",
        );

        // ---------- 网络（http.rs / account.rs） ----------
        freeze(
            &mut out,
            "ProxyTestResult",
            &ProxyTestResult {
                ok: true,
                latency_ms: Some(120),
                error: Some(String::new()),
                target: "https://raw.githubusercontent.com/octocat/Hello-World/HEAD/README".into(),
                via_proxy: true,
            },
            &["ok", "latencyMs", "error", "target", "viaProxy"],
            "「测试」按钮的真实探测结果：viaProxy 证明这一次确实走了代理（不是悄悄直连测了个寂寞），\
             error 区分「连不上代理 / 代理拒绝 / 目标不可达」，UI 必须原样展示、不得笼统成「失败」。",
        );

        freeze(
            &mut out,
            "EndpointInfo",
            &EndpointInfo {
                effective: Some(crate::account::DEV_DEFAULT_ENDPOINT.into()),
                source: "devDefault".into(),
                user_value: String::new(),
            },
            &["effective", "source", "userValue"],
            "source ∈ env | user | devDefault | none。userValue 为空而 effective 有值时，\
             说明生效的是环境变量或 dev 默认地址——「网络」页签必须把这层区别显示出来，\
             否则用户会像本轮一样，在没填过任何地址的情况下完全不知道客户端在连哪儿。",
        );

        // ---------- 更新 ----------
        freeze(
            &mut out,
            "UpdateInfo",
            &UpdateInfo {
                latest_version: "1.0.0".into(),
                current_version: "1.0.0".into(),
                has_update: false,
                release_url: String::new(),
                release_notes: String::new(),
                msi_url: None,
            },
            &[
                "latestVersion", "currentVersion", "hasUpdate", "releaseUrl", "releaseNotes",
                "msiUrl",
            ],
            "msiUrl 为 null 时前端不该提供「立即更新」（只能去 release 页手动下）。",
        );

        // ---------- 插件设置 schema ----------
        freeze(
            &mut out,
            "SettingsOption",
            &SettingsOption {
                value: serde_json::Value::Null,
                label: String::new(),
            },
            &["value", "label"],
            "select 控件的一个选项。",
        );

        freeze(
            &mut out,
            "SettingsItem",
            &SettingsItem {
                key: "k".into(),
                kind: "text".into(),
                label: String::new(),
                description: String::new(),
                default: serde_json::Value::Null,
                options: Vec::new(),
                min: Some(0.0),
                max: Some(1.0),
                step: Some(1.0),
                mode: Some("folder".into()),
                placeholder: String::new(),
            },
            &[
                "key", "type", "label", "description", "default", "options", "min", "max", "step",
                "mode", "placeholder",
            ],
            "字段名是 `type` 不是 `kind`（Rust 侧 #[serde(rename = \"type\")]）；\
             min/max/step/mode 带 skip_serializing_if，这里构造成 Some 钉全字段集。",
        );

        freeze(
            &mut out,
            "SettingsGroup",
            &SettingsGroup {
                title: String::new(),
                description: String::new(),
                items: Vec::new(),
            },
            &["title", "description", "items"],
            "SettingsSchema.groups 的数组元素。",
        );

        freeze(
            &mut out,
            "SettingsSchema",
            &SettingsSchema {
                version: 1,
                groups: Vec::new(),
            },
            &["version", "groups"],
            "规范化后顶层永远是 groups（settings.json 里的顶层 items 会被归一化成一个匿名组）。",
        );

        // ---------- 开发者中心：插件调试环境（dev/*，全部 camelCase） ----------
        let issue = DevIssue {
            level: "warn".into(),
            field: "name".into(),
            message: String::new(),
        };
        freeze(
            &mut out,
            "DevIssue",
            &issue,
            &["level", "field", "message"],
            "level=error 表示致命（该插件 runnable=false）；field 精确到 `features[0].cmds[1]` 这种下标路径。",
        );

        let feature = DevFeature {
            code: "main".into(),
            explain: String::new(),
            keywords: Vec::new(),
            has_regex: false,
            has_text: false,
        };
        freeze(
            &mut out,
            "DevFeature",
            &feature,
            &["code", "explain", "keywords", "hasRegex", "hasText"],
            "「触发模拟器」按它渲染可点的触发入口。",
        );

        freeze(
            &mut out,
            "DevDirInfo",
            &DevDirInfo {
                path: String::new(),
                fixed: true,
                exists: true,
                plugin_count: 0,
            },
            &["path", "fixed", "exists", "pluginCount"],
            "fixed=true 的是固定 dev-plugins/ 根，UI 不得给它「移除」按钮；\
             exists=false 是「目录被删/改名」的如实标注，不是加载失败。",
        );

        freeze(
            &mut out,
            "DevPluginInfo",
            &DevPluginInfo {
                id: "demo".into(),
                dir: String::new(),
                root: String::new(),
                name: "demo".into(),
                version: "1.0.0".into(),
                description: String::new(),
                author: String::new(),
                logo: None,
                feature_count: 0,
                cmds: Vec::new(),
                permissions: Vec::new(),
                granted: Vec::new(),
                runnable: true,
                issues: vec![issue],
                features: vec![feature],
            },
            &[
                "id", "dir", "root", "name", "version", "description", "author", "logo",
                "featureCount", "cmds", "permissions", "granted", "runnable", "issues", "features",
            ],
            "id 取**目录名**（name 可能写错，目录名才是可靠锚点，所有 dev_* 命令都按它寻址）；\
             granted 来自**独立的**调试授权表，与正式插件的 PluginInfo.granted 无关。",
        );

        freeze(
            &mut out,
            "DevLogEntry",
            &DevLogEntry {
                seq: 1,
                at: String::new(),
                plugin_id: "demo".into(),
                kind: "api".into(),
                level: "info".into(),
                method: "db.set".into(),
                args: String::new(),
                result: String::new(),
                ms: 0.0,
                ok: true,
            },
            &[
                "seq", "at", "pluginId", "kind", "level", "method", "args", "result", "ms", "ok",
            ],
            "seq 由后端统一分配、全局单调（清空也不回退），前端用它做增量拉取；\
             args/result 超长会被后端截断（尾部带「（已截断）」）。",
        );

        freeze(
            &mut out,
            "DevMockConfig",
            &DevMockConfig::default(),
            &["mode", "delayMs", "reason", "pushed", "pulled"],
            "写入时会被夹紧到可执行范围（delayMs ≤ 10s、未知 mode 归一为 success），\
             dev_get_mock 读回的就是实际会发生的行为——不存在「设了不生效」的控件。",
        );

        freeze(
            &mut out,
            "DevKv",
            &DevKv {
                key: "k".into(),
                value: String::new(),
                bytes: 0,
                updated_at: None,
            },
            &["key", "value", "bytes", "updatedAt"],
            "updatedAt 只有 kind=\"data\" 才有（plugin_kv 表本就不记时间），\
             为 null 时 UI 显示「—」，不许编造时间。",
        );

        freeze(
            &mut out,
            "DevConfig",
            &DevConfig {
                search_visible: false,
                dirs: Vec::new(),
                mock: DevMockConfig::default(),
                db_path: String::new(),
                files_root: String::new(),
            },
            &["searchVisible", "dirs", "mock", "dbPath", "filesRoot"],
            "dbPath / filesRoot 是**真实路径**，UI 直接展示，让开发者自己确认写的不是正式库。",
        );

        // ---------- 落盘：供前端契约校验脚本读取 ----------
        let json = serde_json::to_string_pretty(&out).expect("契约清单必须能序列化");
        let path = snapshot_path();
        std::fs::write(&path, format!("{json}\n"))
            .unwrap_or_else(|e| panic!("写契约清单 {} 失败: {e}", path.display()));
    }
}
