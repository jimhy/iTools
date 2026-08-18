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

/** 全盘文件索引（直读 NTFS MFT 的提权守护进程）的状态快照，与 Rust 命令
 *  `file_index_status` 的返回**逐字段**一致。
 *
 *  ⚠ 这些字段是 `/f` 覆盖范围的唯一真相来源：`running=false` 说明全盘索引没开，
 *  此时 `/f` 走的是覆盖面很窄的降级后端（大致只有用户目录），界面**必须**如实说出来。 */
export interface FileIndexStatus {
  /** 提权索引守护进程是否在跑。false 时其余字段没有意义（没有守护可问），不得据此显示任何数字 */
  running: boolean;
  /** "building"=首次/重建中；"ready"=全部盘就绪；"partial"=部分盘失败；"error"=守护内部异常 */
  state: string;
  /** 已就绪的盘符，如 ["C","D"]（只含本机固定 NTFS 磁盘） */
  ready_drives: string[];
  /** [盘符, 失败原因]；原因是后端翻好的中文（如「拒绝访问」），原样展示给用户 */
  failed_drives: [string, string][];
  /** 可搜索条目总数。
   *  ⚠ `state="building"` 期间这个值是**上一轮**建索引的旧值（首次建索引时为 0）——
   *  守护只在一轮建完时才写回它，所以建索引期间绝不能拿它当进度显示。 */
  entries: number;
  /** 索引真实占用（MB） */
  memory_mb: number;
  /** 因排除规则跳过的条目数 */
  excluded: number;
}

/** `file_index_enable`（用户主动开启全盘索引，会弹一次 UAC）的返回，
 *  与 Rust 侧 `FileIndexEnableResult` **逐字段**一致。 */
export interface FileIndexEnableResult {
  /** 本次的真实结局，四取一（判定逻辑在 Rust 侧 `commands::file_index_enable`）：
   *  - `already_running`：守护本来就在跑，这次没弹 UAC；
   *  - `starting`：已获授权、守护已上线，正在建索引（期间 `state="building"`）；
   *  - `declined`：**确证**用户在 UAC 上点了「否」；
   *  - `pending`：等待窗口内既没等到守护上线、也没确证被拒（UAC 可能还开着）。
   *
   *  ⚠ 别在前端替 `pending` 补成「被拒绝」——后端已经说明它判不出来了，
   *  再往前推一步就是替用户编结论。 */
  outcome: string;
  /** 返回时守护是否已在应答。为 true 只代表服务活着，索引可能还在建（进度看 `file_index_status`） */
  running: boolean;
  /** 后端给好的中文说明，与 `outcome` 一一对应，**原样**显示即可，不要在前端另拼文案 */
  message: string;
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
  /** 是否让**所有出站请求**经过 `proxy_address` 配置的代理。
   *  真实生效范围（后端按此实现，UI 文案必须与之一致）：插件下载与安装 / 插件更新检查与下载 /
   *  云同步与登录登出注销 / app 更新检查与 msi 下载 / 插件的 `itools.fetch`。
   *  **本地地址恒直连**：localhost、整段 127.0.0.0/8、::1、私网段（10/8、172.16/12、192.168/16）
   *  与 *.local 不受本开关影响，IPv4-mapped 写法（`::ffff:127.0.0.1`）会折回 IPv4 后同样直连
   *  （否则本机同步服务端会被塞进代理，联调直接断掉）。 */
  proxy_enabled: boolean;
  /** 代理地址。口径以 `http.rs::normalize_proxy` 为准（改这里先去读那个函数）：
   *  `host:port`（不写 scheme 默认按 HTTP 代理）、`http://…`、`https://…`（归一为 HTTP CONNECT 代理，
   *  不与代理之间再套 TLS）、以及带认证的 `user:pass@host:port`（按**最后一个** `@` 切分）。
   *
   *  **SOCKS 可用**（ureq 开了 `socks-proxy` feature），形态以 ureq 2.12.1 的实际实现为准：
   *  - `socks5://`、`socks://`、`socks5h://` → 统一按 SOCKS5 处理（域名交给代理解析），支持 `user:pass` 鉴权；
   *    其中 `socks5h://` 是本侧归一的——ureq 不认这个字面量，但它的 socks5 本就是远端解析域名，语义等同。
   *  - `socks4://` / `socks4a://` → SOCKS4 / 4A，**不支持鉴权**：ureq 把 userid 写死成空串，
   *    带凭据会被后端拒绝（而不是静默丢掉密码）。
   *
   *  端口必填、IPv6 字面量不支持，其余非法形态一律在入口被拒。
   *  UI 文案（settings.ts 的「代理地址」副标题）必须与本注释同口径。
   *
   *  `proxy_enabled=false` 时保留但不生效；`proxy_enabled=true` 时后端**保存前**就会校验本字段，
   *  空值或非法值会让整次 `save_settings` 失败（不只是代理项不保存）。 */
  proxy_address: string;
  local_launch_items: LaunchItem[];
  disabled_plugins: string[];
  plugin_permissions: Record<string, string[]>;
  /** 插件窗口尺寸记忆：插件 id → [宽, 高]（逻辑像素）。
   *  **后端独占字段**：由插件窗口 resize / 关闭时的 `SettingsStore::set_plugin_window` 写入。
   *  设置页保存的是整包快照，这里声明出来只是为了让快照原样带回去；
   *  即便前端瞎改，`settings.rs::preserve_backend_owned` 也会用 store 现值覆盖（前端改不动它）。
   *  历史教训：TS 里漏声明本字段时，保存一次设置就把所有插件调好的窗口尺寸清空了。 */
  plugin_window_sizes: Record<string, [number, number]>;
  /** 插件下载源偏好，取值 `"auto"` | `"official"` | `"mirror"`（见 MirrorMode）。
   *  **后端独占字段**：由 `plugin_mirror_set_mode` 命令独占维护，整包保存时被 `preserve_backend_owned`
   *  还原成 store 现值 —— 想改模式请调那个命令，改这里不生效。
   *  类型写 string 而非 MirrorMode 是如实反映后端：store 里存的是任意字符串，
   *  Rust `Mode::parse` 对认不出的值一律回落到 auto。 */
  plugin_mirror_mode: string;
  /** 截图全局快捷键（宿主内置原生截图，空 = 不启用） */
  screenshot_hotkey: string;
  /** 贴图全局快捷键（宿主内置原生贴图：读剪贴板贴成浮窗，空 = 不启用） */
  pin_hotkey: string;
  /** 云同步服务器地址（用户手填，如 https://cloud.example.com:7101）。空 = 使用内置默认服务，见 EndpointInfo.builtinDefault。 */
  sync_endpoint: string;
  /** 开发者中心的调试插件目录（用户手动添加的绝对路径；固定的 `dev-plugins/` 不在此列）。
   *  **后端独占字段**：由 `dev_add_dir` / `dev_remove_dir` 独占维护，整包保存时被
   *  `preserve_backend_owned` 还原成 store 现值。 */
  dev_plugin_dirs: string[];
  /** 调试环境的插件授权表：插件 id → 已授权的高危能力。
   *  **后端独占字段**：由 `dev_set_permission` 独占维护。
   *  与正式的 `plugin_permissions` **完全分开**——在开发者中心授的权只在调试窗口生效，
   *  不会让该插件正式安装后也带着权限。 */
  dev_plugin_permissions: Record<string, string[]>;
  /** 调试插件是否并入主搜索索引（默认 false，避免开发中的插件污染日常搜索）。
   *  **后端独占字段**：由 `dev_set_search_visible` 独占维护。 */
  dev_search_visible: boolean;
}

/** 插件的 Git 安装来源，与 Rust 侧 `GitSource`（camelCase）一致。
 *  `subPath` 为空表示仓库根目录即插件；`revision` 为空表示跟随仓库默认分支。 */
export interface GitSource {
  /** 接口**风格名**："gitee" | "github"。注意：自建 Gitee/Gitea 站点的 host 同样是 "gitee"，
   *  它不是域名，不可直接拿来展示（展示请用 `domain`，见 plugin-install.ts 的 sourceDomain）。 */
  host: string;
  /** 真实来源域名（如 gitee.com / github.com / git.corp.com）。
   *  Rust 侧 `#[serde(default)]`：本字段之前的老锁文件里没有它，反序列化后为空串。 */
  domain: string;
  owner: string;
  repo: string;
  /** 仓库内子目录，"" = 仓库根 */
  subPath: string;
  /** 分支 / tag / 完整 commit sha，"" = 跟随默认分支 */
  revision: string;
  /** 规范化后的完整安装 URL（可原样再次安装） */
  url: string;
  /** 仓库网页地址，供「查看仓库」 */
  pageUrl: string;
}

/** 安装前的插件包预览（尚未落地到 plugins 目录），与 Rust 侧 `InstallPreview` 一致。
 *  `token` 是后端暂存目录的句柄：确认安装或放弃都必须回传，否则暂存不会被清理。 */
export interface InstallPreview {
  token: string;
  name: string;
  version: string;
  description: string;
  author: string;
  /** 插件声明的高危能力（安装后仍默认未授权） */
  permissions: string[];
  featureCount: number;
  /** 关键字预览 */
  cmds: string[];
  /** logo 的 data URL（无则 null） */
  logo: string | null;
  /** README.md 原文（后端已截断，无则 null） */
  readme: string | null;
  source: GitSource;
  /** 同名插件是否已安装 */
  alreadyInstalled: boolean;
  /** 本次安装与已安装版本**来源相同**（同仓库/同子目录）。
   *  后端只在「换来源 / 全新安装」时清空 plugin_permissions，同源覆盖会**保留**历史授权，
   *  UI 的授权文案必须据此分支（否则会对同源升级谎称「安装后默认不授权」）。
   *  未安装时恒为 false。 */
  sameSource: boolean;
  /** 已安装的版本号（未安装则 null） */
  installedVersion: string | null;
  /** 同名的是随包分发的内置插件。**这不表示本次安装会被拒** —— 见 builtinBlocked。 */
  isBuiltin: boolean;
  /** 本次安装是否**因为内置规则被拒**（= 是内置插件 且 来源是 Git 直装）。
   *  市场安装放行：市场里每个版本都过了服务端审核，落地后播种也会跳过它。
   *  按钮禁用看这个字段，不要看 isBuiltin —— 只看后者会让市场里已上架的内置插件
   *  显示成「无法安装」（上架了却点不动）。 */
  builtinBlocked: boolean;
  fileCount: number;
  totalBytes: number;
  /** 本次归档**实际**来自哪个源的可读名（如 `github.com（官方直连）` / `gh-proxy.com`）。
   *  这是下载**之后**的事实，不是「可能会走哪里」的猜测——UI 必须据此如实告知。 */
  downloadSource: string;
  /** 本次是否真的经由第三方镜像中转（镜像有能力篡改内容）。 */
  viaMirror: boolean;
  /** 本次下载是否做过 sha256 校验。手动粘贴仓库 URL 安装**没有可信哈希来源** → 恒 false，
   *  此时 UI 必须显示「本次下载未经哈希校验」，不许含糊过去（见 install.rs 同名字段注释）。 */
  hashVerified: boolean;
}

/** 单个插件的更新检查结果，与 Rust 侧 `PluginUpdate` 一致。
 *  诚实语义：`checked=false` 表示**没有真的查过**（无 Git 来源 / 被锁定），不得当作「已是最新」；
 *  `error` 非空表示这次检查失败，必须如实呈现原因。 */
export interface PluginUpdate {
  name: string;
  currentVersion: string;
  /** 远端最新版本（未检查或失败则 null） */
  latestVersion: string | null;
  hasUpdate: boolean;
  /** 来源 revision 是完整 commit sha → 锁定版本，不跟随更新 */
  pinned: boolean;
  /** 是否真的完成了一次远端检查 */
  checked: boolean;
  /** 检查失败的真实原因 */
  error: string | null;
  /** 本次请求**实际**用的下载源可读名；没发过请求（无 Git 来源 / 已锁定 / 命令整体失败）则 null */
  downloadSource: string | null;
  /** 本次请求是否经由第三方镜像（没发请求时为 false） */
  viaMirror: boolean;
}

// ---------- 插件下载源（GitHub 官方源 + 第三方镜像） ----------
//
// ⚠ 本节的每个字段都是从 `src-tauri/src/plugin/mirror.rs` 的结构体**逐字段**抄过来的
//   （Rust 侧统一 `#[serde(rename_all = "camelCase")]`）。
//   `invoke<T>()` 只是**类型断言**，写错了 `tsc --noEmit` 照样全绿、运行期才静默错位
//   （上一轮就因为把扁平的 MirrorConfigView 声明成嵌套的 `{ config: … }`，
//   导致面板对着 3 个正在用的内置镜像显示「当前配置里没有任何镜像站」）。
//   改这里之前请先对照 mirror.rs，不要凭印象改名。
//
// ✅ 这类漏改现在**有工具会发现**：`npm run check:contract`（scripts/check-contract.mjs）
//   拿 Rust 侧导出的字段清单（src-tauri/contract-snapshot.json，由 src-tauri/src/contract.rs 的
//   `contract_field_names_frozen` 测试生成）与本文件的 interface **双向**比对——
//   本文件少声明会报「TS 少声明」、多声明幽灵字段会报「TS 多声明」，清单缺失也不放行，一律 exit 1。
//   它已串进 `npm run check` / `npm run build` 与 tauri 的 beforeBuildCommand，release 路径必过。
//
//   历史教训（这道闸门就是为它们建的）：先是把扁平的 MirrorConfigView 声明成嵌套的
//   `{ config: … }`（TS 多声明方向），后是漏掉 host / unlisted 两个字段（TS 少声明方向）——
//   两次都是 `cargo test` 与 `tsc --noEmit` 双绿、功能却断在中间。
//
//   所以：**改 Rust 结构体后要重跑 `cargo test -j1 contract_field_names_frozen` 重新生成清单**，
//   否则清单会停留在旧版本，校验脚本比对的是过期快照（脚本本身不校验清单新鲜度）。

/** 一个第三方镜像站，与 Rust 侧 `MirrorEntry` 一致
 *  （`plugin_mirror_config` 返回的 `mirrors[]` 元素就是它 —— Rust 里 `type MirrorView = MirrorEntry`）。
 *  raw / archive **各自**给模板：实测各镜像支持的路径形态并不一致
 *  （如 ghfast.top 的归档形态 403 而 raw 正常），不能由一个模板推导另一个。
 *  占位符只有 {owner} {repo} {ref} {path}（archive 模板不含 {path}）。 */
export interface MirrorEntry {
  id: string;
  label: string;
  raw: string;
  archive: string;
  /** 服务端最近一次探测的结论；**false 的镜像不参与下载候选**（见 mirror.rs build_candidates）。
   *  探测数据可能已过期，UI 必须标注其时间来源。 */
  healthy: boolean;
  /** 服务端探测到的延迟（ms）；未知则 null */
  latencyMs: number | null;
  /** 最近一次探测成功的时间（ISO8601）；从未成功则 null */
  lastOkAt: string | null;
  /** 该镜像的**真实主机名**（如 `gh-proxy.com`；模板带显式端口时形如 `evil.tld:8443`，**端口不会被省略**）。
   *
   *  ⚠ Rust 侧标了 `#[serde(skip_deserializing)]`：这个字段**只出不进**——服务端 JSON 里的同名字段
   *  一律被丢弃，值由客户端 `sanitize` 从 `raw` 模板现场解析（mirror.rs `tpl_host`：取 `https://`
   *  之后、首个 `/ ? #` 之前的 authority，含 userinfo 时取最后一个 `@` 之后那段，转小写，**保留端口**）。
   *  `label` 是服务端可以随便填的文本（完全可以写着 `gh-proxy.com` 而模板指向别处），
   *  所以 UI 要展示「这个源到底是谁」**必须**用本字段：它同时也是后端判定 `unlisted` 所用的那个字符串，
   *  前端自己 `new URL(tpl).hostname` 解析会**丢掉端口**，显示出来的主机名与后端判定依据对不上。 */
  host: string;
  /** 该镜像的主机**不在客户端内置默认清单里**（= 由同步服务端下发引入的新主机）。
   *
   *  同样是 `skip_deserializing` 的**只出不进**字段，由 `sanitize` 现场判定，服务端无法把自己标成「内置」；
   *  判定口径是 raw 与 archive 两个模板的主机**都**在内置清单里才算内置，只要有一个不在就为 true。
   *  真实后果（mirror.rs `build_candidates`）：`unlisted` 的镜像在候选排序里被排到内置主机**之后**
   *  （竞速只并发前 3 个，内置主机可用时它通常压根不会被请求），但**并未被禁用**——
   *  内置主机全挂时它照样会被用上，「镜像失效了能换新的」正是这套机制存在的理由。
   *  因此 UI 必须如实标注：这是一条内置清单未背书的新代码分发路径。 */
  unlisted: boolean;
}

/** 当前生效配置来自兜底链的哪一层：服务端下发 / 本地缓存 / 编译进客户端的内置默认。
 *  对应 `MirrorConfigView.origin`（Rust 侧是 String，取值来自 `Origin::as_str`）。 */
export type MirrorOrigin = "server" | "cache" | "builtin";

/** 下载源模式：自动（官方抢跑）/ 只用 GitHub 官方 / 镜像优先。
 *
 *  真实语义见 mirror.rs `build_candidates` + `race_groups` + `HEAD_START`——**分组抢跑**，
 *  不是「谁先 2xx 谁赢」的裸竞速（那条已被定性为漏洞：响应快的恶意镜像能稳定压过官方源）：
 *  - `auto`：官方源先单独发请求，它在 400ms 抢跑窗口内成功 ⇒ 镜像**一个请求都收不到**；
 *    官方失败（不必等满窗口）或窗口到期，镜像才入场竞速，此时官方仍在赛道上、晚到的成功也算数；
 *  - `mirror`：**镜像优先**，不是「仅镜像」——镜像抢跑 400ms，镜像全败或窗口一到期官方就入场兜底
 *    （不是「等镜像全部失败」）；
 *  - `official`：候选集合里只有官方一个。 */
export type MirrorMode = "auto" | "official" | "mirror";

/** 下载源状态快照，与 Rust 侧 `MirrorConfigView`（camelCase）**逐字段**一致。
 *  这是一个**扁平**结构：mirrors / version / updatedAt 都在顶层，没有 `config` 这一层。 */
export interface MirrorConfigView {
  /** 当前生效配置来自哪一层兜底链（字段名是 origin，不是 source） */
  origin: MirrorOrigin;
  /** 配置服务器地址；未配置为 null（此时只用内置/缓存，属正常形态，不是错误） */
  serverUrl: string | null;
  /** 上次**成功**从服务端拉到配置的时间（RFC3339）；从未成功则 null */
  fetchedAt: string | null;
  /** 上次拉取配置失败的真实原因（字段名是 lastError，不是 error）；成功或未拉过则 null */
  lastError: string | null;
  mode: MirrorMode;
  /** 配置版本号 */
  version: number;
  /** 配置本身的更新时间（RFC3339）；内置默认为空串 */
  updatedAt: string;
  /** 镜像列表（顶层直接就是它）。官方源**不在**其中，它是客户端内置的固定候选。 */
  mirrors: MirrorEntry[];
  /** 官方源的可读名（后端下发，如 `github.com（官方直连）`），供 UI 直接展示 */
  officialLabel: string;
}

/** 测速的路径形态：raw 单文件 / archive 归档 zip。
 *  安装与更新下载插件包走 archive，检查更新读远端 plugin.json 走 raw —— 两者可以一通一不通。 */
export type MirrorTestTarget = "raw" | "archive";

/** 就地测速的**单条**结果，与 Rust 侧 `MirrorTestResult`（camelCase）一致。
 *  ⚠ `plugin_mirror_test` 对每个候选源分别测 raw 与 archive **两遍**，
 *  所以返回数组里同一个 `id` 会出现**两条**，靠 `target` 区分——按 id 去重会吃掉其中一条。
 *  `ok=false` 时 `error` 必须给出可区分的真实原因（DNS / 超时 / 403 / 404 / 校验失败）。 */
export interface MirrorTestResult {
  id: string;
  label: string;
  target: MirrorTestTarget;
  /** 是否为第三方镜像（false = GitHub 官方源）。后端**没有** `official` 字段。 */
  isMirror: boolean;
  /** 本次实际请求的完整 URL */
  url: string;
  ok: boolean;
  latencyMs: number | null;
  error: string | null;
  /** 是否比对了内容哈希。探测配置没给 sha256、或 target=archive（只发 Range 探首字节）时为 false */
  hashChecked: boolean;
  /** 哈希是否匹配（`hashChecked=false` 时无意义，不得据此下结论） */
  hashOk: boolean;
}

/** 已装插件信息，与 Rust 侧 `PluginInfo` 保持一致 */
export interface PluginInfo {
  /** 插件 id（机器标识，ASCII）。**UI 展示一律用 `display_name`**，
   *  否则同一个插件会在这处叫「云端笔记」、那处叫「deskbox」。 */
  name: string;
  /** 给用户看的名字；清单未声明时后端已回落成 `name`，前端可直接用。 */
  display_name: string;
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
  /** Git 安装来源；手工放入 plugins 目录或随包内置则为 null（无法自动更新） */
  source: GitSource | null;
  /** 是否随安装包分发的内置插件 */
  builtin: boolean;
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
 *  `synced=false` 时 `reason` ∈ cloud_not_configured | not_logged_in | offline | session_expired | error。
 *  （session_expired：会话失效，已自愈清本地登录态，需重新登录。） */
export interface SyncResult {
  synced: boolean;
  reason?: string;
  pushed: number;
  pulled: number;
  message?: string;
}

/** 单个命名空间的条数，与 Rust 侧 `NsCount` 一致（`ns` 形如 `app` / `plugin:<插件名>`）。 */
export interface NsCount {
  ns: string;
  /** **存储项**数（键的数量）——整份待办清单只占 1 项，每篇笔记正文各占 1 项。
   *  给用户看的数字请用 `items`，别拿这个当"数据条数"。 */
  count: number;
  /** 用户视角的业务条目明细（插件在 `plugin.json` 的 `dataSets` 里声明后才有）。
   *  为空 = 该插件没声明，界面只能按存储项显示并说明原因。 */
  items?: ItemCount[];
}

/** 一类业务条目的计数，与 Rust 侧 `ItemCount` 一致。 */
export interface ItemCount {
  /** 给用户看的名字，如「笔记」。 */
  label: string;
  count: number;
}

/** 云端用量快照，与 Rust 侧 `CloudUsage`（camelCase）一致。字段均为服务端真实统计值。 */
export interface CloudUsage {
  counts: NsCount[];
  total: number;
  /** 云端占用的原始字节数（服务端各记录 value 文本长度之和）。 */
  bytes: number;
}

/** 「我的数据」用量汇总，与 Rust 侧 `DataUsage`（camelCase）一致。
 *  本地始终真实；`cloud=null` 表示云端不可用，`cloudReason` 说明原因
 *  ∈ cloud_not_configured | not_logged_in | offline | session_expired | error。 */
export interface DataUsage {
  /** 可参与云同步的本地数据（`itools.data.*`）。 */
  local: NsCount[];
  localTotal: number;
  /** 纯本地数据（`itools.db.*`），**设计上不上云**——UI 必须与 `local` 分开呈现，
   *  否则用户会以为这部分也会同步。 */
  localOnly: NsCount[];
  localOnlyTotal: number;
  cloud: CloudUsage | null;
  cloudReason?: string;
}

// ---------- 网络：代理测试 + 生效中的同步服务器地址 ----------

/** 代理连通性测试的结果，与 Rust 侧 `ProxyTestResult`（camelCase）**逐字段**一致。
 *
 *  诚实语义（UI 必须照此呈现，不得简化）：
 *  - `viaProxy=false` 表示这次请求**根本没经过代理**（地址空 / 无法解析 / 目标命中本地直连规则）。
 *    此时 `ok=true` 只证明「直连通」，**不能**证明代理可用 —— UI 必须明说本次没走代理，
 *    否则用户会把「直连成功」误读成「代理配置正确」。
 *  - `ok=false` 时 `error` 是后端给的真实原因（连接被拒 / 超时 / 认证失败 / DNS 等），
 *    UI 必须原样展示，不许吞成「失败」。
 *  - `latencyMs` 可能为 null（未测到耗时），此时不得编造数字。 */
export interface ProxyTestResult {
  ok: boolean;
  /** 本次请求耗时（毫秒）；未知则 null */
  latencyMs: number | null;
  /** 失败的真实原因；成功则 null */
  error: string | null;
  /** 本次实际请求的目标（后端固定的探测目标，如某个 URL / host） */
  target: string;
  /** 本次请求是否**真的**经过了代理 */
  viaProxy: boolean;
}

/** 云同步服务器地址的来源，对应 Rust 侧 `EndpointInfo.source`：
 *  - `env`：环境变量 `ITOOLS_SYNC_ENDPOINT` 指定（优先级最高，界面改不动）；
 *  - `user`：用户在「我的账号 → 数据同步」手填的 `AppSettings.sync_endpoint`；
 *  - `devDefault`：**debug 构建**下的默认本地服务端地址（`http://127.0.0.1:8787`），
 *    用户从没填过；release 构建不启用这条兜底；
 *  - `builtin`：内置的官方默认服务地址（用户没填时 release 下生效）；
 *  - `none`：以上都没有 → 云端未接入，只能本地（仅测试构建会出现）。 */
export type EndpointSource = "env" | "user" | "devDefault" | "builtin" | "none";

/** 当前**实际生效**的云同步服务器地址快照，与 Rust 侧 `EndpointInfo`（camelCase）一致。
 *  只读展示用：编辑入口只有「我的账号 → 数据同步」一处，避免两处输入框不同步。 */
export interface EndpointInfo {
  /** 实际会被请求的地址；`source="none"` 时为 null */
  effective: string | null;
  source: EndpointSource;
  /** 用户手填的那个值（`AppSettings.sync_endpoint`）。
   *  它与 `effective` 可能不同 —— 被 env 覆盖时就是如此，UI 需据此提示「你填的这个当前不生效」。 */
  userValue: string;
  /** 内置的官方默认服务地址。UI 拿它写「留空即使用官方默认服务（xxx）」，
   *  不在前端另写一遍字面量（两处各写一遍迟早分叉成假信息）。 */
  builtinDefault: string;
}

/** 上次更新检查的结果快照，与 Rust 侧 `UpdateStatus` 一致。**本地瞬时，不发请求**。
 *
 *  存在的意义：自动检查对失败刻意静默、没有新版时角标也不显示，于是
 *  「查过了、已是最新」和「压根没查成」在界面上长得一模一样。有了它，
 *  界面才能如实说出「什么时候查的、查出什么」。 */
export interface UpdateStatus {
  /** 上次检查完成的时刻（Unix 毫秒）；**0 = 本次启动后还没检查过**（别渲染成「已是最新」） */
  checkedAt: number;
  /** 上次检查成功时的结果；失败或还没查过为 null */
  info: UpdateInfo | null;
  /** 上次检查失败的真实原因；成功或还没查过为 null */
  error: string | null;
  /** 当前应用版本（无论查没查过都有） */
  currentVersion: string;
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

// ---------- 开发者中心（插件调试环境） ----------
//
// ⚠ 本节每个字段都是从「开发者中心」契约**逐字段照抄**的（Rust 侧统一 `#[serde(rename_all="camelCase")]`），
//   不是从 Rust 结构体「推导」出来的。理由同上方镜像那一节：`invoke<T>()` 只是类型断言，
//   写错了 tsc --noEmit 照样全绿、运行期才静默错位。改这里请先对照 Rust 侧结构体，
//   并重跑 `cargo test -j1 contract_field_names_frozen` + `npm run check:contract`。
//
// 语义要点（决定 UI 怎么写，不能想当然）：
// - 调试环境与正式环境**物理隔离**：调试插件的存储写在独立的 SQLite 文件（DevConfig.dbPath），
//   沙盒文件写在独立根（DevConfig.filesRoot），授权写在独立的 dev 授权表，
//   `itools.data.sync()` 走本地 Mock（DevMockConfig）——调试数据不会进正式库、也不会上云。
// - 正式插件要求 `plugin.json.name` == 目录名（不一致直接跳过不加载）；**调试环境刻意放宽**：
//   照样加载，但把不一致作为一条 DevIssue 如实报上来（否则开发者改错一个字插件就凭空消失）。

/** 清单/目录的一条校验问题，与 Rust 侧 `DevIssue` 一致。 */
export interface DevIssue {
  /** `"error"` | `"warn"`。两者**硬绑定**了能否运行：`runnable = 没有任何一条 error`
   *  （`dev/mod.rs`：`info.runnable = !issues.iter().any(|i| i.level == "error")`），
   *  即 error ⟺ runnable=false，warn 不影响运行。UI 仍应以 `runnable` 字段本身为准去禁用「运行」，
   *  但不要把两者说成互不相干的两回事。 */
  level: string;
  /** 出问题的字段/位置（如 `name` / `index.html` / `features[0].cmds`），UI 逐条按它分组显示。 */
  field: string;
  /** 人类可读的问题描述（后端原文，前端不改写、不吞掉）。 */
  message: string;
}

/** 插件清单里的一个 feature（一条可被触发的功能），与 Rust 侧 `DevFeature` 一致。 */
export interface DevFeature {
  /** feature 的 code，触发时原样回传给插件的 onEnter。 */
  code: string;
  /** feature 的说明文案（plugin.json 里的 explain）。 */
  explain: string;
  /** 该 feature 声明的**纯文本关键字**（cmds 里的字符串项）。 */
  keywords: string[];
  /** 该 feature 声明了正则匹配（cmds 里有 `{ type: "regex" }` 项）。 */
  hasRegex: boolean;
  /** 该 feature 声明了文本匹配：cmds 里有 `{ "type": "text", … }` 项。
   *  后端只认 `type == "text"` 这一个取值（`dev/mod.rs` 的 `Cmd::Typed(t) if t.kind == "text"`，
   *  与 `plugin/mod.rs` 的匹配侧同源），其它 type 既不算 text 也不算 regex。 */
  hasText: boolean;
}

/** 一个调试目录（加载源），与 Rust 侧 `DevDirInfo` 一致。 */
export interface DevDirInfo {
  /** 目录的绝对路径。 */
  path: string;
  /** 固定的 `dev-plugins/` 目录：**不可移除**（UI 不得给它「移除」按钮）。 */
  fixed: boolean;
  /** 目录当前是否真实存在（false = 路径已被删除/改名，属如实标注，不是加载失败）。 */
  exists: boolean;
  /** 该目录下扫到的调试插件数量。 */
  pluginCount: number;
}

/** 一个调试插件的完整视图，与 Rust 侧 `DevPluginInfo` 一致。 */
export interface DevPluginInfo {
  /** 调试环境内的唯一 id（协议 URL、存储命名空间、日志都用它）。 */
  id: string;
  /** 插件目录的绝对路径。 */
  dir: string;
  /** 该插件所属的调试目录（对应某个 DevDirInfo.path）。 */
  root: string;
  /** plugin.json 里的 name（**可能与目录名不一致**——调试环境不因此跳过，只报 issue）。 */
  name: string;
  version: string;
  description: string;
  author: string;
  /** logo 的 data URL；无 logo.png 则 null。 */
  logo: string | null;
  featureCount: number;
  /** 全插件拍平后的关键字预览。 */
  cmds: string[];
  /** 插件声明的高危能力。 */
  permissions: string[];
  /** 在**调试授权表**里已授权的能力（与正式安装后的授权互不影响）。 */
  granted: string[];
  /** 能否真的运行（缺 index.html 等致命问题为 false）。false 时 UI 必须禁用「运行」并说明原因。 */
  runnable: boolean;
  /** 清单/目录校验问题列表（空数组 = 干净）。 */
  issues: DevIssue[];
  features: DevFeature[];
}

/** 一条调试日志，与 Rust 侧 `DevLogEntry` 一致。
 *  探针挂在 bridge.js 的 invoke() 单点上 —— 覆盖范围见开发者中心「日志」tab 里的说明。 */
export interface DevLogEntry {
  /** 单调递增序号，增量拉取用（`dev_log_list(afterSeq)` 返回 seq > afterSeq 的条目）。 */
  seq: number;
  /** 发生时间（RFC3339）。 */
  at: string;
  /** 产生这条日志的调试插件 id。 */
  pluginId: string;
  /** `"api"`（经门面的一次命令调用）| `"console"`（插件的 console 输出）| `"error"`（未捕获异常）。 */
  kind: string;
  /** `"info"` | `"warn"` | `"error"`。 */
  level: string;
  /** api：被调用的方法名；console/error：来源标记。 */
  method: string;
  /** 入参的 JSON 文本（后端可能已截断）。 */
  args: string;
  /** 返回值或错误信息的文本（后端可能已截断）。 */
  result: string;
  /** 耗时（毫秒）；非 api 类型没有真实耗时，恒 0，UI 不得据此显示时长。 */
  ms: number;
  /** 本次调用是否成功（kind=error 恒 false）。 */
  ok: boolean;
}

/** 云同步 Mock 配置（全局，不区分插件），与 Rust 侧 `DevMockConfig` 一致。
 *  插件在调试环境调 `itools.data.sync()` 时拿到的就是这里配置的结果，**不会真的联网**。 */
export interface DevMockConfig {
  /** `"success"` | `"fail"` | `"offline"` | `"notLoggedIn"`。 */
  mode: string;
  /** 人为延迟（毫秒），用来测插件的 loading/超时处理。 */
  delayMs: number;
  /** mode=fail 时回给插件的失败原因。 */
  reason: string;
  /** mode=success 时伪造的上行条数。 */
  pushed: number;
  /** mode=success 时伪造的下行条数。 */
  pulled: number;
}

/** 测试库里的一条键值，与 Rust 侧 `DevKv` 一致。 */
export interface DevKv {
  key: string;
  /** 值的 JSON 文本（原样存储的字符串）。 */
  value: string;
  /** 该条占用的字节数（后端真实统计）。 */
  bytes: number;
  /** 更新时间（RFC3339）；后端没有记录该条的时间则 null（UI 显示「—」，不编造时间）。 */
  updatedAt: string | null;
}

/** 插件开发 MCP 服务器的运行状态，与 Rust 侧 `McpStatus` 一致。
 *  running / port / url 都来自**实际的监听结果**，不是配置项的回显——
 *  起不来时 running=false 且 error 里是真实原因，界面不许显示一个假的「运行中」。 */
export interface McpStatus {
  running: boolean;
  /** 实际监听的端口（未运行为 0） */
  port: number;
  /** 给 AI 客户端填的完整地址（未运行为空串） */
  url: string;
  /** 未能启动时的真实原因 */
  error: string | null;
}

/** 一个 AI 助手的接入状态，与 Rust 侧 `AiClient` 一致。
 *  MCP 与 skill 两件事**分别**呈现状态，因为它们可能一个成一个败。 */
export interface AiClient {
  /** `claude` / `codex` / `cursor`，安装与卸载时传它。 */
  id: string;
  label: string;
  /** 家目录下有没有它的配置目录。没有多半是没装这个客户端（仍允许安装，但要如实标注）。 */
  detected: boolean;
  /** MCP 已接入（读配置文件得来，不依赖 CLI 在不在）。 */
  mcpReady: boolean;
  /** 配着的地址与当前实际监听地址不一致（换过端口）——「配了但连不上」，比没配更迷惑人。 */
  mcpStale: boolean;
  /** skill 已装且是 iTools 装的。 */
  skillReady: boolean;
  skillOutdated: boolean;
  skillVersion: string | null;
  /** 接入 MCP 依赖的命令行工具在不在。**为假时一键按钮必须禁用**。 */
  cliReady: boolean;
  /** MCP 配置文件完整路径——写别人的文件，用户有权先看见写到哪。 */
  configPath: string;
  skillPath: string;
  /** 需要额外说清的情况（skill 目录不是 iTools 装的、CLI 缺失、地址过期等）。 */
  note: string | null;
  /** 两样都齐且都不过期。**由后端算好**，前端别再拼一遍判断逻辑。 */
  ready: boolean;
}

/** AI 助手接入的整页状态，与 Rust 侧 `AiClientsStatus` 一致。 */
export interface AiClientsStatus {
  /** MCP 服务器在不在跑。没跑就拿不到接入地址，一键按钮要禁用并说明原因。 */
  mcpRunning: boolean;
  mcpUrl: string;
  /** 随包 skill 源可用吗（打包漏了 resources 就是 false）。 */
  skillSourceAvailable: boolean;
  skillSourceError: string | null;
  bundledVersion: string;
  /** 检测到几个客户端。 */
  detectedCount: number;
  clients: AiClient[];
}

/** 开发者中心的全局配置快照，与 Rust 侧 `DevConfig` 一致。 */
export interface DevConfig {
  /** 调试插件是否参与主搜索（默认 false）。 */
  searchVisible: boolean;
  /** 全部调试目录（第一项通常是固定的 dev-plugins/）。 */
  dirs: DevDirInfo[];
  mock: DevMockConfig;
  /** 调试用 SQLite 文件的**真实路径**（与正式库是两个文件，物理隔离）。 */
  dbPath: string;
  /** 调试沙盒文件根目录的**真实路径**。 */
  filesRoot: string;
}

// ---------- 插件提审（开发者中心 → 发布） ----------
//
// ⚠ 与 src-tauri/src/dev/submit.rs 的结构体逐字段对应。
//   状态**全部来自服务端**：客户端不推断「应该算是审核中吧」这类结论。

/** 提审单的状态。与服务端 `server/src/market.rs::status` 一一对应。 */
export type SubmissionStatus =
  /** 已收下，服务端正在跑模型审核 */
  | "reviewing"
  /** 审核通过，已上线到市场 */
  | "approved"
  /** 审核未通过（message 里是原因） */
  | "rejected"
  /** 审核没能完成（模型未配置 / 调用失败 / 服务重启），需维护者人工处理 —— **不是通过** */
  | "manual";

/** 一条提审记录。 */
export interface Submission {
  id: string;
  name: string;
  version: string;
  status: SubmissionStatus | string;
  /** 给作者看的一句话结论 / 驳回原因（服务端原文，原样展示，不要改写） */
  message: string;
  contentHash: string;
  fileCount: number;
  sizeBytes: number;
  createdAt: number;
  updatedAt: number;
  /** 模型裁决原文（只有查单条详情时才有；结构见服务端 llm.rs 的 Verdict） */
  review?: unknown;
}

/** 一个调试插件的发布状态快照。 */
export interface PublishStatus {
  name: string;
  localVersion: string;
  /** 市场上已上线的版本；null = 还没上线过 */
  onlineVersion: string | null;
  revoked: boolean;
  revokedReason: string;
  /** 本地版本是否高于线上（可以提交新版本） */
  canSubmitNewVersion: boolean;
  latest: Submission | null;
  history: Submission[];
  /** 查询过程中的真实错误（未登录 / 服务器不可达等）。
   *  **非空时上面的字段可能不完整**，UI 必须如实展示这条，不能渲染成「没有记录」。 */
  error: string | null;
}

/** 提审前的本地自检结论。**通过它不代表会过审** —— 代码审核在服务端。 */
export interface Preflight {
  /** 阻断项：非空则不允许提交（服务端也一定会拒） */
  blockers: string[];
  /** 提醒项：不阻断提交 */
  warnings: string[];
}

/** 主面板数据，与 Rust 侧 `HomeData` 保持一致 */
export interface HomeData {
  user: string;
  recent: SearchItem[];
  pinned: SearchItem[];
}

// ---------- 插件市场 ----------
//
// ⚠ 与 src-tauri/src/plugin/market.rs 的结构体逐字段对应，并已纳入契约闸门
//   （contract.rs 的 freeze 清单 + npm run check:contract 双向校验）。
//   同时也是 registry/schema/entry.schema.json 的镜像：改字段要三处一起改，
//   否则 CI 生成的 index.json 客户端会读不全（而 tsc 与 cargo test 都不会报错）。

/** 市场索引里的一个插件条目。 */
export interface MarketEntry {
  name: string;
  displayName: string;
  description: string;
  author: string;
  version: string;
  /** 插件包在服务端的**相对**下载路径（如 `/api/market/package/demo`）。
   *  客户端拼上自己配置的服务器地址——反代 / 内网 / 端口转发都不影响。 */
  package: string;
  /** 审核时算出的内容哈希（`sha256:…`）。
   *  空串表示索引没给，此时安装会**退化为不校验**并如实告知，绝不假装校验过。 */
  contentHash: string;
  /** 审核该版本的模型名（如 `gpt-5.5`）。空串 = 没有自动审核记录。
   *  UI 必须据此如实措辞：「由 X 自动审核」≠「人工审计过」。 */
  reviewedBy: string;
  /** 首次上线 / 最近更新时间（Unix 毫秒；0 = 索引未提供） */
  publishedAt: number;
  updatedAt: number;
  sizeBytes: number;
  /** README 原文（服务端已截断），供市场页展示 */
  readme: string;
  category: string;
  tags: string[];
  keywords: string[];
  /** 插件申请的高危能力（安装后仍默认未授权） */
  permissions: string[];
  /** 每项权限的用途说明，收录时要求作者填写 */
  permissionReasons: Record<string, string>;
  minAppVersion: string;
  license: string;
  homepage: string;
  /** 若 repo 里是构建产物，这里是可复现的源码仓库 */
  sourceRepo: string;
  screenshots: string[];
  /** 已下架：客户端必须警告，且不允许安装 */
  revoked: boolean;
  /** 下架原因，原样展示给用户 */
  revokedReason: string;
  addedAt: string;
  /** 本次收录的审核记录链接，供用户查证「凭什么信任它」 */
  auditReport: string;
  fileCount: number;
}

/** 市场列表的一次拉取结果。 */
export interface MarketView {
  plugins: MarketEntry[];
  /** "remote" = 本次成功拉到；"cache" = 拉取失败、用的本地缓存；"none" = 两者都没有。
   *  这个区分必须呈现给用户：拿着旧缓存和刚更新过是不同的事实。 */
  origin: "remote" | "cache" | "none";
  /** 拉取失败的真实原因（成功时 null）——不得吞掉 */
  error: string | null;
  /** 已安装的插件名 → 版本，供标注「已安装 / 版本不同」 */
  installed: Record<string, string>;
  /** 索引来源 —— 当前生效的服务器地址 */
  source: string;
  /** 服务端的审核方式：`llm` = 大模型自动审核；`manual` = 未接入自动审核、由维护者人工放行。
   *  空串 = 索引里没说。市场页文案必须据此分支，不能一律写成「已审核」。 */
  reviewMode: string;
  /** 审核模型名（reviewMode="llm" 时有值） */
  reviewModel: string;
  /** 这批数据是什么时候拿到的（Unix 毫秒；0 = 未知 / 无数据）。UI 用它显示「更新于 X 前」。 */
  fetchedAt: number;
  /** 这批数据已过新鲜期（默认 1 小时）→ 前端应在后台静默 marketList(true) 再重绘。
   *  与 origin 是两件事：origin 说数据从哪来，stale 说该不该更新。 */
  stale: boolean;
}
