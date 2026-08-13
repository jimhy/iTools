//! 开发者中心「存储」tab：查看/编辑调试插件写进**测试库**的键值。
//!
//! 这一页最重要的事是把两套存储 API 的区别讲清楚 —— 它直接决定「数据会不会上云」：
//!   itools.db.*    → 纯本地键值，永远不参与云同步；
//!   itools.data.*  → 参与云同步（正式环境里 sync 会把它推到服务器）。
//! 开发者最容易把两者搞混，把该本地的东西写进了会上云的那套。所以两张表**分开展示**，各自标注。
//!
//! 诚信约束（doc/开发准则.md）：
//! - 表里的条数/字节数/更新时间全部来自后端 `dev_storage_list` 的真实返回，不估算、不补零；
//!   `updatedAt` 为 null 时显示「—」，绝不拿当前时间冒充；
//! - 「调试数据不会上云」是**物理隔离**（独立 SQLite 文件）的结果，不是前端过滤出来的，
//!   所以敢在界面上把这句话写死；同时把测试库与沙盒的真实路径显示出来，用户可自行核对；
//! - 任何一次读写失败都原样显示后端错误，不吞、不假装成功。

import type { DevConfig, DevKv, DevPluginInfo } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { errText } from "./plugin-install";

const IC_EDIT =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20h4l10-10a2.8 2.8 0 0 0-4-4L4 16z"/><path d="M13.5 6.5l4 4"/></svg>';
const IC_DEL =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 13a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-13"/></svg>';
const IC_X =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>';

/** 两套存储的展示元数据。kind 的取值即 `dev_storage_*` 的 `kind` 参数。 */
const KINDS = [
  {
    kind: "db",
    title: "itools.db.*（纯本地）",
    // desc 走 textContent（ui.ts 的 text 分支），不渲染 Markdown：星号会原样显示成 **，别写。
    desc: "只存在本机，正式环境里也从不参与云同步。适合缓存、窗口位置、临时状态这类换台机器就该重来的数据。",
    cls: "dev-kv-local",
  },
  {
    kind: "data",
    title: "itools.data.*（参与云同步）",
    desc: "正式环境里这套数据会被 sync 推到云端服务器、并在多设备之间同步。适合用户真正要跨设备保留的数据。调试环境写在测试库里，Mock 掉了同步，不会真的上传。",
    cls: "dev-kv-cloud",
  },
] as const;

/** 字节数 → 人类可读（真实值）。 */
function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${units[i]}`;
}

/** RFC3339 → 本地时间文本；后端没给（null）或解析不了就 `—`，不编造时间。 */
function timeText(at: string | null): string {
  if (!at) return "—";
  const d = new Date(at);
  if (Number.isNaN(d.getTime())) return at;
  const p = (n: number): string => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** value 的单行预览（长值折叠成一行，展开看全文）。 */
function preview(v: string): string {
  const one = v.replace(/\s+/g, " ").trim();
  return one.length > 120 ? one.slice(0, 120) + " …" : one;
}

// ---------------------------------------------------------------- 文本弹窗

interface TextModalOpts {
  title: string;
  /** 标题下的一段说明（可空） */
  desc?: string;
  /** 文本域初值 */
  value: string;
  /** 只读（导出用） */
  readOnly?: boolean;
  /** 文本域上方的附加控件（如「新增键值」的 key 输入框） */
  head?: HTMLElement;
  /** 确认按钮文案；不给则只有「关闭」（纯查看） */
  okLabel?: string;
  /** 确认回调：抛错则把错误显示在弹窗里、不关闭 */
  onOk?: (text: string) => Promise<void>;
  /** 是否显示「复制」按钮 */
  copyable?: boolean;
  /** JSON 合法性提示：true 时实时校验并给出提醒（**不阻断保存**，见下方说明） */
  jsonHint?: boolean;
  toast: (msg: string) => void;
}

/**
 * 通用文本弹窗（编辑值 / 新增 / 导入 / 导出共用）。
 *
 * 关于 JSON 校验：只**提示**不**阻断**。存储 API 到底接不接受非 JSON 文本由后端说了算，
 * 前端擅自拦下会凭空造出一条后端并不存在的限制；真被拒绝时后端的错误会原样显示在弹窗里。
 */
function openTextModal(o: TextModalOpts): void {
  const ta = h("textarea", { class: "dev-textarea", spellcheck: "false" });
  ta.value = o.value;
  ta.readOnly = !!o.readOnly;

  const hint = h("div", { class: "dev-modal-hint", text: "" });
  hint.hidden = true;
  const syncHint = (): void => {
    if (!o.jsonHint) return;
    const t = ta.value.trim();
    if (!t) {
      hint.hidden = false;
      hint.className = "dev-modal-hint dev-hint-warn";
      hint.textContent = "值为空。";
      return;
    }
    try {
      JSON.parse(t);
      hint.hidden = false;
      hint.className = "dev-modal-hint dev-hint-ok";
      hint.textContent = "✓ 合法 JSON";
    } catch (e) {
      hint.hidden = false;
      hint.className = "dev-modal-hint dev-hint-warn";
      hint.textContent = `⚠ 不是合法 JSON（${e instanceof Error ? e.message : String(e)}）——仍可保存，但插件读取时可能解析失败。`;
    }
  };
  if (o.jsonHint) {
    ta.addEventListener("input", syncHint);
    syncHint();
  }

  const errEl = h("div", { class: "dev-modal-err", text: "" });
  errEl.hidden = true;

  const closeBtn = h("button", { class: "edit-close", html: IC_X });
  const cancelBtn = h("button", { class: "btn btn-quiet", text: o.onOk ? "取消" : "关闭" });
  const actions = h("div", { class: "modal-actions" });

  if (o.copyable) {
    const copyBtn = h("button", { class: "btn btn-sm", text: "复制" });
    copyBtn.addEventListener("click", async () => {
      const text = ta.value;
      try {
        await navigator.clipboard.writeText(text);
        o.toast("已复制到剪贴板");
        return;
      } catch {
        /* 无剪贴板权限 / 非安全上下文：走下面的兜底 */
      }
      ta.focus();
      ta.select();
      let ok = false;
      try {
        ok = document.execCommand("copy");
      } catch {
        ok = false;
      }
      o.toast(ok ? "已复制到剪贴板" : "复制失败，请用 Ctrl+A、Ctrl+C 手动复制");
    });
    actions.appendChild(copyBtn);
  }
  actions.appendChild(cancelBtn);

  if (o.okLabel && o.onOk) {
    const label = o.okLabel;
    const onOk = o.onOk;
    const okBtn = h("button", { class: "btn btn-sm btn-primary", text: label });
    okBtn.addEventListener("click", async () => {
      okBtn.disabled = true;
      okBtn.textContent = "处理中…";
      errEl.hidden = true;
      try {
        await onOk(ta.value);
        close();
      } catch (err) {
        // 后端 Err(String) 原样呈现在弹窗里（不吞成两个字，也不关闭弹窗丢掉用户输入）
        console.error("dev storage modal action failed", err);
        errEl.hidden = false;
        errEl.textContent = errText(err, "操作失败");
        okBtn.disabled = false;
        okBtn.textContent = label;
      }
    });
    actions.appendChild(okBtn);
  }

  const modal = h(
    "div",
    { class: "modal modal-dev-text" },
    h("div", { class: "modal-title-row" }, h("div", { class: "modal-title", text: o.title }), closeBtn),
    o.desc ? h("div", { class: "dev-modal-desc", text: o.desc }) : null,
    o.head ?? null,
    ta,
    hint,
    errEl,
    actions,
  );
  const mask = h("div", { class: "modal-mask" }, modal);

  function close(): void {
    document.removeEventListener("keydown", onKey, true);
    mask.remove();
  }
  const onKey = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    e.stopPropagation(); // 抢在 main.ts 的关窗监听前，只关本弹窗
    close();
  };
  closeBtn.addEventListener("click", close);
  cancelBtn.addEventListener("click", close);
  mask.addEventListener("mousedown", (e) => {
    if (e.target === mask) close();
  });
  document.addEventListener("keydown", onKey, true);

  document.body.appendChild(mask);
  // 有 key 输入框（新增键值）时先聚焦它，否则直接进文本域
  (o.head?.querySelector("input") ?? ta).focus();
}

// ---------------------------------------------------------------- 面板

export interface DevStorageOpts {
  plugin: DevPluginInfo;
  config: DevConfig;
  toast: (msg: string) => void;
}

/** 渲染「存储」tab 到 host（会先清空 host）。 */
export function renderDevStorage(host: HTMLElement, o: DevStorageOpts): void {
  const { plugin, config, toast } = o;
  host.innerHTML = "";

  // ---------- 顶部：测试库真实路径 + 隔离承诺 ----------
  const paths = h(
    "div",
    { class: "dev-note dev-note-strong" },
    h("div", { class: "dev-note-title", text: "调试数据写在哪里" }),
    h(
      "div",
      { class: "dev-path-row" },
      h("span", { class: "dev-path-label", text: "测试库" }),
      h("code", { class: "dev-path", text: config.dbPath, title: config.dbPath }),
    ),
    h(
      "div",
      { class: "dev-path-row" },
      h("span", { class: "dev-path-label", text: "沙盒文件根" }),
      h("code", { class: "dev-path", text: config.filesRoot, title: config.filesRoot }),
    ),
    h("div", {
      text: "这是一个与正式数据库分开的独立 SQLite 文件，与正式插件数据物理隔离。云同步只会遍历正式库里的命名空间，扫不到这个文件 —— 所以这里的调试数据永远不会被上传到云端，也不会污染正式安装后的数据。",
    }),
  );

  const sections = h("div", { class: "dev-kv-sections" });

  // ---------- 整体操作：清空 / 导出 / 导入 ----------
  const exportBtn = h("button", { class: "btn btn-sm", text: "导出 JSON" });
  exportBtn.addEventListener("click", async () => {
    exportBtn.disabled = true;
    try {
      const json = await api.devStorageExport(plugin.id);
      openTextModal({
        title: `导出「${plugin.name || plugin.id}」的调试数据`,
        desc: "以下是后端导出的原始 JSON，可复制保存，之后用「导入 JSON」还原。",
        value: json,
        readOnly: true,
        copyable: true,
        toast,
      });
    } catch (err) {
      console.error("dev_storage_export failed", err);
      toast(errText(err, "导出失败"));
    } finally {
      exportBtn.disabled = false;
    }
  });

  const importBtn = h("button", { class: "btn btn-sm", text: "导入 JSON" });
  importBtn.addEventListener("click", () => {
    openTextModal({
      title: `导入调试数据到「${plugin.name || plugin.id}」`,
      desc: "粘贴之前导出的 JSON。导入只影响调试测试库，不会碰正式数据。",
      value: "",
      okLabel: "导入",
      jsonHint: true,
      toast,
      onOk: async (text) => {
        await api.devStorageImport(plugin.id, text);
        toast("已导入");
        reloadAll();
      },
    });
  });

  // 清空：两步确认（先变「确认清空」，3s 内再点才真清）
  let clearPending = false;
  let clearTimer: number | undefined;
  const clearBtn = h("button", { class: "btn btn-sm btn-danger", text: "清空全部调试数据" });
  const resetClear = (): void => {
    clearPending = false;
    clearBtn.textContent = "清空全部调试数据";
  };
  clearBtn.addEventListener("click", async () => {
    if (!clearPending) {
      clearPending = true;
      clearBtn.textContent = "确认清空？（db + data）";
      clearTimer = window.setTimeout(resetClear, 3000);
      return;
    }
    window.clearTimeout(clearTimer);
    clearBtn.disabled = true;
    try {
      await api.devStorageClear(plugin.id);
      toast("已清空该插件的调试数据");
      reloadAll();
    } catch (err) {
      console.error("dev_storage_clear failed", err);
      toast(errText(err, "清空失败"));
    } finally {
      clearBtn.disabled = false;
      resetClear();
    }
  });

  const bar = h(
    "div",
    { class: "dev-kv-bar" },
    h("span", { class: "dev-kv-bar-title", text: "整体操作" }),
    h("div", { class: "dev-kv-bar-actions" }, exportBtn, importBtn, clearBtn),
  );

  host.append(paths, bar, sections);

  // ---------- 每套存储一张表 ----------
  const reloaders: Array<() => void> = [];
  function reloadAll(): void {
    reloaders.forEach((f) => f());
  }

  for (const meta of KINDS) {
    const body = h("div", { class: "dev-kv-body" });
    const countEl = h("span", { class: "dev-kv-count", text: "加载中…" });

    const addBtn = h("button", { class: "btn btn-sm", text: "新增键值" });
    addBtn.addEventListener("click", () => {
      const keyInput = h("input", { class: "field-input-sm dev-key-input", placeholder: "key" });
      openTextModal({
        title: `新增键值 · ${meta.title}`,
        desc: "写入的是调试测试库。key 已存在会被覆盖。",
        head: h("div", { class: "dev-modal-head" }, h("span", { class: "dev-modal-head-label", text: "键名" }), keyInput),
        value: "",
        okLabel: "写入",
        jsonHint: true,
        toast,
        onOk: async (text) => {
          const key = keyInput.value.trim();
          if (!key) throw new Error("键名不能为空");
          await api.devStorageSet(plugin.id, meta.kind, key, text);
          toast(`已写入 ${key}`);
          load();
        },
      });
    });

    const section = h(
      "div",
      { class: `dev-kv-sec ${meta.cls}` },
      h(
        "div",
        { class: "dev-kv-sec-head" },
        h(
          "div",
          { class: "dev-kv-sec-title-wrap" },
          h("span", { class: "dev-kv-sec-title", text: meta.title }),
          countEl,
        ),
        addBtn,
      ),
      h("div", { class: "dev-kv-sec-desc", text: meta.desc }),
      body,
    );
    sections.appendChild(section);

    function rowEl(kv: DevKv): HTMLElement {
      const editBtn = h("button", { class: "icon-btn", html: IC_EDIT, title: "编辑值" });
      editBtn.addEventListener("click", () => {
        openTextModal({
          title: `编辑 ${kv.key}`,
          desc: `${meta.title} · 写入调试测试库`,
          value: kv.value,
          okLabel: "保存",
          jsonHint: true,
          toast,
          onOk: async (text) => {
            await api.devStorageSet(plugin.id, meta.kind, kv.key, text);
            toast("已保存");
            load();
          },
        });
      });

      let delPending = false;
      let delTimer: number | undefined;
      const delBtn = h("button", { class: "icon-btn dev-del", html: IC_DEL, title: "删除这条" });
      delBtn.addEventListener("click", async () => {
        if (!delPending) {
          delPending = true;
          delBtn.classList.add("confirm");
          delBtn.textContent = "确认删除";
          delTimer = window.setTimeout(() => {
            delPending = false;
            delBtn.classList.remove("confirm");
            delBtn.innerHTML = IC_DEL;
          }, 3000);
          return;
        }
        window.clearTimeout(delTimer);
        try {
          await api.devStorageRemove(plugin.id, meta.kind, kv.key);
          toast(`已删除 ${kv.key}`);
          load();
        } catch (err) {
          console.error("dev_storage_remove failed", err);
          toast(errText(err, "删除失败"));
          delPending = false;
          delBtn.classList.remove("confirm");
          delBtn.innerHTML = IC_DEL;
        }
      });

      const valEl = h("div", { class: "dev-kv-val", text: preview(kv.value), title: "点击展开全文" });
      let expanded = false;
      valEl.addEventListener("click", () => {
        expanded = !expanded;
        valEl.classList.toggle("expanded", expanded);
        valEl.textContent = expanded ? kv.value : preview(kv.value);
      });

      return h(
        "div",
        { class: "dev-kv-row" },
        h("div", { class: "dev-kv-key", text: kv.key, title: kv.key }),
        valEl,
        h("div", { class: "dev-kv-size", text: formatBytes(kv.bytes) }),
        h("div", { class: "dev-kv-time", text: timeText(kv.updatedAt) }),
        h("div", { class: "dev-kv-acts" }, editBtn, delBtn),
      );
    }

    function load(): void {
      body.innerHTML = "";
      body.appendChild(h("div", { class: "dev-kv-loading", text: "加载中…" }));
      countEl.textContent = "加载中…";
      api
        .devStorageList(plugin.id, meta.kind)
        .then((rows) => {
          body.innerHTML = "";
          countEl.textContent = `${rows.length} 条`;
          if (!rows.length) {
            body.appendChild(
              h("div", {
                class: "dev-kv-empty",
                text: "还没有数据。插件调用对应的写入 API 后，写进去的内容会出现在这里。",
              }),
            );
            return;
          }
          const total = rows.reduce((a, b) => a + b.bytes, 0);
          countEl.textContent = `${rows.length} 条 · ${formatBytes(total)}`;
          body.append(
            h(
              "div",
              { class: "dev-kv-row dev-kv-head" },
              h("div", { class: "dev-kv-key", text: "键" }),
              h("div", { class: "dev-kv-val", text: "值（JSON）" }),
              h("div", { class: "dev-kv-size", text: "大小" }),
              h("div", { class: "dev-kv-time", text: "更新时间" }),
              h("div", { class: "dev-kv-acts", text: "" }),
            ),
            ...rows.map(rowEl),
          );
        })
        .catch((err) => {
          console.error("dev_storage_list failed", err);
          body.innerHTML = "";
          countEl.textContent = "读取失败";
          body.appendChild(
            h("div", { class: "dev-kv-error", text: "读取失败：" + errText(err, "未知错误") }),
          );
        });
    }

    reloaders.push(load);
    load();
  }
}
