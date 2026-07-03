//! 本地启动面板：总开关 + 自定义文件/文件夹清单（选择/拖放添加、单删/批删、立即启动）。
import type { AdminCtx } from "./main";
import type { AppSettings, LaunchItem } from "../types";
import { h } from "./ui";
import { setDropZone } from "./dnd";
import * as api from "./api";

const FOLDER_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>';
const FILE_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z"/><path d="M14 3v5h5"/></svg>';
const EJECT_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 17h12"/><path d="M12 5l6 8H6z"/></svg>';

// ---------- 轻量下拉菜单 ----------
interface MenuEntry {
  label: string;
  onClick: () => void;
  danger?: boolean;
}
let openState: { menu: HTMLElement; onDoc: (e: MouseEvent) => void } | null = null;

function closeMenu(): void {
  if (openState) {
    document.removeEventListener("mousedown", openState.onDoc);
    openState.menu.remove();
    openState = null;
  }
}

function openMenu(anchor: HTMLElement, entries: MenuEntry[]): void {
  closeMenu();
  const menu = h("div", { class: "popmenu" });
  for (const e of entries) {
    menu.appendChild(
      h("button", {
        class: "popmenu-item" + (e.danger ? " danger" : ""),
        text: e.label,
        onClick: () => {
          closeMenu();
          e.onClick();
        },
      }),
    );
  }
  document.body.appendChild(menu);
  const r = anchor.getBoundingClientRect();
  menu.style.top = `${r.bottom + 4}px`;
  menu.style.right = `${Math.max(8, window.innerWidth - r.right)}px`;
  const onDoc = (e: MouseEvent): void => {
    if (!menu.contains(e.target as Node) && e.target !== anchor) closeMenu();
  };
  setTimeout(() => document.addEventListener("mousedown", onDoc), 0);
  openState = { menu, onDoc };
}

export async function renderLaunch(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let settings: AppSettings;
  try {
    settings = await api.getSettings();
  } catch (err) {
    console.error("get_settings failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "本地启动设置加载失败" }));
    return;
  }

  const selected = new Set<string>();
  let iconMap: Record<string, string> = {};

  async function reloadIcons(): Promise<void> {
    const paths = settings.local_launch_items.map((it) => it.path);
    if (!paths.length) {
      iconMap = {};
      return;
    }
    try {
      iconMap = await api.loadIcons(paths);
    } catch {
      iconMap = {};
    }
  }

  const listWrap = h("div", { class: "launch-list" });

  function iconFor(it: LaunchItem): HTMLElement {
    const b64 = iconMap[it.path];
    if (b64) {
      const img = h("img", { class: "launch-ic", src: `data:image/png;base64,${b64}`, alt: "" });
      return img;
    }
    return h("span", { class: "launch-ic launch-ic-fallback", html: it.is_dir ? FOLDER_ICON : FILE_ICON });
  }

  function renderRow(it: LaunchItem): HTMLElement {
    const check = h("input", { type: "checkbox", class: "launch-check" }) as HTMLInputElement;
    check.checked = selected.has(it.id);
    check.addEventListener("change", () => {
      if (check.checked) selected.add(it.id);
      else selected.delete(it.id);
    });

    const runBtn = h("button", {
      class: "launch-run",
      title: "立即启动",
      html: EJECT_ICON,
      onClick: async () => {
        try {
          await api.runLaunchItem(it.path);
          ctx.toast("已启动");
        } catch (err) {
          console.error("run_launch_item failed", err);
          ctx.toast(typeof err === "string" ? err : "启动失败");
        }
      },
    });

    const delBtn = h("button", {
      class: "launch-del",
      title: "移除",
      html: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>',
      onClick: async () => {
        settings = await api.removeLaunchItems([it.id]);
        selected.delete(it.id);
        await reloadIcons();
        rerenderList();
        ctx.toast("已移除");
      },
    });

    return h(
      "div",
      { class: "launch-row" },
      iconFor(it),
      h(
        "div",
        { class: "launch-row-text" },
        h("div", { class: "launch-row-name", text: it.name }),
        h("div", { class: "launch-row-path", text: it.path, title: it.path }),
      ),
      delBtn,
      runBtn,
      check,
    );
  }

  function rerenderList(): void {
    listWrap.innerHTML = "";
    const items = settings.local_launch_items;
    if (!items.length) {
      listWrap.appendChild(
        h(
          "div",
          { class: "launch-empty" },
          h("div", { class: "launch-empty-title", text: "还没有自定义启动项" }),
          h("div", { class: "launch-empty-desc", text: "点击右上角「+」添加文件/文件夹，或直接把它们拖到这里。" }),
        ),
      );
      return;
    }
    items.forEach((it) => listWrap.appendChild(renderRow(it)));
  }

  // ---------- 子标题 + 操作 ----------
  async function addPaths(paths: string[]): Promise<void> {
    if (!paths.length) return;
    settings = await api.addLaunchItems(paths);
    await reloadIcons();
    rerenderList();
    ctx.toast("已添加");
  }

  const plusBtn = h("button", {
    class: "icon-btn",
    title: "添加",
    html: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>',
  });
  plusBtn.addEventListener("click", () =>
    openMenu(plusBtn, [
      {
        label: "选择文件",
        onClick: async () => {
          const paths = await api.pickLaunchFiles();
          await addPaths(paths);
        },
      },
      {
        label: "选择文件夹",
        onClick: async () => {
          const path = await api.pickLaunchFolder();
          if (path) await addPaths([path]);
        },
      },
    ]),
  );

  const moreBtn = h("button", {
    class: "icon-btn",
    title: "更多",
    html: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/></svg>',
  });
  moreBtn.addEventListener("click", () =>
    openMenu(moreBtn, [
      {
        label: "全选",
        onClick: () => {
          settings.local_launch_items.forEach((it) => selected.add(it.id));
          rerenderList();
        },
      },
      {
        label: "取消全选",
        onClick: () => {
          selected.clear();
          rerenderList();
        },
      },
      {
        label: "删除选中",
        danger: true,
        onClick: async () => {
          if (!selected.size) {
            ctx.toast("未选择任何项");
            return;
          }
          settings = await api.removeLaunchItems([...selected]);
          selected.clear();
          await reloadIcons();
          rerenderList();
          ctx.toast("已删除选中");
        },
      },
      {
        label: "清空列表",
        danger: true,
        onClick: async () => {
          const ids = settings.local_launch_items.map((it) => it.id);
          if (!ids.length) return;
          settings = await api.removeLaunchItems(ids);
          selected.clear();
          await reloadIcons();
          rerenderList();
          ctx.toast("已清空");
        },
      },
    ]),
  );

  const subhead = h(
    "div",
    { class: "launch-subhead" },
    h("span", { class: "launch-subhead-title", text: "自定义文件启动" }),
    h("div", { class: "launch-actions" }, plusBtn, moreBtn),
  );

  const intro = h("div", {
    class: "launch-intro",
    text: "添加常用的文件 / 文件夹 / 程序：加入后可在主搜索栏直接搜到并打开，或点右侧「立即启动」。",
  });

  // 拖放添加（整个列表区）
  setDropZone(listWrap, (paths) => void addPaths(paths));

  await reloadIcons();
  rerenderList();

  root.append(h("div", { class: "launch-scroll" }, intro, subhead, listWrap));
}
