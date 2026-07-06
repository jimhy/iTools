//! 管理中心与 Rust 后端之间的 invoke 封装，集中类型标注。
//! 参数键用 camelCase（Tauri 2 默认自动转 snake_case）。
import { invoke } from "@tauri-apps/api/core";
import type {
  AppSettings,
  ProfileView,
  LaunchItem,
  PluginInfo,
  AccountState,
  SyncResult,
  UpdateInfo,
  SettingsSchema,
} from "../types";

// ---------- 设置 ----------
export const getSettings = () => invoke<AppSettings>("get_settings");
export const saveSettings = (settings: AppSettings) =>
  invoke<void>("save_settings", { settings });

// ---------- 账号 ----------
export const getProfile = () => invoke<ProfileView>("get_profile");
export const setNickname = (nickname: string) =>
  invoke<ProfileView>("set_nickname", { nickname });
export const setAvatar = (path: string) =>
  invoke<ProfileView>("set_avatar", { path });

// ---------- 云账号 & 数据同步（本地优先 + 配置化云端 + 诚实降级） ----------
export const accountState = () => invoke<AccountState>("account_state");
export const accountLogin = (username: string, password: string) =>
  invoke<AccountState>("account_login", { username, password });
export const setDataSync = (enabled: boolean) =>
  invoke<AccountState>("set_data_sync", { enabled });
export const logoutAccount = (allDevices: boolean) =>
  invoke<AccountState>("logout_account", { allDevices });
export const deleteAccount = (username: string, password: string) =>
  invoke<AccountState>("delete_account", { username, password });
export const syncNow = () => invoke<SyncResult>("sync_now");

// ---------- 版本更新（Gitee Releases 源） ----------
export const getAppVersion = () => invoke<string>("get_app_version");
export const checkUpdate = () => invoke<UpdateInfo>("check_update");
export const openReleasePage = (url: string) =>
  invoke<void>("open_release_page", { url });
export const downloadUpdate = (url: string) =>
  invoke<string>("download_update", { url });
export const launchInstaller = (path: string) =>
  invoke<void>("launch_installer_and_quit", { path });

// ---------- 图片 ----------
export const pickImage = () => invoke<string | null>("pick_image");
export const readAvatar = (path: string) =>
  invoke<string>("read_avatar", { path });
export const readImage = (path: string) =>
  invoke<string>("read_image", { path });

// ---------- 本地启动 ----------
export const pickLaunchFiles = () => invoke<string[]>("pick_launch_files");
export const pickLaunchFolder = () => invoke<string | null>("pick_launch_folder");
export const addLaunchItems = (paths: string[]) =>
  invoke<AppSettings>("add_launch_items", { paths });
export const removeLaunchItems = (ids: string[]) =>
  invoke<AppSettings>("remove_launch_items", { ids });
export const runLaunchItem = (path: string) =>
  invoke<void>("run_launch_item", { path });
export const buildLaunchItem = (path: string) =>
  invoke<LaunchItem>("build_launch_item", { path });

// ---------- 图标 ----------
export const loadIcons = (paths: string[]) =>
  invoke<Record<string, string>>("load_icons", { paths });

// ---------- 插件管理 ----------
export const listPlugins = () => invoke<PluginInfo[]>("list_plugins");
export const setPluginEnabled = (name: string, enabled: boolean) =>
  invoke<void>("set_plugin_enabled", { name, enabled });
export const setPluginPermission = (name: string, perm: string, granted: boolean) =>
  invoke<void>("set_plugin_permission", { name, perm, granted });
export const deletePlugin = (name: string) =>
  invoke<void>("delete_plugin", { name });
export const rescanPlugins = () => invoke<number>("rescan_plugins");

// ---------- 插件详情页：README + schema 驱动设置 ----------
/** 读插件 README.md（无则 null） */
export const pluginReadme = (name: string) =>
  invoke<string | null>("plugin_readme", { name });
/** 读插件设置 schema（无 settings.json 则 null） */
export const pluginSettingsSchema = (name: string) =>
  invoke<SettingsSchema | null>("plugin_settings_schema", { name });
/** 读插件当前生效设置值（schema 默认 + 用户覆盖） */
export const pluginSettingsValues = (name: string) =>
  invoke<Record<string, unknown>>("plugin_settings_values", { name });
/** 即时保存插件的一个设置值 */
export const pluginSettingsSet = (name: string, key: string, value: unknown) =>
  invoke<void>("plugin_settings_set", { name, key, value });
/** 重置插件全部设置值回默认 */
export const pluginSettingsReset = (name: string) =>
  invoke<void>("plugin_settings_reset", { name });

// ---------- 窗口 ----------
export const closeAdminWindow = () => invoke<void>("close_admin_window");
