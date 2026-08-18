use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

use crate::account::{AccountState, AccountStore};
use crate::launch;
use crate::logging::ilog;
use crate::profile::{ProfileStore, ProfileView};
use crate::search::mft::MftSearch;
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

/// 取路径的扩展名（小写、带点）用于日志；没有扩展名时给「无扩展名」。
///
/// **为什么日志里只留扩展名、不留完整路径**：这里的 path 是用户在系统文件对话框里自选的
/// 图片，完整路径含 Windows 用户名和私人目录结构，文件名本身也可能就是隐私
/// （「护照扫描.jpg」之类）。release 现在会把日志长期写在 %LOCALAPPDATA%\itools\itools.log 里，
/// 用户报障时整份发给我们——那就把用户的目录结构一并交出去了。
/// 而定位「图片读不出来」真正需要的是「什么格式 + 什么错」，扩展名就够了。
fn log_ext(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_else(|| "无扩展名".to_string())
}

/// 读本地图片为 data URL（背景图/头像显示用，免开 asset 协议）。
/// 任意尺寸的原图在 Rust 侧解码并缩放到需要的尺寸后编码为 JPEG——4K 壁纸也只产出几十 KB。
#[tauri::command]
pub async fn read_image(path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // 先算好脱敏标签，后面两条日志都只用它（理由见 [`log_ext`]）
        let ext = log_ext(&path);
        let img = image::open(Path::new(&path)).map_err(|e| {
            ilog!("[iTools] 图片解码失败（{ext}）: {e}");
            format!("图片解码失败: {e}")
        })?;
        // 面板 680 宽、最高约 520，取 2x
        let resized = img.resize_to_fill(1360, 1040, image::imageops::FilterType::Triangle);
        let rgb = resized.to_rgb8();
        let mut jpeg: Vec<u8> = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut jpeg);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 85);
        rgb.write_with_encoder(encoder).map_err(|e| {
            ilog!("[iTools] 图片编码失败（源 {ext}）: {e}");
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

/// 解码任意图片，居中裁剪到 256²，存为 `<数据根>\avatar.jpg`（数据根见 [`crate::paths::data_root`]），
/// 返回该受控路径。
fn save_avatar_copy(src: &str) -> Result<String, String> {
    let img = image::open(Path::new(src)).map_err(|e| format!("头像解码失败: {e}"))?;
    let square = img.resize_to_fill(256, 256, image::imageops::FilterType::Lanczos3);
    let dir = crate::paths::data_root();
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
pub fn data_usage(
    include_cloud: Option<bool>,
    account: State<'_, AccountStore>,
    data: State<'_, DataStore>,
    registry: State<'_, crate::plugin::PluginRegistry>,
) -> DataUsage {
    // 缺省 true：保持老调用方（不传参数）的行为不变
    data.usage(&account, &collect_data_set_specs(&registry), include_cloud.unwrap_or(true))
}

/// 汇总各插件在 `plugin.json` 里声明的 `dataSets`，供「我的数据」页按**用户视角**计数。
///
/// 没声明的插件不会出现在结果里——界面据此退回「N 个存储项」的说法并说明原因。
fn collect_data_set_specs(registry: &crate::plugin::PluginRegistry) -> crate::sync::DataSetSpecs {
    let Ok(plugins) = registry.plugins.read() else {
        return Default::default();
    };
    plugins
        .iter()
        .filter(|p| !p.manifest.data_sets.is_empty())
        .map(|p| {
            let specs = p
                .manifest
                .data_sets
                .iter()
                .map(|d| crate::sync::DataSetSpec {
                    key: d.key.clone(),
                    label: d.label.clone(),
                    count_by: d.count_by.clone(),
                })
                .collect();
            (crate::plugin::commands::plugin_ns(&p.manifest.name), specs)
        })
        .collect()
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

/// 命令：打开独立的更新窗口。
///
/// 主窗口那边只保留一个角标，点了就开这个窗口——更新说明放不进搜索框。
#[tauri::command]
pub fn open_update_window(app: AppHandle) {
    crate::open_update(&app);
}

// ==================== 运行日志的位置 ====================
//
// release 从本版起会把运行日志长期写在 `%LOCALAPPDATA%\itools\itools.log`（带 2MB 轮转），
// 目的是「出问题时我们拿得到证据」。但**没有任何界面告诉用户它在哪**，这个目的就没闭环：
// 用户想反馈问题时不知道该发什么给我们。管理中心「关于」页的「打开日志目录」按钮就是这个出口。

/// 日志文件名。⚠ 与 `logging` 模块里的 `LOG_NAME` 是同一个值的**第二份**，理由见
/// [`resolve_log_file`]；改名时两处必须一起改。
const LOG_FILE_NAME: &str = "itools.log";

/// 本次构建的日志文件完整路径。
///
/// ⚠ **这里重算了一遍 `logging` 模块的落点规则**（debug = exe 同目录、
/// release = `paths::data_root()`），因为 `logging::log_path()` 目前是模块私有的，
/// 外部拿不到。两处必须保持一致：改了 `logging` 的落点却漏了这里，这个按钮就会把用户
/// 领到一个空目录，比没有按钮更误导人。哪天 `logging` 把路径函数公开出来，
/// 这里应当立刻改成直接调用它，并把这份复制删掉。
///
/// 只算路径、**不建目录**：目录该不该存在是 `logging` 说了算，这里凭空建一个空目录
/// 只会让「日志根本没写成」这件事更难被发现。
fn resolve_log_file() -> Result<std::path::PathBuf, String> {
    if cfg!(debug_assertions) {
        // debug：日志就在 target/debug/ 里，与 exe 同目录（开发时手边即取）
        let exe = std::env::current_exe().map_err(|e| format!("取不到程序自身路径：{e}"))?;
        let dir = exe
            .parent()
            .ok_or_else(|| "程序自身路径没有上级目录，无法定位日志".to_string())?;
        Ok(dir.join(LOG_FILE_NAME))
    } else {
        // release：装在 Program Files 之类没有写权限的地方，日志只能落在用户数据根下
        Ok(crate::paths::data_root().join(LOG_FILE_NAME))
    }
}

/// 命令：日志文件的完整路径（管理中心「关于」页显示，用户可照着去找/发给我们）。
///
/// 前端**不许**自己拼这个路径：debug 与 release 的落点本来就不同，数据根还可能因为
/// 取不到 `%LOCALAPPDATA%` 而回退到临时目录——前端写死一份，必然有对不上的那天。
#[tauri::command]
pub fn log_file_path() -> Result<String, String> {
    Ok(resolve_log_file()?.to_string_lossy().into_owned())
}

/// 命令：在资源管理器里打开日志文件所在的目录。
///
/// 目录不存在时**如实报错**，绝不硬着头皮去调 explorer：explorer 拿到一个不存在的路径
/// 会自作主张打开「文档」之类的默认位置，用户看见窗口弹出来就以为找对了地方，
/// 翻半天也找不到日志——那就成了「看着生效、其实没生效」的控件。
#[tauri::command]
pub fn open_log_dir() -> Result<(), String> {
    let file = resolve_log_file()?;
    let dir = file
        .parent()
        .ok_or_else(|| format!("日志路径没有上级目录：{}", file.display()))?;
    if !dir.is_dir() {
        return Err(format!(
            "日志目录不存在：{}（说明这次运行连日志文件都没能建出来，多半是没有写入权限）",
            dir.display()
        ));
    }
    // 与「打开插件目录」走同一条出口：explorer 中转、spawn 即返回，不阻塞命令线程；
    // explorer 不可用时它内部会退回 opener::open。
    launch::open_detached(&dir.to_string_lossy()).map_err(|e| format!("打开日志目录失败：{e}"))
}

#[cfg(test)]
mod log_location_tests {
    use super::*;

    /// 定位结果必须真的指向日志文件本身，且落在一个具体目录里
    /// ——「打开日志目录」按钮开的就是它的父目录。
    #[test]
    fn log_file_is_named_and_placed_sanely() {
        let p = resolve_log_file().expect("测试进程里必定能定出日志路径");
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some(LOG_FILE_NAME),
            "文件名要与 logging 模块写出来的那个一致，实得：{}",
            p.display()
        );
        let dir = p.parent().expect("日志路径必须有上级目录");
        assert!(!dir.as_os_str().is_empty(), "上级目录不能是空路径");
        println!("本构建的日志路径：{}", p.display());
    }

    /// debug 构建的日志与 exe 同目录（`cargo test` 就是 debug）：
    /// 这条同时守住「debug 不会把用户领到 release 的数据目录去」。
    #[cfg(debug_assertions)]
    #[test]
    fn debug_log_sits_next_to_the_exe() {
        let p = resolve_log_file().expect("定位日志路径");
        let exe = std::env::current_exe().expect("取 exe 路径");
        assert_eq!(p.parent(), exe.parent(), "debug 日志应与 exe 同目录");
        assert_ne!(
            p.parent().map(std::path::Path::to_path_buf),
            Some(crate::paths::data_root()),
            "debug 不该把用户指到数据根目录"
        );
    }
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

// ---------- 全盘文件名索引（NTFS MFT 直读，见 search/mft/mod.rs） ----------
//
// 这三个命令是那套「提权守护进程 + 命名管道」的**唯一**前端入口。守护是不是活着、
// 索引建到哪一步、哪个盘失败了——一律来自向守护实发一次 IPC 拿到的回包，
// 拿不到就如实说拿不到（`running=false` / `state="off"`），绝不用零值冒充「就绪」。
//
// ⚠ 三个命令都标了 `#[tauri::command(async)]`。不带 async 的命令走
// `ExecutionContext::Blocking`，函数体被内联进 IPC handler，而 Windows 上 IPC handler
// 由 WebView2 controller 所属的**主 UI 线程**调用——这里每个命令都要么等一次命名管道往返
// （最长 1.5 s，见 `mft::ipc::CLIENT_TIMEOUT`）、要么轮询等 UAC 授权结果（最长
// [`ENABLE_WAIT`]），同步执行会把托盘、全局热键和所有窗口一起卡住。

/// 等待「守护上线 / 用户拒绝授权」的上限。
///
/// UAC 对话框弹出后用户通常两三秒内就做出选择，同意后守护建好命名管道也在 1 s 内
/// （`daemon::run` 是先 `ipc::serve` 应答、建索引放后台线程，所以「能应答」远早于「索引建好」）。
/// 8 s 足以把绝大多数情况定性；超了也**不猜**，如实回 `pending`。
const ENABLE_WAIT: Duration = Duration::from_secs(8);
/// 轮询间隔。守护缺席时一次 `is_running()` 是「打开管道即失败」立刻返回，开销可忽略。
const ENABLE_POLL: Duration = Duration::from_millis(200);

/// 全盘文件名索引的**真实**状态快照，`file_index_status` 的返回值。
///
/// 字段与 `mft::ipc::StatusDto` 一一对应（两侧都**没有** `#[serde(rename_all)]`，
/// 序列化后原样 snake_case），只在外面多加一个 `running`——因为「守护在不在」与
/// 「索引建成什么样」是两件事，混成一个字段前端就没法把「未启用」和「建索引中」分开说。
#[derive(Serialize)]
pub struct FileIndexStatus {
    /// 提权守护是否**真的在应答**（`MftSearch::status()` 拿到了回包）。
    ///
    /// 「正在等用户点 UAC」「刚拉起、管道还没建好」这些中间态一律是 false——
    /// 这里只报证据，不猜进程状态。
    pub running: bool,
    /// `off` = 守护没在应答（本命令给的值，`StatusDto` 里**不存在**这个取值）；
    /// 其余 `building`（首次建 / 重建中）、`ready`（全部盘就绪）、`partial`（部分盘失败）、
    /// `error`（守护内部状态锁中毒）都由守护如实上报，前端必须把 `building` 与 `ready` 分开呈现
    /// ——建索引期间 `/f` 的结果本来就不全，显示成「已就绪」就是骗人。
    pub state: String,
    /// 已建好索引的盘符，如 `["C","D"]`。它**不等于**机器上的全部盘：
    /// 没出现在这里、也没出现在 `failed_drives` 里的盘（非 NTFS / 可移动盘）压根没被枚举。
    pub ready_drives: Vec<String>,
    /// 建索引失败的盘及**原因原文**，如 `("E","拒绝访问（读取磁盘索引需要管理员权限）")`。
    /// 原样展示——只说一句「部分磁盘不可用」等于把用户蒙在鼓里。
    pub failed_drives: Vec<(String, String)>,
    /// 可搜索条目总数（真实计数，不是估算）
    pub entries: usize,
    /// 索引真实占用内存（MB）
    pub memory_mb: usize,
    /// 因噪音目录排除规则跳过的条目数（node_modules、缓存目录等）
    pub excluded: usize,
}

impl FileIndexStatus {
    /// 守护没在应答时唯一诚实的答案：全零 + `state="off"`。
    fn off() -> Self {
        Self {
            running: false,
            state: "off".to_string(),
            ready_drives: Vec::new(),
            failed_drives: Vec::new(),
            entries: 0,
            memory_mb: 0,
            excluded: 0,
        }
    }
}

/// 全盘索引状态：主面板 `/f` 的状态条与管理中心的索引面板都读它。
///
/// 守护没起来（未开启 / 用户拒过提权 / 刚被关掉）时返回 [`FileIndexStatus::off`]，
/// 前端据此显示「未启用全盘索引，`/f` 只能搜到 Windows Search 已索引的位置」。
#[tauri::command(async)]
pub fn file_index_status() -> FileIndexStatus {
    match MftSearch::status() {
        Some(s) => FileIndexStatus {
            running: true,
            state: s.state,
            ready_drives: s.ready_drives,
            failed_drives: s.failed_drives,
            entries: s.entries,
            memory_mb: s.memory_mb,
            excluded: s.excluded,
        },
        None => FileIndexStatus::off(),
    }
}

/// `file_index_enable` 的结果。**必须**能让前端区分下面几种结局，否则它只能瞎猜，
/// 就会做出一个「点了看不出发生了什么」的按钮。
#[derive(Serialize)]
pub struct FileIndexEnableResult {
    /// 本次的真实结局，四取一：
    ///
    /// - `already_running`：守护本来就在跑，这次什么都没做（没弹 UAC）；
    /// - `starting`：已拿到管理员授权、守护已上线，正在建索引（几十秒，期间 `state="building"`）；
    /// - `declined`：**确证**用户在 UAC 对话框上点了「否」；
    /// - `pending`：提权请求已发出，但在 [`ENABLE_WAIT`] 内既没等到守护上线、也没确证被拒
    ///   （UAC 对话框可能还开着，也可能是 `ShellExecuteW` 因别的原因失败了）。
    ///
    /// ⚠ `declined` 判据的局限（如实写在这里，不要在前端替它补语义）：mft 侧那个
    /// 「被拒绝过」的标记是**粘性全局量**，且用户主动关闭索引（`MftSearch::shutdown`）也会置位。
    /// 所以本命令只把它**在本次调用期间由 false 变 true** 当作证据；若之前已经拒过 / 关过一次，
    /// 这次再被拒就只会得到 `pending`——宁可回「不确定」，也不编一个可能不成立的「被拒绝」。
    pub outcome: String,
    /// 本命令返回时守护是否已在应答（`already_running` / `starting` 为 true）。
    /// 为 true 只代表「服务活着」，索引可能还在建——具体进度看 `file_index_status`。
    pub running: bool,
    /// 给用户看的中文说明，与 `outcome` 一一对应，前端可直接显示（不必自己拼文案）。
    pub message: String,
}

/// 用户主动开启全盘索引（管理中心的「开启」按钮）。会触发一次 UAC 提权。
///
/// `ensure_running(true)` 的 `user_initiated=true` 会忽略冷却与「上次被拒」的记忆
/// ——用户明确要开，就该把对话框弹给他，而不是被我们自己的防抖挡住。它内部把
/// `ShellExecuteW("runas")` 放在后台线程，所以返回 `true` **只代表守护本来就在跑**，
/// 返回 false 时结局还没定；本命令随后限时轮询把结局定下来，见
/// [`FileIndexEnableResult::outcome`]。
#[tauri::command(async)]
pub fn file_index_enable() -> FileIndexEnableResult {
    // 先记下调用前的「被拒绝过」标记：它是粘性的，只有**本次由 false 变 true**
    // 才能当作「用户刚刚拒绝了这一次」的证据（理由见 outcome 的文档）。
    let declined_before = MftSearch::spawn_declined();

    if MftSearch::ensure_running(true) {
        return FileIndexEnableResult {
            outcome: "already_running".to_string(),
            running: true,
            message: "全盘索引服务已在运行".to_string(),
        };
    }

    // 提权请求已经发出去了，此刻 UAC 对话框可能还开着。这里等一会儿而不是立刻返回，
    // 是为了让「授权成功，正在建索引」与「授权被取消」当场分开——否则前端拿到的只是
    // 一个「不知道」，用户点完按钮看不出任何变化。
    let deadline = Instant::now() + ENABLE_WAIT;
    loop {
        if MftSearch::is_running() {
            return FileIndexEnableResult {
                outcome: "starting".to_string(),
                running: true,
                message: "已获得管理员授权，正在建立全盘索引（约几十秒；建完前搜索结果可能不全）"
                    .to_string(),
            };
        }
        if !declined_before && MftSearch::spawn_declined() {
            return FileIndexEnableResult {
                outcome: "declined".to_string(),
                running: false,
                message: "管理员授权被取消，全盘索引没有开启（直接读取磁盘索引必须管理员权限）"
                    .to_string(),
            };
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(ENABLE_POLL);
    }
    FileIndexEnableResult {
        outcome: "pending".to_string(),
        running: false,
        message: format!(
            "已发起管理员授权请求，但 {} 秒内索引服务还没上线：UAC 对话框可能仍在等你确认，也可能是启动失败。确认后稍等片刻再看索引状态。",
            ENABLE_WAIT.as_secs()
        ),
    }
}

/// 重建全盘索引（管理中心的「重建索引」按钮）。
///
/// 成功只以守护**确认收下请求**（`Response::Ok`）为准。守护没在跑时给出的是可操作的原因，
/// 而不是笼统的「重建失败」——「服务没开」和「请求没被受理」要用户做的动作完全不同。
///
/// 注意重建本身是**异步**的：守护收下请求后把状态改回 `building` 并在后台重建，
/// 所以返回 `Ok` 只表示「请求已受理」，界面要靠轮询 `file_index_status` 看进度。
#[tauri::command(async)]
pub fn file_index_rebuild() -> Result<(), String> {
    if !MftSearch::is_running() {
        return Err("全盘索引服务没有在运行，请先开启全盘索引".to_string());
    }
    if MftSearch::rebuild() {
        Ok(())
    } else {
        Err("索引服务没有确认这次重建请求（可能刚刚退出或正忙），请稍后重试".to_string())
    }
}

/// 关闭全盘索引，让守护进程退出并交还它占的内存。
///
/// # 为什么这条命令必须存在
///
/// 索引守护是**常驻**进程：主程序退出后它继续活着（这样下次开机 `/f` 立刻可用），
/// 而它实测占 370 MB。只提供「开启」不提供「关闭」，等于背着用户常驻一个几百 MB 的进程
/// ——那正是项目红线里的「让用户误以为可用/不知情」。
///
/// 关闭后本次运行内不会再自动拉起（`MftSearch::shutdown` 会置上「用户已拒绝」标记），
/// 否则下一次按 `/f` 又给他弹一个 UAC。要重新开启得由用户再点一次「开启全盘索引」。
#[tauri::command(async)]
pub fn file_index_disable() -> Result<(), String> {
    if MftSearch::shutdown() {
        Ok(())
    } else {
        Err("索引服务没有确认这次关闭请求，可稍后重试；若一直如此，可在任务管理器结束 iTools 的索引进程".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守护没在跑时，状态必须如实回 `running=false` / `state="off"` / 全零，
    /// **不能**因为「拿不到状态」就退化成一个看着正常的 ready（那正是诚信红线要禁的）。
    #[test]
    fn file_index_status_is_honest_without_daemon() {
        let s = file_index_status();
        if MftSearch::is_running() {
            // 本机真的开着全盘索引：那就反过来校验「在跑时不许报 off」
            println!("本机守护在跑，state={} entries={}", s.state, s.entries);
            assert!(s.running);
            assert_ne!(s.state, "off");
            return;
        }
        assert!(!s.running, "守护缺席时 running 必须是 false");
        assert_eq!(s.state, "off", "守护缺席时 state 必须是 off，不许编 ready");
        assert_eq!(s.entries, 0);
        assert_eq!(s.memory_mb, 0);
        assert!(s.ready_drives.is_empty());
        assert!(s.failed_drives.is_empty());
    }

    /// 守护没在跑时点「重建」必须拿到**说明原因**的错误，而不是一个静默的失败
    #[test]
    fn file_index_rebuild_reports_reason_without_daemon() {
        if MftSearch::is_running() {
            println!("本机守护在跑，跳过该用例");
            return;
        }
        let err = file_index_rebuild().expect_err("守护缺席时必须返回 Err");
        assert!(
            err.contains("没有在运行"),
            "错误文案要告诉用户该先开启索引，实得：{err}"
        );
    }

    /// 图片日志标签只能是扩展名：**一个字符都不能带出目录、用户名或文件名**。
    ///
    /// 这条用例是给「后来者想把完整路径加回日志里图省事」准备的挡板——
    /// release 的日志会长期躺在用户磁盘上，报障时整份发给我们（见 `crate::logging` 的「隐私」段）。
    #[test]
    fn log_ext_leaks_nothing_but_extension() {
        let p = r"C:\Users\张三\Pictures\私人相册\护照扫描.PNG";
        assert_eq!(log_ext(p), ".png", "扩展名要归一成小写");
        let label = log_ext(p);
        for leak in ["Users", "张三", "Pictures", "私人相册", "护照扫描", "\\"] {
            assert!(!label.contains(leak), "日志标签泄漏了「{leak}」：{label}");
        }
        // 没有扩展名时给个可读占位，而不是空串（空串会让日志读起来像少了一段）
        assert_eq!(log_ext(r"C:\Users\张三\无扩展的图"), "无扩展名");
    }
}
