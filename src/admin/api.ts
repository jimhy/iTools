//! 管理中心与 Rust 后端之间的 invoke 封装，集中类型标注。
//! 参数键用 camelCase（Tauri 2 默认自动转 snake_case）。
import { invoke } from "@tauri-apps/api/core";
import type { AppSettings, ProfileView, LaunchItem, PluginInfo } from "../types";

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
export const setDataSync = (enabled: boolean) =>
  invoke<ProfileView>("set_data_sync", { enabled });
export const logoutAccount = (allDevices: boolean) =>
  invoke<ProfileView>("logout_account", { allDevices });
export const deleteAccount = (username: string, password: string) =>
  invoke<ProfileView>("delete_account", { username, password });

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

// ---------- 窗口 ----------
export const closeAdminWindow = () => invoke<void>("close_admin_window");
