//! **更新窗口**：独立窗口里展示新版本说明并执行更新。
//!
//! # 为什么要独立成一个窗口
//!
//! 原来它是主搜索窗口里的一层 DOM overlay。主窗口是个 680×64 的搜索框，
//! 即便撑开高度也放不下一篇 release notes——实际表现是标题被截断、说明挤成一团、
//! 底部按钮被切掉半截，用户根本读不完就得决定要不要更新。
//!
//! 独立窗口不受搜索框尺寸约束，还能自由缩放。主窗口那边只留一个角标，点了就开这个窗口。
//!
//! # 诚实边界
//!
//! - 检查失败**如实说**（这个窗口是用户主动点开的，不同于后台静默检查，
//!   静默那次失败不打扰用户是对的，这次沉默反而让人以为程序坏了）；
//! - 没有安装包直链时按钮改成「前往下载页」，不假装能一键装；
//! - 下载/调起失败退回下载页，并把原因显示出来，不让用户卡在「点了没反应」。

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { renderMarkdown } from "./admin/markdown";
import type { UpdateInfo } from "./types";

const mainEl = document.getElementById("main") as HTMLElement;
const closeBtn = document.getElementById("close") as HTMLButtonElement;

const win = getCurrentWindow();
const hide = (): void => void win.hide();

closeBtn.addEventListener("click", hide);
// Esc 关窗：这是个模态性质的窗口，用户的第一直觉就是按 Esc
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hide();
});

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text) n.textContent = text;
  return n;
}

/** 渲染「有新版本」。 */
function renderUpdate(info: UpdateInfo): void {
  mainEl.textContent = "";

  const head = el("div", "up-head");
  head.append(
    el("div", "up-title", `发现新版本 v${info.latestVersion}`),
    el("div", "up-meta", `当前 v${info.currentVersion} → v${info.latestVersion}`),
  );

  const notes = el("div", "up-notes");
  const raw = (info.releaseNotes || "").trim();
  if (raw) {
    // release notes 是 Markdown，必须渲染。远端图片一律不加载：
    // 这个窗口可能在用户还没决定更新时就打开，去请求外链图片等于把 IP/UA 送出去。
    notes.appendChild(renderMarkdown(raw, { allowRemoteImages: false }));
  } else {
    notes.appendChild(el("p", "up-empty", "这个版本没有提供更新说明。"));
  }

  const warn = el(
    "div",
    "up-warn",
    info.msiUrl
      ? "更新将下载安装包并启动系统安装程序，iTools 会先退出。安装需要管理员授权。"
      : "本次发布没有提供安装包直链，将为你打开下载页手动下载。",
  );

  const actions = el("div", "up-actions");
  const later = el("button", "up-btn", "稍后");
  later.addEventListener("click", hide);
  const go = el("button", "up-btn up-btn-primary", info.msiUrl ? "立即更新" : "前往下载");
  go.addEventListener("click", () => void runUpdate(info, go, warn));
  actions.append(later, go);

  mainEl.append(head, notes, warn, actions);
  go.focus();
}

/** 渲染「已是最新」/「检查失败」这类没有更新可装的情况。 */
function renderPlain(title: string, detail: string): void {
  mainEl.textContent = "";
  const box = el("div", "up-plain");
  box.append(el("div", "up-title", title), el("div", "up-plain-detail", detail));
  const actions = el("div", "up-actions");
  const ok = el("button", "up-btn up-btn-primary", "知道了");
  ok.addEventListener("click", hide);
  actions.appendChild(ok);
  box.appendChild(actions);
  mainEl.appendChild(box);
  ok.focus();
}

/** 执行更新：下载 msi → 调起安装并退出；没有直链则打开下载页。 */
async function runUpdate(info: UpdateInfo, btn: HTMLButtonElement, warn: HTMLElement): Promise<void> {
  if (!info.msiUrl) {
    void invoke("open_release_page", { url: info.releaseUrl });
    hide();
    return;
  }
  btn.disabled = true;
  btn.textContent = "正在下载…";
  warn.textContent = "正在下载安装包，请勿关闭窗口。";
  try {
    const path = await invoke<string>("download_update", { url: info.msiUrl });
    warn.textContent = "下载完成，正在启动安装程序…";
    await invoke("launch_installer_and_quit", { path });
  } catch (err) {
    console.error("update failed", err);
    // 失败要说清楚，并给一条能走通的路，而不是让按钮卡在「正在下载」
    btn.disabled = false;
    btn.textContent = "前往下载页";
    warn.textContent = `更新失败：${String(err)}。可以点右边按钮手动下载。`;
    btn.onclick = () => {
      void invoke("open_release_page", { url: info.releaseUrl });
      hide();
    };
  }
}

async function boot(): Promise<void> {
  try {
    const info = await invoke<UpdateInfo>("check_update");
    if (info.hasUpdate) {
      renderUpdate(info);
    } else {
      renderPlain("已经是最新版本", `当前版本 v${info.currentVersion}。`);
    }
  } catch (err) {
    // 与后台静默检查不同：这个窗口是用户主动打开的，失败必须如实说，
    // 沉默会让人以为程序坏了
    renderPlain("检查更新失败", `${String(err)}。请稍后再试，或到设置页手动检查。`);
  }
}

void boot();
