//! 管理中心入口：自绘标题栏、左侧导航路由、主题应用、全局拖放初始化。
import "../admin.css";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getSettings, closeAdminWindow } from "./api";
import type { Theme } from "../types";
import { toast } from "./ui";
import { initDnd, clearDropZone } from "./dnd";
import { renderAccount } from "./account";
import { renderSettings } from "./settings";
import { renderLaunch } from "./launch";
import { renderPlugins } from "./plugins";
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
}

type ViewId = "account" | "data" | "settings" | "ai" | "launch" | "plugins" | "all" | "market";

const ctx: AdminCtx = { toast, applyTheme };

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
  navItems.forEach((n) => n.classList.toggle("active", n.dataset.view === view));
  contentEl.innerHTML = "";
  contentEl.scrollTop = 0;
  switch (view) {
    case "account":
      void renderAccount(contentEl, ctx);
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
    case "data":
      renderPlaceholder(contentEl, "我的数据", "插件功能完成后开放，敬请期待。");
      break;
    case "ai":
      renderPlaceholder(contentEl, "AI Agent 连接", "后续规划中，敬请期待。");
      break;
    case "all":
      renderPlaceholder(contentEl, "所有功能", "后续规划中，敬请期待。");
      break;
    case "market":
      renderPlaceholder(contentEl, "插件应用市场", "后续规划中，敬请期待。");
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
