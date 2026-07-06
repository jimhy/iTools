//! 插件管理面板：列出 plugins/ 下已装插件，启用/禁用（是否参与主搜索）、删除、一键重新加载。
import type { AdminCtx } from "./main";
import type { PluginInfo } from "../types";
import { h, makeSwitch } from "./ui";
import * as api from "./api";
import { renderPluginDetail } from "./plugin-detail";

const PLUGIN_GLYPH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';
const TRASH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 13a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-13"/></svg>';
const REFRESH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>';

/** 高危能力名 → 展示文案 */
const PERM_LABELS: Record<string, string> = {
  runCommand: "执行程序",
  network: "联网",
};
const permLabel = (perm: string): string => PERM_LABELS[perm] ?? perm;

export async function renderPlugins(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let list: PluginInfo[];
  try {
    list = await api.listPlugins();
  } catch (err) {
    console.error("list_plugins failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "插件列表加载失败" }));
    return;
  }

  const listWrap = h("div", { class: "plugin-list" });

  function logoFor(p: PluginInfo): HTMLElement {
    if (p.logo) return h("img", { class: "plugin-logo", src: p.logo, alt: "" });
    return h("span", { class: "plugin-logo plugin-logo-fallback", html: PLUGIN_GLYPH });
  }

  // 进入插件详情页；返回时重渲染列表（root 由详情页/此处负责清空）
  async function openDetail(p: PluginInfo): Promise<void> {
    await renderPluginDetail(root, ctx, p, () => {
      root.innerHTML = "";
      void renderPlugins(root, ctx);
    });
  }

  function renderCard(p: PluginInfo): HTMLElement {
    const card = h("div", { class: "plugin-card clickable" + (p.enabled ? "" : " disabled") });

    const meta = h(
      "div",
      { class: "plugin-meta" },
      h(
        "div",
        { class: "plugin-name-row" },
        h("span", { class: "plugin-name", text: p.name }),
        h("span", { class: "plugin-ver", text: "v" + p.version }),
      ),
      h("div", { class: "plugin-desc", text: p.description || "（无描述）" }),
      p.cmds.length
        ? h("div", { class: "plugin-cmds", text: "关键字：" + p.cmds.join("  ·  ") })
        : null,
    );

    // 高危能力授权（插件声明了才显示；开关切换即授权/撤销）
    if (p.permissions.length) {
      const permsRow = h("div", { class: "plugin-perms" });
      for (const perm of p.permissions) {
        const permSw = makeSwitch(p.granted.includes(perm), async (checked) => {
          try {
            await api.setPluginPermission(p.name, perm, checked);
            ctx.toast((checked ? "已授权 " : "已撤销 ") + permLabel(perm));
          } catch (err) {
            console.error("set_plugin_permission failed", err);
            ctx.toast("操作失败");
          }
        });
        permSw.classList.add("switch-sm");
        permsRow.appendChild(
          h(
            "div",
            { class: "plugin-perm-chip", title: "授权后该插件才能使用「" + permLabel(perm) + "」能力" },
            h("span", { class: "plugin-perm-name", text: permLabel(perm) }),
            permSw,
          ),
        );
      }
      meta.appendChild(permsRow);
    }

    // 删除：两步确认（先变「确认删除」，3s 内再点才真删）
    let pending = false;
    let pendTimer: number | undefined;
    const delBtn = h("button", { class: "plugin-del", title: "删除插件", html: TRASH });
    delBtn.addEventListener("click", async () => {
      if (!pending) {
        pending = true;
        delBtn.classList.add("confirm");
        delBtn.textContent = "确认删除";
        pendTimer = window.setTimeout(() => {
          pending = false;
          delBtn.classList.remove("confirm");
          delBtn.innerHTML = TRASH;
        }, 3000);
        return;
      }
      window.clearTimeout(pendTimer);
      try {
        await api.deletePlugin(p.name);
        ctx.toast("已删除插件 " + p.name);
        await reload();
      } catch (err) {
        console.error("delete_plugin failed", err);
        ctx.toast(typeof err === "string" ? err : "删除失败");
        // 删除失败：复位确认态，避免下次一点即删
        pending = false;
        delBtn.classList.remove("confirm");
        delBtn.innerHTML = TRASH;
      }
    });

    const sw = makeSwitch(p.enabled, async (checked) => {
      try {
        await api.setPluginEnabled(p.name, checked);
        card.classList.toggle("disabled", !checked);
        ctx.toast(checked ? "已启用" : "已禁用（不参与搜索）");
      } catch (err) {
        console.error("set_plugin_enabled failed", err);
        ctx.toast("操作失败");
      }
    });

    card.append(logoFor(p), meta, h("div", { class: "plugin-actions" }, delBtn, sw));
    // 点卡片主体进详情；点开关/权限/删除等交互控件区不触发
    card.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".plugin-actions, .plugin-perms")) return;
      void openDetail(p);
    });
    return card;
  }

  function rerender(): void {
    listWrap.innerHTML = "";
    if (!list.length) {
      listWrap.appendChild(
        h(
          "div",
          { class: "plugin-empty" },
          h("div", { class: "plugin-empty-title", text: "还没有安装插件" }),
          h("div", {
            class: "plugin-empty-desc",
            text: "把插件目录放进项目的 plugins/ 文件夹（或让 AI 生成后放入），再点右上角「重新加载」。",
          }),
        ),
      );
      return;
    }
    list.forEach((p) => listWrap.appendChild(renderCard(p)));
  }

  async function reload(): Promise<void> {
    try {
      list = await api.listPlugins();
    } catch {
      /* 保留旧列表 */
    }
    rerender();
  }

  const reloadBtn = h("button", { class: "icon-btn", title: "重新扫描 plugins 目录", html: REFRESH });
  reloadBtn.addEventListener("click", async () => {
    try {
      const n = await api.rescanPlugins();
      await reload();
      ctx.toast(`已重新加载（${n} 条命令）`);
    } catch (err) {
      console.error("rescan_plugins failed", err);
      ctx.toast("重载失败");
    }
  });

  const subhead = h(
    "div",
    { class: "launch-subhead" },
    h("span", { class: "launch-subhead-title", text: "已安装插件" }),
    h("div", { class: "launch-actions" }, reloadBtn),
  );

  const intro = h("div", {
    class: "launch-intro",
    text: "管理 plugins/ 目录里的插件：开关控制是否参与主搜索，删除会移除插件目录。改/新增插件后点右上角「重新加载」即时生效。",
  });

  rerender();
  root.append(h("div", { class: "launch-scroll" }, intro, subhead, listWrap));
}
