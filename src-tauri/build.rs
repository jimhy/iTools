// 把 App 自有命令纳入 ACL——Tauri v2 默认允许任意窗口调用所有 invoke_handler 命令，
// 列入 AppManifest::commands 后这些命令改为「必须在 capability 显式 allow」，
// 从而能把不可信的插件窗口（itplugin 远程源）限制为只能调 plugin_* 白名单。
// 每个命令会生成 allow-<kebab-name> / deny-<kebab-name> 权限，在 capabilities/*.json 里引用。
// 需 'static，故用 const（AppManifest::commands 要求 &'static [&'static str]）。
const COMMANDS: &[&str] = &[
        // 主程序命令（仅 main/admin 授予）
        "search",
        "execute",
        "record_usage",
        "load_icons",
        "home_data",
        "toggle_pin",
        "get_settings",
        "save_settings",
        "pick_image",
        "pick_app",
        "read_image",
        "read_avatar",
        "get_profile",
        "set_nickname",
        "set_avatar",
        "account_state",
        "account_login",
        "set_data_sync",
        "sync_now",
        "data_usage",
        "logout_account",
        "delete_account",
        "pick_launch_files",
        "pick_launch_folder",
        "add_launch_items",
        "remove_launch_items",
        "run_launch_item",
        "build_launch_item",
        "open_admin_window",
        "close_admin_window",
        "set_settings_persist",
        // 网络：代理连通性实测 + 当前实际生效的同步服务器地址（仅管理中心用）
        "test_proxy",
        "sync_endpoint_info",
        "check_update",
        "get_app_version",
        "open_release_page",
        "download_update",
        "launch_installer_and_quit",
        "open_plugin_window",
        "rescan_plugins",
        "list_plugins",
        "set_plugin_enabled",
        "set_plugin_permission",
        "delete_plugin",
        // 插件安装 / 更新 / 下载源（仅管理中心授予）
        "plugin_install_preview",
        "plugin_install_confirm",
        "plugin_install_cancel",
        "plugin_check_updates",
        "plugin_update",
        "plugin_open_source_page",
        "plugin_mirror_config",
        "plugin_mirror_refresh",
        "plugin_mirror_test",
        "plugin_mirror_set_mode",
        "market_list",
        "market_install_preview",
        // 插件命令（仅 plugin 窗口授予）
        "plugin_take_enter",
        "plugin_hide",
        "plugin_exit",
        "plugin_set_height",
        "plugin_copy_text",
        "plugin_read_text",
        "plugin_read_image",
        "plugin_write_image",
        "plugin_save_image",
        "plugin_read_file",
        "plugin_write_file",
        "plugin_remove_file",
        "plugin_read_local_image",
        "plugin_open_external",
        "plugin_open_path",
        "plugin_notify",
        "plugin_run_command",
        "plugin_fetch",
        "plugin_db_get",
        "plugin_db_set",
        "plugin_db_remove",
        "plugin_db_keys",
        "plugin_list_displays",
        "plugin_capture_full",
        "plugin_capture_region",
        // 由截图覆盖层窗口（capture-overlay）调用，不授予插件窗口
        "capture_region_report",
        "capture_overlay_ready",
        "plugin_register_hotkey",
        "plugin_unregister_hotkey",
        "plugin_create_pin",
        // 由贴图窗口（pin-*）调用，作用于自身
        "pin_resize",
        "pin_move",
        "pin_close",
        "plugin_ocr",
        "plugin_start_audio_record",
        "plugin_stop_audio_record",
        "plugin_start_gif_record",
        "plugin_stop_gif_record",
        "plugin_account_state",
        "plugin_data_get",
        "plugin_data_set",
        "plugin_data_remove",
        "plugin_data_keys",
        "plugin_data_sync",
        // 插件详情页：README 读取 + schema 驱动设置（管理中心按 name 显式 + 插件运行时只读）
        "plugin_readme",
        "plugin_settings_schema",
        "plugin_settings_values",
        "plugin_settings_set",
        "plugin_settings_reset",
        "plugin_get_settings",
        "plugin_get_setting",
        // 开发者中心 / 插件调试环境（仅管理中心授予）
        "dev_list_plugins",
        "dev_rescan",
        "dev_get_config",
        "dev_pick_dir",
        "dev_add_dir",
        "dev_remove_dir",
        "dev_set_search_visible",
        "dev_open_plugin",
        "dev_open_dir",
        "dev_set_permission",
        "dev_storage_list",
        "dev_storage_set",
        "dev_storage_remove",
        "dev_storage_clear",
        "dev_storage_export",
        "dev_storage_import",
        "dev_log_list",
        "dev_log_clear",
        "dev_get_mock",
        "dev_set_mock",
        // 插件提审（打包上传 + 查审核状态；只在管理中心的开发者中心用）
        "dev_publish_status",
        "dev_preflight",
        "dev_submit_plugin",
        "dev_submission_detail",
        // 插件开发 MCP 服务器的运行状态（开发者中心面板显示连接地址用）
        "mcp_status",
        // 由插件调试窗口（plugin-dev）的 bridge 探针调用，**不授予**正式插件窗口
        "dev_log_push",
];

/// 应用清单（Windows）源文件，见 [`emit_app_manifest`]。
const APP_MANIFEST: &str = "windows-app-manifest.xml";

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS))
        // 应用清单改由本文件的 emit_app_manifest 发（tauri 只负责图标 + 版本信息）。
        // 不这么做，`cargo test` 的 lib 单测二进制会拿不到清单而启动即崩，理由见那个函数。
        .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
    tauri_build::try_build(attributes).expect("tauri build 失败");
    emit_app_manifest();
}

/// 把 `windows-app-manifest.xml` 编成资源，链进**所有**目标（bin / bin 单测 / lib 单测 / cdylib）。
///
/// # 为什么必须自己发，不能用 tauri 那份（2026-08-12 实测取证，别再改回去）
///
/// 依赖 `rfd` 会让链接器引入 `comctl32!TaskDialogIndirect` 的导入
/// （`dumpbin /imports itools_lib-*.exe` 可复核，comctl32.dll 那节里就有它）。
/// 该函数只存在于 **comctl32 v6**（WinSxS 的 `Microsoft.Windows.Common-Controls 6.0.0.0`）；
/// `System32\comctl32.dll` 是 v5.82，**不导出**它。要用上 v6 必须有应用清单声明依赖。
///
/// 而 tauri-build 生成的资源是用 `cargo:rustc-link-arg-bins` 发的，
/// **只进 bin 与 bin 的单测二进制，lib 的单测二进制拿不到** ——于是 `cargo test` 产出的
/// `target/debug/deps/itools_lib-*.exe` 没有清单，**一启动就退**，退出码
/// `0xC0000139`（STATUS_ENTRYPOINT_NOT_FOUND），一个用例都跑不了。
///
/// 修法就是这里：`new_without_app_manifest()` 让 tauri 不再发清单，我们改用
/// **不带后缀**的 `cargo:rustc-link-arg`（`embed_resource::compile_for_everything`）发一份
/// ——所有目标各拿一份，且因为 tauri 不再发，不会重复。
///
/// 走过的弯路（别再试）：
/// - `cargo:rustc-link-arg-tests` 只覆盖 `tests/` 目录的集成测试，本仓没有该目录，直接报
///   「does not have a test target」；cargo 也没有「只给 lib 单测」的粒度；
/// - 保留 tauri 的清单、再额外用不带后缀的 `rustc-link-arg` 补发，会让 bin 与 bin 单测
///   重复链接同一类资源 → `CVT1100 资源重复` / `LNK1123`。
///
/// ⚠ `0xC0000139` 极具迷惑性：它发生在**加载期**，与测试代码无关；而任何一次增删依赖都会顺带
/// 重链一次测试 exe，极易被误判成「刚加的那个依赖有毒」——2026-08-12 就误判过一次
/// （详见 `Cargo.toml` 里 ureq 那段）。**再见到测试二进制启动即崩，先查清单，再怀疑依赖。**
fn emit_app_manifest() {
    // 应用清单是 Windows 独有的概念；交叉编译到别的目标时什么都不做
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").expect("缺 CARGO_MANIFEST_DIR"))
        .join(APP_MANIFEST);
    println!("cargo:rerun-if-changed={}", manifest.display());

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("缺 OUT_DIR"));
    // 资源脚本必须与 tauri-build 自己那份（同目录的 resource.rc）**重名不得**，
    // 否则编出来的 .lib 会互相覆盖。
    let rc = out_dir.join("app-manifest.rc");
    // `1 24 "文件"`：资源 ID 1（CREATEPROCESS_MANIFEST_RESOURCE_ID）+ 类型 24（RT_MANIFEST），
    // 与 tauri-winres 的写法等价。rc 的字符串是 C 风格转义，路径里的 `\` 必须换成 `/`。
    let path = manifest.display().to_string().replace('\\', "/");
    std::fs::write(&rc, format!("#pragma code_page(65001)\n1 24 \"{path}\"\n"))
        .expect("写入 app-manifest.rc 失败");
    // manifest_required：编不出来就让构建**失败**，绝不静默产出一个跑不起来的二进制
    embed_resource::compile_for_everything(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("编译应用清单资源失败（rc.exe / llvm-rc 不可用？）");
}
