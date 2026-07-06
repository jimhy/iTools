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
  /** 截图全局快捷键（宿主内置原生截图，空 = 不启用） */
  screenshot_hotkey: string;
  /** 贴图全局快捷键（宿主内置原生贴图：读剪贴板贴成浮窗，空 = 不启用） */
  pin_hotkey: string;
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
  /** 是否有 README.md（详情页「说明」tab 是否可用） */
  has_readme: boolean;
  /** 是否有 settings.json（详情页「设置」tab 是否可用） */
  has_settings: boolean;
}

/** 插件设置控件类型，与 Rust 侧 plugin::settings 支持的 type 对齐 */
export type SettingsItemType =
  | "text"
  | "textarea"
  | "number"
  | "boolean"
  | "select"
  | "path"
  | "color"
  | "hotkey";

/** 一个下拉选项 */
export interface SettingsOption {
  value: unknown;
  label: string;
}

/** 插件设置项声明，与 Rust 侧 `SettingsItem` 对齐 */
export interface SettingsItem {
  key: string;
  type: SettingsItemType;
  label: string;
  description?: string;
  default?: unknown;
  /** select 选项 */
  options?: SettingsOption[];
  /** number 范围/步进 */
  min?: number;
  max?: number;
  step?: number;
  /** path 选择模式（缺省 folder） */
  mode?: "file" | "folder";
  placeholder?: string;
}

/** 一组设置项，与 Rust 侧 `SettingsGroup` 对齐 */
export interface SettingsGroup {
  title: string;
  description: string;
  items: SettingsItem[];
}

/** 插件设置 schema（规范化后顶层永远是 groups），与 Rust 侧 `SettingsSchema` 对齐 */
export interface SettingsSchema {
  version: number;
  groups: SettingsGroup[];
}

/** 「从不」自动清除的哨兵值（与 Rust `AUTO_CLEAR_NEVER` 对齐） */
export const AUTO_CLEAR_NEVER = 4294967295;

/** 账号资料（显示型），与 Rust 侧 `Profile` 保持一致。
 *  登录态/同步开关已移到 `AccountState`；`phone` 默认空（未绑定不展示）。 */
export interface Profile {
  nickname: string;
  avatar_path: string | null;
  phone: string;
  first_use_ts: number;
}

/** 账号资料快照（含派生的陪伴天数），与 Rust 侧 `ProfileView`（flatten）一致 */
export interface ProfileView extends Profile {
  companion_days: number;
}

/** 云账号态，与 Rust 侧 `AccountState`（camelCase）一致。
 *  本地优先 + 配置化云端：`cloudConfigured=false` 表示云端未接入（只能本地）。 */
export interface AccountState {
  loggedIn: boolean;
  username: string;
  cloudConfigured: boolean;
  syncEnabled: boolean;
}

/** 数据同步结果，与 Rust 侧 `SyncResult`（camelCase）一致。
 *  `synced=false` 时 `reason` ∈ cloud_not_configured | not_logged_in | offline | error。 */
export interface SyncResult {
  synced: boolean;
  reason?: string;
  pushed: number;
  pulled: number;
  message?: string;
}

/** 版本更新信息，与 Rust 侧 `UpdateInfo`（camelCase）一致 */
export interface UpdateInfo {
  latestVersion: string;
  currentVersion: string;
  hasUpdate: boolean;
  releaseUrl: string;
  releaseNotes: string;
  msiUrl: string | null;
}

/** 主面板数据，与 Rust 侧 `HomeData` 保持一致 */
export interface HomeData {
  user: string;
  recent: SearchItem[];
  pinned: SearchItem[];
}
