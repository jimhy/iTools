import { invoke } from "@tauri-apps/api/core";
// release notes 是 Markdown，复用管理中心那份零依赖渲染器（无 innerHTML，无 XSS 面）
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { LogicalSize, LogicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import type {
  AppSettings,
  FileIndexStatus,
  HomeData,
  SearchItem,
  UpdateInfo,
} from "./types";
import { AUTO_CLEAR_NEVER } from "./types";
import { TOOL_ICONS } from "./tool-icons";
import { SYSTEM_ICONS } from "./system-icons";
import "./styles.css";

const appWindow = getCurrentWindow();

const WINDOW_WIDTH = 680;
const SEARCH_ROW_HEIGHT = 64;
/** 窗口锚定的屏幕上部比例（顶端留白 = 屏幕高 × 此值），向下伸缩 */
const TOP_RATIO = 0.1;
/** 主面板分区折叠时最多展示的格子数（约一行） */
const HOME_COLLAPSED_CELLS = 9;
/** 搜索结果网格折叠时最多展示的格子数（约两行） */
const SEARCH_COLLAPSED_CELLS = 18;

const appEl = document.querySelector<HTMLDivElement>("#app")!;
const panel = document.querySelector<HTMLDivElement>(".panel")!;
const input = document.querySelector<HTMLInputElement>("#query")!;
const list = document.querySelector<HTMLUListElement>("#results")!;
const pane = document.querySelector<HTMLDivElement>("#home")!;
const avatarEl = document.querySelector<HTMLDivElement>("#avatar")!;
const avatarLetterEl = document.querySelector<HTMLSpanElement>("#avatar-letter")!;
const updateBadgeEl = document.querySelector<HTMLSpanElement>("#update-badge")!;

// ---------- 状态 ----------

/** home=主面板；grid=应用搜索网格；list=/f 文件搜索列表 */
type Mode = "home" | "grid" | "list";
let mode: Mode = "home";

let items: SearchItem[] = [];
let selected = 0;
let queryToken = 0; // 竞态守卫：只接受最新一次查询的结果
let debounceTimer: number | undefined;

interface GridCell {
  el: HTMLDivElement;
  /** 可执行条目 */
  item?: SearchItem;
  /** 内置工具磁贴：点击填入查询 */
  fill?: string;
}
let homeData: HomeData | null = null;
let gridCells: GridCell[] = [];
let gridSel = -1;
/** 本次拖入主面板的文件真实路径；非空表示当前处于「拖放待选插件」状态。 */
let droppedFiles: string[] = [];
/** 最近一次搜索网格的数据（供展开/收起重渲染） */
let lastGridItems: SearchItem[] = [];
const sectionExpanded: Record<string, boolean> = {};
let menuEl: HTMLDivElement | null = null;

// ---------- 内置工具磁贴 ----------

const BUILTIN_TILES: {
  title: string;
  fill: string;
  icon: string;
  /** 悬停说明。磁贴只有一行标题、放不下解释，覆盖范围又只能靠文字说清时才加。 */
  hint?: string;
}[] = [
  { title: "计算器", fill: "1+2*3", icon: "calc" },
  { title: "时间戳", fill: "now", icon: "clock" },
  { title: "颜色转换", fill: "#ff8800", icon: "color" },
  { title: "进制转换", fill: "0xFF", icon: "hex" },
  { title: "打开网址", fill: "github.com", icon: "globe" },
  {
    title: "文件搜索",
    fill: "/f ",
    icon: "fsearch",
    // 只写「文件搜索」会让人以为它一直搜全盘，而覆盖范围其实取决于全盘索引开没开
    // （没开时大致只有用户目录）。这里先给一句实话，进 /f 后由状态条给出当前真实状态。
    hint: "按文件名搜索：/f 关键词。\n开启全盘索引后覆盖本机各固定 NTFS 磁盘；未开启时只能搜到 Windows 系统索引已收录的位置（通常只有用户目录）。当前状态在 /f 界面顶部有显示。",
  },
];

// ---------- 通用 ----------

/**
 * Enter/Esc 的动作挂起到 keyup 才执行：keydown 阶段不藏窗、不启动、不让出焦点，
 * 按键物理释放后才行动——彻底杜绝按键消息穿透到下层应用。
 */
let pendingKeyAction: (() => void) | null = null;

function armKeyAction(action: () => void): void {
  pendingKeyAction = action;
  // 兜底：keyup 丢失（焦点意外转移等）也要执行
  window.setTimeout(() => {
    if (pendingKeyAction === action) {
      pendingKeyAction = null;
      action();
    }
  }, 350);
}

/**
 * 关闭（Esc/执行后）：清状态回主界面并藏窗——下次呼出是主界面。
 * 与「失焦隐藏」(hideKeepState) 区分：那个保留状态，呼出恢复原界面。
 */
async function hide(): Promise<void> {
  await appWindow.hide();
  // 拖放态要一并退出：否则下次唤起还停在上一批文件的候选列表上，
  // 输入框还是禁用的，用户会以为 iTools 卡死了
  droppedFiles = [];
  input.disabled = false;
  input.value = "";
  showHome();
}

/** 失焦隐藏：只藏窗、完整保留当前界面状态，再呼出恢复原样 */
async function hideKeepState(): Promise<void> {
  await appWindow.hide();
}


function setMode(next: Mode): void {
  mode = next;
  panel.classList.toggle("pane-grid", next !== "list");
  // 全盘索引状态条只属于 /f 列表模式：切走就藏起来并停掉轮询（timer 不能在后台空转）
  if (next !== "list") hideFsStatus();
}

function svgIcon(paths: string): string {
  return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${paths}</svg>`;
}

/** kind → 统一的线性兜底图标（真实图标加载后会替换掉） */
const GLYPH_PATHS: Record<SearchItem["kind"], string> = {
  app: `<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>`,
  file: `<path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/>`,
  folder: `<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>`,
  command: `<path d="M13 2 3 14h8l-1 8 10-12h-8z"/>`,
  plugin: `<path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/>`,
};

function fallbackIcon(kind: SearchItem["kind"]): string {
  return svgIcon(GLYPH_PATHS[kind] ?? GLYPH_PATHS.file);
}

/** 选中行右侧的动作提示文案 */
function hintFor(item: SearchItem): string {
  if (item.action === "copy") return "复制";
  if (item.kind === "app") return "启动";
  return "打开";
}

/** 把窗口锚定到屏幕上部、水平居中（显示时调用一次；之后只改高度、顶部不动，输入框不跳） */
async function anchorTop(): Promise<void> {
  const x = Math.round((window.screen.availWidth - WINDOW_WIDTH) / 2);
  const y = Math.round(window.screen.availHeight * TOP_RATIO);
  await appWindow.setPosition(new LogicalPosition(Math.max(0, x), Math.max(0, y)));
}

/** 内容可用最大高度：窗口顶在屏幕上部，向下伸缩到接近屏幕底（留边距），超过才滚动。 */
function maxContentHeight(): number {
  const avail = window.screen.availHeight;
  const top = Math.round(avail * TOP_RATIO);
  return Math.max(320, avail - top - 48 - SEARCH_ROW_HEIGHT);
}

/**
 * 量一个内容容器的**真实内容高度**。
 *
 * # 为什么不能直接读 scrollHeight（2026-08-18 的「窗口缩不回去」就是它）
 *
 * `#home` 与 `.results` 都是 `flex: 1` + `overflow-y: auto`，作为 `.panel`
 * （`height: 100%`）的 flex 子项，它们会被 **flex-grow 撑满剩余空间**。于是
 * `scrollHeight` 返回的是「被撑开后的高度」，而不是内容自己的高度——
 * 一旦窗口被撑高过（比如 `/f` 出了一屏结果），回到主面板时量到的仍是那个大值，
 * 于是算出的新高度 ≈ 当前高度，窗口**再也缩不回去**，主面板下方留一大片空白。
 *
 * 解法是量之前先把 flex-grow 摘掉、高度交还给内容，量完立刻还原。
 * 两次同步 reflow 都在同一帧内完成，不会被绘制出来，用户看不到闪烁。
 */
function contentHeight(el: HTMLElement): number {
  const prevFlex = el.style.flex;
  const prevHeight = el.style.height;
  el.style.flex = "0 0 auto";
  el.style.height = "auto";
  const measured = el.scrollHeight;
  el.style.flex = prevFlex;
  el.style.height = prevHeight;
  return measured;
}

/** 窗口高度随内容伸缩（宽度固定）；内容超过屏幕可用高度才在内部滚动 */
async function resizeToContent(): Promise<void> {
  const cap = maxContentHeight();
  let height = SEARCH_ROW_HEIGHT;
  if (mode === "list") {
    // 索引状态条常驻在结果列表之上，它的高度必须计入，否则会被窗口边缘裁掉
    // （「/f」还没打关键词时列表是空的，此时窗口高度就只由这条状态条决定）
    const barHeight =
      fsStatusEl && !fsStatusEl.hidden ? fsStatusEl.offsetHeight : 0;
    const listHeight = items.length > 0 ? contentHeight(list) + 2 : 0;
    height += Math.min(barHeight + listHeight, cap);
  } else {
    // +2 补 #home 的 1px 上边框与亚像素取整，否则折叠态也会冒出滚动条
    height += Math.min(contentHeight(pane) + 2, cap);
  }
  await appWindow.setSize(new LogicalSize(WINDOW_WIDTH, Math.round(height)));
}

/** 展开/收起后按当前模式重渲染网格 */
function rerenderPane(): void {
  if (mode === "home") {
    renderHome();
  } else if (mode === "grid") {
    renderSearchGrid(lastGridItems);
  }
}

// ---------- 右键固定菜单 ----------

function closeMenu(): void {
  menuEl?.remove();
  menuEl = null;
}

function showPinMenu(x: number, y: number, item: SearchItem): void {
  closeMenu();
  const isPinned = homeData?.pinned.some((p) => p.id === item.id) ?? false;
  menuEl = document.createElement("div");
  menuEl.className = "ctx-menu";
  const entry = document.createElement("div");
  entry.className = "ctx-item";
  entry.textContent = isPinned ? "取消固定" : "固定到「已固定」";
  entry.addEventListener("click", async () => {
    closeMenu();
    try {
      await invoke("toggle_pin", { item });
      await refreshHome();
      if (mode === "home") renderHome();
    } catch (err) {
      console.error("toggle_pin failed", err);
    }
  });
  menuEl.appendChild(entry);
  document.body.appendChild(menuEl);
  // 防溢出：先挂载测量再定位
  const rect = menuEl.getBoundingClientRect();
  menuEl.style.left = `${Math.min(x, window.innerWidth - rect.width - 8)}px`;
  menuEl.style.top = `${Math.min(y, window.innerHeight - rect.height - 8)}px`;
}

document.addEventListener("click", () => closeMenu());

// ---------- 网格基建（主面板与搜索网格共用） ----------

function resetGrid(): void {
  pane.innerHTML = "";
  gridCells = [];
  gridSel = -1;
}

function renderSection(
  title: string,
  key: string,
  sectionItems: SearchItem[],
  collapsedCount: number,
): HTMLDivElement {
  const wrap = document.createElement("div");
  wrap.className = "home-section";

  const head = document.createElement("div");
  head.className = "section-head";
  const label = document.createElement("span");
  label.className = "section-title";
  label.textContent = title;
  head.appendChild(label);

  const expanded = sectionExpanded[key] ?? false;
  if (sectionItems.length > collapsedCount) {
    const link = document.createElement("span");
    link.className = "section-link";
    link.textContent = expanded ? "收起" : `展开 (${sectionItems.length})`;
    link.addEventListener("click", (e) => {
      e.stopPropagation();
      sectionExpanded[key] = !expanded;
      rerenderPane();
    });
    head.appendChild(link);
  }
  wrap.appendChild(head);

  const grid = document.createElement("div");
  grid.className = "home-grid";
  const visible = expanded ? sectionItems : sectionItems.slice(0, collapsedCount);
  for (const item of visible) {
    grid.appendChild(createCell({ item, pinnable: true }));
  }
  wrap.appendChild(grid);
  return wrap;
}

function createCell(opts: {
  item?: SearchItem;
  fill?: string;
  pinnable: boolean;
  title?: string;
  /** 直接指定的图标（完整 data URL，如内置工具的彩色 PNG） */
  iconUrl?: string;
  /** 悬停说明（原生 tooltip）：一行标题说不清能力边界时用 */
  hint?: string;
}): HTMLDivElement {
  const el = document.createElement("div");
  el.className = "cell";
  if (opts.hint) el.title = opts.hint;
  const index = gridCells.length;

  const icon = document.createElement("div");
  icon.className = "cell-icon";
  // 图标优先级：显式 iconUrl（内置工具）> 系统命令彩色图标（按标题）> 提取的真实图标 > 兜底字形
  const sysUrl = opts.item ? SYSTEM_ICONS[opts.item.title] : undefined;
  // item.icon 多为裸 base64（PNG），需补 data: 前缀；插件 logo 已是完整 data URL（可能含 jpg/svg），原样用
  const rawIcon = opts.item?.icon;
  const itemIconSrc = rawIcon
    ? rawIcon.startsWith("data:")
      ? rawIcon
      : `data:image/png;base64,${rawIcon}`
    : undefined;
  const iconSrc = opts.iconUrl ?? sysUrl ?? itemIconSrc;
  if (iconSrc) {
    const img = document.createElement("img");
    img.src = iconSrc;
    img.alt = "";
    icon.appendChild(img);
  } else {
    icon.classList.add("glyph");
    icon.innerHTML = fallbackIcon(opts.item?.kind ?? "app");
  }

  const label = document.createElement("div");
  label.className = "cell-label";
  label.textContent = opts.title ?? opts.item?.title ?? "";

  el.append(icon, label);

  el.addEventListener("mousemove", () => {
    if (gridSel !== index) selectCell(index);
  });
  el.addEventListener("click", () => execCell(index));
  if (opts.pinnable && opts.item) {
    const item = opts.item;
    el.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showPinMenu(e.clientX, e.clientY, item);
    });
  }

  gridCells.push({ el, item: opts.item, fill: opts.fill });
  return el;
}

function selectCell(index: number): void {
  if (index < 0 || index >= gridCells.length) return;
  gridCells[gridSel]?.el.classList.remove("selected");
  gridSel = index;
  const cell = gridCells[gridSel];
  cell.el.classList.add("selected");
  cell.el.scrollIntoView({ block: "nearest" });
}

function execCell(index: number): void {
  const cell = gridCells[index];
  if (!cell) return;
  if (cell.fill !== undefined) {
    input.value = cell.fill;
    input.focus();
    scheduleSearch();
    return;
  }
  if (cell.item) {
    // 插件：打开插件页面窗口（不走 execute 的外部打开分支）
    if (cell.item.kind === "plugin") {
      // 拖放进来的一批文件：走 open_plugin_files，把**真实路径**交给插件
      // （网页里的 File 对象拿不到路径，这是过去 files 触发做不了的根本原因）
      if (droppedFiles.length > 0) {
        const paths = droppedFiles.slice();
        invoke("open_plugin_files", { target: cell.item.target, paths }).catch((err) =>
          console.error("open_plugin_files failed", err),
        );
      } else {
        invoke("open_plugin_window", {
          target: cell.item.target,
          query: input.value.trim(),
        }).catch((err) => console.error("open_plugin_window failed", err));
      }
      // 插件不经 execute，单独补记一次使用，使其进入「最近使用」
      invoke("record_usage", { item: cell.item }).catch((err) =>
        console.error("record_usage failed", err),
      );
      void hide();
      return;
    }
    // 不等启动完成：后台启动，立即藏窗（秒隐手感）
    invoke("execute", { item: cell.item }).catch((err) =>
      console.error("execute failed", err),
    );
    void hide();
  }
}

/** 网格键盘导航：按视觉行分组，上下移动时保持水平位置最近 */
function gridRows(): number[][] {
  const rows: number[][] = [];
  let lastTop = Number.NEGATIVE_INFINITY;
  gridCells.forEach((cell, i) => {
    const top = cell.el.getBoundingClientRect().top;
    if (Math.abs(top - lastTop) > 4) {
      rows.push([i]);
      lastTop = top;
    } else {
      rows[rows.length - 1].push(i);
    }
  });
  return rows;
}

function gridMoveH(delta: number): void {
  if (gridCells.length === 0) return;
  if (gridSel < 0) {
    selectCell(0);
    return;
  }
  const next = gridSel + delta;
  if (next >= 0 && next < gridCells.length) selectCell(next);
}

function gridMoveV(dir: number): void {
  if (gridCells.length === 0) return;
  if (gridSel < 0) {
    selectCell(0);
    return;
  }
  const rows = gridRows();
  const rowIdx = rows.findIndex((r) => r.includes(gridSel));
  const targetRow = rows[rowIdx + dir];
  if (!targetRow) return;
  const rect = gridCells[gridSel].el.getBoundingClientRect();
  const cx = rect.left + rect.width / 2;
  let best = targetRow[0];
  let bestDist = Number.POSITIVE_INFINITY;
  for (const i of targetRow) {
    const r = gridCells[i].el.getBoundingClientRect();
    const dist = Math.abs(r.left + r.width / 2 - cx);
    if (dist < bestDist) {
      bestDist = dist;
      best = i;
    }
  }
  selectCell(best);
}

/** 网格可见项的真实图标按需加载 */
async function loadGridIcons(): Promise<void> {
  const targets = [
    ...new Set(
      gridCells
        .filter(
          (c) =>
            c.item &&
            !c.item.icon &&
            c.item.kind !== "command" &&
            !SYSTEM_ICONS[c.item.title], // 系统命令用内置彩色图标，别用系统提取覆盖
        )
        .map((c) => c.item!.target),
    ),
  ];
  if (targets.length === 0) return;
  try {
    const map = await invoke<Record<string, string>>("load_icons", {
      paths: targets,
    });
    for (const cell of gridCells) {
      const target = cell.item?.target;
      if (!target) continue;
      const b64 = map[target];
      if (!b64) continue;
      cell.item!.icon = b64;
      const iconEl = cell.el.querySelector<HTMLDivElement>(".cell-icon");
      if (iconEl) {
        iconEl.classList.remove("glyph");
        iconEl.textContent = "";
        const img = document.createElement("img");
        img.src = `data:image/png;base64,${b64}`;
        img.alt = "";
        iconEl.appendChild(img);
      }
    }
  } catch (err) {
    console.error("load grid icons failed", err);
  }
}

// ---------- 主面板 ----------

function showHome(): void {
  setMode("home");
  items = [];
  list.innerHTML = "";
  renderHome();
  void refreshHome();
}

/** 拉取首屏数据（问候语 + 最近使用 + 已固定）。
 *
 *  ⚠ 主窗口的 WebView 会**先于后端 setup 完成**就执行到这里：`UsageStore` / `PluginRegistry`
 *  等 State 要等插件扫描结束才 manage（实测约 1 秒），这期间调用会被
 *  「state not managed for field `store`」直接拒掉。
 *
 *  原来失败就 return，于是界面永久停在「只有内置工具」的半截状态，且**不会自愈**——
 *  必须做点别的操作（打开插件等）才恢复。所以这里退避重试；后端 setup 收尾还会广播
 *  `app-ready` 再催一次，两条路互为兜底（事件可能在监听注册之前就发出，光靠它不保险）。
 */
async function refreshHome(retry = 0): Promise<void> {
  try {
    homeData = await invoke<HomeData>("home_data");
  } catch (err) {
    // 启动早期的失败几乎都是时序问题；累计约 4 秒仍不行才认输
    if (retry < 8) {
      window.setTimeout(() => void refreshHome(retry + 1), 120 * (retry + 1));
      return;
    }
    console.error("home_data failed", err);
    return;
  }
  // 问候语与头像常驻搜索栏
  const user = homeData.user;
  if (user) {
    // 只改字母那个子元素：直接给 #avatar 设 textContent 会连更新角标一起抹掉
    avatarLetterEl.textContent = user[0].toUpperCase();
  }
  updatePlaceholder();
  if (mode === "home") renderHome();
}

function renderHome(): void {
  resetGrid();
  if (homeData?.recent.length) {
    pane.appendChild(
      renderSection("最近使用", "recent", homeData.recent, HOME_COLLAPSED_CELLS),
    );
  }
  if (homeData?.pinned.length) {
    pane.appendChild(
      renderSection("已固定", "pinned", homeData.pinned, HOME_COLLAPSED_CELLS),
    );
  }
  pane.appendChild(renderBuiltinSection());
  if (gridCells.length > 0) selectCell(0);
  void loadGridIcons();
  void resizeToContent();
}

// 「市场精选」分区已移除：它从来只是一块写死的「敬请期待」，既不来自真实数据、
// 也点不动，占着首屏一整块位置。按开发准则第 6 条（不做无效控件 / 不用占位冒充功能），
// 空着不如不显示。要恢复请接真实的市场索引（plugin::market），有内容才渲染。

function renderBuiltinSection(): HTMLDivElement {
  const wrap = document.createElement("div");
  wrap.className = "home-section";

  const head = document.createElement("div");
  head.className = "section-head";
  const label = document.createElement("span");
  label.className = "section-title";
  label.textContent = "内置工具";
  head.appendChild(label);
  wrap.appendChild(head);

  const grid = document.createElement("div");
  grid.className = "home-grid";
  for (const tile of BUILTIN_TILES) {
    grid.appendChild(
      createCell({
        fill: tile.fill,
        pinnable: false,
        title: tile.title,
        iconUrl: TOOL_ICONS[tile.icon],
        hint: tile.hint,
      }),
    );
  }
  wrap.appendChild(grid);
  return wrap;
}

// ---------- 搜索结果网格（默认模式） ----------

function renderSearchGrid(found: SearchItem[]): void {
  lastGridItems = found;
  resetGrid();

  const plugins = found.filter((i) => i.kind === "plugin");
  const apps = found.filter((i) => i.kind === "app");
  const cmds = found.filter((i) => i.kind === "command");

  // 插件命中意图强，置顶展示
  if (plugins.length > 0) {
    pane.appendChild(renderSection("插件", "pl", plugins, SEARCH_COLLAPSED_CELLS));
  }
  if (apps.length > 0) {
    pane.appendChild(
      renderSection("搜索结果", "sr", apps, SEARCH_COLLAPSED_CELLS),
    );
  }
  if (cmds.length > 0) {
    pane.appendChild(renderSection("匹配结果", "mr", cmds, SEARCH_COLLAPSED_CELLS));
  }
  if (plugins.length === 0 && apps.length === 0 && cmds.length === 0) {
    const empty = document.createElement("div");
    empty.className = "grid-empty";
    empty.textContent = "未找到匹配结果";
    pane.appendChild(empty);
  }

  if (gridCells.length > 0) selectCell(0);
  void loadGridIcons();
  void resizeToContent();
}

// ---------- 搜索 ----------

/** 触发一次搜索（带防抖），空查询回到主面板 */
// ---------- 拖放文件：交给声明了 files / img 触发的插件 ----------
//
// 为什么必须在原生层接：网页里的 File 对象**拿不到真实路径**（浏览器安全模型），
// 而「用 adb 装这个 apk」「压缩这几个文件」这类插件要的正是路径。Tauri 的 OS 级
// 拖放事件给的是真实路径，所以这条链路只能走原生。
async function handleDroppedFiles(paths: string[]): Promise<void> {
  droppedFiles = paths.slice();
  if (droppedFiles.length === 0) return;
  try {
    const found = await invoke<SearchItem[]>("search_files", { paths: droppedFiles });
    // 用文件名当提示文本，让用户看清自己拖进来的是什么
    const names = droppedFiles.map((p) => p.split(/[\/]/).pop() ?? p);
    input.value = names.length === 1 ? names[0] : `${names.length} 个文件`;
    input.disabled = true; // 拖放态下输入框只作展示，避免用户以为能继续打字筛选
    setMode("grid");
    delete sectionExpanded["pl"];
    delete sectionExpanded["sr"];
    delete sectionExpanded["mr"];
    items = [];
    list.innerHTML = "";
    if (found.length === 0) {
      // 诚实告知：没有插件能处理这批文件，而不是显示一个空面板让用户以为在加载
      pane.innerHTML = "";
      const tip = document.createElement("div");
      tip.className = "plugin-empty-title";
      tip.textContent = "没有插件能处理这类文件";
      pane.appendChild(tip);
      return;
    }
    renderSearchGrid(found);
  } catch (err) {
    console.error("search_files failed", err);
    clearDropState();
  }
}

/** 退出拖放态，恢复正常搜索。 */
function clearDropState(): void {
  if (droppedFiles.length === 0 && !input.disabled) return;
  droppedFiles = [];
  input.disabled = false;
  input.value = "";
  showHome();
}

void getCurrentWebview().onDragDropEvent((event) => {
  if (event.payload.type === "drop") {
    void handleDroppedFiles(event.payload.paths);
  }
});

/** 把插件注入的结果追加到当前网格；token 过期（用户已输入新内容）则整批丢弃。 */
async function appendPushResults(query: string, token: number): Promise<void> {
  try {
    const pushed = await invoke<SearchItem[]>("search_push", { query });
    if (token !== queryToken || pushed.length === 0) return;
    if (mode !== "grid") return;
    renderSearchGrid(lastGridItems.concat(pushed));
  } catch (err) {
    // 注入失败不该影响主搜索结果——静默记一条即可
    console.error("search_push failed", err);
  }
}

function scheduleSearch(): void {
  window.clearTimeout(debounceTimer);
  debounceTimer = window.setTimeout(runSearch, 120);
}

async function runSearch(): Promise<void> {
  // 拖放态下输入框是禁用的展示文本，不该被当成搜索词
  if (droppedFiles.length > 0) return;
  const query = input.value.trim();
  const token = ++queryToken;
  if (query.length === 0) {
    showHome();
    return;
  }
  try {
    const found = await invoke<SearchItem[]>("search", { query });
    if (token !== queryToken) return; // 已有更新的查询，丢弃过期结果
    if (query.startsWith("/f")) {
      // 文件搜索：列表形式
      setMode("list");
      pane.innerHTML = "";
      renderResults(found);
      void loadIcons(token);
      // /f 的覆盖范围取决于全盘索引状态，必须一并如实呈现（内部按 FS_POLL_MS 节流，
      // 逐键输入不会把命名管道打满）
      void refreshFsStatus();
    } else {
      // 默认：应用/命令网格形式；每次新查询重置展开状态
      delete sectionExpanded["pl"];
      delete sectionExpanded["sr"];
      delete sectionExpanded["mr"];
      setMode("grid");
      items = [];
      list.innerHTML = "";
      renderSearchGrid(found);
      // 搜索结果注入：并发问一次已常驻的插件，拿到什么追加什么。
      // 独立于主 search，慢插件不会拖住上面这一屏；宿主那边已限时 250ms。
      void appendPushResults(query, token);
    }
  } catch (err) {
    console.error("search failed", err);
  }
}

// ---------- /f 全盘索引状态条 ----------

/**
 * `/f` 到底能搜到哪些文件，**取决于运行期状态**，所以必须把状态如实摊在用户眼前：
 *
 * - 全盘索引（提权守护 + 直读 NTFS MFT）没开 → `/f` 走降级后端，覆盖面大致只有用户目录，
 *   其它磁盘一条都搜不到（这就是「/f 只能搜到 C 盘」那个反馈的成因）；
 * - 正在建索引 → 结果尚不完整，且这期间数字会变，得能自己刷新；
 * - 部分盘失败 → 得让用户看见是哪个盘、为什么失败。
 *
 * 面板上只写「文件搜索」而不说覆盖范围，就是让用户误以为搜的是全盘——这条状态条不是装饰。
 */

/** 建索引期间的刷新间隔：2 秒足够肉眼跟上，也不至于把命名管道打满 */
const FS_POLL_MS = 2000;
/** 点了「开启全盘索引」/「重建索引」之后最多等多久。
 *  守护要先过 UAC 再建索引；若用户拒了授权，状态永远不会变，轮询不能一直转下去。 */
const FS_WAIT_MS = 120_000;

let fsStatusEl: HTMLDivElement | null = null;
/** 最近一次拿到的真实状态；null = 没拿到（原因在 fsStatusError 里） */
let fsStatus: FileIndexStatus | null = null;
/** 取状态失败的真实原因（空串 = 没失败）。拿不到就说拿不到，不猜、不装作正常 */
let fsStatusError = "";
/** 动作反馈：开启 / 重建的真实结论（含「用户拒绝了提权」），单独一行显示 */
let fsNotice = "";
let fsPollTimer: number | undefined;
let fsFetching = false;
let fsLastFetchMs = 0;
/** 在此时刻前保持轮询（等守护提权起来 / 等重建开始），见 FS_WAIT_MS */
let fsWaitUntil = 0;
/** 上一次观察到的「全盘索引已在服务」；null = 还没观察过（首次不触发重跑搜索） */
let fsCovered: boolean | null = null;
/** 开启/重建的调用在途：按钮禁用，避免连点弹出两次 UAC */
let fsActionBusy = false;

/** 状态条元素懒建。位置：搜索栏之下、结果列表之上（.panel 的子节点顺序） */
function ensureFsStatusEl(): HTMLDivElement {
  if (!fsStatusEl) {
    fsStatusEl = document.createElement("div");
    fsStatusEl.className = "fs-status";
    fsStatusEl.hidden = true;
    panel.insertBefore(fsStatusEl, list);
  }
  return fsStatusEl;
}

/** 切出 /f 模式：藏起状态条、停轮询、清掉一次性动作反馈 */
function hideFsStatus(): void {
  stopFsPoll();
  fsNotice = "";
  if (fsStatusEl) fsStatusEl.hidden = true;
}

function stopFsPoll(): void {
  window.clearTimeout(fsPollTimer);
  fsPollTimer = undefined;
}

/** 只在「状态还会变」时排下一轮。已就绪 / 已 partial / 取不到状态就不轮询——
 *  状态不会自己变，每 2 秒问一次纯属浪费。轮询在切出 /f、面板失焦时都会停。 */
function scheduleFsPoll(): void {
  stopFsPoll();
  if (mode !== "list") return;
  const running = fsStatus?.running === true;
  const building = running && fsStatus?.state === "building";
  // 未开启也要盯着：`/f` 的查询本身会顺手请求提权拉起守护
  //（search/mod.rs 的 spawn_mft_bootstrap），用户在**那个** UAC 上点了「是」之后，
  // 这条状态条得自己变成「正在建索引」，而不是一直挂着「未开启」骗人。
  const off = fsStatus !== null && !running;
  // 「等守护起来」：点过开启/重建后，直到守护给出 building 之外的结论，或等待窗口到期
  const waiting = Date.now() < fsWaitUntil && !(running && !building);
  if (!building && !off && !waiting) return;
  fsPollTimer = window.setTimeout(() => void refreshFsStatus(true), FS_POLL_MS);
}

/** 取一次状态并重绘。`force=false` 时按 FS_POLL_MS 节流——/f 每敲一个字都会走到这里。 */
async function refreshFsStatus(force = false): Promise<void> {
  if (mode !== "list") return;
  renderFsStatus(); // 先把已有状态显示出来（可能只是重新进了 /f）
  if (fsFetching) return;
  if (!force && Date.now() - fsLastFetchMs < FS_POLL_MS) return;
  fsFetching = true;
  try {
    const raw = await invoke<unknown>("file_index_status");
    fsStatus = normalizeFsStatus(raw);
    fsStatusError = fsStatus
      ? ""
      : "索引状态的返回结构不认识（后端与界面版本不一致？）";
  } catch (err) {
    fsStatus = null;
    fsStatusError = `查询索引状态失败：${errText(err)}`;
  } finally {
    fsFetching = false;
    fsLastFetchMs = Date.now();
  }
  if (mode !== "list") return; // 期间用户切走了：不渲染也不排下一轮
  // 已就绪就把动作反馈收掉，别让「正在请求授权…」一直挂在一条已就绪的状态上
  if (fsStatus?.running && fsStatus.state === "ready") fsNotice = "";
  // 覆盖范围刚刚变大（守护上线 / 索引建完）：屏幕上那批结果还是降级后端给的，
  // 不重跑一次的话，状态条说着「全盘索引」，列表里却还是只有用户目录那几条。
  const covered = fsStatus?.running === true && fsStatus.state !== "building";
  if (covered && fsCovered === false && input.value.trim().length > 2) {
    void runSearch();
  }
  fsCovered = covered;
  renderFsStatus();
  scheduleFsPoll();
}

/** 后端字段的运行期校验：缺字段就当「拿不到状态」，绝不让 undefined 渲染成一个数字。 */
function normalizeFsStatus(raw: unknown): FileIndexStatus | null {
  if (!raw || typeof raw !== "object") return null;
  const o = raw as Record<string, unknown>;
  const running = o.running;
  if (typeof running !== "boolean") return null; // 契约的核心字段都没有，别硬撑
  const num = (v: unknown): number =>
    typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : 0;
  const readyRaw = pick(o, "ready_drives", "readyDrives");
  const failedRaw = pick(o, "failed_drives", "failedDrives");
  return {
    running,
    state: typeof o.state === "string" ? o.state : "",
    ready_drives: Array.isArray(readyRaw)
      ? readyRaw.filter((d): d is string => typeof d === "string")
      : [],
    failed_drives: Array.isArray(failedRaw)
      ? failedRaw
          .filter(
            (f): f is [string, string] =>
              Array.isArray(f) && typeof f[0] === "string" && typeof f[1] === "string",
          )
          .map(([d, why]) => [d, why] as [string, string])
      : [],
    entries: num(o.entries),
    memory_mb: num(pick(o, "memory_mb", "memoryMb")),
    excluded: num(o.excluded),
  };
}

/** 取字段：约定是 snake_case（后端 StatusDto 两侧都没加 rename_all），
 *  但序列化风格万一被改成 camelCase，这里也不至于把状态渲染成空白。 */
function pick(o: Record<string, unknown>, snake: string, camel: string): unknown {
  return o[snake] !== undefined ? o[snake] : o[camel];
}

/** "C" → "C:"；不是单个盘符字母就原样显示（后端把异常盘记成 "?"） */
function driveLabel(d: string): string {
  return /^[a-zA-Z]$/.test(d) ? `${d.toUpperCase()}:` : d;
}

function driveList(drives: string[]): string {
  return drives.map(driveLabel).join(" ");
}

/** 大数按中文习惯压缩：4640000 → "464 万"、12345 → "1.2 万" */
function formatCount(n: number): string {
  if (n < 10_000) return String(n);
  const wan = n / 10_000;
  return `${wan >= 100 ? Math.round(wan) : wan.toFixed(1)} 万`;
}

/** 把 invoke 的拒绝值变成一句能给用户看的话。后端多是 `Err(String)`，
 *  但也可能是结构化对象——取不出人话就说「未知错误」，不显示 undefined。 */
function errText(err: unknown): string {
  let raw = "";
  if (typeof err === "string") raw = err;
  else if (err instanceof Error) raw = err.message;
  else if (err !== null && err !== undefined) {
    raw = pickMessage(err) || JSON.stringify(err) || "";
  }
  const s = raw.trim() || "未知错误";
  return s.length > 160 ? `${s.slice(0, 160)}…` : s;
}

/** 从命令返回里取给用户看的 message（`file_index_enable` 的结论就在这个字段里）。
 *  不是字符串就返回空串——绝不把 undefined 当成文案显示出去。 */
function pickMessage(res: unknown): string {
  if (typeof res === "string") return res.trim();
  if (res && typeof res === "object") {
    const m = (res as { message?: unknown }).message;
    if (typeof m === "string") return m.trim();
  }
  return "";
}

interface FsView {
  variant: "info" | "busy" | "warn" | "err";
  main: string;
  details: string[];
  /** 悬停补充（条目数、占用、覆盖范围的细则），不占版面 */
  tip?: string;
  action?: { label: string; run: () => void; primary?: boolean; tip?: string };
}

/** 由真实状态推出要显示什么。每一句都只依赖已拿到的字段，没有的项就不显示。 */
function fsStatusView(): FsView {
  const s = fsStatus;
  if (!s) {
    return {
      variant: "err",
      main: "拿不到全盘索引状态",
      details: [
        fsStatusError,
        "现在无法确认 /f 的覆盖范围，结果可能只有用户目录一带的文件。",
      ],
      action: { label: "重试", run: () => void refreshFsStatus(true) },
    };
  }
  if (!s.running) {
    return {
      variant: "warn",
      main: "未开启全盘索引",
      details: [
        // 措辞上的分寸：降级后端是 Windows Search（覆盖范围由系统索引配置决定，
        // 本机实测只有 C:\Users\）或 walkdir 兜底（桌面/文档/下载/图片）。两者都在用户目录一带，
        // 但「其它盘一定搜不到」得留余地——用户若把 D: 加进了系统索引，那就能搜到。
        "当前 /f 只能搜到 Windows 系统索引已收录的位置（通常只有用户目录），其它磁盘一般搜不到。",
      ],
      action: {
        label: "开启全盘索引",
        primary: true,
        tip: "需要一次管理员授权（UAC）；授权后由一个独立的索引进程读取各磁盘的文件名",
        run: () => void enableFileIndex(),
      },
    };
  }
  if (s.state === "building") {
    return {
      variant: "busy",
      main: "正在建立全盘索引…",
      // 这里**故意不显示条目数**：守护只在一轮建完时才写回 entries，建索引期间那个值是
      // 上一轮的旧数（首次为 0），显示出来就是假进度。见 types.ts 里 entries 的说明。
      details: ["首次约需 40 秒，此刻的结果尚不完整。"],
    };
  }
  if (s.state === "ready") {
    // 就绪态只留很轻的一行，不抢结果列表的视觉
    const parts = ["全盘索引"];
    const drives = driveList(s.ready_drives);
    if (drives) parts.push(drives);
    if (s.entries > 0) parts.push(`${formatCount(s.entries)}条`);
    return {
      variant: "info",
      main: parts.join(" · "),
      details: [],
      tip: fsReadyTip(s),
      // 守护常驻并占着几百 MB，用户必须有地方关掉它（见 disableFileIndex）
      action: {
        label: "关闭",
        run: () => void disableFileIndex(),
        tip: `关闭全盘索引并释放约 ${s.memory_mb || "数百"} MB 内存；关闭后其它磁盘将搜不到`,
      },
    };
  }
  if (s.state === "partial") {
    const details = s.failed_drives.map(
      ([d, why]) => `${driveLabel(d)} 未索引：${why}`,
    );
    const ok = driveList(s.ready_drives);
    if (ok) {
      const count = s.entries > 0 ? ` · ${formatCount(s.entries)}条` : "";
      details.push(`已就绪：${ok}${count}`);
    }
    return {
      variant: "err",
      main: "部分磁盘未能索引，这些盘里的文件搜不到",
      details,
      action: { label: "重建索引", run: () => void rebuildFileIndex() },
    };
  }
  // "error" 与任何没见过的状态：把守护报的原文摊出来，不装作正常
  const details = s.failed_drives.map(([d, why]) => `${driveLabel(d)}：${why}`);
  details.unshift(`索引进程报告的状态：${s.state || "（空）"}`);
  return {
    variant: "err",
    main: "全盘索引状态异常",
    details,
    action: { label: "重建索引", run: () => void rebuildFileIndex() },
  };
}

function fsReadyTip(s: FileIndexStatus): string {
  const lines: string[] = [];
  const drives = driveList(s.ready_drives);
  if (drives) lines.push(`已索引磁盘：${drives}`);
  if (s.entries > 0) lines.push(`可搜条目：${s.entries.toLocaleString("zh-CN")}`);
  if (s.memory_mb > 0) lines.push(`索引占用：${s.memory_mb} MB`);
  if (s.excluded > 0) {
    lines.push(`按排除规则跳过：${s.excluded.toLocaleString("zh-CN")} 条`);
  }
  lines.push("只索引本机固定 NTFS 磁盘；U 盘 / 网络盘 / 非 NTFS 卷不在其中。");
  return lines.join("\n");
}

/** 渲染状态条（全量重建内容，纯 textContent，不拼 HTML） */
function renderFsStatus(): void {
  const el = ensureFsStatusEl();
  // 首次状态还没回来（既没成功也没失败）先不占位，免得闪一条空条
  if (mode !== "list" || (!fsStatus && !fsStatusError)) {
    el.hidden = true;
    return;
  }
  const view = fsStatusView();
  el.className = `fs-status fs-status--${view.variant}`;
  el.title = view.tip ?? "";
  el.textContent = "";
  el.hidden = false;

  const dot = document.createElement("span");
  dot.className = "fs-dot";

  const body = document.createElement("div");
  body.className = "fs-body";
  const main = document.createElement("div");
  main.className = "fs-main";
  main.textContent = view.main;
  body.appendChild(main);
  for (const line of view.details) {
    if (!line) continue;
    const d = document.createElement("div");
    d.className = "fs-detail";
    d.textContent = line;
    body.appendChild(d);
  }
  if (fsNotice) {
    const note = document.createElement("div");
    note.className = "fs-note";
    note.textContent = fsNotice;
    body.appendChild(note);
  }
  el.append(dot, body);

  if (view.action) {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = view.action.primary
      ? "fs-action fs-action-primary"
      : "fs-action";
    btn.textContent = fsActionBusy ? "处理中…" : view.action.label;
    btn.disabled = fsActionBusy;
    if (view.action.tip) btn.title = view.action.tip;
    // tabIndex=-1：本窗口的 Enter 被全局键盘处理占用（执行选中结果），若这个按钮能被 Tab
    // 聚焦，聚焦后按 Enter 却触发不了它——那正是「看着能用、按了不生效」的控件。只走点击。
    btn.tabIndex = -1;
    btn.addEventListener("click", view.action.run);
    el.appendChild(btn);
  }
  void resizeToContent();
}

/** 用户主动开启全盘索引：会弹一次 UAC，结果（含被拒绝）如实回显 */
async function enableFileIndex(): Promise<void> {
  if (fsActionBusy) return;
  fsActionBusy = true;
  fsNotice = "正在请求管理员授权…";
  renderFsStatus();
  // UAC 对话框会抢走焦点，而本窗口「失焦即隐藏」——不抑制的话点下按钮面板就没了，
  // 用户根本看不到授权结果。上限 FS_WAIT_MS 兜底：万一命令迟迟不返回，抑制也会自己到期。
  const prevSuppress = suppressHideUntil;
  suppressHideUntil = Date.now() + FS_WAIT_MS;
  try {
    const res = await invoke<unknown>("file_index_enable");
    // 后端把「已在运行 / 正在建索引 / 授权被取消 / 还没定」都写进了 message
    //（见 commands.rs 的 FileIndexEnableResult），原样转达，不在这里改写结论
    fsNotice = pickMessage(res) || "已请求开启（后端未给出说明）";
    // outcome=starting/pending 时守护可能还在起（UAC 对话框也可能还开着），
    // 这段时间持续刷新，让「建索引中 → 已就绪」自己变出来
    fsWaitUntil = Date.now() + FS_WAIT_MS;
  } catch (err) {
    fsNotice = `开启失败：${errText(err)}`;
  } finally {
    fsActionBusy = false;
    // 恢复失焦隐藏，留 400ms 缓冲：UAC 关闭后焦点要回到本窗口，别在这一瞬被判失焦
    suppressHideUntil = Math.max(prevSuppress, Date.now() + 400);
    renderFsStatus();
    void refreshFsStatus(true);
  }
}

/**
 * 关闭全盘索引：让守护退出、交还它占的内存。
 *
 * 这个入口必须存在——守护是常驻进程（主程序退出后它继续活着，好让下次开机即搜），
 * 实测占 370 MB。只给「开启」不给「关闭」就是背着用户常驻几百 MB。
 * 关掉后本次运行不会再自动拉起（后端会记住），要用得再点一次「开启全盘索引」。
 */
async function disableFileIndex(): Promise<void> {
  if (fsActionBusy) return;
  fsActionBusy = true;
  fsNotice = "正在关闭全盘索引…";
  renderFsStatus();
  try {
    const res = await invoke<unknown>("file_index_disable");
    fsNotice =
      pickMessage(res) ||
      "已关闭全盘索引并释放内存；/f 将回落到系统索引范围（其它磁盘一般搜不到）";
    // 关闭后状态会变成 running:false，让轮询把状态条切到「未开启」
    fsWaitUntil = Date.now() + 3000;
  } catch (err) {
    fsNotice = `关闭失败：${errText(err)}`;
  } finally {
    fsActionBusy = false;
    renderFsStatus();
    void refreshFsStatus(true);
  }
}

/** 重建索引（部分盘失败 / 状态异常时的恢复手段） */
async function rebuildFileIndex(): Promise<void> {
  if (fsActionBusy) return;
  fsActionBusy = true;
  fsNotice = "正在请求重建索引…";
  renderFsStatus();
  try {
    const res = await invoke<unknown>("file_index_rebuild");
    // 后端签名是 Result<(), String>：resolve（即 null）只代表**守护确认收下了请求**，
    // 重建本身是异步的，所以措辞只能说「已受理」，进度交给轮询。
    fsNotice = pickMessage(res) || "已受理重建请求，正在后台重建（期间结果可能不全）";
    fsWaitUntil = Date.now() + FS_WAIT_MS;
  } catch (err) {
    fsNotice = `重建失败：${errText(err)}`;
  } finally {
    fsActionBusy = false;
    renderFsStatus();
    void refreshFsStatus(true);
  }
}

// ---------- 文件搜索列表（/f 模式） ----------

/** 渲染后按需拉取可见项的真实系统图标并就地回填 */
async function loadIcons(token: number): Promise<void> {
  const targets = items
    .filter((i) => !i.icon && i.kind !== "command")
    .map((i) => i.target);
  if (targets.length === 0) return;
  try {
    const map = await invoke<Record<string, string>>("load_icons", {
      paths: targets,
    });
    if (token !== queryToken) return; // 查询已变，别回填过期图标
    for (let i = 0; i < items.length; i++) {
      const b64 = map[items[i].target];
      if (b64 && !items[i].icon) {
        items[i].icon = b64;
        setRowIcon(i, b64);
      }
    }
  } catch (err) {
    console.error("load_icons failed", err);
  }
}

/** 把第 index 行的占位字形替换为真实图标 */
function setRowIcon(index: number, b64: string): void {
  const row = list.querySelector<HTMLLIElement>(
    `.result[data-index="${index}"]`,
  );
  const iconEl = row?.querySelector<HTMLDivElement>(".result-icon");
  if (!iconEl) return;
  iconEl.classList.remove("glyph");
  iconEl.textContent = "";
  const img = document.createElement("img");
  img.src = `data:image/png;base64,${b64}`;
  img.alt = "";
  iconEl.appendChild(img);
}

function renderResults(next: SearchItem[]): void {
  items = next;
  selected = 0;
  list.innerHTML = "";
  for (let i = 0; i < items.length; i++) {
    list.appendChild(renderRow(items[i], i));
  }
  updateSelection();
  void resizeToContent();
}

function renderRow(item: SearchItem, index: number): HTMLLIElement {
  const li = document.createElement("li");
  li.className = "result";
  li.dataset.index = String(index);

  const icon = document.createElement("div");
  icon.className = "result-icon";
  if (item.icon) {
    const img = document.createElement("img");
    img.src = `data:image/png;base64,${item.icon}`;
    img.alt = "";
    icon.appendChild(img);
  } else {
    icon.classList.add("glyph");
    icon.innerHTML = fallbackIcon(item.kind);
  }

  const text = document.createElement("div");
  text.className = "result-text";
  const title = document.createElement("div");
  title.className = "result-title";
  title.textContent = item.title;
  const subtitle = document.createElement("div");
  subtitle.className = "result-subtitle";
  subtitle.textContent = item.subtitle;
  text.append(title, subtitle);

  const hint = document.createElement("div");
  hint.className = "result-hint";
  hint.textContent = hintFor(item);

  li.append(icon, text, hint);
  li.addEventListener("mousemove", () => {
    if (selected !== index) {
      selected = index;
      updateSelection();
    }
  });
  li.addEventListener("click", () => execute(index));
  if (item.kind !== "command") {
    li.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showPinMenu(e.clientX, e.clientY, item);
    });
  }
  return li;
}

function updateSelection(): void {
  const rows = list.querySelectorAll<HTMLLIElement>(".result");
  rows.forEach((row, i) => {
    row.classList.toggle("selected", i === selected);
    if (i === selected) row.scrollIntoView({ block: "nearest" });
  });
}

function move(delta: number): void {
  if (items.length === 0) return;
  selected = (selected + delta + items.length) % items.length;
  updateSelection();
}

function execute(index: number): void {
  const item = items[index];
  if (!item) return;
  // 不等启动完成：后台启动，立即藏窗（秒隐手感）
  invoke("execute", { item }).catch((err) =>
    console.error("execute failed", err),
  );
  void hide();
}

// ---------- 事件 ----------

input.addEventListener("input", scheduleSearch);

document.addEventListener("keydown", (e) => {
  const inGrid = mode !== "list";
  switch (e.key) {
    case "ArrowDown":
      e.preventDefault();
      if (inGrid) {
        gridMoveV(1);
      } else {
        move(1);
      }
      break;
    case "ArrowUp":
      e.preventDefault();
      if (inGrid) {
        gridMoveV(-1);
      } else {
        move(-1);
      }
      break;
    case "ArrowLeft":
      if (inGrid) {
        e.preventDefault();
        gridMoveH(-1);
      }
      break;
    case "ArrowRight":
      if (inGrid) {
        e.preventDefault();
        gridMoveH(1);
      }
      break;
    case "Enter": {
      e.preventDefault();
      if (e.repeat) break; // 长按 Enter 只执行一次
      // 捕获当前选择，挂起到 keyup 执行（届时按键已释放，不会穿透）
      const grid = inGrid;
      const gridIndex = gridSel < 0 ? 0 : gridSel;
      const listIndex = selected;
      armKeyAction(() => {
        if (grid) {
          execCell(gridIndex);
        } else {
          execute(listIndex);
        }
      });
      break;
    }
    case "Escape":
      e.preventDefault();
      if (e.repeat) break;
      // 拖放态下 Esc 先退回正常搜索，而不是直接把面板收起来：
      // 用户拖错了文件想重来，不该被整个关掉再重新唤起。
      if (droppedFiles.length > 0) {
        armKeyAction(() => clearDropState());
        break;
      }
      armKeyAction(() => void hide());
      break;
    default:
      // Ctrl+1..9 快速执行对应结果（仅文件列表模式）
      if (!inGrid && e.ctrlKey && /^[1-9]$/.test(e.key)) {
        e.preventDefault();
        execute(Number(e.key) - 1);
      }
  }
});

// keyup 时机执行挂起动作：按键消息已被本窗口完整消费，不会落到下层应用
document.addEventListener("keyup", (e) => {
  if (pendingKeyAction && (e.key === "Enter" || e.key === "Escape")) {
    const action = pendingKeyAction;
    pendingKeyAction = null;
    action();
  }
});

/** 拖动窗口期间（startDragging 会短暂失焦）抑制「失焦隐藏」的截止时间戳 */
let suppressHideUntil = 0;
/** 窗口是否处于「已隐藏」态：仅在隐藏→显示时锚定上部，避免撤销用户的拖动 */
let justHidden = true;

// 临时界面架构：失焦只藏窗、状态保留——再呼出恢复原界面；
// 回主界面的时机只有 Esc / 执行动作（它们走 hide()）。
appWindow.onFocusChanged(({ payload: focused }) => {
  if (focused) {
    cancelAutoClear(); // 呼出即取消待清除
    if (justHidden) {
      justHidden = false;
      void anchorTop(); // 仅「隐藏→显示」时锚回上部；拖动后重新获焦不锚，免得弹回原位
    }
    input.focus();
    input.select(); // 全选：呼出后直接打字即开始新搜索，不打字则保留原界面
    void applySettings(); // 兜底刷新外观（事件可能在窗口隐藏期丢失）
    // 停留在 /f 界面被再次呼出：索引状态可能已经变了（隐藏期不轮询），补取一次
    if (mode === "list") void refreshFsStatus(true);
  } else {
    // 拖动窗口引起的短暂失焦不隐藏（否则一按住拖动/点边缘就把面板隐藏掉）
    if (Date.now() < suppressHideUntil) return;
    void hideKeepState();
    justHidden = true;
    stopFsPoll(); // 窗口已隐藏：别让索引状态的轮询在后台空转
    scheduleAutoClear(); // 失焦按设置定时清除搜索内容
  }
});

// 头像 = 管理中心入口
avatarEl.addEventListener("click", () => {
  void invoke("open_admin_window");
});

/** 开始拖动窗口：startDragging 会让窗口短暂进入系统移动模式并失焦，
 *  先抑制这段时间的「失焦隐藏」，避免一按住拖动/点边缘就把面板隐藏掉。 */
function beginDrag(): void {
  suppressHideUntil = Date.now() + 700;
  void appWindow.startDragging();
}

// 整条搜索栏（含输入框）都可拖动窗口，但兼顾输入框可用：
// 单击 = 正常聚焦/定位光标；按住拖动（移动超过阈值）才拖窗口。头像是按钮不参与。
const searchRow = document.querySelector<HTMLDivElement>(".search-row")!;
let searchDragStart: { x: number; y: number } | null = null;
searchRow.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if ((e.target as HTMLElement).closest("#avatar")) return; // 头像 = 管理中心入口
  searchDragStart = { x: e.screenX, y: e.screenY };
});
window.addEventListener("mousemove", (e) => {
  if (!searchDragStart) return;
  // 用屏幕坐标：拖动跨越窗口移动时 client 坐标会失真
  if (Math.abs(e.screenX - searchDragStart.x) + Math.abs(e.screenY - searchDragStart.y) > 4) {
    searchDragStart = null;
    beginDrag();
  }
});
window.addEventListener("mouseup", () => {
  searchDragStart = null;
});

// 主面板 / 结果列表的空白处（磁贴、展开链接、结果行以外）也可拖动窗口
function paneDragHandler(e: MouseEvent): void {
  if (e.button !== 0) return;
  const target = e.target as HTMLElement;
  if (target.closest(".cell") || target.closest(".section-link") || target.closest(".result")) {
    return; // 可点击元素不触发拖动
  }
  e.preventDefault();
  beginDrag();
}
pane.addEventListener("mousedown", paneDragHandler);
list.addEventListener("mousedown", paneDragHandler);

/** 背景应用去重键（image|dim，未启用为空串；undefined = 从未应用过） */
let appliedBgKey: string | undefined;

/** 自定义占位符（来自设置 search_placeholder）；空则回退问候语 */
let customPlaceholder = "";

/** 占位符单一真相：自定义优先，否则 "Hi, {用户名}"（无用户名时 "Hi"） */
function updatePlaceholder(): void {
  if (customPlaceholder) {
    input.placeholder = customPlaceholder;
  } else {
    const user = homeData?.user;
    input.placeholder = user ? `Hi, ${user}` : "Hi";
  }
}

// ---------- 失焦自动清除搜索内容 ----------
let autoClearSeconds = AUTO_CLEAR_NEVER;
let autoClearTimer: number | undefined;

function cancelAutoClear(): void {
  window.clearTimeout(autoClearTimer);
  autoClearTimer = undefined;
}

/** 失焦后按 auto_clear_seconds 定时清空搜索框（0=立即，NEVER=从不）；再呼出即取消 */
function scheduleAutoClear(): void {
  cancelAutoClear();
  if (autoClearSeconds === AUTO_CLEAR_NEVER) return;
  autoClearTimer = window.setTimeout(() => {
    input.value = "";
    if (mode !== "home") showHome();
  }, autoClearSeconds * 1000);
}

/** 一次拉取设置并应用全部外观项：占位符 / 自动清除 / 背景（含启用开关与暗化蒙版）。
 *  注：主题（system/light/dark）仅作用于管理中心；主搜索窗依赖固定浅色 Acrylic 底，
 *  强制深色会致文字压浅底不可读，故此处不改主窗口主题（跟随系统）。 */
async function applySettings(force = false): Promise<void> {
  let s: AppSettings;
  try {
    s = await invoke<AppSettings>("get_settings");
  } catch (err) {
    console.error("applySettings failed", err);
    return;
  }

  customPlaceholder = s.search_placeholder?.trim() || "";
  updatePlaceholder();

  autoClearSeconds = s.auto_clear_seconds ?? AUTO_CLEAR_NEVER;

  // 背景图：受「启用背景图」开关约束，并叠加按 background_dim(0-100) 计算的暗化蒙版
  const img = s.background_image;
  const active = s.background_enabled && !!img;
  const key = active ? `${img}|${s.background_dim}` : "";
  if (!force && key === appliedBgKey) return;
  if (active && img) {
    try {
      const dataUrl = await invoke<string>("read_image", { path: img });
      const dim = Math.min(100, Math.max(0, s.background_dim)) / 100;
      const layers = ["linear-gradient(var(--bg-tint), var(--bg-tint))"];
      if (dim > 0) {
        layers.push(`linear-gradient(rgba(0,0,0,${dim}), rgba(0,0,0,${dim}))`);
      }
      layers.push(`url("${dataUrl}")`);
      appEl.style.backgroundImage = layers.join(", ");
      appEl.classList.add("has-bg");
      appliedBgKey = key; // 仅在成功应用后记忆去重键
    } catch (err) {
      console.error("read_image failed", err);
      appliedBgKey = undefined; // 失败不缓存，下次调用可重试
    }
  } else {
    appEl.style.backgroundImage = "";
    appEl.classList.remove("has-bg");
    appliedBgKey = key;
  }
}

void applySettings();
void listen("settings-changed", () => void applySettings(true));
// 账号资料变更（改昵称/头像、退出/注销）后刷新主界面问候语与头像字母
void listen("profile-changed", () => void refreshHome());
// 后端 setup 完成：首屏那次调用可能正好撞在 State 尚未 manage 的窗口期，这里补取一次
void listen("app-ready", () => void refreshHome());

input.focus();
showHome();

// ---------- 版本更新：自动检查 + 头像角标 + 确认弹窗 ----------

/** 启动后多久做第一次检查：让开机那阵子的磁盘/网络先让给正经事。 */
const UPDATE_FIRST_CHECK_MS = 20_000;
/** 之后的检查周期：**每小时**一次。
 *
 *  GitHub 未认证 API 的限流是 60 次/小时/IP，每小时 1 次远在阈值之下；
 *  而 iTools 是常驻应用，6 小时一次意味着上午发的版本下午才被发现。 */
const UPDATE_INTERVAL_MS = 60 * 60 * 1000;

/** 最近一次检查到的新版本信息；null = 没有新版（角标就不显示）。 */
let pendingUpdate: UpdateInfo | null = null;
/** 更新弹窗是否开着——开着时必须抑制「失焦隐藏」，否则点按钮的瞬间面板就没了。 */

/**
 * 静默检查更新：**失败一律不打扰用户**。
 *
 * 检查更新是后台行为，网络不通、GitHub 限流、被墙都很常见；为此弹错误提示纯属骚扰。
 * 失败就当作「暂时没有新版」，下个周期再说；用户真想知道，设置页还有手动检查（那里会报错）。
 */
async function checkUpdateSilently(): Promise<void> {
  try {
    const info = await invoke<UpdateInfo>("check_update");
    pendingUpdate = info.hasUpdate ? info : null;
  } catch {
    pendingUpdate = null; // 查不到 ≠ 没有新版，但绝不据此谎报「有更新」
  }
  updateBadgeEl.hidden = pendingUpdate === null;
  if (pendingUpdate) {
    updateBadgeEl.title = `有新版本 v${pendingUpdate.latestVersion}，点击更新`;
  }
}

updateBadgeEl.addEventListener("click", (e) => {
  e.stopPropagation(); // 别触发头像的「打开管理中心」
  // 更新说明是一整篇 Markdown，塞进这个 680×64 的搜索框会被截得没法读，
  // 所以交给独立窗口去展示；它自己会再查一次 check_update 拿最新信息。
  void invoke("open_update_window");
});

window.setTimeout(() => void checkUpdateSilently(), UPDATE_FIRST_CHECK_MS);
window.setInterval(() => void checkUpdateSilently(), UPDATE_INTERVAL_MS);
