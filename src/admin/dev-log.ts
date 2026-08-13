//! 开发者中心「日志」tab：调试插件的 API 调用 / console 输出 / 未捕获异常实时流。
//!
//! 诚信约束（doc/开发准则.md）：
//! - 表格里的每一条都来自后端 `dev_log_list` 的真实返回，前端不生成、不补齐、不猜测任何一条；
//! - 探针的**覆盖范围有盲区**（只能抓到经 `window.itools.*` 门面的调用），面板必须写明，
//!   否则「日志里没有」会被误读成「插件没调用」；
//! - 拉取失败如实显示后端原文，并降速重试；绝不静默吞掉、也绝不把「拉不到」渲染成「没有日志」。
//!
//! 取数策略：**定时增量拉取为主，事件为辅**。
//! 事件只是让新日志更快出现；即使事件名对不上（或后端根本不 emit），轮询也会把日志补齐。

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { DevLogEntry, DevPluginInfo } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { errText } from "./plugin-install";

/** 后端推送「有新日志」的事件名候选。
 *
 *  ⚠ 确切事件名以探针侧的产出为准。这里登记候选：收到其中任意一个就立刻拉一次增量。
 *  一个都没匹配上也**不会丢日志** —— 下面的定时增量拉取才是保证，事件只是加速器。
 *  （所以本文件绝不能改成「只靠事件」。） */
const LOG_EVENTS = ["dev-log", "dev-log-changed", "dev-logs-changed"] as const;

/** 正常轮询间隔（ms）。这是开发者工具，实时性优先。 */
const POLL_MS = 800;
/** 拉取失败后的退避间隔（ms）：既不刷屏报错，也不放弃自愈。 */
const POLL_SLOW_MS = 5000;
/** 面板最多保留的日志条数（超出丢弃最旧的）。这是**前端**的上限，与后端缓冲区上限无关。 */
const MAX_ROWS = 2000;

const IC_TRASH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 13a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-13"/></svg>';

/** kind → 展示文案；认不出的 kind **原样显示**（不隐瞒后端产出的新类型）。 */
const KIND_LABEL: Record<string, string> = {
  api: "API",
  console: "console",
  error: "异常",
};

/** 把 RFC3339 时间格式化成 `HH:MM:SS.mmm`；解析不了就原样返回（不编造时间）。 */
function timeText(at: string): string {
  const d = new Date(at);
  if (Number.isNaN(d.getTime())) return at;
  const p = (n: number, w = 2): string => String(n).padStart(w, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

/** JSON 文本美化；不是合法 JSON（例如后端已截断）就原样返回，内容一字不改。 */
function pretty(s: string): string {
  if (!s) return "";
  try {
    return JSON.stringify(JSON.parse(s), null, 2);
  } catch {
    return s;
  }
}

export interface DevLogViewOpts {
  /** 取当前调试插件列表：用于「按插件过滤」下拉，以及把 pluginId 显示成插件名。 */
  getPlugins: () => DevPluginInfo[];
  toast: (msg: string) => void;
}

export interface DevLogView {
  /** 面板元素。切 tab 时从 DOM 摘下/挂回即可，**轮询不会停**（避免后端缓冲区滚掉未读日志）。 */
  el: HTMLElement;
  /** 插件列表变化后刷新「按插件过滤」下拉。 */
  refreshPlugins: () => void;
  /** 停止轮询并解绑事件监听。离开开发者中心时必须调用。 */
  dispose: () => void;
}

/** 创建日志视图（创建即开始轮询）。 */
export function createDevLogView(opts: DevLogViewOpts): DevLogView {
  /** 已拉到的日志（升序）。 */
  const rows: DevLogEntry[] = [];
  /** 已拉到的最大 seq，增量拉取的游标。 */
  let lastSeq = 0;
  let disposed = false;
  let inflight = false;
  let timer: number | undefined;
  const unlistens: UnlistenFn[] = [];

  /** 过滤条件 */
  let failOnly = false;
  let kindFilter = "all";
  let pluginFilter = "all";
  /** 新日志到达时是否自动滚到底 */
  let autoScroll = true;

  // ---------- 头部：过滤与操作 ----------
  const failBox = h("input", { type: "checkbox" });
  failBox.addEventListener("change", () => {
    failOnly = failBox.checked;
    applyFilter();
  });

  const kindSel = h("select", { class: "select dev-select-sm" });
  for (const [value, label] of [
    ["all", "全部类型"],
    ["api", "只看 API 调用"],
    ["console", "只看 console"],
    ["error", "只看未捕获异常"],
  ] as const) {
    kindSel.appendChild(h("option", { value, text: label }));
  }
  kindSel.addEventListener("change", () => {
    kindFilter = kindSel.value;
    applyFilter();
  });

  const pluginSel = h("select", { class: "select dev-select-sm" });
  pluginSel.addEventListener("change", () => {
    pluginFilter = pluginSel.value;
    applyFilter();
  });

  const scrollBtn = h("button", { class: "btn btn-sm", text: "暂停滚动" });
  scrollBtn.addEventListener("click", () => {
    autoScroll = !autoScroll;
    scrollBtn.textContent = autoScroll ? "暂停滚动" : "恢复滚动";
    scrollBtn.classList.toggle("dev-btn-on", !autoScroll);
    if (autoScroll) scrollToBottom();
  });

  const clearBtn = h("button", { class: "icon-btn", html: IC_TRASH, title: "清空日志（后端缓冲区一并清空）" });
  clearBtn.addEventListener("click", async () => {
    try {
      await api.devLogClear();
    } catch (err) {
      console.error("dev_log_clear failed", err);
      opts.toast(errText(err, "清空日志失败"));
      return;
    }
    rows.length = 0;
    // 后端 seq 是否重置未知：无论重置与否，从 0 重新拉都是正确的（拉到的就是清空后还在的那些）
    lastSeq = 0;
    listEl.innerHTML = "";
    updateEmpty();
    opts.toast("已清空调试日志");
  });

  const countEl = h("span", { class: "dev-log-count", text: "0 条" });

  const head = h(
    "div",
    { class: "dev-log-head" },
    h("label", { class: "dev-check" }, failBox, h("span", { text: "只看失败" })),
    kindSel,
    pluginSel,
    h("span", { class: "dev-log-head-gap" }),
    countEl,
    scrollBtn,
    clearBtn,
  );

  // ---------- 采集状态（诚实标注拉取失败） ----------
  const statusEl = h("div", { class: "dev-log-status", text: "" });
  statusEl.hidden = true;
  function setStatus(msg: string | null): void {
    if (!msg) {
      statusEl.hidden = true;
      statusEl.textContent = "";
      return;
    }
    statusEl.hidden = false;
    statusEl.textContent = msg;
  }

  // ---------- 列表 ----------
  const listEl = h("div", { class: "dev-log-list" });
  const emptyEl = h("div", { class: "dev-log-empty", text: "" });

  function updateEmpty(): void {
    const shown = rows.length ? listEl.querySelectorAll(".dev-log-item:not([hidden])").length : 0;
    countEl.textContent = rows.length === shown ? `${rows.length} 条` : `${shown} / ${rows.length} 条`;
    listEl.hidden = shown === 0; // 一条都不显示时连空表框一起收起，只留下面的说明
    if (!rows.length) {
      emptyEl.hidden = false;
      emptyEl.textContent =
        "还没有日志。在「运行」tab 里启动一个调试插件，它经 window.itools.* 发出的调用、console 输出与未捕获异常会实时出现在这里。";
      return;
    }
    if (!shown) {
      emptyEl.hidden = false;
      emptyEl.textContent = "当前过滤条件下没有日志（共 " + rows.length + " 条被过滤掉了）。";
      return;
    }
    emptyEl.hidden = true;
  }

  function matches(e: DevLogEntry): boolean {
    if (failOnly && e.ok) return false;
    if (kindFilter !== "all" && e.kind !== kindFilter) return false;
    if (pluginFilter !== "all" && e.pluginId !== pluginFilter) return false;
    return true;
  }

  function applyFilter(): void {
    const items = listEl.children;
    for (let i = 0; i < items.length; i++) {
      const el = items[i] as HTMLElement;
      const idx = Number(el.dataset.idx);
      const e = rows[idx];
      el.hidden = !e || !matches(e);
    }
    updateEmpty();
  }

  /** 插件 id → 展示名（列表里查不到就显示 id 本身，不编造名字）。 */
  function pluginName(id: string): string {
    const p = opts.getPlugins().find((x) => x.id === id);
    return p ? p.name || p.id : id;
  }

  function buildItem(e: DevLogEntry, idx: number): HTMLElement {
    const isApi = e.kind === "api";
    const stateCls = e.ok ? "dev-log-ok" : "dev-log-fail";
    const row = h(
      "div",
      { class: `dev-log-row ${stateCls}` },
      h("span", { class: "dev-log-time", text: timeText(e.at) }),
      h("span", { class: "dev-log-plugin", text: pluginName(e.pluginId), title: e.pluginId }),
      h("span", { class: `dev-log-kind dev-log-kind-${e.kind}`, text: KIND_LABEL[e.kind] ?? e.kind }),
      h("span", { class: `dev-log-method dev-log-lv-${e.level}`, text: e.method || "—", title: e.method }),
      // ms 只有 api 类型才有真实耗时（console/异常恒 0），不拿 0 冒充「0 毫秒」
      h("span", { class: "dev-log-ms", text: isApi ? `${e.ms.toFixed(1)} ms` : "—" }),
      h("span", { class: "dev-log-state", text: e.ok ? "成功" : "失败" }),
    );

    const item = h("div", { class: "dev-log-item", dataset: { idx: String(idx) } }, row);

    let detail: HTMLElement | null = null;
    row.addEventListener("click", () => {
      if (detail) {
        detail.remove();
        detail = null;
        row.classList.remove("expanded");
        return;
      }
      detail = h(
        "div",
        { class: "dev-log-detail" },
        h("div", { class: "dev-log-detail-label", text: "参数 args" }),
        h("pre", { class: "dev-log-pre", text: pretty(e.args) || "（空）" }),
        h("div", { class: "dev-log-detail-label", text: e.ok ? "返回 result" : "错误 result" }),
        h("pre", { class: `dev-log-pre ${e.ok ? "" : "dev-log-pre-fail"}`, text: pretty(e.result) || "（空）" }),
        h("div", {
          class: "dev-log-detail-meta",
          text: `seq ${e.seq}　·　插件 ${e.pluginId}　·　级别 ${e.level}　·　时间 ${e.at}`,
        }),
      );
      item.appendChild(detail);
      row.classList.add("expanded");
    });
    return item;
  }

  function atBottom(): boolean {
    return listEl.scrollTop + listEl.clientHeight >= listEl.scrollHeight - 24;
  }
  function scrollToBottom(): void {
    listEl.scrollTop = listEl.scrollHeight;
  }

  function push(entries: DevLogEntry[]): void {
    const stick = autoScroll && atBottom();
    for (const e of entries) {
      const idx = rows.length;
      rows.push(e);
      if (e.seq > lastSeq) lastSeq = e.seq;
      const item = buildItem(e, idx);
      item.hidden = !matches(e);
      listEl.appendChild(item);
    }
    // 超出上限：丢最旧的。rows 与 DOM 用 data-idx 关联，裁剪后需要整体重排索引
    if (rows.length > MAX_ROWS) {
      const drop = rows.length - MAX_ROWS;
      rows.splice(0, drop);
      for (let i = 0; i < drop && listEl.firstChild; i++) listEl.removeChild(listEl.firstChild);
      const items = listEl.children;
      for (let i = 0; i < items.length; i++) (items[i] as HTMLElement).dataset.idx = String(i);
    }
    updateEmpty();
    if (stick) scrollToBottom();
  }

  // ---------- 取数 ----------
  /** 拉一次增量。返回本次是否成功（决定下一次的间隔）。 */
  async function pull(): Promise<boolean> {
    if (disposed || inflight) return true;
    inflight = true;
    try {
      const got = await api.devLogList(lastSeq);
      if (disposed) return true;
      if (got.length) push(got);
      setStatus(null);
      return true;
    } catch (err) {
      console.error("dev_log_list failed", err);
      if (!disposed) {
        setStatus(
          `日志拉取失败：${errText(err, "未知错误")}（已降到 ${POLL_SLOW_MS / 1000}s 重试一次，下面显示的是失败前拿到的内容）`,
        );
      }
      return false;
    } finally {
      inflight = false;
    }
  }

  function schedule(ms: number): void {
    window.clearTimeout(timer);
    if (disposed) return;
    timer = window.setTimeout(() => void tick(), ms);
  }

  async function tick(): Promise<void> {
    if (disposed) return;
    // 管理中心窗口被隐藏/最小化时不打扰后端（关窗只 hide 不销毁，本视图仍挂着）。
    // 恢复可见后照常增量拉取，后端缓冲区里还在的部分会被补齐。
    if (document.visibilityState === "hidden") {
      schedule(POLL_MS);
      return;
    }
    const ok = await pull();
    schedule(ok ? POLL_MS : POLL_SLOW_MS);
  }

  const onVisible = (): void => {
    if (document.visibilityState === "visible") void pull();
  };
  document.addEventListener("visibilitychange", onVisible);

  for (const name of LOG_EVENTS) {
    void listen(name, () => void pull())
      .then((un) => {
        if (disposed) void un();
        else unlistens.push(un);
      })
      .catch((err) => console.error(`listen ${name} failed`, err));
  }

  // ---------- 说明（探针盲区必须写明） ----------
  const note = h(
    "div",
    { class: "dev-note" },
    h("div", { class: "dev-note-title", text: "这里能抓到什么、抓不到什么" }),
    h("div", {
      text: "探针挂在插件桥 bridge.js 的 invoke() 单点上，只覆盖经 window.itools.* 门面发出的调用。插件若绕过门面、直接用 window.__TAURI_INTERNALS__.invoke() 调命令，这里不会有记录 —— 那是「抓不到」，不是「没调用」。",
    }),
    h("div", {
      text: `console 输出与未捕获异常同样只覆盖调试插件窗口内的页面上下文。日志由后端缓冲，本面板每 ${POLL_MS} 毫秒增量拉取一次（收到后端推送则立即拉）；管理中心窗口隐藏期间暂停拉取，恢复后补齐后端缓冲区里还在的部分。面板最多保留最近 ${MAX_ROWS} 条。`,
    }),
  );

  const el = h("div", { class: "dev-log" }, note, head, statusEl, listEl, emptyEl);

  function refreshPlugins(): void {
    const list = opts.getPlugins();
    const keep = pluginFilter;
    pluginSel.innerHTML = "";
    pluginSel.appendChild(h("option", { value: "all", text: "全部插件" }));
    for (const p of list) {
      pluginSel.appendChild(h("option", { value: p.id, text: p.name || p.id }));
    }
    // 之前选中的插件已经不在列表里：回落到「全部」，但把这件事反映到过滤结果上
    const exists = keep === "all" || list.some((p) => p.id === keep);
    pluginFilter = exists ? keep : "all";
    pluginSel.value = pluginFilter;
    applyFilter();
  }

  refreshPlugins();
  updateEmpty();
  void pull().then((ok) => schedule(ok ? POLL_MS : POLL_SLOW_MS));

  return {
    el,
    refreshPlugins,
    dispose: () => {
      disposed = true;
      window.clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
      for (const un of unlistens.splice(0)) {
        try {
          un();
        } catch (err) {
          console.error("unlisten dev log failed", err);
        }
      }
    },
  };
}
