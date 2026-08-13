//! 插件市场页：列出索引里的插件，一键安装。
//!
//! # 这一页的信任模型（文案必须与它一致）
//!
//! 市场收录的插件带着**审核时算出的内容哈希**，安装时逐字节校验——所以不管插件包经由
//! GitHub 官方源还是第三方镜像下载，装到的必然是收录时那份。这也是「镜像可篡改传输内容」
//! 这条风险的收口：镜像即使被控制，也只能让你装不上，不能让你装到被改过的代码。
//!
//! 但「哈希校验过」**不等于**「代码被审计过」。当前收录只做机械校验
//! （格式、清单一致性、无可执行文件），代码是否有恶意行为靠维护者人工看，
//! 原计划的 AI 自动审核尚未启用。这一点必须在页面上写清楚，不能让「市场」两个字
//! 自动被理解成「官方背书、可以放心」。

import type { AdminCtx } from "./main";
import type { MarketEntry, MarketView } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { openInstallModal } from "./plugin-install";

const REFRESH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>';

/** 高危能力名 → 展示文案（与插件管理页同一套口径）。 */
const PERM_LABELS: Record<string, string> = {
  runCommand: "执行程序",
  network: "联网",
  "screen-capture": "屏幕捕获",
  "audio-capture": "录音",
  hotkey: "全局热键",
};
const permLabel = (p: string): string => PERM_LABELS[p] ?? p;

const errText = (err: unknown, fallback: string): string =>
  typeof err === "string" ? err : err instanceof Error ? err.message : fallback;

export async function renderMarket(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  const listWrap = h("div", { class: "plugin-list" });
  let view: MarketView | null = null;
  let loading = false;

  const refreshBtn = h("button", { class: "icon-btn", title: "重新拉取市场索引", html: REFRESH });

  function statusBlock(v: MarketView): HTMLElement | null {
    // 拉取失败但有缓存可用 → 提示级；完全没数据 → 告警级。
    // 「正常降级」与「真的不可用」必须分开呈现，前者渲染成故障会误导用户。
    if (v.origin === "cache") {
      return h(
        "div",
        { class: "info-box" },
        h("div", { text: "没能拉到最新的市场索引，当前展示的是上次缓存的内容。" }),
        v.error ? h("div", { class: "info-box-detail", text: `失败原因：${v.error}` }) : null,
      );
    }
    if (v.origin === "none") {
      return h(
        "div",
        { class: "info-box info-box-warn" },
        h("div", { text: "拉不到市场索引，本地也没有缓存，所以列表是空的。" }),
        v.error ? h("div", { class: "info-box-detail", text: `失败原因：${v.error}` }) : null,
        h("div", {
          class: "info-box-detail",
          text: "这不影响手动安装：在「插件管理 → 安装插件」里粘贴仓库地址一样可以装。",
        }),
      );
    }
    return null;
  }

  /** 页面顶部的信任说明——这段不能省，也不能写成「已审核，请放心安装」。 */
  function trustBlock(): HTMLElement {
    return h(
      "div",
      { class: "info-box" },
      h("div", { text: "关于市场里的插件" }),
      h("div", {
        class: "info-box-detail",
        text:
          "① 每个插件都锁定在收录时审核的那个 commit，并带有内容哈希：安装时逐字节校验，" +
          "经由任何下载源（含第三方镜像）拿到的包只要与收录时不一致，一律拒绝安装。",
      }),
      h("div", {
        class: "info-box-detail",
        text:
          "② 但收录目前只做机械校验（格式、清单一致性、不含可执行文件），" +
          "代码是否有恶意行为由维护者人工判断，自动化的代码审核尚未启用。" +
          "「在市场里」不等于「代码被审计过」，装之前仍请自行判断来源是否可信。",
      }),
      h("div", {
        class: "info-box-detail",
        text: "③ 插件申请的高危能力在安装后一律是未授权状态，需要你到插件详情里逐项开启。",
      }),
    );
  }

  function card(e: MarketEntry, installedVersion: string | undefined): HTMLElement {
    const c = h("div", { class: "plugin-card" + (e.revoked ? " disabled" : "") });

    const nameRow = h(
      "div",
      { class: "plugin-name-row" },
      h("span", { class: "plugin-name", text: e.displayName || e.name }),
      h("span", { class: "plugin-ver", text: "v" + e.version }),
    );
    if (e.revoked) {
      nameRow.appendChild(h("span", { class: "plugin-badge plugin-badge-danger", text: "已下架" }));
    } else if (installedVersion === e.version) {
      nameRow.appendChild(h("span", { class: "plugin-badge", text: "已安装" }));
    } else if (installedVersion) {
      nameRow.appendChild(
        h("span", {
          class: "plugin-badge plugin-badge-accent",
          text: `已装 v${installedVersion}`,
          title: `本机已安装 v${installedVersion}，市场收录的是 v${e.version}`,
        }),
      );
    }

    const meta = h(
      "div",
      { class: "plugin-meta" },
      nameRow,
      h("div", { class: "plugin-desc", text: e.description || "（无描述）" }),
    );

    if (e.revoked) {
      meta.appendChild(
        h("div", {
          class: "plugin-note plugin-note-danger",
          text: `下架原因：${e.revokedReason || "未给出原因"}`,
        }),
      );
    }

    const bits: string[] = [];
    if (e.author) bits.push(`作者 ${e.author}`);
    if (e.category) bits.push(e.category);
    if (e.fileCount) bits.push(`${e.fileCount} 个文件`);
    // 明确告诉用户这是锁定版本安装，与「跟随分支」不同
    bits.push(`锁定 ${e.revision.slice(0, 7)}`);
    meta.appendChild(h("div", { class: "plugin-cmds", text: bits.join("  ·  ") }));

    if (e.keywords.length) {
      meta.appendChild(h("div", { class: "plugin-cmds", text: "关键字：" + e.keywords.join("  ·  ") }));
    }

    if (e.permissions.length) {
      const reasons = e.permissions
        .map((p) => `${permLabel(p)}：${e.permissionReasons[p] || "作者未说明用途"}`)
        .join("\n");
      meta.appendChild(
        h("div", {
          class: "plugin-note",
          text: "申请的高危能力：" + e.permissions.map(permLabel).join("、") + "（安装后默认不授权）",
          title: reasons,
        }),
      );
    }

    const installBtn = h("button", {
      class: "btn btn-primary",
      text: e.revoked ? "已下架" : installedVersion === e.version ? "重新安装" : installedVersion ? "更新" : "安装",
    });
    installBtn.disabled = e.revoked;
    if (e.revoked) installBtn.title = "该插件已被市场下架，不能从这里安装";
    installBtn.addEventListener("click", () => {
      openInstallModal({
        toast: ctx.toast,
        onInstalled: () => void load(),
        source: {
          title: `从市场安装 ${e.displayName || e.name}`,
          // 市场专用预览：会带上索引里的内容哈希做逐字节校验
          fetchPreview: () => api.marketInstallPreview(e.name),
        },
      });
    });

    c.append(meta, h("div", { class: "plugin-actions" }, installBtn));
    return c;
  }

  function rerender(): void {
    listWrap.innerHTML = "";
    if (!view) {
      listWrap.appendChild(h("div", { class: "plugin-empty" }, h("div", { class: "plugin-empty-title", text: "加载中…" })));
      return;
    }
    const s = statusBlock(view);
    if (s) listWrap.appendChild(s);

    if (!view.plugins.length) {
      listWrap.appendChild(
        h(
          "div",
          { class: "plugin-empty" },
          h("div", { class: "plugin-empty-title", text: "市场里还没有插件" }),
          h("div", {
            class: "plugin-empty-desc",
            text:
              view.origin === "remote"
                ? "索引拉取成功，但里面还没有收录任何插件。你可以在「插件管理 → 安装插件」里粘贴仓库地址手动安装。"
                : "拉不到索引，见上方原因。",
          }),
        ),
      );
      return;
    }
    for (const e of view.plugins) listWrap.appendChild(card(e, view.installed[e.name]));
  }

  async function load(): Promise<void> {
    if (loading) return;
    loading = true;
    refreshBtn.disabled = true;
    try {
      view = await api.marketList();
    } catch (err) {
      // market_list 正常情况下不会 reject（失败也带缓存返回），走到这里说明命令本身出了问题
      console.error("market_list failed", err);
      ctx.toast(errText(err, "市场加载失败"));
      view = { plugins: [], origin: "none", error: errText(err, "命令调用失败"), installed: {}, source: "" };
    } finally {
      loading = false;
      refreshBtn.disabled = false;
      rerender();
    }
  }

  refreshBtn.addEventListener("click", () => void load());

  const subhead = h(
    "div",
    { class: "launch-subhead" },
    h("span", { class: "launch-subhead-title", text: "插件市场" }),
    h("div", { class: "launch-actions" }, refreshBtn),
  );

  rerender();
  root.append(h("div", { class: "launch-scroll" }, trustBlock(), subhead, listWrap));
  await load();

  // 索引来源展示在最后：多数用户不关心，但自建/联调时必须能一眼看出连的是哪个市场
  const srcLine = h("div", { class: "launch-intro", text: "" });
  root.querySelector(".launch-scroll")?.appendChild(srcLine);
  if (view) srcLine.textContent = `索引来源：${(view as MarketView).source}（可用环境变量 ITOOLS_REGISTRY 覆盖）`;
}
