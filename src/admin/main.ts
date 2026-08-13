//! 管理中心入口：自绘标题栏、左侧导航路由、主题应用、全局拖放初始化。
import "../admin.css";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getSettings, closeAdminWindow } from "./api";
import type { Theme } from "../types";
import { toast } from "./ui";
import { initDnd, clearDropZone } from "./dnd";
import { renderAccount } from "./account";
import { renderData } from "./data";
import { renderSettings } from "./settings";
import { renderLaunch } from "./launch";
import { renderPlugins } from "./plugins";
import { renderMarket } from "./market";
import { renderNetwork } from "./network";
import { renderDev } from "./dev";
import { renderPlaceholder } from "./placeholder";

const win = getCurrentWindow();
const contentEl = document.querySelector<HTMLElement>("#content")!;
const navItems = Array.from(document.querySelectorAll<HTMLButtonElement>(".nav-item"));

/** 面板向框架回调的上下文 */
export interface AdminCtx {
  /** 顶部轻提示 */
  toast: (msg: string) => void;
  /** 即时应用主题（设置面板切主题时用） */
  applyTheme: (theme: Theme) => void;
  /**
   * 注册「离开本面板时要做的清理」（解绑 Tauri 事件监听、清定时器等）。
   *
   * 切换视图时框架只做 `contentEl.innerHTML = ""`，DOM 没了但 `listen()` 注册的 IPC 监听还挂着；
   * 管理中心窗口是**只隐藏不销毁**的，这类监听会跨越整个 app 生命周期存活，
   * 后端每 emit 一次就白白唤醒一次前端。面板凡是注册了 DOM 之外的东西，都必须在这里登记清理函数。
   *
   * 每个视图只保留最后一次登记的清理函数；切走时框架调用它，随后清空。
   *
   * ⚠ 边界：本机制**只在 switchView（切换左侧导航项）时触发**。点关闭 / Esc 走的是
   * `close_admin_window` → `win.hide()`，窗口只隐藏不销毁、也不切页，此时清理函数**不会**被调用，
   * 面板仍处于「已挂载」状态。凡是「窗口看不见时就不该干的活」（如后端 emit 驱动的自动刷新），
   * 必须由面板自己按 `document.visibilityState` 另做门控（见 plugins.ts 的可见性门控），
   * 不能指望这里。
   */
  registerDispose: (fn: () => void) => void;
  /**
   * 切到另一个左侧导航项（面板内的「前往 XX 设置」跳转用）。
   *
   * 必须走这里、不要自己 `contentEl.innerHTML = ""` 再渲染：switchView 会先跑上个面板
   * registerDispose 登记的清理（解绑 IPC 监听、清定时器），绕过它会漏掉这一步。
   */
  goto: (view: ViewId) => void;
}

export type ViewId =
  | "account"
  | "data"
  | "settings"
  | "ai"
  | "launch"
  | "plugins"
  | "dev"
  | "all"
  | "network"
  | "market";

/** 当前视图登记的清理函数（null = 无需清理）。 */
let currentDispose: (() => void) | null = null;

const ctx: AdminCtx = {
  toast,
  applyTheme,
  registerDispose: (fn) => {
    currentDispose = fn;
  },
  // switchView 是函数声明，提升到此处之前，直接引用安全
  goto: (view) => switchView(view),
};

// ---------- 主题 ----------

const media = window.matchMedia("(prefers-color-scheme: dark)");
let themePref: Theme = "system";

/** 应用主题：system 跟随系统，其余强制。写到 <html data-theme> 供 CSS 变量切换。 */
function applyTheme(theme: Theme): void {
  themePref = theme;
  const effective = theme === "system" ? (media.matches ? "dark" : "light") : theme;
  document.documentElement.dataset.theme = effective;
}

// 跟随系统时，系统深浅色变化需实时反映
media.addEventListener("change", () => {
  if (themePref === "system") applyTheme("system");
});

// ---------- 路由 ----------

function switchView(view: ViewId): void {
  clearDropZone(); // 上个面板可能注册过拖放区，切换即清
  // 上个面板登记的清理（Tauri 事件监听等）：innerHTML="" 只清 DOM，清不掉 IPC 监听
  const dispose = currentDispose;
  currentDispose = null;
  if (dispose) {
    try {
      dispose();
    } catch (err) {
      console.error("面板清理失败", err);
    }
  }
  navItems.forEach((n) => n.classList.toggle("active", n.dataset.view === view));
  contentEl.innerHTML = "";
  contentEl.scrollTop = 0;
  switch (view) {
    case "account":
      void renderAccount(contentEl, ctx);
      break;
    case "network":
      void renderNetwork(contentEl, ctx);
      break;
    case "settings":
      void renderSettings(contentEl, ctx);
      break;
    case "launch":
      void renderLaunch(contentEl, ctx);
      break;
    case "plugins":
      void renderPlugins(contentEl, ctx);
      break;
    case "dev":
      void renderDev(contentEl, ctx);
      break;
    case "data":
      void renderData(contentEl, ctx);
      break;
    case "ai":
      renderPlaceholder(contentEl, "AI Agent 连接", "后续规划中，敬请期待。");
      break;
    case "all":
      renderPlaceholder(contentEl, "所有功能", "后续规划中，敬请期待。");
      break;
    case "market":
      void renderMarket(contentEl, ctx);
      break;
  }
}

navItems.forEach((item) => {
  item.addEventListener("click", () => switchView(item.dataset.view as ViewId));
});

// ---------- 自绘标题栏 ----------

const titlebar = document.querySelector<HTMLElement>("#titlebar")!;
titlebar.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if ((e.target as HTMLElement).closest(".tb-btn")) return; // 按钮不触发拖动
  void win.startDragging();
});
titlebar.addEventListener("dblclick", (e) => {
  if ((e.target as HTMLElement).closest(".tb-btn")) return;
  void win.toggleMaximize();
});

document.querySelector("#tb-min")?.addEventListener("click", () => void win.minimize());
document.querySelector("#tb-max")?.addEventListener("click", () => void win.toggleMaximize());
document.querySelector("#tb-close")?.addEventListener("click", () => void closeAdminWindow());

// Esc 关闭（隐藏复用）
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") void closeAdminWindow();
});

// ---------- 启动 ----------

async function boot(): Promise<void> {
  void initDnd();
  try {
    const settings = await getSettings();
    applyTheme(settings.theme ?? "system");
  } catch (err) {
    console.error("初始化主题失败", err);
    applyTheme("system");
  }
  // 窗口每次重新显示时刷新主题（设置可能在别处改过）
  win.onFocusChanged(async ({ payload: focused }) => {
    if (!focused) return;
    try {
      const s = await getSettings();
      applyTheme(s.theme ?? "system");
    } catch {
      /* 忽略 */
    }
  });
  switchView("account");
}

void boot();
