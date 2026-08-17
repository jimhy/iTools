//! 网络设置：云同步服务器 + 网络代理。
//!
//! # 为什么单独一页
//!
//! 这两块此前散在两处：服务器地址在「我的账号 → 修改账号 → 数据同步」，代理在「设置」页尾部，
//! 而「设置」页里还放了一份只读的地址展示。用户要找「服务器地址在哪填」得先猜它属于账号还是设置——
//! 真实发生过（有人在设置页里找不到它）。现在归拢到一页，**编辑入口全局只此一处**。
//!
//! # 保存语义（与设置页一致，别改成别的）
//!
//! 改动写进本地副本后 300ms 防抖保存；切走本页时会把还在窗口里的改动立刻落盘。
//! 后端 `save_settings` 对代理配置是**先校验再落盘**：开着开关却给不出可用地址时整次保存都不写入，
//! 所以失败时必须把开关退回磁盘真实值并原样显示后端给的中文原因 —— 否则就会留下
//! 「开关显示开着、实际关着」的分叉（准则里「看着能用、点了不生效」的典型形态）。

import type { AdminCtx } from "./main";
import type { AccountState, AppSettings } from "../types";
import { h, makeSwitch } from "./ui";
import * as api from "./api";

/** 把 invoke 抛出的东西转成可读文本。Tauri 的 `Err(String)` 到前端就是个字符串，
 *  这里**不**把它替换成「操作失败」一类的自造文案 —— 后端给的真实原因才是用户要的。 */
function errText(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/** 代理地址的**脱敏**展示串：抹掉 `://` 之后、最后一个 `@` 之前的凭据。
 *
 *  后端为凭据不外泄做了三道闸（`ProxySpec` 手写 Debug / `ProxySpec::display` / `redact_proxy_credentials`），
 *  前端不能在最后一步把输入框原值贴回界面 —— 用户填 `user:pass@127.0.0.1:7897` 时密码会明晃晃显示在
 *  测试结果里，一张截图反馈就泄了。切分口径与后端一致：按**最后一个** `@`（密码里可以含 `@`）。 */
function maskProxyAddr(addr: string): string {
  const s = addr.trim();
  const i = s.indexOf("://");
  const scheme = i >= 0 ? s.slice(0, i + 3) : "";
  const rest = i >= 0 ? s.slice(i + 3) : s;
  const at = rest.lastIndexOf("@");
  if (at < 0) return s;
  return `${scheme}***@${rest.slice(at + 1)}`;
}

export async function renderNetwork(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let settings: AppSettings;
  try {
    settings = await api.getSettings();
  } catch (err) {
    console.error("get_settings failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "设置加载失败" }));
    return;
  }
  let account: AccountState | null = null;

  // ---------- 防抖保存 ----------
  let saveTimer: number | undefined;
  function scheduleSave(): void {
    window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      saveTimer = undefined;
      void save();
    }, 300);
  }

  // ---------- 保存失败时的「回到磁盘真实状态」机制 ----------
  let savedProxyEnabled = settings.proxy_enabled;
  /** 已落盘的代理地址（每次保存成功后更新）。保存被拒时，运行期真正生效的仍是它 ——
   *  而地址框里显示的是被拒的那个值。不点破这层分叉，界面显示的地址就与实际出口不是一回事。 */
  let savedProxyAddress = settings.proxy_address;
  /** 保存失败时的处理（代理控件创建后被赋成真正的实现）。 */
  let onSaveRejected: (why: string) => void = () => {};
  /** 保存成功后收起「未保存」提示。 */
  let clearProxyNote: () => void = () => {};

  async function save(): Promise<void> {
    try {
      await api.saveSettings(settings);
      savedProxyEnabled = settings.proxy_enabled;
      savedProxyAddress = settings.proxy_address;
      clearProxyNote();
    } catch (err) {
      console.error("save_settings failed", err);
      // 后端给的是可照做的中文原因（「已开启网络代理，但没有填写代理地址（形如 127.0.0.1:7897）」
      // 「代理端口不合法：应为 1~65535 的数字」…），吞成「保存失败」等于把唯一的修正指引丢掉。
      const why = errText(err);
      ctx.toast(why);
      onSaveRejected(why);
    }
  }

  // 切走本面板时，把还在 300ms 防抖窗口里的改动立刻落盘，否则最后一次编辑会被静默丢弃。
  ctx.registerDispose(() => {
    if (saveTimer === undefined) return;
    window.clearTimeout(saveTimer);
    saveTimer = undefined;
    void save();
  });

  // ---------- 布局原语 ----------
  function group(title: string, ...rows: HTMLElement[]): HTMLElement {
    return h("div", { class: "set-group" }, h("div", { class: "set-group-title", text: title }), ...rows);
  }
  function row(label: string, desc: string | null, ...controls: HTMLElement[]): HTMLElement {
    return h(
      "div",
      { class: "set-row" },
      h(
        "div",
        { class: "set-row-text" },
        h("div", { class: "set-row-label", text: label }),
        desc ? h("div", { class: "set-row-desc", text: desc }) : null,
      ),
      h("div", { class: "set-row-control" }, ...controls),
    );
  }

  // ---------- 云同步服务器 ----------
  //
  // 这里同时回答两个问题：
  //   ① 「我现在到底连的是哪个地址、它是谁定的」—— 由 sync_endpoint_info 给出**实际生效**的值，
  //      它可能来自环境变量、来自你填的、或是 debug 构建的默认值，三者优先级不同；
  //   ② 「我在哪儿改」—— 编辑入口从「我的账号 → 修改账号 → 数据同步」搬到了这里，
  //      全局只此一处，不会出现两个输入框互相不同步。

  // ---------- 地址编辑 + 登录同步 ----------
  //
  // iTools 是**本地优先**的：数据始终先落本机、离线可用；填了服务器地址并登录之后，
  // 才会把数据同步上去。地址只保存在本机、不随程序内置（安全红线：不写死服务端地址）。

  const epInput = h("input", { class: "field-input-sm", type: "text", placeholder: "如 https://api.example.com:7010" });
  // 不回显已保存的地址（界面上不展示服务器地址）。
  // placeholder 要如实区分「已配置 / 未配置」——否则一个空输入框既可能是没配过、
  // 也可能是配了不显示，而这两种状态下点「保存并接入」的后果是相反的。
  function refreshEpPlaceholder(): void {
    epInput.placeholder = settings.sync_endpoint
      ? "已配置（留空保存将清除）"
      : "留空 = 使用默认服务";
  }
  refreshEpPlaceholder();
  const epSave = h("button", { class: "btn btn-primary", text: "保存并接入" });
  epSave.addEventListener("click", async () => {
    // 地址栏为空直接返回：输入框不回显已保存的地址，空着多半是「刚打开页面还没填」，
    // 而不是「想清掉配置」。若把空值当成清除，一次误点就会静默断开云端。
    const addr = epInput.value.trim();
    if (!addr) return;
    epSave.disabled = true;
    const prev = settings.sync_endpoint;
    try {
      // 地址是整包设置的一部分，走同一个 save()，好让代理那边的校验/回滚逻辑照常生效
      settings.sync_endpoint = addr;
      await api.saveSettings(settings);
      savedProxyEnabled = settings.proxy_enabled;
      savedProxyAddress = settings.proxy_address;
      ctx.toast(settings.sync_endpoint ? "已保存服务器地址" : "已清除服务器地址");
      epInput.value = "";
      refreshEpPlaceholder();
      await loadAccount();
    } catch (err) {
      console.error("save sync_endpoint failed", err);
      settings.sync_endpoint = prev;
      epInput.value = "";
      refreshEpPlaceholder();
      ctx.toast(errText(err));
    } finally {
      epSave.disabled = false;
    }
  });

  /** 登录态与同步开关所在的行（随账号状态重绘）。 */
  const syncBox = h("div", { class: "set-note" });

  async function loadAccount(): Promise<void> {
    try {
      account = await api.accountState();
    } catch (err) {
      console.error("account_state failed", err);
      syncBox.replaceChildren(
        h("div", { class: "info-box info-box-warn" }, h("div", { text: `读取账号状态失败：${errText(err)}` })),
      );
      return;
    }
    const a = account;
    if (!a.cloudConfigured) {
      syncBox.replaceChildren(
        h(
          "div",
          { class: "info-box" },
          h("div", { class: "info-title", text: "当前为「仅本地」" }),
          h("div", {
            text: "还没有可用的服务器地址，数据只保存在本机。填入上面的地址并保存后即可登录并跨设备同步。",
          }),
        ),
      );
      return;
    }
    if (!a.loggedIn) {
      syncBox.replaceChildren(
        h(
          "div",
          { class: "info-box" },
          h("div", { class: "info-title", text: "已配置服务器，但未登录" }),
          h("div", { text: "登录云账号后才能开启同步。登录入口在「我的账号」页。" }),
          h(
            "div",
            { class: "set-note-actions" },
            h("button", { class: "btn", text: "去我的账号登录", onClick: () => ctx.goto("account") }),
          ),
        ),
      );
      return;
    }
    // 已登录 + 已配置：真实开关 + 立即同步
    const sw = makeSwitch(a.syncEnabled, async (checked) => {
      try {
        account = await api.setDataSync(checked);
        ctx.toast(checked ? "已开启登录同步" : "已关闭登录同步");
      } catch (err) {
        console.error("set_data_sync failed", err);
        ctx.toast(errText(err));
      }
    });
    syncBox.replaceChildren(row("开启云同步", null, sw));
  }

  const editGroup = group(
    "服务器地址",
    h(
      "div",
      { class: "set-row" },
      h("div", { class: "set-row-text" }, h("div", { class: "set-row-label", text: "地址" })),
      h("div", { class: "set-row-control" }, epInput, epSave),
    ),
    syncBox,
  );

  // ---------- 网络 · 代理 ----------
  //
  // 本组曾是「假控件」：开关与地址只写进 settings.json，全仓没有任何一处读它们，
  // 所有出站请求一个字节都没走代理，UI 却只用「（演示）」两字带过。现已真接上后端，
  // 下面的作用范围与直连规则是**照后端实现如实描述**的，改动任一侧都必须同步改另一侧。

  const proxyAddr = h("input", {
    class: "field-input-sm",
    type: "text",
    placeholder: "如 127.0.0.1:7897",
  });
  proxyAddr.value = settings.proxy_address;

  // 「这次改动没落盘」的如实提示（默认隐藏）。它承担两件事：
  //   ① 保存被后端拒绝时，把后端原文与「本页改动都没写入」讲清楚；
  //   ② 开关被退回时，说明为什么它自己跳回去了 —— 不让开关无声地自己变化。
  const proxyNote = h("div", { class: "info-box info-box-warn set-note" });
  proxyNote.style.display = "none";
  /** 显示提示；`action` 非空时在提示下方给出一个可点的补救按钮（如「恢复为已保存的地址」）。 */
  function showProxyNote(text: string, action?: HTMLElement | null): void {
    proxyNote.replaceChildren(h("div", { text }));
    if (action) proxyNote.appendChild(h("div", { class: "set-note-actions" }, action));
    proxyNote.style.display = "";
  }
  clearProxyNote = () => {
    proxyNote.style.display = "none";
  };

  const proxySwitch = makeSwitch(settings.proxy_enabled, (checked) => {
    if (checked && proxyAddr.value.trim() === "") {
      // 用户最自然的顺序是「先开开关、再填地址」，而这一刻保存必被后端拒绝
      // （「已开启网络代理，但没有填写代理地址」）。与其让他吃一记报错、界面还停在「开着」的假状态，
      // 不如当场把开关退回去并把光标送到该填的地方。
      setProxyEnabled(false);
      showProxyNote(
        "请先填写代理地址（如 127.0.0.1:7897）再打开开关：没有地址的「已开启」后端不会保存，" +
          "所以开关已恢复为关闭 —— 当前仍是全部直连。",
      );
      proxyAddr.focus();
      return;
    }
    settings.proxy_enabled = checked;
    scheduleSave();
  });
  /** 开关内部的 checkbox：回滚时要连界面一起改（直接改 checked 不会触发 change，不会引发再次保存）。 */
  const proxySwitchInput = proxySwitch.querySelector("input") as HTMLInputElement;
  /** 把 settings 与界面上的开关一起设成 v（不触发保存）。 */
  function setProxyEnabled(v: boolean): void {
    settings.proxy_enabled = v;
    proxySwitchInput.checked = v;
  }

  /** 把地址框改回落盘值并立刻重存一次。**只在用户主动点击时执行** ——
   *  绝不在保存失败时自动回退他正在敲的字（那等于替他撤销输入，比显示不一致更糟）。 */
  const restoreAddrBtn = h("button", {
    class: "btn",
    text: "恢复为已保存的地址",
    onClick: () => {
      proxyAddr.value = savedProxyAddress;
      settings.proxy_address = savedProxyAddress;
      syncProxyTestBtn();
      // 上一次的测试结论是针对刚才那个地址的，地址换了就不作数
      proxyTestRow.style.display = "none";
      showProxyNote(
        `已把地址改回上次保存的 ${maskProxyAddr(savedProxyAddress)}，正在重新保存本页改动…`,
      );
      void save();
    },
  });

  onSaveRejected = (why: string) => {
    const drifted = settings.proxy_enabled !== savedProxyEnabled;
    if (!drifted) {
      // 开关本来就等于落盘值（多半是地址被判非法 / 被清空），这次整包没保存。
      // 光说「没写入」还不够：地址框里留着的是被拒的那个值，而运行期真正在用的
      // 仍是上次保存的地址 —— 必须点破这层分叉，否则界面显示的出口不是真实出口。
      let text = `这次保存被后端拒绝，本页这一次的改动都没有写入：${why}`;
      if (settings.proxy_address === savedProxyAddress) {
        showProxyNote(text); // 地址框与落盘值一致，不存在「显示的与生效的不是同一个」
        return;
      }
      const hasSaved = savedProxyAddress.trim() !== "";
      if (savedProxyEnabled && hasSaved) {
        text +=
          "。注意：磁盘上的代理开关是「已开启」，当前实际生效的仍是上次保存的地址 " +
          `${maskProxyAddr(savedProxyAddress)} —— 不是上面输入框里显示的这个。` +
          "把地址改到能被接受为止，或点下面的按钮改回已保存的值。";
      } else if (hasSaved) {
        text +=
          `。磁盘上保存的仍是上次的地址 ${maskProxyAddr(savedProxyAddress)}` +
          "（代理开关未开启，出站请求本就全部直连）。";
      } else {
        text += "。磁盘上还没有保存过代理地址，当前出站请求全部直连。";
      }
      showProxyNote(text, hasSaved ? restoreAddrBtn : null);
      return;
    }
    setProxyEnabled(savedProxyEnabled);
    const head =
      `代理开关已退回落盘的真实状态（${savedProxyEnabled ? "已开启" : "未开启"}）` +
      `—— 后端拒绝了这次保存：${why}`;
    showProxyNote(`${head}。正在重新保存本页其它改动…`);
    // 开关已回到落盘值，代理校验不会再拦它。补存一次，别让同一次保存里的其它设置项跟着丢。
    // （刻意直接调 api 而不是 save()：避免失败时又回到本函数里绕圈。）
    api.saveSettings(settings).then(
      () => {
        savedProxyEnabled = settings.proxy_enabled;
        // 这次补存是整包写入，地址也跟着落了盘 —— 落盘值必须同步更新，
        // 否则下次被拒时会拿一个早就不是磁盘内容的旧地址去告诉用户「现在生效的是它」。
        savedProxyAddress = settings.proxy_address;
        showProxyNote(`${head}。本页其它设置已正常保存。`);
      },
      (e) => {
        console.error("save_settings retry failed", e);
        showProxyNote(`${head}。本页其它设置也没能保存：${errText(e)}`);
      },
    );
  };

  const proxyTestBtn = h("button", { class: "btn", text: "测试" });
  const proxyTestText = h("div", { class: "set-row-desc" });
  const proxyTestRow = h("div", { class: "set-row" }, proxyTestText);
  proxyTestRow.style.display = "none";

  let proxyTesting = false;
  /** 地址为空时无从测起 → 禁用并说明原因，不做「点了没反应」的按钮。 */
  function syncProxyTestBtn(): void {
    const empty = proxyAddr.value.trim() === "";
    proxyTestBtn.disabled = empty || proxyTesting;
    proxyTestBtn.title = empty
      ? "请先填写代理地址"
      : "用当前填写的地址发起一次真实请求（与上面的开关无关）";
  }
  syncProxyTestBtn();

  proxyAddr.addEventListener("input", () => {
    settings.proxy_address = proxyAddr.value;
    scheduleSave();
    syncProxyTestBtn();
    // 地址已改，上一次的测试结论对新地址不作数 —— 收起来，不留过期结论误导用户
    if (!proxyTesting) proxyTestRow.style.display = "none";
  });

  proxyTestBtn.addEventListener("click", async () => {
    const addr = proxyAddr.value.trim();
    if (!addr || proxyTesting) return;
    // 回显一律用脱敏串：原值可能是 user:pass@host:port，明文密码不该出现在界面上
    const shown = maskProxyAddr(addr);
    proxyTesting = true;
    syncProxyTestBtn();
    proxyTestBtn.textContent = "测试中…";
    proxyTestText.textContent = `正在通过 ${shown} 发起一次真实请求…`;
    proxyTestRow.style.display = "";
    try {
      const r = await api.testProxy(addr);
      const lat = r.latencyMs === null ? "耗时未知" : `${r.latencyMs} ms`;
      if (r.ok && r.viaProxy) {
        proxyTestText.textContent = `代理可用：经 ${shown} 访问 ${r.target} 成功，${lat}。`;
      } else if (r.ok) {
        // viaProxy=false：直连通了而已。不说清楚，用户会把这当成「代理配好了」。
        proxyTestText.textContent =
          `连接成功，但本次并未经过代理（直连 ${r.target}，${lat}）——` +
          "这只能说明网络通，不能证明代理可用。请检查地址是否被后端识别。";
      } else {
        const why = r.error && r.error.trim() ? r.error : "后端未给出失败原因";
        proxyTestText.textContent = r.viaProxy
          ? `代理测试失败（经 ${shown} 访问 ${r.target}）：${why}`
          : `测试失败，且本次并未经过代理（直连 ${r.target}）：${why}`;
      }
    } catch (err) {
      console.error("test_proxy failed", err);
      proxyTestText.textContent = `测试未能完成：${errText(err)}`;
    } finally {
      proxyTesting = false;
      proxyTestBtn.textContent = "测试";
      syncProxyTestBtn();
    }
  });

  const proxy = group(
    "网络代理",
    row("启用代理", null, proxySwitch),
    // ⚠ 这里原本有一段说明，逐条列举后端 http.rs::normalize_proxy 接受的地址形态
    //（HTTP / socks5 / socks4 的差异、鉴权、端口必填、IPv6 不支持等）。按产品要求本页不再
    // 显示任何行内说明，该段已移除——**后端支持的形态没有变**，改动代理解析时不必回来同步这里。
    row(
      "代理地址",
      null,
      proxyAddr,
      proxyTestBtn,
    ),
    proxyTestRow,
    proxyNote,
    // 这里原本有三个说明框（哪些流量走代理 / 本地地址一律直连 / 「测试」按钮测的是什么）。
    // 按产品要求本页不再显示说明性内容，已移除。**后端行为一条都没变**：
    // 本地地址直连仍由 http.rs::is_bypass_host 硬性保证，改那边时不必回来同步这里。
  );

  root.appendChild(h("div", { class: "settings-scroll" }, editGroup, proxy));
  void loadAccount();
}
