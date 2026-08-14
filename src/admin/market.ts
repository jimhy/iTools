//! 插件市场页：列出索引里的插件，一键安装。
//!
//! # 这一页的信任模型（文案必须与它一致）
//!
//! 索引与插件包都来自**你配置的那台服务器**（`GET /api/market/index` 与
//! `/api/market/package/{name}`），不再经过 GitHub。条目带着发布时算出的内容哈希，
//! 客户端下载后逐字节校验，对不上一律拒装——这挡的是「传输途中被改过」。
//!
//! 审核由服务端的**大模型**做：读代码判断有无恶意行为、权限是否名副其实、描述是否相符。
//! 但这不等于人工审计，页面文案必须如实区分：
//!
//! - `reviewMode = "llm"` → 「由 <模型名> 自动审核」；
//! - `reviewMode = "manual"` → 服务端**没有接入**自动审核，条目是维护者人工放行的；
//! - 空 → 索引没说（老服务端），一律按「审核方式未知」措辞。
//!
//! 不能让「市场」两个字自动被理解成「官方背书、可以放心」。

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

const fmtSize = (n: number): string =>
  n >= 1024 * 1024 ? `${(n / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;

/** Unix 毫秒 → 本地日期。0 / 非法值返回空串（宁可不显示，也不显示 1970-01-01）。 */
const fmtDate = (ms: number): string => {
  if (!ms || !Number.isFinite(ms)) return "";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "" : d.toLocaleDateString();
};

export async function renderMarket(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  const listWrap = h("div", { class: "plugin-list" });
  // 信任说明的内容取决于服务端说了什么审核方式，所以要能随索引重绘
  const trustWrap = h("div");
  let view: MarketView | null = null;
  let loading = false;

  const refreshBtn = h("button", { class: "icon-btn", title: "重新拉取市场索引", html: REFRESH });

  function statusBlock(v: MarketView): HTMLElement | null {
    // ⚠ origin="cache" 有两种截然不同的情况，不能用同一句话糊过去：
    //   ① error 非空 → 真的拉失败了，在用旧数据兜底（要如实说明原因）；
    //   ② error 为空 → 缓存还在新鲜期内，**这是正常的**（本来就不该每次都请求）。
    // 把 ② 也写成「没能拉到最新」，就是把正常行为渲染成故障——那是假信息。
    if (v.origin === "cache" && v.error) {
      return h(
        "div",
        { class: "info-box" },
        h("div", { text: "没能拉到最新的市场索引，当前展示的是上次缓存的内容。" }),
        h("div", { class: "info-box-detail", text: `失败原因：${v.error}` }),
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
  function trustBlock(v: MarketView | null): HTMLElement {
    // 审核方式必须照服务端说的写。写死「已由 AI 审核」而服务端其实没接模型，就是假信息。
    const reviewLine =
      v?.reviewMode === "llm"
        ? `② 每个版本上架前都由 ${v.reviewModel || "服务端配置的模型"} 通读代码审核（恶意行为、` +
          "权限是否名副其实、描述是否相符）。但这是**自动**审核，不是人工审计，也不可能杜绝一切问题——" +
          "装之前仍请自行判断来源是否可信。"
        : v?.reviewMode === "manual"
          ? "② 当前服务端**没有接入自动代码审核**，上架的条目只过了机械校验（格式、清单、不含可执行文件），" +
            "代码是否有恶意行为由维护者人工判断。「在市场里」不等于「代码被审计过」。"
          : "② 这个服务端没有说明它用的是哪种审核方式，因此无法确认上架的插件被审到什么程度。" +
            "请把市场里的插件当作「来源未经核实」来对待。";

    return h(
      "div",
      { class: "info-box" },
      h("div", { text: "关于市场里的插件" }),
      h("div", {
        class: "info-box-detail",
        text:
          "① 索引与插件包都来自你配置的那台服务器。条目带有发布时算出的内容哈希，" +
          "客户端下载后逐字节校验，对不上一律拒绝安装。",
      }),
      h("div", { class: "info-box-detail", text: reviewLine }),
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
    if (e.sizeBytes) bits.push(fmtSize(e.sizeBytes));
    if (e.updatedAt) bits.push(`更新于 ${fmtDate(e.updatedAt)}`);
    // 审核方式逐条目标注：同一个市场里可能混着「模型审过的」和「早期人工放行的」
    if (e.reviewedBy) bits.push(`${e.reviewedBy} 审核`);
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
        onInstalled: () => void load(true),
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
    trustWrap.replaceChildren(trustBlock(view));
    freshEl.textContent = freshText(view);
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
                ? "服务器上还没有上架任何插件。你可以在「插件管理 → 安装插件」里粘贴 Git 仓库地址手动安装，" +
                  "或在「开发者中心」把自己的插件提交审核。"
                : "拉不到索引，见上方原因。",
          }),
        ),
      );
      return;
    }
    for (const e of view.plugins) listWrap.appendChild(card(e, view.installed[e.name]));
  }

  /** 加载策略（stale-while-revalidate）：
   *  1. 先要缓存（force=false）—— 有就立刻渲染，不转圈、不发请求；
   *  2. 若后端标了 stale（超过 1 小时没更新），再**后台**拉一次并重绘；
   *  3. 点刷新按钮 = 显式意图，直接 force=true。
   *  于是常见路径（一小时内反复进市场页）零请求、瞬开，而数据该新的时候会自己变新。 */
  async function load(force = false): Promise<void> {
    if (loading) return;
    loading = true;
    refreshBtn.disabled = true;
    try {
      view = await api.marketList(force);
    } catch (err) {
      // market_list 正常情况下不会 reject（失败也带缓存返回），走到这里说明命令本身出了问题
      console.error("market_list failed", err);
      ctx.toast(errText(err, "市场加载失败"));
      view = {
        plugins: [],
        origin: "none",
        error: errText(err, "命令调用失败"),
        installed: {},
        source: "",
        reviewMode: "",
        reviewModel: "",
        fetchedAt: 0,
        stale: true,
      };
    } finally {
      loading = false;
      refreshBtn.disabled = false;
      rerender();
    }
    // 缓存过期 → 后台静默更新一次再重绘。不 await：让用户先看到内容。
    // 只在非强制路径触发，避免「强制拉完仍标 stale」时无限递归。
    if (!force && view?.stale) void load(true);
  }

  refreshBtn.addEventListener("click", () => void load(true));

  // 数据新鲜度：让用户一眼知道自己看的是不是新的（缓存命中时尤其重要）
  const freshEl = h("span", { class: "launch-subhead-sub", text: "" });
  const subhead = h(
    "div",
    { class: "launch-subhead" },
    h("span", { class: "launch-subhead-title", text: "插件市场" }),
    freshEl,
    h("div", { class: "launch-actions" }, refreshBtn),
  );

  /** 「刚刚 / N 分钟前 / N 小时前 / 具体日期」——0 或未知一律不显示，不编造时间。 */
  function freshText(v: MarketView | null): string {
    if (!v || !v.fetchedAt) return "";
    const diff = Date.now() - v.fetchedAt;
    if (diff < 0) return "";
    const mins = Math.floor(diff / 60000);
    const when =
      mins < 1 ? "刚刚更新" : mins < 60 ? `${mins} 分钟前更新` : mins < 1440 ? `${Math.floor(mins / 60)} 小时前更新` : fmtDate(v.fetchedAt) + " 更新";
    // 后台正在拉新数据时如实说一声，免得用户以为界面卡住了
    return v.stale ? `${when}，正在检查更新…` : when;
  }

  rerender();
  root.append(h("div", { class: "launch-scroll" }, trustWrap, subhead, listWrap));
  await load();

  // 索引来源展示在最后：多数用户不关心，但自建/联调时必须能一眼看出连的是哪个市场
  const srcLine = h("div", { class: "launch-intro", text: "" });
  root.querySelector(".launch-scroll")?.appendChild(srcLine);
  if (view) {
    srcLine.textContent =
      `市场服务器：${(view as MarketView).source}（在「设置 → 网络 → 服务器地址」里更改）`;
  }
}
