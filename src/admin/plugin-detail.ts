//! 插件详情页：顶部返回 + 插件头部（含 Git 来源与更新入口）+ 两个 tab（说明 README / 设置）。
//!
//! 诚实占位：无 README.md 时「说明」tab 明确告知未提供；无 settings.json 时「设置」tab 明确
//! 告知无可配置项——不伪造空表单/空文档。
//! 来源行同样如实区分「Git 来源 / 内置 / 本地安装」，未检查过就不显示「已是最新」。

import type { AdminCtx } from "./main";
import type { PluginInfo, PluginUpdate } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { renderMarkdown } from "./markdown";
import { renderSettingsForm } from "./settings-form";
import { errText, sourceLabel, revisionLabel } from "./plugin-install";

const BACK_ICON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M15 18l-6-6 6-6"/></svg>';
const PLUGIN_GLYPH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';

/** 占位块（诚实告知内容缺失）。 */
function placeholder(title: string, desc?: string): HTMLElement {
  return h(
    "div",
    { class: "detail-empty" },
    h("div", { class: "detail-empty-title", text: title }),
    desc ? h("div", { class: "detail-empty-desc", text: desc }) : null,
  );
}

/**
 * 渲染插件详情页。
 * @param onBack 返回插件列表的回调
 * @param update 该插件最近一次更新检查结果（列表页传入；null = 本次会话未检查过）
 */
export async function renderPluginDetail(
  root: HTMLElement,
  ctx: AdminCtx,
  plugin: PluginInfo,
  onBack: () => void,
  update: PluginUpdate | null = null,
): Promise<void> {
  root.innerHTML = "";

  const backBtn = h("button", { class: "detail-back", html: BACK_ICON });
  backBtn.appendChild(h("span", { text: "插件" }));
  backBtn.addEventListener("click", onBack);

  const logo = plugin.logo
    ? h("img", { class: "detail-logo", src: plugin.logo, alt: "" })
    : h("span", { class: "detail-logo plugin-logo-fallback", html: PLUGIN_GLYPH });

  const header = h(
    "div",
    { class: "detail-header" },
    logo,
    h(
      "div",
      { class: "detail-meta" },
      h(
        "div",
        { class: "detail-name-row" },
        h("span", { class: "detail-name", text: plugin.display_name }),
        h("span", { class: "detail-ver", text: "v" + plugin.version }),
      ),
      plugin.author ? h("div", { class: "detail-author", text: "作者：" + plugin.author }) : null,
      h("div", { class: "detail-desc", text: plugin.description || "（无描述）" }),
    ),
  );

  // ---------- 来源 & 更新 ----------
  /** 来源行：Git 来源显示仓库与版本引用并给「查看仓库 / 更新」；否则如实说明为何不能自动更新。 */
  function buildSourceRow(): HTMLElement {
    const row = h("div", { class: "detail-source" });
    const src = plugin.source;

    if (!src) {
      row.appendChild(
        h("span", {
          class: "detail-source-text",
          text: plugin.builtin
            ? "内置插件：随 iTools 安装包分发，跟随 iTools 版本更新"
            : "本地安装：手工放入 plugins 目录，无法自动检查更新",
        }),
      );
      return row;
    }

    const parts = [`来源：${sourceLabel(src)}`];
    if (src.subPath) parts.push(`子目录 /${src.subPath}`);
    parts.push(`版本引用 ${revisionLabel(src.revision)}`);
    row.appendChild(
      h("span", { class: "detail-source-text", text: parts.join("　·　"), title: src.url }),
    );

    const repoBtn = h("button", { class: "btn btn-sm", text: "查看仓库" });
    repoBtn.addEventListener("click", async () => {
      try {
        await api.pluginOpenSourcePage(plugin.name);
      } catch (err) {
        console.error("plugin_open_source_page failed", err);
        ctx.toast(errText(err, "打开仓库失败"));
      }
    });
    row.appendChild(repoBtn);

    // 更新入口：只有列表页已确认「有新版」才出现更新按钮，其余状态如实标注
    if (update?.hasUpdate && update.latestVersion) {
      const latest = update.latestVersion;
      const updBtn = h("button", { class: "btn btn-sm btn-primary", text: `更新到 v${latest}` });
      updBtn.addEventListener("click", async () => {
        updBtn.disabled = true;
        updBtn.textContent = "更新中…";
        try {
          const res = await api.pluginUpdate(plugin.name);
          ctx.toast(`已更新到 v${res.currentVersion}`);
          onBack(); // 插件目录已整体替换，返回列表重新读取
        } catch (err) {
          console.error("plugin_update failed", err);
          const msg = errText(err, "更新失败");
          ctx.toast(msg);
          updBtn.title = msg;
          updBtn.disabled = false;
          updBtn.textContent = `更新到 v${latest}`;
        }
      });
      row.appendChild(updBtn);
    } else if (update?.error) {
      row.appendChild(
        h("span", {
          class: "plugin-badge plugin-badge-warn",
          text: "检查失败",
          title: `检查更新失败：${update.error}`,
        }),
      );
    } else if (update?.pinned) {
      row.appendChild(
        h("span", {
          class: "plugin-badge plugin-badge-muted",
          text: "已锁定",
          title: "安装来源锁定在指定 commit，不跟随远端更新",
        }),
      );
    } else if (update?.checked) {
      row.appendChild(
        h("span", { class: "plugin-badge plugin-badge-muted", text: "已是最新" }),
      );
    }
    return row;
  }

  // ---------- tabs ----------
  const readmeTab = h("button", { class: "detail-tab", text: "说明" });
  const settingsTab = h("button", { class: "detail-tab", text: "设置" });
  const tabBar = h("div", { class: "detail-tabs" }, readmeTab, settingsTab);

  const contentEl = h("div", { class: "detail-content" });
  contentEl.appendChild(h("div", { class: "detail-loading", text: "加载中…" }));

  root.append(
    h(
      "div",
      { class: "detail-scroll" },
      h("div", { class: "detail-topbar" }, backBtn),
      header,
      buildSourceRow(),
      tabBar,
      contentEl,
    ),
  );

  // ---------- 异步构建两个 tab 的内容 ----------
  let readmeView: HTMLElement;
  try {
    const md = await api.pluginReadme(plugin.name);
    readmeView =
      md && md.trim()
        ? renderMarkdown(md)
        : placeholder("该插件未提供说明文档", "作者可在插件目录放一个 README.md 介绍用法。");
  } catch (err) {
    console.error("plugin_readme failed", err);
    readmeView = placeholder("说明加载失败");
  }

  let settingsView: HTMLElement;
  try {
    const schema = await api.pluginSettingsSchema(plugin.name);
    const hasItems = !!schema && schema.groups.some((g) => g.items.length > 0);
    if (schema && hasItems) {
      const values = await api.pluginSettingsValues(plugin.name);
      settingsView = renderSettingsForm(plugin.name, schema, values, ctx.toast);
    } else {
      settingsView = placeholder(
        "该插件没有可配置项",
        "作者可在插件目录放一个 settings.json 声明设置项，iTools 会自动生成设置界面。",
      );
    }
  } catch (err) {
    console.error("plugin_settings_schema failed", err);
    settingsView = placeholder("设置加载失败");
  }

  function activate(which: "readme" | "settings"): void {
    readmeTab.classList.toggle("active", which === "readme");
    settingsTab.classList.toggle("active", which === "settings");
    contentEl.innerHTML = "";
    contentEl.appendChild(which === "readme" ? readmeView : settingsView);
  }
  readmeTab.addEventListener("click", () => activate("readme"));
  settingsTab.addEventListener("click", () => activate("settings"));

  // 默认 tab：有 README 或没有设置时进「说明」；否则（无 README 但有设置）进「设置」
  activate(plugin.has_readme || !plugin.has_settings ? "readme" : "settings");
}
