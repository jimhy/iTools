//! 插件管理面板：列出已装插件，启用/禁用（是否参与主搜索）、删除、重新加载，
//! 以及「从 Git 安装」与「检查更新 → 一键更新」闭环。
//!
//! 诚信约束（doc/开发准则.md）：更新状态徽标严格区分「已是最新 / 有新版 / 检查失败 / 已锁定 /
//! 内置 / 本地安装（非 Git 来源，无法自动更新）」——**没有真的查过就绝不显示「已是最新」**；
//! 检查失败如实显示原因（title 悬浮），不静默当作最新。

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { AdminCtx } from "./main";
import type { PluginInfo, PluginUpdate } from "../types";
import { h, makeSwitch, confirmDialog, panelError } from "./ui";
import * as api from "./api";
import { renderPluginDetail } from "./plugin-detail";
import {
  openInstallModal,
  permLabel,
  errText,
  sourceLabel,
  revisionLabel,
  isCommitSha,
} from "./plugin-install";
import { openMirrorModal } from "./plugin-mirror";

const PLUGIN_GLYPH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';
const TRASH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2M6 7l1 13a1 1 0 0 0 1 1h8a1 1 0 0 0 1-1l1-13"/></svg>';
const REFRESH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>';
const LINK =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/></svg>';

const SOURCE_IC =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12"/><path d="M8 11l4 4 4-4"/><path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"/></svg>';

/** 插件名 → 最近一次更新检查结果（没有条目 = 还没查过这个插件）。
 *  提到模块级：「进详情 → 返回」会重新执行 renderPlugins，若结果随函数作用域一起丢弃，
 *  每来回一次就得重新联网全量检查一遍（见 AUTO_CHECK_INTERVAL_MS 的说明）。 */
const updates = new Map<string, PluginUpdate>();

/** 插件名 → 最近一次操作（更新）失败的后端原文。
 *  toast 只停 1.8s 且会被后续提示顶掉，而「DNS 失败 / 超时 / 403 限流 / 404 / 校验失败」正是
 *  用户自查网络问题的关键信息——必须在卡片上留痕，直到下一次该插件的操作成功。 */
const actionErrors = new Map<string, string>();

/** 插件名 → 最近一次**成功更新**的下载来源留痕（如「本次更新经镜像 gh-proxy.com 下载」）。
 *  后端 `PluginUpdate.downloadSource / viaMirror` 已如实回传本次实际用的源，
 *  toast 一闪而过留不住——经第三方镜像装进来的代码，用户有权在卡片上看到这件事。 */
const updateSources = new Map<string, string>();

/** 「本次经哪个源下载」的展示文案；后端没给源名（没发过请求）时返回 null，不编造。 */
function downloadSourceText(u: PluginUpdate, action: string): string | null {
  if (!u.downloadSource) return null;
  return u.viaMirror
    ? `${action}经第三方镜像「${u.downloadSource}」下载（镜像有能力篡改传输内容）`
    : `${action}经 GitHub 官方源「${u.downloadSource}」直连下载`;
}

/** 上一次**发起**全量检查的时间戳（ms，0 = 从未）。发起时即记录，在途重入也会被节流挡住。 */
let lastCheckAt = 0;

/** 是否正在进行一次全量检查。
 *  必须与 updates 一样是**模块级**：checking 若留在 renderPlugins 的闭包里，
 *  「检查在途 → 切页 → 切回」后新一轮渲染的 checking 恒为 false，而 updates 又刚被清空，
 *  于是有 Git 来源的插件既不显示「检查中…」也不显示任何结论，看起来像「查过了、没事」。 */
let checking = false;

/** 当前挂载中的列表渲染实例。检查是异步的，发起它的那次渲染可能早已被 innerHTML="" 丢弃，
 *  结果必须回灌到**当下**这个实例，而不是自己闭包里那份脱离文档的 DOM。 */
interface ActiveRender {
  isMounted: () => boolean;
  getList: () => PluginInfo[];
  rerender: () => void;
  syncCheckBtn: () => void;
  reload: () => Promise<void>;
  /** plugins-changed 监听器的解绑句柄（注册是异步的，可能晚于本实例被替换） */
  unlisten: UnlistenFn | null;
  /** 窗口不可见期间到达过 plugins-changed：恢复可见/取得焦点后补一次刷新（见 renderPlugins 里的门控） */
  pendingReload: boolean;
  /** 上面那套可见性/焦点监听的解绑（与 unlisten 一样必须在 dispose 里解掉） */
  offVisibility: (() => void) | null;
}
let active: ActiveRender | null = null;

/** plugins-changed 事件的合并计时器（文件监听会在一次目录变更里连发多条） */
let changedTimer: number | undefined;

/** 把检查状态同步到当前挂载的渲染实例（没有挂载实例时是无操作，结果仍留在 updates 里）。 */
function syncActive(): void {
  active?.syncCheckBtn();
  active?.rerender();
}

/** 自动（静默）检查的节流窗口：5 分钟内不再自动发起。
 *  没有节流时，列表 ↔ 详情来回点 10 次 = 10 轮全量 raw 请求，GitHub raw 很快 403 限流，
 *  徽标齐刷刷变「检查失败」，而用户根本不知道是自己点出来的。
 *  手动点「检查更新」是用户显式要求，不受此限制。 */
const AUTO_CHECK_INTERVAL_MS = 5 * 60 * 1000;

/**
 * 淘汰与当前列表对不上的旧结论：插件已被删除，或版本号已经变了（外部热重载 / 手工替换目录 /
 * 管理中心窗口只是隐藏没销毁，模块级 updates 会跨越整个 app 生命周期存活）。
 * @returns 是否有条目被淘汰（有则说明本地已变化，节流窗口内也应重新检查一次）
 */
function reconcileUpdates(list: PluginInfo[]): boolean {
  const byName = new Map(list.map((p) => [p.name, p]));
  let dropped = false;
  for (const [name, u] of Array.from(updates)) {
    const p = byName.get(name);
    if (!p || u.currentVersion !== p.version) {
      updates.delete(name);
      dropped = true;
    }
  }
  for (const name of Array.from(actionErrors.keys())) {
    if (!byName.has(name)) actionErrors.delete(name);
  }
  // 来源留痕说的是「本机现在这份文件是从哪儿来的」：插件还在就继续成立（版本变了正是它造成的），
  // 只有插件被删/换掉才作废
  for (const name of Array.from(updateSources.keys())) {
    if (!byName.has(name)) updateSources.delete(name);
  }
  return dropped;
}

export async function renderPlugins(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  // 上一次渲染实例（可能仍持有 plugins-changed 监听）就此作废：先解绑，保证全局至多一个监听，
  // 「列表 ↔ 详情」来回切不会越积越多、造成刷新风暴。
  disposeActive();

  let list: PluginInfo[];
  try {
    list = await api.listPlugins();
  } catch (err) {
    console.error("list_plugins failed", err);
    panelError(root, "插件列表加载失败：" + errText(err, "未知错误"), () =>
      void renderPlugins(root, ctx),
    );
    return;
  }

  // 本地列表与上一轮结论对账：版本已变 / 插件已删的结论一律作废（宁可没有徽标，也不显示过期结论）
  const staleDropped = reconcileUpdates(list);

  const listWrap = h("div", { class: "plugin-list" });

  function logoFor(p: PluginInfo): HTMLElement {
    if (p.logo) return h("img", { class: "plugin-logo", src: p.logo, alt: "" });
    return h("span", { class: "plugin-logo plugin-logo-fallback", html: PLUGIN_GLYPH });
  }

  // 进入插件详情页；返回时重渲染列表（root 由详情页/此处负责清空）
  async function openDetail(p: PluginInfo): Promise<void> {
    await renderPluginDetail(
      root,
      ctx,
      p,
      () => {
        root.innerHTML = "";
        void renderPlugins(root, ctx);
      },
      updates.get(p.name) ?? null,
    );
  }

  /** 更新状态徽标（无可展示状态则 null）。 */
  function statusBadge(p: PluginInfo): HTMLElement | null {
    const chip = (text: string, cls: string, title: string): HTMLElement =>
      h("span", { class: `plugin-badge ${cls}`, text, title });

    const u = updates.get(p.name);
    if (u) {
      // 检查/更新请求实际走了哪个源（后端如实回传）：附到 title 里，不占卡片版面
      const via = downloadSourceText(u, "本次请求");
      const withVia = (s: string): string => (via ? `${s}\n${via}` : s);
      if (u.hasUpdate && u.latestVersion) {
        return chip(
          `新版 v${u.latestVersion}`,
          "plugin-badge-new",
          withVia(`远端最新版本 v${u.latestVersion}`),
        );
      }
      if (u.error) {
        return chip("检查失败", "plugin-badge-warn", `检查更新失败：${u.error}`);
      }
      if (u.pinned) {
        const rev = p.source ? revisionLabel(p.source.revision) : "";
        return chip(
          rev ? `已锁定 ${rev}` : "已锁定",
          "plugin-badge-muted",
          "安装来源锁定在指定 commit，不跟随远端更新",
        );
      }
      if (u.checked) {
        return chip("已是最新", "plugin-badge-muted", withVia("已与远端比对，当前为最新版本"));
      }
      // checked=false：后端没有真的查过，落到下面的来源型徽标
    }
    if (p.builtin) {
      return chip("内置", "plugin-badge-muted", "随安装包分发的内置插件，随 iTools 版本更新");
    }
    if (!p.source) {
      return chip("本地安装", "plugin-badge-muted", "不是从 Git 安装的插件，无法自动检查更新");
    }
    if (checking) {
      return chip("检查中…", "plugin-badge-muted", "正在向远端仓库比对版本");
    }
    return null; // 有 Git 来源但尚未检查：不假装任何状态
  }

  /** 「更新」按钮：只有确认有新版时才出现。 */
  function updateButton(p: PluginInfo, u: PluginUpdate): HTMLButtonElement {
    const btn = h("button", { class: "btn btn-sm btn-primary", text: "更新" });
    btn.title = `更新到 v${u.latestVersion ?? "?"}`;
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      btn.disabled = true;
      btn.textContent = "更新中…";
      try {
        const res = await api.pluginUpdate(p.name);
        updates.set(res.name, res);
        actionErrors.delete(p.name);
        // 后端已如实回传本次包体实际用的源：toast 里说一次，卡片上留痕（toast 1.8s 就没了）
        const via = downloadSourceText(res, "本次更新");
        if (via) updateSources.set(res.name, via);
        else updateSources.delete(res.name);
        ctx.toast(
          res.viaMirror && res.downloadSource
            ? `已更新到 v${res.currentVersion}（经镜像 ${res.downloadSource}）`
            : `已更新到 v${res.currentVersion}`,
        );
        await reload();
      } catch (err) {
        console.error("plugin_update failed", err);
        const msg = errText(err, "更新失败");
        // 后端会给出可区分的原因（DNS 失败 / 超时 / 403 限流 / 404 / 校验失败），
        // toast 一闪而过，这里同时把原文留在卡片上，用户才有的可查。
        actionErrors.set(p.name, msg);
        ctx.toast(msg);
        btn.title = msg;
        btn.disabled = false;
        btn.textContent = "更新";
        rerender();
      }
    });
    return btn;
  }

  /** 「查看仓库」按钮（仅有 Git 来源的插件）。 */
  function repoButton(p: PluginInfo): HTMLButtonElement | null {
    if (!p.source) return null;
    const btn = h("button", {
      class: "icon-btn",
      html: LINK,
      title: `查看仓库 ${sourceLabel(p.source)}`,
    });
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        await api.pluginOpenSourcePage(p.name);
      } catch (err) {
        console.error("plugin_open_source_page failed", err);
        ctx.toast(errText(err, "打开仓库失败"));
      }
    });
    return btn;
  }

  function renderCard(p: PluginInfo): HTMLElement {
    const card = h("div", { class: "plugin-card clickable" + (p.enabled ? "" : " disabled") });

    const meta = h(
      "div",
      { class: "plugin-meta" },
      h(
        "div",
        { class: "plugin-name-row" },
        h("span", { class: "plugin-name", text: p.display_name }),
        h("span", { class: "plugin-ver", text: "v" + p.version }),
        statusBadge(p),
      ),
      h("div", { class: "plugin-desc", text: p.description || "（无描述）" }),
      p.cmds.length
        ? h("div", { class: "plugin-cmds", text: "关键字：" + p.cmds.join("  ·  ") })
        : null,
    );

    // 高危能力授权（插件声明了才显示；开关切换即授权/撤销）
    if (p.permissions.length) {
      const permsRow = h("div", { class: "plugin-perms" });
      // 同 .plugin-actions：在容器上截断，不依赖 closest（理由见下方 actions 处的注释）
      permsRow.addEventListener("click", (e) => e.stopPropagation());
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

    // 最近一次更新失败的真实原因（含 DNS / 超时 / 403 限流 / 404 / 校验失败），留痕到下次成功为止
    const lastErr = actionErrors.get(p.name);
    if (lastErr) {
      meta.appendChild(
        h("div", { class: "plugin-card-error", text: "上次更新失败：" + lastErr, title: lastErr }),
      );
    }

    // 最近一次成功更新走的下载源：经第三方镜像装进来的代码，用户有权看见（不只是 toast 一闪）
    const via = updateSources.get(p.name);
    if (via) meta.appendChild(h("div", { class: "plugin-card-note", text: via, title: via }));

    // 删除：弹窗确认。
    //
    // 原先是「点一次变『确认删除』，3 秒内再点才真删」，实测很难用：鼠标一犹豫就超时复位，
    // 于是每次点到的都是第一步，看着就像「点了没反应」。而且那个改按钮文案的动作会把
    // 用户正点着的 <svg> 从 DOM 里摘掉，事件冒泡到卡片时 closest() 判断失效，反倒跳去了详情页。
    // 换成弹窗后两个毛病一起没了：状态明确、不超时、后果写得清楚。
    const delBtn = h("button", { class: "plugin-del", title: "删除插件", html: TRASH });
    delBtn.addEventListener("click", async () => {
      const ok = await confirmDialog({
        title: "删除插件",
        message: `确定删除「${p.display_name}」吗？`,
        // 如实说明边界：后端只删插件目录与授权/来源记录，**不动**该插件存过的数据
        // （见 plugin/commands.rs 的 delete_plugin）。不写清楚，用户会以为数据也一并没了。
        warn: "插件目录将被移除。它保存的数据不会删除，重新安装后仍然可用。",
        confirmText: "删除",
      });
      if (!ok) return;
      try {
        await api.deletePlugin(p.name);
        updates.delete(p.name);
        actionErrors.delete(p.name);
        updateSources.delete(p.name);
        ctx.toast(`已删除插件 ${p.display_name}`);
        await reload();
      } catch (err) {
        console.error("delete_plugin failed", err);
        ctx.toast(errText(err, "删除失败"));
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

    const u = updates.get(p.name);
    const actions = h("div", { class: "plugin-actions" });
    // 操作区的点击一律不冒泡到卡片，否则会打开详情页。
    //
    // 不能只依赖下面卡片处理器里的 `e.target.closest('.plugin-actions')`：删除按钮**第一次点击时会把
    // 自己的内容从 SVG 换成「确认删除」文字**，于是用户真正点到的那个 <path> 在事件继续冒泡之前
    // 就已经脱离了 DOM 树——对一个已移除的节点调 closest() 只会得到 null，拦截当场失效。
    // 现象就是「点删除直接进详情页、删除永远走不到第二步」。在容器上截断是唯一稳的做法。
    actions.addEventListener("click", (e) => e.stopPropagation());
    if (u && u.hasUpdate) actions.appendChild(updateButton(p, u));
    const repoBtn = repoButton(p);
    if (repoBtn) actions.appendChild(repoBtn);
    actions.append(delBtn, sw);

    card.append(logoFor(p), meta, actions);
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
            text:
              "点「安装插件」从 GitHub 仓库安装；也可以把插件目录直接放进 plugins/ 文件夹，再点右上角「重新加载」。",
          }),
          h("button", {
            class: "btn btn-primary",
            text: "从 GitHub 安装插件",
            onClick: openInstall,
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
    } catch (err) {
      console.error("list_plugins failed", err);
      /* 保留旧列表 */
    }
    // 外部热重载 / 手工替换目录都会改版本号，旧结论必须作废；节流窗口过了才顺带重查（避免刷新风暴）
    const dropped = reconcileUpdates(list);
    rerender();
    if (dropped && Date.now() - lastCheckAt >= AUTO_CHECK_INTERVAL_MS) void runCheck(true);
  }

  /** 打开「从 Git 安装」弹窗；安装成功后刷新列表并重查更新状态。 */
  function openInstall(): void {
    openInstallModal({
      toast: ctx.toast,
      onInstalled: (name) => {
        updates.delete(name); // 版本已变，旧的检查结果作废
        actionErrors.delete(name);
        // 这次是「安装」不是「更新」，来源事实已在安装弹窗里如实展示过；旧的更新留痕不再对应当前文件
        updateSources.delete(name);
        void reload().then(() => runCheck(true));
      },
    });
  }

  /**
   * 跑一次全量更新检查（节流由调用方决定：自动检查受 AUTO_CHECK_INTERVAL_MS 限制，手动不受限）。
   * @param silent 静默模式（进入页面时的后台检查）：失败不弹 toast，但**照样**把失败写进徽标——
   *               静默不等于假装成功。
   */
  async function runCheck(silent: boolean): Promise<void> {
    if (checking) return;
    checking = true;
    lastCheckAt = Date.now(); // 发起即记账：失败也计入节流，避免离线/限流时被反复重试打爆
    // 上一轮结果在重新检查期间即作废：宁可显示「检查中…」，也不拿旧结论当现状
    updates.clear();
    // 一律作用于**当前挂载**的渲染实例：这次检查可能是上一次渲染发起的，它的 DOM 早已脱离文档
    syncActive();
    try {
      const res = await api.pluginCheckUpdates();
      res.forEach((u) => updates.set(u.name, u));
      if (!silent) ctx.toast(checkSummary(res));
    } catch (err) {
      console.error("plugin_check_updates failed", err);
      const msg = errText(err, "检查更新失败");
      // 整体失败（断网 / 命令报错）必须留痕：否则 updates 为空，有 Git 来源的插件不显示任何徽标，
      // 用户会误以为「查过了、没问题」。这里把整体错误落到每个有来源的插件上，徽标如实显示
      // 「检查失败」并在 title 给出原因。（pinned 由本地 revision 直接判定——后端对锁定插件
      // 本就不发请求，这次失败与它们无关，标成「检查失败」反而是假信息。）
      for (const p of active?.getList() ?? list) {
        if (!p.source || p.builtin) continue;
        const pinned = isCommitSha(p.source.revision);
        updates.set(p.name, {
          name: p.name,
          currentVersion: p.version,
          latestVersion: null,
          hasUpdate: false,
          pinned,
          checked: false,
          error: pinned ? null : msg,
          // 命令整体失败：没有任何一次成功的远端请求，来源无从谈起，如实留 null（不猜是哪个源）
          downloadSource: null,
          viaMirror: false,
        });
      }
      if (!silent) ctx.toast(msg);
    } finally {
      checking = false;
      syncActive();
    }
  }

  /** 手动检查后的汇总文案（如实区分「有新版 / 失败 / 全部最新 / 已锁定 / 没有可检查的插件」）。 */
  function checkSummary(res: PluginUpdate[]): string {
    const upd = res.filter((u) => u.hasUpdate).length;
    const failed = res.filter((u) => !!u.error).length;
    const checkedCount = res.filter((u) => u.checked).length;
    // pinned（#<40 位 sha> 锁定）后端直接返回 checked=false / error=null，根本没发请求。
    // 漏掉这一类会在「插件全被锁定」时谎报「没有从 Git 安装的插件」——与徽标上的「已锁定」自相矛盾。
    const pinned = res.filter((u) => u.pinned).length;
    const parts: string[] = [];
    if (upd) parts.push(`${upd} 个插件有新版本`);
    if (failed) parts.push(`${failed} 个检查失败`);
    if (!upd && !failed && checkedCount) parts.push("全部已是最新");
    if (pinned) parts.push(`${pinned} 个插件已锁定 commit，未参与检查`);
    if (parts.length) return parts.join("，");
    return "没有从 Git 安装的插件，无需检查";
  }

  // ---------- 头部 ----------
  const installBtn = h("button", {
    class: "btn btn-sm btn-primary",
    text: "安装插件",
    title: "从 GitHub 仓库安装插件",
    onClick: openInstall,
  });

  const mirrorBtn = h("button", {
    class: "icon-btn",
    html: SOURCE_IC,
    title: "插件下载源：官方 / 镜像竞速设置与测速",
    onClick: () => openMirrorModal({ toast: ctx.toast }),
  });

  const checkBtn = h("button", {
    class: "btn btn-sm",
    text: "检查更新",
    title: "向远端仓库比对所有 Git 来源插件的版本",
    onClick: () => void runCheck(false),
  });

  /** 检查按钮的进行中态 */
  function syncCheckBtn(): void {
    checkBtn.disabled = checking;
    checkBtn.textContent = checking ? "检查中…" : "检查更新";
  }

  const reloadBtn = h("button", { class: "icon-btn", title: "重新扫描 plugins 目录", html: REFRESH });
  reloadBtn.addEventListener("click", async () => {
    try {
      const n = await api.rescanPlugins();
      await reload();
      ctx.toast(`已重新加载（${n} 条命令）`);
    } catch (err) {
      console.error("rescan_plugins failed", err);
      ctx.toast(errText(err, "重载失败"));
    }
  });

  const subhead = h(
    "div",
    { class: "launch-subhead" },
    h("span", { class: "launch-subhead-title", text: "已安装插件" }),
    h(
      "div",
      { class: "launch-actions plugin-head-actions" },
      installBtn,
      checkBtn,
      mirrorBtn,
      reloadBtn,
    ),
  );

  const intro = h("div", {
    class: "launch-intro",
    text: "管理已安装的插件：开关控制是否参与主搜索，删除会移除插件目录。可从 GitHub 仓库直接安装，也可把插件目录放进 plugins/ 后点「重新加载」。",
  });

  rerender();
  root.append(h("div", { class: "launch-scroll" }, intro, subhead, listWrap));

  // ---------- 注册为当前挂载实例 + 监听后端的 plugins-changed ----------
  const instance: ActiveRender = {
    isMounted: () => listWrap.isConnected,
    getList: () => list,
    rerender,
    syncCheckBtn,
    reload,
    unlisten: null,
    pendingReload: false,
    offVisibility: null,
  };
  active = instance;
  // 离开插件页（切到别的导航项）时解绑本页的 plugins-changed 监听与下面的可见性监听。
  // ⚠ registerDispose **只覆盖切页**：它由 main.ts 的 switchView 调用。
  //   关闭管理中心走的是 close_admin_window → win.hide()（commands.rs），窗口只隐藏不销毁、
  //   也不触发 switchView，所以停在插件页时这个 dispose 永远不会跑 —— 「窗口隐藏」那条路径
  //   由紧接着的可见性门控负责，不要再把 registerDispose 说成能管住隐藏场景。
  ctx.registerDispose(disposeActive);

  // ---------- 窗口不可见时的刷新门控 ----------
  // 隐藏期间 listWrap 仍 isConnected、isMounted() 仍为真，若不拦，后端每 emit 一次 plugins-changed
  // （安装完成 install.rs / 目录热更新 watch.rs）就会在一个用户看不见的窗口里跑一次完整 reload()
  // （listPlugins + 全量重渲染）。这里的做法是：不可见时只记一笔 pendingReload，不刷新；
  // 重新可见（或 webview 重新取得焦点）时补上那一次，**不丢更新**。
  // 诚实说明其边界：判据是 WebView 的 document.visibilityState。若本平台的 WebView 不把
  // 「窗口被 hide/最小化」映射成 visibilityState=hidden，本门控就不生效，行为与从前完全一致
  // （仍是每 emit 一次刷新一次）——只会不生效，不会更糟，也不会漏刷新。
  const flushPending = (): void => {
    if (active !== instance || !instance.isMounted() || !instance.pendingReload) return;
    instance.pendingReload = false;
    void reload();
  };
  const onVisibility = (): void => {
    if (document.visibilityState === "visible") flushPending();
  };
  document.addEventListener("visibilitychange", onVisibility);
  // 兜底触发：窗口重新显示时 Rust 侧 open_admin 会 show + unminimize + set_focus（lib.rs），
  // webview 随之取得焦点。万一某平台只在隐藏时更新 visibilityState、重新显示时不派发事件，
  // 这一路也能把压下的那次刷新放出来（隐藏的窗口拿不到焦点，所以以焦点为准是安全的）。
  window.addEventListener("focus", flushPending);
  instance.offVisibility = (): void => {
    document.removeEventListener("visibilitychange", onVisibility);
    window.removeEventListener("focus", flushPending);
  };
  syncCheckBtn(); // 检查可能是上一次渲染发起、此刻仍在途：按钮必须如实显示「检查中…」

  // 后端安装完成（install.rs）与插件目录热重载（watch.rs）都会 emit("plugins-changed")。
  // 监听是异步注册的：若注册返回时本实例已被替换，立刻解绑，保证全局至多一个监听。
  void listen("plugins-changed", () => {
    // 文件监听可能在一次目录变更里连发多条，合并到 300ms 一次，避免刷新风暴
    window.clearTimeout(changedTimer);
    changedTimer = window.setTimeout(() => {
      // 已切到别的页 / 进了详情页：不刷新（重新进入本就会拉一次最新列表）
      if (active !== instance || !instance.isMounted()) return;
      // 管理中心窗口被隐藏/最小化：不在用户看不见的窗口里跑全量刷新，只记一笔，恢复可见时补上
      if (document.visibilityState === "hidden") {
        instance.pendingReload = true;
        return;
      }
      void reload();
    }, 300);
  })
    .then((un) => {
      if (active === instance) instance.unlisten = un;
      else void un();
    })
    .catch((err) => console.error("listen plugins-changed failed", err));

  // 进入页面后后台静默检查一次（不阻塞首屏；失败不弹 toast，但会写进徽标）。
  // 节流：距上次检查不足 AUTO_CHECK_INTERVAL_MS 就沿用已有结果——「进详情 → 返回」会重跑本函数，
  // 没有节流时每来回一次就是一轮全量远端请求，极易被 raw 站点限流。
  // 例外：刚才对账淘汰过条目（插件被删/版本已变），说明沿用的结果已经不成立，无视节流重查一次。
  if (staleDropped || Date.now() - lastCheckAt >= AUTO_CHECK_INTERVAL_MS) void runCheck(true);
}

/** 丢弃上一个渲染实例：解绑其事件监听（plugins-changed + 可见性/焦点）并取消待执行的合并刷新。 */
function disposeActive(): void {
  window.clearTimeout(changedTimer);
  const prev = active;
  active = null;
  if (prev?.offVisibility) {
    try {
      prev.offVisibility();
    } catch (err) {
      console.error("remove visibility listener failed", err);
    }
    prev.offVisibility = null;
  }
  if (prev?.unlisten) {
    try {
      prev.unlisten();
    } catch (err) {
      console.error("unlisten plugins-changed failed", err);
    }
    prev.unlisten = null;
  }
}
