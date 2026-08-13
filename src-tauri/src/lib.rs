// iTools 主库入口：命令注册、协议、托盘、窗口、插件系统。
mod account;
mod commands;
/// 前后端契约快照（纯测试模块：钉死跨 invoke 边界的字段名 + 导出机器可读清单）。
#[cfg(test)]
mod contract;
mod db;
/// 插件调试环境（开发者中心）：与正式插件物理隔离的加载源 / 存储 / 沙盒 / 授权 / 同步 Mock。
mod dev;
mod hotkey;
/// **统一出站 HTTP 出口**：全应用所有对外请求都从这里取 Agent，网络代理才可能真正生效。
mod http;
mod launch;
mod logging;
mod plugin;
mod profile;
mod search;
mod settings;
mod store;
mod sync;
mod updater;
mod window;

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager,
};
use tauri_plugin_global_shortcut::{
    Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState,
};

use account::AccountStore;
use logging::ilog;
use profile::ProfileStore;
use search::SearchIndex;
use settings::SettingsStore;
use sync::DataStore;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 最先初始化文件日志（exe 同目录 itools.log），后续所有 [iTools] 日志都落文件+stderr
    logging::init();
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_notification::init())
        // 自定义协议：itplugin://localhost/<id>/<path>（Windows 上为 http://itplugin.localhost）
        // 把插件目录下的 HTML/资源喂给插件窗口。运行时从 PluginRegistry 拿根（定位在 setup 用 app 完成）。
        .register_uri_scheme_protocol("itplugin", |ctx, request| {
            use tauri::Manager;
            let root = ctx
                .app_handle()
                .try_state::<plugin::PluginRegistry>()
                .map(|r| r.root.clone())
                .unwrap_or_default();
            plugin::serve(&root, &request)
        })
        // 调试插件资源：itplugindev://localhost/<id>/<path>（Windows 上为 http://itplugindev.localhost）
        // 与 itplugin 的区别：调试插件分散在多个调试根，按 id→目录映射表解析（见 dev::serve）。
        .register_uri_scheme_protocol(dev::DEV_SCHEME, |ctx, request| {
            use tauri::Manager;
            match ctx.app_handle().try_state::<std::sync::Arc<dev::DevRuntime>>() {
                Some(rt) => dev::serve(&rt, &request),
                None => plugin::serve_status(503),
            }
        })
        // 截图框选覆盖层：itoverlay://localhost/overlay.html（内嵌页）+ /frozen.png（冻结的整屏）
        .register_uri_scheme_protocol("itoverlay", |ctx, request| {
            use tauri::Manager;
            let ok = |mime: &str, body: Vec<u8>| {
                tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", mime)
                    .header("Access-Control-Allow-Origin", "*")
                    .header(
                        "Content-Security-Policy",
                        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
                    )
                    .body(body)
                    .unwrap()
            };
            if request.uri().path().ends_with("frozen.png") {
                if let Some(flow) = ctx.app_handle().try_state::<plugin::capture::CaptureFlow>() {
                    if let Some(bmp) = flow.frozen_png() {
                        // 冻结图现为 BMP（编码近乎瞬时）；浏览器按 content-type 解码，与 <img> 无关扩展名
                        return ok("image/bmp", bmp);
                    }
                }
                return tauri::http::Response::builder().status(404).body(Vec::new()).unwrap();
            }
            ok(
                "text/html; charset=utf-8",
                include_str!("plugin/overlay.html").as_bytes().to_vec(),
            )
        })
        // 贴图浮窗：itpin://localhost/view/<id>（pin 页）+ /img/<id>（按 id 取图）
        .register_uri_scheme_protocol("itpin", |ctx, request| {
            use tauri::Manager;
            let ok = |mime: &str, body: Vec<u8>| {
                tauri::http::Response::builder()
                    .status(200)
                    .header("Content-Type", mime)
                    .header("Access-Control-Allow-Origin", "*")
                    .header(
                        "Content-Security-Policy",
                        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'",
                    )
                    .body(body)
                    .unwrap()
            };
            let path = request.uri().path();
            if let Some(id) = path.strip_prefix("/img/") {
                if let Some(pins) = ctx.app_handle().try_state::<plugin::pin::Pins>() {
                    if let Some(png) = pins.img(id) {
                        return ok("image/png", png);
                    }
                }
                return tauri::http::Response::builder().status(404).body(Vec::new()).unwrap();
            }
            ok(
                "text/html; charset=utf-8",
                include_str!("plugin/pin.html").as_bytes().to_vec(),
            )
        })
        .setup(|app| {
            // 统一本地存储：单个 SQLite 库（%LOCALAPPDATA%\itools\itools.db），取代散落的
            // usage/settings/account/profile.json 及插件 KV(kv.json)/本地优先数据(data/<ns>.json)；
            // 首启自动从旧 JSON 惰性迁移。插件沙盒文件（writeFile）仍在文件系统，不入库。
            let db = std::sync::Arc::new(db::Db::open_default());
            // 设置最先加载：搜索索引与视觉效果都依赖它
            let settings_store = SettingsStore::load(db.clone());
            let current = settings_store.get();
            // 用户手填的云端地址（设置里读；env/debug 兜底见 account::cloud_endpoint）装入运行期。
            account::set_user_endpoint(&current.sync_endpoint);
            // 出站代理装入运行期：之后所有网络请求（插件下载 / 更新 / 云同步 / 登录 / itools.fetch）
            // 都按它选链路（本机与内网地址恒直连，见 http::is_bypass_host）。
            // 地址非法时 refresh 内部已 fail-closed 回退直连，这里只如实记一笔，不阻断启动。
            if let Err(e) = http::refresh(current.proxy_enabled, &current.proxy_address) {
                ilog!("[iTools] 代理配置无效，本次启动全部请求直连：{e}");
            }
            // 启用代理时 refresh 内部已记过一笔；这里补上「直连」那一半，
            // 使日志里**永远**有一行说明本次启动的出站链路（排查用户网络问题的第一现场）。
            if !http::proxy_configured() {
                ilog!("[iTools] 出站链路：直连（未启用代理）");
            }
            // 插件下载源偏好（auto/official/mirror）装入运行期：镜像模块据此决定候选源。
            plugin::mirror::set_mode(&current.plugin_mirror_mode);

            let search_index = SearchIndex::new(current.custom_apps.clone());
            // 「本地启动」清单里的项一并纳入搜索（可在主搜索栏搜到并打开）
            search_index.set_custom_items(current.local_launch_items.clone());
            // 解析插件根：dev 用项目 plugins/，打包用可写的 appData（首启从随包资源播种）
            let plugins_root = plugin::resolve_plugins_root(app.handle());
            // 登记「随包内置插件」名单（resource_dir/plugins 下的目录名）：这些插件每次启动都会被
            // 播种回来，禁止 Git 安装覆盖。必须在扫描/列表之前完成，否则 PluginInfo.builtin 会漏判。
            // 传入插件根：只有「会被资源目录播种的那个可写根」才登记内置名单，
            // 否则 dev 下（插件根是项目 plugins/，与 target/debug/plugins 是两份拷贝）会把 5 个示例插件全锁死。
            plugin::install::init_builtins(app.handle(), &plugins_root);
            // 清理上次进程崩溃残留的安装暂存（.staging/），避免半截包留在插件根下。
            plugin::install::cleanup_staging(&plugins_root);
            // 恢复「更新失败且回滚也失败」寄存在 .recover-<name>/ 的旧插件：
            // 那种失败多半是目录被杀软/资源管理器临时锁住，重启这一刻锁已释放，直接搬回原位。
            // 必须在 scan_plugins 之前，否则本次启动会漏掉刚恢复的插件。
            plugin::install::recover_orphans(&plugins_root);
            if plugins_root.is_dir() {
                ilog!("[iTools] 插件目录: {}", plugins_root.display());
            } else {
                ilog!("[iTools] 插件目录不存在: {}", plugins_root.display());
            }
            // 插件调试环境：独立的调试根 + 独立测试库 + 独立沙盒（与正式插件互不可见）。
            // 必须在扫描 / 建索引之前建好——「调试插件进主搜索」开关开着时首屏就要并进去。
            let dev_runtime = std::sync::Arc::new(dev::DevRuntime::new(
                dev::dev_home(),
                dev::resolve_fixed_root(),
            ));
            // 有新调试日志时给管理中心推 `dev-log` 事件（面板收到就立刻增量拉一次）。
            // 只是加速器：面板本身有定时增量拉取兜底，事件丢了也不会丢日志。
            dev_runtime.logs.attach(app.handle().clone());
            let dev_n = dev_runtime.rescan(&current.dev_plugin_dirs);
            ilog!(
                "[iTools] 调试插件目录: {}（{dev_n} 个调试插件）",
                dev_runtime.fixed_root.display()
            );

            // 扫描插件目录并注入搜索（页面插件，搜到后回车打开插件页；禁用的不参与搜索）
            let loaded_plugins = plugin::scan_plugins(&plugins_root);
            let disabled_plugins = current.disabled_plugins.clone();
            let mut plugin_cmds: Vec<_> = plugin::expand_commands(&loaded_plugins)
                .into_iter()
                .filter(|c| !disabled_plugins.contains(&c.plugin_id))
                .collect();
            // 调试插件默认**不进**主搜索；开关开启时才并入（id 带 dev: 前缀，回车走调试窗口）
            if current.dev_search_visible {
                plugin_cmds.extend(dev_runtime.search_commands());
            }
            search_index.set_plugin_commands(plugin_cmds);
            app.manage(search_index);
            app.manage(store::UsageStore::load(db.clone()));
            app.manage(settings_store);
            // 账号资料（个人中心）——home_data 等命令依赖它，必须在 setup 里 manage
            app.manage(ProfileStore::load(db.clone()));
            // 云账号登录态（本地优先）+ 本地优先数据层（云同步引擎）
            app.manage(AccountStore::load(db.clone()));
            app.manage(DataStore::load(db.clone()));
            // 「登录后自动同步」调度器：数据变更后防抖自动上行（受 sync_enabled + 登录态门禁）
            app.manage(sync::AutoSync::default());
            // 统一 SQLite 库句柄：插件 KV 命令（plugin_db_*）注入它读写 plugin_kv 表
            app.manage(db);
            // 插件运行期注册表（open_plugin_window / plugin_* 命令依赖）。
            // 调试运行时同时挂在它上面（供 plugin_* 命令按会话分流）与作为独立 state（供 dev_* 命令）。
            let plugins_root_watch = plugins_root.clone();
            app.manage(plugin::PluginRegistry::new(
                plugins_root,
                loaded_plugins,
                dev_runtime.clone(),
            ));
            app.manage(dev_runtime);
            // 插件热更新：监听 plugins/ 目录，改动后自动重扫 + 刷新已打开插件窗（免重启 / 免手动重载）
            plugin::watch::start(app.handle(), plugins_root_watch);
            // 从 Git 安装插件的暂存区（预览 → 确认之间的中转）
            app.manage(plugin::install::InstallStaging::default());
            // 区域截图流程状态（冻结图 + 选区结果通道）
            app.manage(plugin::capture::CaptureFlow::default());
            // 插件全局热键注册表
            app.manage(plugin::hotkey::PluginHotkeys::default());
            // 贴图图片仓
            app.manage(plugin::pin::Pins::default());
            // 录音 / 录屏 运行期状态
            app.manage(plugin::audio::AudioState::default());
            app.manage(plugin::record::RecordState::default());
            // 宿主内置截图（headless）的热键状态
            app.manage(plugin::capture::ScreenshotState::default());
            // 宿主内置贴图（headless）的热键状态
            app.manage(plugin::pin::PinHotkeyState::default());

            // 全局快捷键：任意已注册热键的 Pressed 事件即切换主窗口
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(|app, shortcut, event| {
                        if event.state() == ShortcutState::Pressed {
                            // 宿主截图热键最优先（原生 headless 截图）；
                            // 否则命中插件热键则唤起插件；再否则切换主窗口
                            if plugin::capture::is_screenshot_hotkey(app, shortcut.id()) {
                                plugin::capture::trigger_screenshot(app, false);
                            } else if plugin::pin::is_pin_hotkey(app, shortcut.id()) {
                                plugin::pin::trigger_pin(app);
                            } else if !plugin::hotkey::dispatch(app, shortcut.id()) {
                                window::toggle(app);
                            }
                        }
                    })
                    .build(),
            )?;
            register_toggle_hotkey(app.handle(), &current.hotkey);
            // 宿主内置截图热键（默认 ctrl+shift+a，可在设置里改；空 = 不注册）
            if !current.screenshot_hotkey.trim().is_empty() {
                if let Err(e) =
                    plugin::capture::register_screenshot_hotkey(app.handle(), &current.screenshot_hotkey)
                {
                    ilog!("[iTools] 截图热键注册失败：{e}");
                }
            }
            // 宿主内置贴图热键（默认 f3，可在设置里改；空 = 不注册）
            if !current.pin_hotkey.trim().is_empty() {
                if let Err(e) = plugin::pin::register_pin_hotkey(app.handle(), &current.pin_hotkey) {
                    ilog!("[iTools] 贴图热键注册失败：{e}");
                }
            }

            // 主窗口毛玻璃（透明度来自设置）+ 圆角
            if let Some(win) = app.get_webview_window("main") {
                window::apply_effects(&win, current.opacity);
                // 每次主窗口显示/获焦时按最新设置重应用毛玻璃不透明度：
                // 在管理中心调「搜索框不透明度」时主窗口是隐藏的，隐藏态改 Acrylic 底色不一定即时生效，
                // 显示时补应用一次，保证调完再唤起就能看到新透明度。
                let main_win = win.clone();
                win.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(true) = event {
                        if let Some(store) = main_win.app_handle().try_state::<SettingsStore>() {
                            window::apply_opacity(&main_win, store.get().opacity);
                        }
                    }
                });
            }

            // 管理中心窗口（静态创建、常驻复用）：
            // 常规大窗口——点 X = 隐藏而非销毁（下次秒开），失焦不隐藏（区别于旧的小设置窗）。
            if let Some(admin_win) = app.get_webview_window("admin") {
                // 无边框不透明大窗：DWM 圆角，避免四角露出 WebView2 白底
                window::apply_rounded(&admin_win);
                let win_hide = admin_win.clone();
                admin_win.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = win_hide.hide();
                    }
                });
            }

            // 开机自启与设置对齐（仅在设置开启且系统未注册时补注册）
            if current.autostart {
                use tauri_plugin_autostart::ManagerExt;
                let autostart = app.autolaunch();
                if !autostart.is_enabled().unwrap_or(false) {
                    if let Err(err) = autostart.enable() {
                        ilog!("[iTools] 开机自启注册失败: {err}");
                    }
                }
            }

            setup_tray(app.handle())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::execute,
            commands::record_usage,
            commands::load_icons,
            commands::home_data,
            commands::toggle_pin,
            commands::get_settings,
            commands::save_settings,
            commands::pick_image,
            commands::pick_app,
            commands::read_image,
            commands::read_avatar,
            commands::get_profile,
            commands::set_nickname,
            commands::set_avatar,
            commands::account_state,
            commands::account_login,
            commands::set_data_sync,
            commands::sync_now,
            commands::data_usage,
            commands::logout_account,
            commands::delete_account,
            commands::pick_launch_files,
            commands::pick_launch_folder,
            commands::add_launch_items,
            commands::remove_launch_items,
            commands::run_launch_item,
            commands::build_launch_item,
            commands::open_admin_window,
            commands::close_admin_window,
            commands::set_settings_persist,
            // 网络：代理连通性实测 + 「当前实际生效的同步服务器地址」
            http::test_proxy,
            account::sync_endpoint_info,
            updater::check_update,
            updater::get_app_version,
            updater::open_release_page,
            updater::download_update,
            updater::launch_installer_and_quit,
            plugin::commands::open_plugin_window,
            plugin::commands::plugin_take_enter,
            plugin::commands::rescan_plugins,
            plugin::commands::list_plugins,
            plugin::commands::set_plugin_enabled,
            plugin::commands::set_plugin_permission,
            plugin::commands::delete_plugin,
            plugin::install::plugin_install_preview,
            plugin::install::plugin_install_confirm,
            plugin::install::plugin_install_cancel,
            plugin::install::plugin_check_updates,
            plugin::install::plugin_update,
            plugin::install::plugin_open_source_page,
            plugin::mirror::plugin_mirror_config,
            plugin::mirror::plugin_mirror_refresh,
            plugin::mirror::plugin_mirror_test,
            plugin::mirror::plugin_mirror_set_mode,
            plugin::market::market_list,
            plugin::market::market_install_preview,
            plugin::settings::plugin_readme,
            plugin::settings::plugin_settings_schema,
            plugin::settings::plugin_settings_values,
            plugin::settings::plugin_settings_set,
            plugin::settings::plugin_settings_reset,
            plugin::commands::plugin_hide,
            plugin::commands::plugin_exit,
            plugin::commands::plugin_set_height,
            plugin::commands::plugin_copy_text,
            plugin::commands::plugin_read_text,
            plugin::commands::plugin_read_image,
            plugin::commands::plugin_write_image,
            plugin::commands::plugin_save_image,
            plugin::commands::plugin_read_file,
            plugin::commands::plugin_write_file,
            plugin::commands::plugin_remove_file,
            plugin::commands::plugin_read_local_image,
            plugin::commands::plugin_open_external,
            plugin::commands::plugin_open_path,
            plugin::commands::plugin_notify,
            plugin::commands::plugin_run_command,
            plugin::commands::plugin_fetch,
            plugin::commands::plugin_db_get,
            plugin::commands::plugin_db_set,
            plugin::commands::plugin_db_remove,
            plugin::commands::plugin_db_keys,
            plugin::settings::plugin_get_settings,
            plugin::settings::plugin_get_setting,
            plugin::capture::plugin_list_displays,
            plugin::capture::plugin_capture_full,
            plugin::capture::plugin_capture_region,
            plugin::capture::capture_region_report,
            plugin::capture::capture_overlay_ready,
            plugin::hotkey::plugin_register_hotkey,
            plugin::hotkey::plugin_unregister_hotkey,
            plugin::pin::plugin_create_pin,
            plugin::pin::pin_resize,
            plugin::pin::pin_move,
            plugin::pin::pin_close,
            plugin::ocr::plugin_ocr,
            plugin::audio::plugin_start_audio_record,
            plugin::audio::plugin_stop_audio_record,
            plugin::record::plugin_start_gif_record,
            plugin::record::plugin_stop_gif_record,
            plugin::commands::plugin_account_state,
            plugin::commands::plugin_data_get,
            plugin::commands::plugin_data_set,
            plugin::commands::plugin_data_remove,
            plugin::commands::plugin_data_keys,
            plugin::commands::plugin_data_sync,
            // 开发者中心（仅管理中心窗口可调）
            dev::commands::dev_list_plugins,
            dev::commands::dev_rescan,
            dev::commands::dev_get_config,
            dev::commands::dev_pick_dir,
            dev::commands::dev_add_dir,
            dev::commands::dev_remove_dir,
            dev::commands::dev_set_search_visible,
            dev::commands::dev_open_plugin,
            dev::commands::dev_open_dir,
            dev::commands::dev_set_permission,
            dev::commands::dev_log_list,
            dev::commands::dev_log_clear,
            dev::commands::dev_get_mock,
            dev::commands::dev_set_mock,
            dev::storage::dev_storage_list,
            dev::storage::dev_storage_set,
            dev::storage::dev_storage_remove,
            dev::storage::dev_storage_clear,
            dev::storage::dev_storage_export,
            dev::storage::dev_storage_import,
            // 插件调试窗口的 bridge 探针上报（只在 capabilities/plugin-dev.json 放行）
            dev::logs::dev_log_push
        ])
        .run(tauri::generate_context!())
        .expect("运行 iTools 失败");
}

/// 注册唤起热键：优先用设置里的组合；无效/被占用则回退候选链。
/// 全部失败也不 panic（可用托盘唤起）。
pub fn register_toggle_hotkey(app: &AppHandle, preferred: &str) {
    let mut candidates: Vec<Shortcut> = Vec::new();
    if let Some(s) = hotkey::parse_hotkey(preferred) {
        candidates.push(s);
    }
    candidates.extend([
        Shortcut::new(Some(Modifiers::ALT), Code::Space),
        Shortcut::new(Some(Modifiers::ALT | Modifiers::SHIFT), Code::Space),
        Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space),
    ]);

    for shortcut in candidates {
        match app.global_shortcut().register(shortcut) {
            Ok(()) => {
                ilog!("[iTools] 全局快捷键已绑定: {shortcut:?}");
                return;
            }
            Err(err) => {
                ilog!("[iTools] 快捷键 {shortcut:?} 绑定失败（可能被占用）: {err}");
            }
        }
    }
    ilog!("[iTools] 所有候选快捷键均被占用，请通过托盘图标唤起 iTools。");
}

/// 打开管理中心窗口（主面板头像/托盘共用入口）。窗口在 tauri.conf.json 静态声明
/// （启动即创建、默认隐藏），这里只显示+聚焦并收起主面板。
///
/// ⚠ 历史坑（tauri#13963 / wry#583）：曾动态创建此窗口，`build()` 跑在
/// 同步 command / `run_on_main_thread` 回调里会死锁——静态声明是官方推荐姿势。
pub fn open_admin(app: &AppHandle) {
    // 显式收起主面板：不依赖失焦事件（大窗抢焦点的时序不可靠）
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.hide();
    }
    if let Some(win) = app.get_webview_window("admin") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else {
        ilog!("[iTools] 管理中心窗口不存在（应由 tauri.conf.json 静态创建）");
    }
}

/// 构建常驻系统托盘：左键点击唤起，右键菜单 显示/管理中心/退出
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "显示 iTools", true, None::<&str>)?;
    let admin_item = MenuItem::with_id(app, "admin", "管理中心", true, None::<&str>)?;
    let reload_item = MenuItem::with_id(app, "reload_plugins", "重新加载插件", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &admin_item, &reload_item, &quit_item])?;

    let mut builder = TrayIconBuilder::new()
        .tooltip("iTools")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(win) = app.get_webview_window("main") {
                    window::show(&win);
                }
            }
            "admin" => open_admin(app),
            "reload_plugins" => {
                if let (Some(reg), Some(st)) = (
                    app.try_state::<plugin::PluginRegistry>(),
                    app.try_state::<SettingsStore>(),
                ) {
                    let cmds = reg.reload(&st.get().disabled_plugins);
                    let n = cmds.len();
                    // 同时重扫调试插件：托盘的「重新加载插件」对两边都该生效
                    let dev_n = reg.dev.rescan(&st.get().dev_plugin_dirs);
                    dev::apply_plugin_search(app, cmds);
                    ilog!("[iTools] 托盘触发插件重载：{n} 条可搜索命令，{dev_n} 个调试插件");
                }
            }
            "quit" => {
                // 设置/使用记录均即时落盘，无需清理；强制退出保证一定能退
                app.exit(0);
                std::process::exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                if let Some(win) = tray.app_handle().get_webview_window("main") {
                    window::show(&win);
                }
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}
