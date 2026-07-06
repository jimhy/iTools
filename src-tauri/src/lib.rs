// iTools 主库入口：命令注册、协议、托盘、窗口、插件系统。
mod account;
mod commands;
mod hotkey;
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
            // 设置最先加载：搜索索引与视觉效果都依赖它
            let settings_store = SettingsStore::load();
            let current = settings_store.get();

            let search_index = SearchIndex::new(current.custom_apps.clone());
            // 「本地启动」清单里的项一并纳入搜索（可在主搜索栏搜到并打开）
            search_index.set_custom_items(current.local_launch_items.clone());
            // 解析插件根：dev 用项目 plugins/，打包用可写的 appData（首启从随包资源播种）
            let plugins_root = plugin::resolve_plugins_root(app.handle());
            if plugins_root.is_dir() {
                ilog!("[iTools] 插件目录: {}", plugins_root.display());
            } else {
                ilog!("[iTools] 插件目录不存在: {}", plugins_root.display());
            }
            // 扫描插件目录并注入搜索（页面插件，搜到后回车打开插件页；禁用的不参与搜索）
            let loaded_plugins = plugin::scan_plugins(&plugins_root);
            let disabled_plugins = current.disabled_plugins.clone();
            let plugin_cmds: Vec<_> = plugin::expand_commands(&loaded_plugins)
                .into_iter()
                .filter(|c| !disabled_plugins.contains(&c.plugin_id))
                .collect();
            search_index.set_plugin_commands(plugin_cmds);
            app.manage(search_index);
            app.manage(store::UsageStore::load());
            app.manage(settings_store);
            // 账号资料（个人中心）——home_data 等命令依赖它，必须在 setup 里 manage
            app.manage(ProfileStore::load());
            // 云账号登录态（本地优先）+ 本地优先数据层（云同步引擎）
            app.manage(AccountStore::load());
            app.manage(DataStore::load());
            // 插件运行期注册表（open_plugin_window / plugin_* 命令依赖）
            app.manage(plugin::PluginRegistry::new(plugins_root, loaded_plugins));
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
            plugin::commands::plugin_data_sync
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
                if let (Some(reg), Some(idx), Some(st)) = (
                    app.try_state::<plugin::PluginRegistry>(),
                    app.try_state::<SearchIndex>(),
                    app.try_state::<SettingsStore>(),
                ) {
                    let cmds = reg.reload(&st.get().disabled_plugins);
                    let n = cmds.len();
                    idx.set_plugin_commands(cmds);
                    ilog!("[iTools] 托盘触发插件重载：{n} 条可搜索命令");
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
