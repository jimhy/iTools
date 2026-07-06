import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize, LogicalPosition } from "@tauri-apps/api/dpi";
import { listen } from "@tauri-apps/api/event";
import type { AppSettings, HomeData, SearchItem } from "./types";
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
/** 最近一次搜索网格的数据（供展开/收起重渲染） */
let lastGridItems: SearchItem[] = [];
const sectionExpanded: Record<string, boolean> = {};
let menuEl: HTMLDivElement | null = null;

// ---------- 内置工具磁贴 ----------

const BUILTIN_TILES: { title: string; fill: string; icon: string }[] = [
  { title: "计算器", fill: "1+2*3", icon: "calc" },
  { title: "时间戳", fill: "now", icon: "clock" },
  { title: "颜色转换", fill: "#ff8800", icon: "color" },
  { title: "进制转换", fill: "0xFF", icon: "hex" },
  { title: "打开网址", fill: "github.com", icon: "globe" },
  { title: "文件搜索", fill: "/f ", icon: "fsearch" },
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

/** 窗口高度随内容伸缩（宽度固定）；内容超过屏幕可用高度才在内部滚动 */
async function resizeToContent(): Promise<void> {
  const cap = maxContentHeight();
  let height = SEARCH_ROW_HEIGHT;
  if (mode === "list") {
    if (items.length > 0) {
      height += Math.min(list.scrollHeight + 2, cap);
    }
  } else {
    // +2 补 #home 的 1px 上边框与亚像素取整，否则折叠态也会冒出滚动条
    height += Math.min(pane.scrollHeight + 2, cap);
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
}): HTMLDivElement {
  const el = document.createElement("div");
  el.className = "cell";
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
      invoke("open_plugin_window", {
        target: cell.item.target,
        query: input.value.trim(),
      }).catch((err) => console.error("open_plugin_window failed", err));
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

async function refreshHome(): Promise<void> {
  try {
    homeData = await invoke<HomeData>("home_data");
  } catch (err) {
    console.error("home_data failed", err);
    return;
  }
  // 问候语与头像常驻搜索栏
  const user = homeData.user;
  if (user) {
    avatarEl.textContent = user[0].toUpperCase();
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
  pane.appendChild(renderMarketSection());

  if (gridCells.length > 0) selectCell(0);
  void loadGridIcons();
  void resizeToContent();
}

/** 市场精选：插件市场尚未开放，暂显示「敬请期待」占位 */
function renderMarketSection(): HTMLDivElement {
  const wrap = document.createElement("div");
  wrap.className = "home-section";

  const head = document.createElement("div");
  head.className = "section-head";
  const label = document.createElement("span");
  label.className = "section-title";
  label.textContent = "市场精选";
  head.appendChild(label);
  wrap.appendChild(head);

  const coming = document.createElement("div");
  coming.className = "section-coming";
  coming.textContent = "敬请期待";
  wrap.appendChild(coming);
  return wrap;
}

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
function scheduleSearch(): void {
  window.clearTimeout(debounceTimer);
  debounceTimer = window.setTimeout(runSearch, 120);
}

async function runSearch(): Promise<void> {
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
    } else {
      // 默认：应用/命令网格形式；每次新查询重置展开状态
      delete sectionExpanded["pl"];
      delete sectionExpanded["sr"];
      delete sectionExpanded["mr"];
      setMode("grid");
      items = [];
      list.innerHTML = "";
      renderSearchGrid(found);
    }
  } catch (err) {
    console.error("search failed", err);
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
  } else {
    // 拖动窗口引起的短暂失焦不隐藏（否则一按住拖动/点边缘就把面板隐藏掉）
    if (Date.now() < suppressHideUntil) return;
    void hideKeepState();
    justHidden = true;
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

input.focus();
showHome();
