use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::account::{AccountState, AccountStore};
use crate::launch;
use crate::logging::ilog;
use crate::profile::{ProfileStore, ProfileView};
use crate::search::{icon, SearchIndex, SearchItem};
use crate::settings::{AppSettings, LaunchItem, SettingsStore};
use crate::store::UsageStore;
use crate::sync::{DataStore, DataUsage, SyncResult};

/// 前端查询入口。
/// 默认应用搜索给足数量（网格「展开 (N)」要显示全部匹配）；/f 文件搜索保持精简列表。
#[tauri::command]
pub fn search(query: String, index: State<'_, SearchIndex>) -> Vec<SearchItem> {
    let limit = if query.trim_start().starts_with("/f") {
        30
    } else {
        100
    };
    index.query(&query, limit)
}

/// 执行一条结果：
/// - action = "copy"：把 target 复制到剪贴板（计算/进制/时间戳/颜色等即时命令）
/// - 其它：用系统默认方式打开 target（.lnk 会由 shell 解析到真实程序）
///
/// 成功执行的应用/文件会写入「最近使用」。
#[tauri::command]
pub fn execute(item: SearchItem, store: State<'_, UsageStore>) -> Result<(), String> {
    let result = match item.action.as_str() {
        "copy" => {
            let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
            clipboard.set_text(&item.target).map_err(|e| e.to_string())
        }
        // explorer 中转启动，实现集中在 launch 模块
        _ => launch::open_detached(&item.target),
    };
    if result.is_ok() {
        store.record(&item);
    }
    result
}

/// 只记录一次使用（写入「最近使用」），不执行任何打开动作。
/// 插件经 `open_plugin_window` 打开（不走 `execute`），由前端在打开后调用本命令补记使用，
/// 使插件与应用一样进入「最近使用」。
#[tauri::command]
pub fn record_usage(item: SearchItem, store: State<'_, UsageStore>) {
    store.record(&item);
}

/// 主面板数据：问候用户名 + 最近使用 + 已固定（图标由前端按需 load_icons 补齐）
#[derive(Serialize)]
pub struct HomeData {
    pub user: String,
    pub recent: Vec<SearchItem>,
    pub pinned: Vec<SearchItem>,
}

/// 判定「最近使用 / 已固定」里的一条**插件**记录当前该显示、该隐藏、还是该清掉。
///
/// `target` 形如 `<插件id>#<feature code>`，调试插件的 id 带 `dev:` 前缀（见 [`crate::dev::DEV_ID_PREFIX`]）。
///
/// - **调试插件**：`dev_search_visible` 关闭时不显示。设计契约是「调试插件默认不出现在主界面、
///   由该开关控制」，而此前它只管住了搜索索引这一半——用过一次调试插件后，即便关掉开关，
///   「最近使用」里仍常驻这条 `dev:` 项，点击照样打开调试窗口（`open_plugin` 的 dev 前缀
///   路由不看开关）。开关开着时还要与搜索索引同一条线：**跑不起来的（缺 index.html / 清单坏掉）
///   不露面**（`DevRuntime::search_commands` 就是这么过滤的——点了打不开 = 欺骗），
///   但那只是暂时隐藏；只有插件压根不在注册表里（目录被删 / 改名 / 调试目录被移除）才算 Gone。
/// - **正式插件**：不在注册表里（卸载 / 改名 / 清单坏掉加载失败）就清掉——留着点了也只是报错。
///   注意用的是「插件是否还装着」而不是「是否在搜索索引里」：被用户**停用**的插件仍然装着，
///   它的记录不该被删。
///
/// `Gone` 会**真删记录**，所以只在能**确证**插件不在时才返回它：注册表读不出来（锁中毒，
/// `has_plugin` 返回 `None`）一律保守处理，绝不因为一次无关的 panic 把用户的记录清空。
fn plugin_entry_state(
    target: &str,
    registry: &crate::plugin::PluginRegistry,
    dev_visible: bool,
) -> crate::store::PluginEntry {
    use crate::store::PluginEntry;
    let id = target.split('#').next().unwrap_or(target);
    match id.strip_prefix(crate::dev::DEV_ID_PREFIX) {
        Some(dev_id) => match registry.dev.has_plugin(dev_id) {
            Some(false) => PluginEntry::Gone,
            // 读不出调试注册表：不显示（调试项本就默认不该出现），但保留记录
            None => PluginEntry::Hide,
            Some(true) if !dev_visible || !registry.dev.runnable(dev_id) => PluginEntry::Hide,
            Some(true) => PluginEntry::Show,
        },
        None => match registry.has_plugin(id) {
            Some(false) => PluginEntry::Gone,
            // 读不出正式注册表：宁可多显示一条，也不删用户的最近使用
            None | Some(true) => PluginEntry::Show,
        },
    }
}

#[tauri::command]
pub fn home_data(
    store: State<'_, UsageStore>,
    profile: State<'_, ProfileStore>,
    registry: State<'_, crate::plugin::PluginRegistry>,
    settings: State<'_, SettingsStore>,
) -> HomeData {
    let dev_visible = settings.get().dev_search_visible;
    let (recent, pinned) =
        store.snapshot(|target| plugin_entry_state(target, &registry, dev_visible));
    // 问候名优先用账号昵称，回退系统用户名
    let user = {
        let p = profile.get();
        if p.nickname.trim().is_empty() {
            std::env::var("USERNAME").unwrap_or_else(|_| "there".to_string())
        } else {
            p.nickname
        }
    };
    HomeData {
        user,
        recent,
        pinned,
    }
}

/// 固定/取消固定一个条目，返回操作后是否处于固定状态
#[tauri::command]
pub fn toggle_pin(item: SearchItem, store: State<'_, UsageStore>) -> bool {
    store.toggle_pin(&item)
}

// ---------- 设置 ----------

#[tauri::command]
pub fn get_settings(store: State<'_, SettingsStore>) -> AppSettings {
    store.get()
}

/// 保存设置并即时生效：透明度 / 快捷键 / 自定义程序 / 背景图 / 主题 / 占位符（通知主窗口刷新）。
///
/// 标量与外观项（主题、背景开关/暗化、占位符、代理）都随整体保存；
/// 需要副作用的（透明度、快捷键、程序库、自启）在此对比并即时应用，其余靠 `settings-changed` 事件让主窗口重拉。
/// 注：本地启动清单（local_launch_items）由专用命令（add/remove_launch_items）独占管理，
/// 本命令会忽略传入值、保留 store 现值（见下方保留逻辑），避免整包保存与专用命令丢更新竞态。
#[tauri::command]
pub fn save_settings(
    settings: AppSettings,
    app: AppHandle,
    store: State<'_, SettingsStore>,
    index: State<'_, SearchIndex>,
) -> Result<(), String> {
    let old = store.get();
    // 后端独占写入的字段（本地启动清单 / 插件禁用清单 / 授权表 / 下载源偏好 / 插件窗口尺寸）
    // 一律用 store 现值回填：save_settings 是整包保存，若连带覆盖就会与那些专用命令构成
    // 丢更新竞态（删了又被旧快照写回），前端 TS 声明里没有的字段更会被直接清空。
    // 清单与理由见 settings::preserve_backend_owned——**新增此类字段必须去那里补一行**。
    let mut next = settings;
    crate::settings::preserve_backend_owned(&mut next, &old);

    // 代理配置**先校验再保存**：开着开关却给不出可用地址时，如果照样存下去，
    // 运行期只能悄悄退回直连——那就又造出一个「开着、看着生效、其实一个字节都不走」的假控件。
    // 这里直接拒绝保存并把中文原因抛给 UI，让用户当场知道该怎么改。
    if next.proxy_enabled {
        if next.proxy_address.trim().is_empty() {
            return Err("已开启网络代理，但没有填写代理地址（形如 127.0.0.1:7897）".to_string());
        }
        crate::http::normalize_proxy(&next.proxy_address)?;
    }

    store.set(next.clone());
    // 云端地址即时生效：更新运行期端点，随后的登录 / 同步立即指向新地址（无需重启）。
    crate::account::set_user_endpoint(&next.sync_endpoint);
    // 代理即时生效：重建出站 Agent，随后所有请求按新配置选链路（无需重启）。
    // 上面已校验过，正常不会 Err；真出错也 fail-closed 到直连并如实记日志。
    if let Err(e) = crate::http::refresh(next.proxy_enabled, &next.proxy_address) {
        ilog!("[iTools] 代理配置应用失败（已回退直连）：{e}");
    }

    if old.opacity != next.opacity {
        if let Some(win) = app.get_webview_window("main") {
            crate::window::apply_opacity(&win, next.opacity);
        }
    }
    // 全局热键换绑：主唤起热键改动会 unregister_all()（撤掉一切键，含插件热键），需完整重建；
    // 否则只对变化的截图/贴图热键做增量换绑。
    if old.hotkey != next.hotkey {
        let _ = app.global_shortcut().unregister_all();
        crate::register_toggle_hotkey(&app, &next.hotkey);
        // 补注册本体截图/贴图热键（各自先清状态再按 next 注册，空 = 只清不注册）
        crate::plugin::capture::resync_screenshot_hotkey(&app, next.screenshot_hotkey.trim());
        crate::plugin::pin::resync_pin_hotkey(&app, next.pin_hotkey.trim());
        // 补注册所有插件热键（unregister_all 把它们也撤了，不补则插件全局键会静默失效）
        crate::plugin::hotkey::reregister_all(&app);
    } else {
        if old.screenshot_hotkey != next.screenshot_hotkey {
            crate::plugin::capture::resync_screenshot_hotkey(&app, next.screenshot_hotkey.trim());
        }
        if old.pin_hotkey != next.pin_hotkey {
            crate::plugin::pin::resync_pin_hotkey(&app, next.pin_hotkey.trim());
        }
    }
    if old.custom_apps != next.custom_apps {
        index.rescan_apps(next.custom_apps.clone());
    }
    if old.autostart != next.autostart {
        use tauri_plugin_autostart::ManagerExt;
        let manager = app.autolaunch();
        let result = if next.autostart {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(err) = result {
            ilog!("[iTools] 开机自启设置失败: {err}");
        }
    }
    // 背景图/透明度/主题/占位符等外观变化，主窗口监听该事件后重新拉取设置
    let _ = app.emit("settings-changed", ());
    Ok(())
}

/// 弹系统文件选择器选背景图片，返回绝对路径（取消返回 None）
#[tauri::command]
pub async fn pick_image() -> Option<String> {
    DIALOG_OPEN.store(true, Ordering::Relaxed);
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择图片")
            .add_filter("图片", &["png", "jpg", "jpeg", "webp", "bmp", "gif"])
            .pick_file()
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten();
    DIALOG_OPEN.store(false, Ordering::Relaxed);
    picked
}

/// 弹系统文件选择器选程序（exe/lnk），返回绝对路径
#[tauri::command]
pub async fn pick_app() -> Option<String> {
    DIALOG_OPEN.store(true, Ordering::Relaxed);
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择程序")
            .add_filter("程序", &["exe", "lnk", "bat", "cmd"])
            .pick_file()
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten();
    DIALOG_OPEN.store(false, Ordering::Relaxed);
    picked
}

/// 读本地图片为 data URL（背景图/头像显示用，免开 asset 协议）。
/// 任意尺寸的原图在 Rust 侧解码并缩放到需要的尺寸后编码为 JPEG——4K 壁纸也只产出几十 KB。
#[tauri::command]
pub async fn read_image(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let img = image::open(Path::new(&path)).map_err(|e| {
            ilog!("[iTools] 图片解码失败 {path}: {e}");
            format!("图片解码失败: {e}")
        })?;
        // 面板 680 宽、最高约 520，取 2x
        let resized = img.resize_to_fill(1360, 1040, image::imageops::FilterType::Triangle);
        let rgb = resized.to_rgb8();
        let mut jpeg: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut jpeg);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 85);
        rgb.write_with_encoder(encoder).map_err(|e| {
            ilog!("[iTools] 图片编码失败 {path}: {e}");
            format!("图片编码失败: {e}")
        })?;
        Ok(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 读本地图片为方形头像 data URL（居中裁剪到 256×256，圆形由前端 CSS 处理）。
#[tauri::command]
pub async fn read_avatar(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let img = image::open(Path::new(&path)).map_err(|e| format!("头像解码失败: {e}"))?;
        let square = img.resize_to_fill(256, 256, image::imageops::FilterType::Lanczos3);
        let rgb = square.to_rgb8();
        let mut jpeg: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut jpeg);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 88);
        rgb.write_with_encoder(encoder)
            .map_err(|e| format!("头像编码失败: {e}"))?;
        Ok(format!(
            "data:image/jpeg;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- 账号（纯本地模拟） ----------

/// 当前账号资料（含派生的陪伴天数）
#[tauri::command]
pub fn get_profile(profile: State<'_, ProfileStore>) -> ProfileView {
    profile.view()
}

/// 修改昵称，返回最新资料
#[tauri::command]
pub fn set_nickname(
    nickname: String,
    app: AppHandle,
    profile: State<'_, ProfileStore>,
) -> ProfileView {
    let name = nickname.trim().to_string();
    if !name.is_empty() {
        profile.update(|p| p.nickname = name);
    }
    let _ = app.emit("profile-changed", ());
    profile.view()
}

/// 设置头像（传入本地图片绝对路径）：裁剪成方形另存进应用数据目录，profile 只存受控路径，
/// 源文件（常来自下载/桌面临时目录）被移动/删除也不影响头像。返回最新资料。
#[tauri::command]
pub fn set_avatar(
    path: String,
    app: AppHandle,
    profile: State<'_, ProfileStore>,
) -> Result<ProfileView, String> {
    let stored = save_avatar_copy(&path)?;
    profile.update(|p| p.avatar_path = Some(stored));
    let _ = app.emit("profile-changed", ());
    Ok(profile.view())
}

/// 解码任意图片，居中裁剪到 256²，存为 `%LOCALAPPDATA%\itools\avatar.jpg`，返回该受控路径。
fn save_avatar_copy(src: &str) -> Result<String, String> {
    let img = image::open(Path::new(src)).map_err(|e| format!("头像解码失败: {e}"))?;
    let square = img.resize_to_fill(256, 256, image::imageops::FilterType::Lanczos3);
    let dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("itools");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join("avatar.jpg");
    let rgb = square.to_rgb8();
    let file = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
    let mut writer = std::io::BufWriter::new(file);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 88);
    rgb.write_with_encoder(encoder)
        .map_err(|e| format!("头像编码失败: {e}"))?;
    Ok(dest.to_string_lossy().into_owned())
}

// ---------- 云账号 & 数据同步（本地优先 + 配置化云端 + 诚实降级） ----------

/// 当前账号态：登录态 / 用户名 / 云端是否已配置 / 是否开启自动同步。
#[tauri::command]
pub fn account_state(account: State<'_, AccountStore>) -> AccountState {
    account.state()
}

/// 登录云账号：**云端已配置才可能成功**，否则诚实报错（不假装登录）。
#[tauri::command]
pub fn account_login(
    username: String,
    password: String,
    app: AppHandle,
    account: State<'_, AccountStore>,
) -> Result<AccountState, String> {
    let state = account.login(&username, &password)?;
    let _ = app.emit("account-changed", ());
    Ok(state)
}

/// 退出登录：清本地会话；云端登出尽力而为。`all_devices` **真实传给云端**（吊销全部设备会话）。
/// 同时把本地资料重置为游客态。
#[tauri::command]
pub fn logout_account(
    all_devices: bool,
    app: AppHandle,
    account: State<'_, AccountStore>,
    profile: State<'_, ProfileStore>,
) -> AccountState {
    let state = account.logout(all_devices);
    profile.reset_to_guest();
    let _ = app.emit("account-changed", ());
    let _ = app.emit("profile-changed", ());
    state
}

/// 注销账号：**需云端已配置**，走真实鉴权 + 服务端删除；成功后清本地账号态与资料。
/// 未配置端点时诚实报错，不本地伪装删除「服务器数据」。
#[tauri::command]
pub fn delete_account(
    username: String,
    password: String,
    app: AppHandle,
    account: State<'_, AccountStore>,
    profile: State<'_, ProfileStore>,
) -> Result<AccountState, String> {
    let state = account.delete_account(&username, &password)?;
    profile.reset_to_guest();
    let _ = app.emit("account-changed", ());
    let _ = app.emit("profile-changed", ());
    Ok(state)
}

/// 「登录后自动同步」开关：真实控制同步引擎是否在数据变更时上行。
#[tauri::command]
pub fn set_data_sync(enabled: bool, account: State<'_, AccountStore>) -> AccountState {
    account.set_sync_enabled(enabled)
}

/// 立即把**所有本地数据集**（各插件 `plugin:<id>` 命名空间）同步到云端。
/// 修复历史缺陷：原先只同步空的 `app` 命名空间（真实用户数据在 `plugin:<id>`，从不被同步）。
/// 诚实降级：云端未配置 / 未登录时返回 `{ synced:false, reason }`，数据留在本地。
#[tauri::command]
pub fn sync_now(account: State<'_, AccountStore>, data: State<'_, DataStore>) -> SyncResult {
    data.sync_all_gated(&account)
}

/// 「我的数据」用量：本地各命名空间条数（真实）+ 云端用量（真实请求服务端，不可用则诚实标注原因）。
/// 供设置中心「我的数据」页展示每个数据集（主程序 / 各插件）本地与云端各有多少条记录。
///
/// `#[tauri::command(async)]`：本命令会**同步请求云端**。不带 async 的命令走
/// `ExecutionContext::Blocking`，函数体被内联进 IPC handler，而 Windows 上 IPC handler
/// 由 WebView2 controller 所属的主 UI 线程调用——云端慢/超时就会把整个 app 卡住
/// （托盘、全局热键、所有窗口一起排队）。加 async 后交由异步运行时执行。
#[tauri::command(async)]
pub fn data_usage(account: State<'_, AccountStore>, data: State<'_, DataStore>) -> DataUsage {
    data.usage(&account)
}

// ---------- 本地启动 ----------

/// 弹系统文件选择器（多选，任意类型），返回所选文件绝对路径列表
#[tauri::command]
pub async fn pick_launch_files() -> Vec<String> {
    DIALOG_OPEN.store(true, Ordering::Relaxed);
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择要随启动打开的文件")
            .pick_files()
            .map(|paths| {
                paths
                    .into_iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    DIALOG_OPEN.store(false, Ordering::Relaxed);
    picked
}

/// 弹系统文件夹选择器，返回所选文件夹绝对路径（取消返回 None）
#[tauri::command]
pub async fn pick_launch_folder() -> Option<String> {
    DIALOG_OPEN.store(true, Ordering::Relaxed);
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择要随启动打开的文件夹")
            .pick_folder()
            .map(|p| p.to_string_lossy().into_owned())
    })
    .await
    .ok()
    .flatten();
    DIALOG_OPEN.store(false, Ordering::Relaxed);
    picked
}

/// 向本地启动清单追加若干路径（自动判目录、去重），返回最新设置
#[tauri::command]
pub fn add_launch_items(
    paths: Vec<String>,
    store: State<'_, SettingsStore>,
    index: State<'_, SearchIndex>,
) -> AppSettings {
    let mut settings = store.get();
    for path in paths {
        let path = path.trim().to_string();
        if path.is_empty() {
            continue;
        }
        if settings.local_launch_items.iter().any(|it| it.id == path) {
            continue; // 去重
        }
        settings.local_launch_items.push(launch::item_from_path(&path));
    }
    store.set(settings.clone());
    index.set_custom_items(settings.local_launch_items.clone()); // 同步可搜索索引
    settings
}

/// 从本地启动清单移除指定 id（支持批量），返回最新设置
#[tauri::command]
pub fn remove_launch_items(
    ids: Vec<String>,
    store: State<'_, SettingsStore>,
    index: State<'_, SearchIndex>,
) -> AppSettings {
    let mut settings = store.get();
    settings
        .local_launch_items
        .retain(|it| !ids.contains(&it.id));
    store.set(settings.clone());
    index.set_custom_items(settings.local_launch_items.clone()); // 同步可搜索索引
    settings
}

/// 立即启动清单中的某一项（列表右侧「立即启动」按钮）
#[tauri::command]
pub fn run_launch_item(path: String) -> Result<(), String> {
    if !Path::new(&path).exists() {
        return Err("路径不存在".to_string());
    }
    launch::open_detached(&path)
}

/// 供前端拖拽后构造条目预览用（不落盘，仅判目录/取显示名）
#[tauri::command]
pub fn build_launch_item(path: String) -> LaunchItem {
    launch::item_from_path(&path)
}

// ---------- 窗口 ----------

/// 管理中心窗口常驻态（保留但当前恒为常驻：管理中心是常规大窗口，失焦不关）。
pub static SETTINGS_PERSIST: AtomicBool = AtomicBool::new(true);
/// 文件对话框打开中：对话框抢焦点不应触发窗口的失焦关闭
pub static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

/// 前端切换窗口的临时/常驻态（管理中心保留接口，默认常驻）
#[tauri::command]
pub fn set_settings_persist(persist: bool) {
    SETTINGS_PERSIST.store(persist, Ordering::Relaxed);
}

/// 关闭（隐藏）管理中心窗口——标题栏「关闭」按钮的出口
#[tauri::command]
pub fn close_admin_window(app: AppHandle) {
    if let Some(win) = app.get_webview_window("admin") {
        let _ = win.hide();
    }
}

/// 打开管理中心窗口（主面板头像/托盘共用入口）。
/// 窗口静态声明常驻，这里只 show/focus（任意线程安全）。
#[tauri::command]
pub fn open_admin_window(app: AppHandle) {
    crate::open_admin(&app);
}

/// 按需提取给定路径的系统图标（仅前端可见项调用），返回 路径 → base64(PNG)。
/// 命中缓存直接取；未命中则提取并写回缓存（含失败缓存），提取放 spawn_blocking 不占 async 执行器。
#[tauri::command]
pub async fn load_icons(
    paths: Vec<String>,
    index: State<'_, SearchIndex>,
) -> Result<HashMap<String, String>, ()> {
    let cache = index.icon_cache_handle();
    let map = tauri::async_runtime::spawn_blocking(move || {
        icon::init_com_for_thread();
        let mut out = HashMap::new();
        for path in paths {
            let hit = cache.lock().ok().and_then(|g| g.get(&path).cloned());
            let value = match hit {
                Some(v) => v,
                None => {
                    let v = icon::icon_base64_png(std::path::Path::new(&path));
                    if let Ok(mut g) = cache.lock() {
                        g.insert(path.clone(), v.clone());
                    }
                    v
                }
            };
            if let Some(b64) = value {
                out.insert(path, b64);
            }
        }
        out
    })
    .await
    .unwrap_or_default();
    Ok(map)
}
