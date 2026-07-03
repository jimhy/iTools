/** 搜索结果的一条记录，与 Rust 侧 `SearchItem` 序列化结构保持一致 */
export interface SearchItem {
  id: string;
  title: string;
  subtitle: string;
  kind: "app" | "file" | "folder" | "command" | "plugin";
  target: string;
  icon?: string | null;
  action: "open" | "copy" | "plugin";
}

/** 主题取值 */
export type Theme = "system" | "light" | "dark";

/** 本地启动的一条目，与 Rust 侧 `LaunchItem` 保持一致 */
export interface LaunchItem {
  id: string;
  path: string;
  name: string;
  is_dir: boolean;
}

/** 应用设置，与 Rust 侧 `AppSettings` 保持一致 */
export interface AppSettings {
  opacity: number;
  background_image: string | null;
  hotkey: string;
  custom_apps: string[];
  autostart: boolean;
  theme: Theme;
  background_enabled: boolean;
  background_dim: number;
  search_placeholder: string;
  separate_hotkey: string;
  auto_clear_seconds: number;
  proxy_enabled: boolean;
  proxy_address: string;
  local_launch_items: LaunchItem[];
  disabled_plugins: string[];
  plugin_permissions: Record<string, string[]>;
}

/** 已装插件信息，与 Rust 侧 `PluginInfo` 保持一致 */
export interface PluginInfo {
  name: string;
  description: string;
  version: string;
  author: string;
  feature_count: number;
  cmds: string[];
  logo: string | null;
  enabled: boolean;
  /** 声明所需的高危能力（runCommand / network） */
  permissions: string[];
  /** 已授权的能力 */
  granted: string[];
}

/** 「从不」自动清除的哨兵值（与 Rust `AUTO_CLEAR_NEVER` 对齐） */
export const AUTO_CLEAR_NEVER = 4294967295;

/** 账号资料，与 Rust 侧 `Profile` 保持一致 */
export interface Profile {
  nickname: string;
  avatar_path: string | null;
  phone: string;
  first_use_ts: number;
  data_sync_enabled: boolean;
  wechat_bound: boolean;
  logged_in: boolean;
}

/** 账号资料快照（含派生的陪伴天数），与 Rust 侧 `ProfileView`（flatten）一致 */
export interface ProfileView extends Profile {
  companion_days: number;
}

/** 主面板数据，与 Rust 侧 `HomeData` 保持一致 */
export interface HomeData {
  user: string;
  recent: SearchItem[];
  pinned: SearchItem[];
}
