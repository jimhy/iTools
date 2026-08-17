//! 管理中心与 Rust 后端之间的 invoke 封装，集中类型标注。
//! 参数键用 camelCase（Tauri 2 默认自动转 snake_case）。
import { invoke as rawInvoke } from "@tauri-apps/api/core";
import type {
  AppSettings,
  ProfileView,
  LaunchItem,
  PluginInfo,
  AccountState,
  SyncResult,
  DataUsage,
  UpdateInfo,
  UpdateStatus,
  ProxyTestResult,
  EndpointInfo,
  SettingsSchema,
  MarketView,
  InstallPreview,
  PluginUpdate,
  MirrorConfigView,
  MirrorTestResult,
  MirrorMode,
  DevPluginInfo,
  DevConfig,
  DevKv,
  DevLogEntry,
  DevMockConfig,
  McpStatus,
  AiClientsStatus,
  PublishStatus,
  Preflight,
  Submission,
} from "../types";

// ---------- 启动竞态兜底 ----------

/**
 * 管理中心窗口由 `tauri.conf.json` 静态创建，它的 WebView 会**先于后端 `setup()` 完成**
 * 就加载页面并执行 JS；而 `ProfileStore` / `AccountStore` / `PluginRegistry` 等 State
 * 要等插件扫描、播种结束才 `manage`（首次安装尤其慢）。撞进这个窗口期的 invoke 会被
 * 「state not managed for field `xxx` on command `yyy`」直接拒掉——表现为首屏「我的账号」
 * 显示「账号信息加载失败」，且**不会自愈**（管理中心窗口只隐藏不销毁，boot 只跑一次，
 * 必须点别的导航再点回来才重渲染）。
 *
 * 主窗口在 v1.4.4 已用同样的思路修过（见 `src/main.ts` 的 refreshHome）。这里把重试下沉到
 * invoke 封装层，管理中心所有面板一并受益，面板代码一行不用改。
 *
 * 只对「State 尚未 manage」这一种错误重试：它只可能出现在启动窗口期；真的漏了 manage 时
 * 重试 8 次（累计约 4.3 秒）后照样抛出，不会把 bug 藏起来。
 */
function isStateNotManaged(err: unknown): boolean {
  return typeof err === "string" && err.includes("state not managed");
}

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => window.setTimeout(resolve, ms));

async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  for (let retry = 0; ; retry++) {
    try {
      return await rawInvoke<T>(cmd, args);
    } catch (err) {
      if (retry >= 8 || !isStateNotManaged(err)) throw err;
      await sleep(120 * (retry + 1)); // 退避：120/240/…/960ms，累计约 4.3 秒
    }
  }
}

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
/** 「我的数据」用量（本地各命名空间条数 + 云端用量；不可用时 cloud=null + cloudReason）。
 *  `includeCloud=false` 时后端**完全不碰网络**，只回本地统计（毫秒级）——
 *  页面据此先秒开本地、再异步补云端，避免每次进页面都卡在一次云端往返上。 */
export const dataUsage = (includeCloud = true) =>
  invoke<DataUsage>("data_usage", { includeCloud });

// ---------- 版本更新（Gitee Releases 源） ----------
export const getAppVersion = () => invoke<string>("get_app_version");
export const checkUpdate = () => invoke<UpdateInfo>("check_update");
/** 上次更新检查的结果快照（**本地瞬时、不发请求**）。
 *  进页面时用它显示「什么时候自动查的、查出什么」——没有它，
 *  「查过了没新版」与「压根没查成」在界面上无法区分。 */
export const updateStatus = () => invoke<UpdateStatus>("update_status");
export const openReleasePage = (url: string) =>
  invoke<void>("open_release_page", { url });
export const downloadUpdate = (url: string) =>
  invoke<string>("download_update", { url });
export const launchInstaller = (path: string) =>
  invoke<void>("launch_installer_and_quit", { path });

// ---------- 网络：代理 & 生效中的同步服务器地址 ----------

/** 用**给定地址**发起一次真实的代理连通性探测（与 `proxy_enabled` 开关无关：
 *  开关关着也能测，测的是这个地址本身）。
 *
 *  ⚠ 返回的 `viaProxy=false` 表示本次**没有**经过代理，此时 `ok=true` 不能当作「代理可用」，
 *  UI 必须如实区分（见 ProxyTestResult 的注释）。 */
export const testProxy = (address: string) =>
  invoke<ProxyTestResult>("test_proxy", { address });

/** 当前**实际生效**的云同步服务器地址及其来源（env / 用户手填 / dev 默认 / 未接入）。
 *  只读命令：改地址请走「我的账号 → 数据同步」（`save_settings` 的 `sync_endpoint`），
 *  网络页签不提供第二个输入框。 */
export const syncEndpointInfo = () => invoke<EndpointInfo>("sync_endpoint_info");

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

// ---------- 从 Git 安装插件 / 插件更新 ----------
/** 下载并解析 Git 仓库里的插件包，返回预览（**不落地**，暂存在后端，token 用于确认/取消）。 */
export const pluginInstallPreview = (url: string) =>
  invoke<InstallPreview>("plugin_install_preview", { url });
/** 确认安装：把 token 对应的暂存目录落地到 plugins 根并重载插件。 */
export const pluginInstallConfirm = (token: string) =>
  invoke<void>("plugin_install_confirm", { token });
/** 放弃安装：丢弃 token 对应的暂存目录（离开安装弹窗的任何路径都要调）。 */
export const pluginInstallCancel = (token: string) =>
  invoke<void>("plugin_install_cancel", { token });
/** 检查全部插件的远端新版本（无 Git 来源的返回 checked=false，失败的带 error）。 */
export const pluginCheckUpdates = () => invoke<PluginUpdate[]>("plugin_check_updates");
/** 按锁文件里的来源重新下载并原子替换该插件目录，返回更新后的状态。 */
export const pluginUpdate = (name: string) =>
  invoke<PluginUpdate>("plugin_update", { name });
/** 用系统浏览器打开该插件的来源仓库网页（无来源则后端报错）。 */
export const pluginOpenSourcePage = (name: string) =>
  invoke<void>("plugin_open_source_page", { name });

// ---------- 插件下载源（官方 + 镜像竞速：分组抢跑，不是裸竞速） ----------
/** 当前生效的镜像配置 + 来源（服务端/本地缓存/内置默认）+ 上次拉取时间 + 当前模式。 */
export const pluginMirrorConfig = () => invoke<MirrorConfigView>("plugin_mirror_config");
/** 立即从服务端重拉镜像配置（失败会原样返回错误，不静默沿用旧配置而谎称成功）。 */
export const pluginMirrorRefresh = () => invoke<MirrorConfigView>("plugin_mirror_refresh");
/** 就地测速全部候选源（含 GitHub 官方源），返回每个源的可达性与延迟。 */
export const pluginMirrorTest = () => invoke<MirrorTestResult[]>("plugin_mirror_test");
/** 设置下载源模式（语义以 mirror.rs `build_candidates` / `race_groups` 为准）：
 *  - `auto`：官方源抢跑 400ms，官方在窗口内成功则镜像一个请求都不发；否则镜像入场竞速；
 *  - `official`：只有官方源一个候选，镜像永不参与；
 *  - `mirror`：**镜像优先**（不是「仅镜像」）——镜像先抢跑 400ms，镜像全败或窗口到期后官方即入场兜底。 */
export const pluginMirrorSetMode = (mode: MirrorMode) =>
  invoke<void>("plugin_mirror_set_mode", { mode });

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

// ---------- 开发者中心（插件调试环境） ----------
//
// 全部 dev_* 命令都只作用于**调试环境**：独立测试库、独立沙盒根、独立授权表、本地同步 Mock。
// 它们碰不到正式插件的数据，也不会把调试数据推上云端（物理隔离，不是过滤条件）。
// 参数键一律 camelCase（Tauri 2 自动转 Rust 侧 snake_case，如 afterSeq → after_seq）。

/** 列出全部调试插件（含清单校验结果 issues / features / 调试授权态）。 */
export const devListPlugins = () => invoke<DevPluginInfo[]>("dev_list_plugins");
/** 重新扫描全部调试目录。返回值是后端的统计口径（见 dev.ts 里的说明，面板不直接展示它）。 */
export const devRescan = () => invoke<number>("dev_rescan");
/** 开发者中心的全局配置：调试目录列表 / 主搜索开关 / 同步 Mock / 测试库与沙盒的真实路径。 */
export const devGetConfig = () => invoke<DevConfig>("dev_get_config");
/** 原生目录选择器；用户取消返回 null。 */
export const devPickDir = () => invoke<string | null>("dev_pick_dir");
/** 添加一个调试目录（可指向 <插件源码>/dist 这类构建产物），返回新的配置快照。 */
export const devAddDir = (path: string) => invoke<DevConfig>("dev_add_dir", { path });
/** 移除一个调试目录（固定的 dev-plugins/ 不可移除，后端会拒绝），返回新的配置快照。 */
export const devRemoveDir = (path: string) => invoke<DevConfig>("dev_remove_dir", { path });
/** 调试插件是否参与主搜索（默认 false）。 */
export const devSetSearchVisible = (enabled: boolean) =>
  invoke<void>("dev_set_search_visible", { enabled });
/** 触发模拟器：以指定 feature / 触发方式 / query 打开调试插件窗口。 */
export const devOpenPlugin = (id: string, code: string, query: string, kind: string) =>
  invoke<void>("dev_open_plugin", { id, code, query, kind });
/** 用资源管理器打开该调试插件的目录。 */
export const devOpenDir = (id: string) => invoke<void>("dev_open_dir", { id });
/** 在**调试授权表**里授权/撤销某项高危能力（不影响正式安装后的授权状态）。 */
export const devSetPermission = (id: string, perm: string, granted: boolean) =>
  invoke<void>("dev_set_permission", { id, perm, granted });

/** 列出该插件在测试库里的键值。`kind`：`"db"` = 纯本地（itools.db.*）；`"data"` = 参与云同步的那套（itools.data.*）。 */
export const devStorageList = (id: string, kind: string) =>
  invoke<DevKv[]>("dev_storage_list", { id, kind });
/** 写入/覆盖一条键值（value 为 JSON 文本）。 */
export const devStorageSet = (id: string, kind: string, key: string, value: string) =>
  invoke<void>("dev_storage_set", { id, kind, key, value });
/** 删除一条键值。 */
export const devStorageRemove = (id: string, kind: string, key: string) =>
  invoke<void>("dev_storage_remove", { id, kind, key });
/** 清空该插件在测试库里的全部数据（db 与 data 两种都清）。 */
export const devStorageClear = (id: string) => invoke<void>("dev_storage_clear", { id });
/** 导出该插件测试库数据为 JSON 文本。 */
export const devStorageExport = (id: string) => invoke<string>("dev_storage_export", { id });
/** 从 JSON 文本导入数据到测试库。 */
export const devStorageImport = (id: string, json: string) =>
  invoke<void>("dev_storage_import", { id, json });

/** 增量拉取调试日志：返回 seq > afterSeq 的条目（传 0 = 拉当前缓冲区里的全部）。 */
export const devLogList = (afterSeq: number) =>
  invoke<DevLogEntry[]>("dev_log_list", { afterSeq });
/** 清空后端日志缓冲区。 */
export const devLogClear = () => invoke<void>("dev_log_clear");
/** 读取云同步 Mock 配置。 */
export const devGetMock = () => invoke<DevMockConfig>("dev_get_mock");
/** 保存云同步 Mock 配置（整包写入）。 */
export const devSetMock = (cfg: DevMockConfig) => invoke<void>("dev_set_mock", { cfg });

// ---------- 插件开发 MCP 服务器 ----------
/** MCP 服务器的运行状态与连接地址。**来自实际监听结果**：起不来时 running=false 且带真实原因。 */
export const mcpStatus = () => invoke<McpStatus>("mcp_status");

// ---------- AI 助手接入（MCP + 插件开发 skill） ----------
/** 读各 AI 助手的接入状态：MCP 配没配、skill 装没装、客户端检测到没有、CLI 在不在。 */
export const aiClientsStatus = () => invoke<AiClientsStatus>("ai_clients_status");
/** 一键接入：同时装 skill 与配 MCP。
 *  两件事**分别报告成败**——一件成一件败时后端会说清哪个成了哪个没成，
 *  UI 必须把这条原文显示出来，不能笼统报「失败」。 */
export const aiClientInstall = (target: string) =>
  invoke<AiClientsStatus>("ai_client_install", { target });
/** 断开接入：删 skill（只删带 iTools 标记的）+ 移除 MCP 配置（只删 itools 那一项）。 */
export const aiClientUninstall = (target: string) =>
  invoke<AiClientsStatus>("ai_client_uninstall", { target });

// ---------- 插件提审 ----------
/** 查一个调试插件的发布状态（线上版本 + 提审历史）。**要联网**，会拉市场索引与提审接口。
 *  任何一段查不到都会写进返回值的 error，UI 必须如实展示，不能渲染成「没有记录」。 */
export const devPublishStatus = (id: string) => invoke<PublishStatus>("dev_publish_status", { id });
/** 提审前的本地自检。只拦服务端**必然**会拒的那几条 —— 通过它不代表会过审。 */
export const devPreflight = (id: string) => invoke<Preflight>("dev_preflight", { id });
/** 打包并提交审核。返回服务端建立的提审单（状态一般是 reviewing）。 */
export const devSubmitPlugin = (id: string) => invoke<Submission>("dev_submit_plugin", { id });
/** 查一条提审单的详情（含模型裁决原文）。 */
export const devSubmissionDetail = (id: string) => invoke<Submission>("dev_submission_detail", { id });

// ---------- 插件市场 ----------
/** 拉取市场列表。失败**不 reject**：会带着 error + 本地缓存返回，由 UI 如实呈现。
 *
 *  `force=false`（默认）：缓存还在就直接返回它，**一个请求都不发**；返回值的 `stale`
 *  告诉你这份数据是不是该更新了。`force=true`：无视缓存真拉一次（用户点「刷新」时用）。
 *  推荐用法：先 marketList() 立即渲染，若 stale 再后台 marketList(true) 重绘。 */
export const marketList = (force = false) => invoke<MarketView>("market_list", { force });
/** 从市场安装（预览阶段）。与手动粘 URL 走同一条链路，额外带上索引给的内容哈希校验。
 *  确认安装仍调 pluginInstallConfirm(token)。 */
export const marketInstallPreview = (name: string) =>
  invoke<InstallPreview>("market_install_preview", { name });

// ---------- 窗口 ----------
export const closeAdminWindow = () => invoke<void>("close_admin_window");
