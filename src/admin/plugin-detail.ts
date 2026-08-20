//! 插件详情页：顶部返回 + 插件头部（含 Git 来源与更新入口）+ 两个 tab（说明 README / 设置）。
//!
//! 诚实占位：无 README.md 时「说明」tab 明确告知未提供；无 settings.json 时「设置」tab 明确
//! 告知无可配置项——不伪造空表单/空文档。
//! 来源行同样如实区分「Git 来源 / 内置 / 本地安装」，未检查过就不显示「已是最新」。

import type { AdminCtx } from "./main";
import type { PluginInfo, PluginUpdate, AuditEntry } from "../types";
import { h, makeSwitch } from "./ui";
import * as api from "./api";
import { renderMarkdown } from "./markdown";
import { renderSettingsForm } from "./settings-form";
import { errText, sourceLabel, revisionLabel, permLabel } from "./plugin-install";

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

  // 「随 iTools 启动」——宿主提供的固定项，排在插件自己的设置项之前。
  //
  // **只对清单声明了 background 的插件显示**。没声明的插件后台跑着什么也不做，
  // 给它显示这个开关就是「开了没用」的假控件，宁可不给。
  const autostartSection = plugin.background ? buildAutostartSection(plugin, ctx) : null;

  let settingsView: HTMLElement;
  try {
    const schema = await api.pluginSettingsSchema(plugin.name);
    const hasItems = !!schema && schema.groups.some((g) => g.items.length > 0);
    if (schema && hasItems) {
      const values = await api.pluginSettingsValues(plugin.name);
      settingsView = renderSettingsForm(plugin.name, schema, values, ctx.toast);
    } else if (plugin.background) {
      // 有宿主提供的固定项（自启动 / 使用记录）时就不能再说「没有可配置项」——
      // 那句话与眼前的控件自相矛盾。
      settingsView = h("div");
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
  // 高危能力使用记录：让用户看得见这个插件到底用了什么、用了多少次。
  // 权限开关只回答「能不能用」，回答不了「用了没有」——出了事无从追溯。
  const auditSection = await buildAuditSection(plugin);
  const extras = [autostartSection, auditSection].filter(Boolean) as HTMLElement[];
  if (extras.length > 0) {
    settingsView = h("div", {}, ...extras, settingsView);
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


/** 「随 iTools 启动」开关（仅清单声明了 background 的插件才会调到这里）。 */
function buildAutostartSection(plugin: PluginInfo, ctx: AdminCtx): HTMLElement {
  const sw = makeSwitch(plugin.background_enabled, async (checked) => {
    try {
      await api.setPluginBackground(plugin.name, checked);
      // 就地更新，避免用户切回列表再进来时看到旧状态
      plugin.background_enabled = checked;
      ctx.toast(checked ? "已开启：iTools 启动时自动运行本插件" : "已关闭自动启动");
    } catch (err) {
      console.error("set_plugin_background failed", err);
      // 失败要把开关拨回去——留在「已开启」的样子上就是骗用户
      const input = sw.querySelector("input");
      if (input) input.checked = plugin.background_enabled;
      ctx.toast(errText(err, "操作失败"));
    }
  });
  return h(
    "div",
    { class: "settings-group" },
    h("div", { class: "settings-group-title", text: "启动" }),
    h(
      "div",
      { class: "settings-item" },
      h(
        "div",
        { class: "settings-item-head" },
        h("div", { class: "settings-item-label", text: "随 iTools 启动" }),
      ),
      h("div", { class: "settings-item-control" }, sw),
      h("div", {
        class: "settings-item-desc",
        text: "在后台运行本插件，使它无需先被唤起就能注册全局快捷键、持续监听。关闭后其快捷键随之失效。",
      }),
    ),
  );
}


/** 「最近的高危能力使用」区块；没有记录则返回 null（不显示空区块）。 */
async function buildAuditSection(plugin: PluginInfo): Promise<HTMLElement | null> {
  let log: AuditEntry[];
  try {
    log = await api.pluginAuditLog(plugin.name, 50);
  } catch (err) {
    console.error("plugin_audit_log failed", err);
    return null;
  }
  if (log.length === 0) return null;

  const fmtTime = (secs: number): string => {
    const d = new Date(secs * 1000);
    const p2 = (n: number): string => String(n).padStart(2, "0");
    return `${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`;
  };

  const rows = log.map((e) => {
    const label = permLabel(e.permission);
    const times = e.count > 1 ? ` × ${e.count}` : "";
    // 被拒的单独标出来：插件反复尝试未授权能力，正是最该被看见的
    const status = e.granted ? "" : "（被拒绝）";
    const dev = e.dev ? "［调试］" : "";
    return h("div", {
      class: e.granted ? "settings-item-desc" : "settings-item-desc audit-denied",
      text: `${fmtTime(e.lastAt)}  ${dev}${label}${times}${status}`,
    });
  });

  return h(
    "div",
    { class: "settings-group" },
    h("div", { class: "settings-group-title", text: "最近的高危能力使用" }),
    h("div", { class: "settings-item" }, ...rows),
    h("div", {
      class: "settings-item-desc",
      text: "仅保留本次运行期间的记录，重启 iTools 后清空。同一能力的连续调用会合并计数。",
    }),
  );
}
