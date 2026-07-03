// 把 App 自有命令纳入 ACL——Tauri v2 默认允许任意窗口调用所有 invoke_handler 命令，
// 列入 AppManifest::commands 后这些命令改为「必须在 capability 显式 allow」，
// 从而能把不可信的插件窗口（itplugin 远程源）限制为只能调 plugin_* 白名单。
// 每个命令会生成 allow-<kebab-name> / deny-<kebab-name> 权限，在 capabilities/*.json 里引用。
// 需 'static，故用 const（AppManifest::commands 要求 &'static [&'static str]）。
const COMMANDS: &[&str] = &[
        // 主程序命令（仅 main/admin 授予）
        "search",
        "execute",
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
        "set_data_sync",
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
        "open_plugin_window",
        "rescan_plugins",
        "list_plugins",
        "set_plugin_enabled",
        "set_plugin_permission",
        "delete_plugin",
        // 插件命令（仅 plugin 窗口授予）
        "plugin_take_enter",
        "plugin_hide",
        "plugin_exit",
        "plugin_set_height",
        "plugin_copy_text",
        "plugin_read_text",
        "plugin_read_file",
        "plugin_write_file",
        "plugin_open_external",
        "plugin_open_path",
        "plugin_notify",
        "plugin_run_command",
        "plugin_fetch",
        "plugin_db_get",
        "plugin_db_set",
        "plugin_db_remove",
        "plugin_db_keys",
];

fn main() {
    let attributes = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));
    tauri_build::try_build(attributes).expect("tauri build 失败");
}
