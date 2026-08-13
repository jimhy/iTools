//! 我的数据：展示每个数据集（主程序 + 各插件）在本地与云端各存了多少条记录。
//!
//! 诚信约束（doc/开发准则.md 第 6、7 条）：
//! - 本地条数来自本机 SQLite 真实统计；云端条数/容量来自服务端 `GET /data/_usage` 真实查询。
//! - 云端未接入 / 未登录 / 离线时，**如实标注状态**，绝不用本地数字冒充云端，容量绝不硬编码。
//! - 「立即同步」仅在云端可用（已登录）时出现，复用真实的 sync_now，不做无效控件。

import type { AdminCtx } from "./main";
import type { PluginInfo, DataUsage } from "../types";
import { h } from "./ui";
import * as api from "./api";

// ---------- 图标 ----------
const IC_LOCAL =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/></svg>';
const IC_CLOUD =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M17.5 19a4.5 4.5 0 0 0 .5-9 6 6 0 0 0-11.5-1.5A4 4 0 0 0 6.5 19z"/></svg>';
const IC_REFRESH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>';
const IC_APP =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/></svg>';
const IC_PLUGIN =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';

/** 云端不可用原因 → 给用户看的一句话（诚实说明，不含歧义）。 */
function cloudReasonText(reason: string | undefined): string {
  switch (reason) {
    case "cloud_not_configured":
      return "云端未接入";
    case "not_logged_in":
      return "未登录";
    case "offline":
      return "无法连接云端";
    case "session_expired":
      return "会话已失效，请重新登录";
    default:
      return "云端读取失败";
  }
}

/** 字节数格式化为人类可读（真实值，B/KB/MB/GB）。 */
function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}

/** 一个数据集行的合并模型。 */
interface Row {
  ns: string;
  name: string;
  /** 插件 logo（data URL）；主程序 / 无 logo 时为 null，用内置字形。 */
  logo: string | null;
  /** 该数据集是否为「已安装的核心/插件」（否 = 云端残留或已卸载）。 */
  present: boolean;
  glyph: string;
  local: number;
  /** 云端条数；云端不可用时为 null。 */
  cloud: number | null;
}

export async function renderData(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let plugins: PluginInfo[];
  let usage: DataUsage;
  try {
    [plugins, usage] = await Promise.all([api.listPlugins(), api.dataUsage()]);
  } catch (err) {
    console.error("load data usage failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "数据用量加载失败" }));
    return;
  }

  /** 把 usage + plugins 合并成有序行集合。 */
  function buildRows(): Row[] {
    const localMap = new Map<string, number>();
    usage.local.forEach((c) => localMap.set(c.ns, c.count));
    const cloudMap = new Map<string, number>();
    const cloudOk = usage.cloud != null;
    usage.cloud?.counts.forEach((c) => cloudMap.set(c.ns, c.count));

    const rows: Row[] = [];
    const taken = new Set<string>();
    const pushRow = (ns: string, name: string, logo: string | null, present: boolean, glyph: string): void => {
      taken.add(ns);
      rows.push({
        ns,
        name,
        logo,
        present,
        glyph,
        local: localMap.get(ns) ?? 0,
        cloud: cloudOk ? cloudMap.get(ns) ?? 0 : null,
      });
    };

    // 1) 主程序永远第一（即便 0 条也展示）
    pushRow("app", "主程序", null, true, IC_APP);
    // 2) 已安装插件按列表顺序（即便 0 条也展示，和示意图一致）
    for (const p of plugins) {
      pushRow(`plugin:${p.name}`, p.name, p.logo, true, IC_PLUGIN);
    }
    // 3) 云端 / 本地存在但已卸载的命名空间（残留数据）——诚实展示，按 ns 升序
    const orphans = new Set<string>();
    localMap.forEach((_, ns) => !taken.has(ns) && orphans.add(ns));
    cloudMap.forEach((_, ns) => !taken.has(ns) && orphans.add(ns));
    Array.from(orphans)
      .sort()
      .forEach((ns) => {
        const name = ns.startsWith("plugin:") ? ns.slice("plugin:".length) : ns;
        pushRow(ns, `${name}（未安装）`, null, false, IC_PLUGIN);
      });
    return rows;
  }

  // ---------- 顶部三栏汇总 ----------
  function summaryCard(): HTMLElement {
    const cloudOk = usage.cloud != null;

    // 左：本地
    const localCol = h(
      "div",
      { class: "data-sum-col" },
      h("span", { class: "data-sum-icon", html: IC_LOCAL }),
      h("div", { class: "data-sum-num", text: String(usage.localTotal) }),
      h("div", { class: "data-sum-label", text: "本地记录（条）" }),
    );

    // 中：刷新 +（可用时）立即同步
    const refreshBtn = h("button", { class: "icon-btn", title: "刷新", html: IC_REFRESH });
    refreshBtn.addEventListener("click", () => void refresh());
    const midChildren: HTMLElement[] = [refreshBtn];
    if (cloudOk) {
      const syncBtn = h("button", { class: "btn btn-primary btn-sm", text: "立即同步" });
      syncBtn.addEventListener("click", async () => {
        syncBtn.disabled = true;
        syncBtn.textContent = "同步中…";
        try {
          const r = await api.syncNow();
          ctx.toast(
            r.synced ? `已同步（上行 ${r.pushed} 条，下行 ${r.pulled} 条）` : r.message || "同步未完成",
          );
          await refresh(); // 同步后云端数可能变化，重新拉取
        } catch (err) {
          console.error("sync_now failed", err);
          ctx.toast("同步失败");
        } finally {
          syncBtn.disabled = false;
          syncBtn.textContent = "立即同步";
        }
      });
      midChildren.push(syncBtn);
    }
    const midCol = h("div", { class: "data-sum-col data-sum-mid" }, ...midChildren);

    // 右：云端（可用显示条数+容量；否则如实标注状态）
    const rightChildren: HTMLElement[] = [h("span", { class: "data-sum-icon", html: IC_CLOUD })];
    if (cloudOk && usage.cloud) {
      rightChildren.push(
        h("div", { class: "data-sum-num", text: String(usage.cloud.total) }),
        h("div", {
          class: "data-sum-label",
          text: `云端记录（条）· 占用 ${formatBytes(usage.cloud.bytes)}`,
        }),
      );
    } else {
      rightChildren.push(
        h("div", { class: "data-sum-num data-sum-num-muted", text: "—" }),
        h("div", { class: "data-sum-label", text: `云端：${cloudReasonText(usage.cloudReason)}` }),
      );
    }
    const rightCol = h("div", { class: "data-sum-col" }, ...rightChildren);

    return h("div", { class: "data-summary card" }, localCol, midCol, rightCol);
  }

  // ---------- 数据集列表 ----------
  function logoEl(row: Row): HTMLElement {
    if (row.logo) return h("img", { class: "data-logo", src: row.logo, alt: "" });
    return h("span", { class: "data-logo data-logo-fallback", html: row.glyph });
  }

  function rowEl(row: Row): HTMLElement {
    const countText =
      row.cloud == null
        ? `本地 ${row.local} 条`
        : `本地 ${row.local} 条　·　云端 ${row.cloud} 条`;
    const meta = h(
      "div",
      { class: "data-row-meta" },
      h(
        "div",
        { class: "data-row-name-line" },
        h("span", { class: "data-row-name", text: row.name }),
        row.present ? null : h("span", { class: "data-row-tag", text: "残留" }),
      ),
      h("div", { class: "data-row-counts", text: countText }),
    );
    return h("div", { class: "data-row" }, logoEl(row), meta);
  }

  function paint(): void {
    root.innerHTML = "";
    const rows = buildRows();
    const list = h("div", { class: "data-list" }, ...rows.map(rowEl));

    const notice =
      usage.cloud == null
        ? h("div", {
            class: "data-note",
            text:
              usage.cloudReason === "not_logged_in"
                ? "登录云账号后可查看并同步云端数据；当前仅显示本地记录。"
                : usage.cloudReason === "cloud_not_configured"
                  ? "尚未接入云端服务，数据仅保存在本地（可在「我的账号 → 数据同步」填写服务器地址接入）。"
                  : "暂时无法读取云端数据，下面仅为本地记录，可点右上角刷新重试。",
          })
        : null;

    const intro = h("div", {
      class: "launch-intro",
      text: "这里统计每个数据集（主程序与各插件）在本地和云端各存了多少条记录。数字均为真实统计。",
    });

    root.append(
      h(
        "div",
        { class: "launch-scroll" },
        intro,
        summaryCard(),
        notice,
        h("div", { class: "launch-subhead" }, h("span", { class: "launch-subhead-title", text: "数据集" })),
        list,
      ),
    );
  }

  /** 重新拉取用量 + 插件并重绘。 */
  async function refresh(): Promise<void> {
    try {
      [plugins, usage] = await Promise.all([api.listPlugins(), api.dataUsage()]);
    } catch (err) {
      console.error("refresh data usage failed", err);
      ctx.toast("刷新失败");
      return;
    }
    paint();
  }

  paint();
}
