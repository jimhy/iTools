//! 我的账号（本地优先 + 配置化云端 + 诚实降级）：
//! 首页 banner + 账号操作；「修改账号」覆盖页（修改头像/昵称/数据同步/账号注销）；登录/退出弹窗。
//!
//! 诚信约束（doc/开发准则.md 第 7 条）：数据始终先落本地、离线可用；登录云账号后才可选同步到云端。
//! 未接入云端（未配置 ITOOLS_SYNC_ENDPOINT）或未登录时，UI **如实标注**「云端未接入 / 未登录」，
//! 不出现「备份到云端 / 多设备同步 / 会员权益」等对用户暗示服务端却不兑现的文案，不展示写死的假手机号。

import type { AdminCtx } from "./main";
import type { ProfileView, AccountState, SyncResult } from "../types";
import { h, makeSwitch } from "./ui";
import { setDropZone, clearDropZone } from "./dnd";
import * as api from "./api";

// ---------- 图标 ----------
const IC_FACE =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9"/><circle cx="9" cy="10.5" r="1"/><circle cx="15" cy="10.5" r="1"/><path d="M8.5 15a4 4 0 0 0 7 0"/></svg>';
const IC_PEN =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4z"/></svg>';
const IC_SYNC =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 0 1-15.7 6M3 12A9 9 0 0 1 18.7 6"/><polyline points="18 2 18.7 6 15 6.7"/><polyline points="6 22 5.3 18 9 17.3"/></svg>';
const IC_BAN =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7"><circle cx="12" cy="12" r="9"/><line x1="5.6" y1="5.6" x2="18.4" y2="18.4"/></svg>';
const IC_CHEVRON =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 6 15 12 9 18"/></svg>';
const IC_X =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>';

/** 昵称首字母（空则「游」） */
function initial(name: string): string {
  const n = name.trim();
  return n ? n[0].toUpperCase() : "游";
}

/** 展示名（空昵称回退「游客」） */
function displayName(name: string): string {
  return name.trim() || "游客";
}

/** 一个信息框（标题 + 若干段正文）。 */
function noticeBox(cls: string, title: string, ...lines: string[]): HTMLElement {
  return h(
    "div",
    { class: `info-box ${cls}` },
    h("div", { class: "info-title", text: title }),
    ...lines.map((t) => h("div", { text: t })),
  );
}

/** 文本/密码输入框。 */
function field(placeholder: string, type = "text"): HTMLInputElement {
  return h("input", { class: "field-input", type, placeholder });
}

/** 把同步结果翻译成给用户看的一句话（诚实：未同步说明原因，不谎报成功）。 */
function syncResultMsg(r: SyncResult): string {
  if (r.synced) return `已同步（上行 ${r.pushed} 条，下行 ${r.pulled} 条）`;
  switch (r.reason) {
    case "cloud_not_configured":
      return "云端服务未接入，数据已保存在本地";
    case "not_logged_in":
      return "未登录，数据已保存在本地";
    case "offline":
      return "无法连接云端，请稍后重试";
    default:
      return r.message || "同步失败";
  }
}

export async function renderAccount(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let profile: ProfileView;
  let account: AccountState;
  try {
    [profile, account] = await Promise.all([api.getProfile(), api.accountState()]);
  } catch (err) {
    console.error("load account failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "账号信息加载失败" }));
    return;
  }

  /** 重新拉取资料 + 账号态并重绘（登录/退出/注销后调用）。 */
  async function refreshAll(): Promise<void> {
    try {
      [profile, account] = await Promise.all([api.getProfile(), api.accountState()]);
    } catch (err) {
      console.error("refresh account failed", err);
    }
    paint();
  }

  /** 构造圆形头像：有头像路径则异步载入，否则显示首字母。 */
  function avatarEl(size: number): HTMLElement {
    const el = h("div", { class: "avatar-img", text: initial(profile.nickname) });
    el.style.width = `${size}px`;
    el.style.height = `${size}px`;
    el.style.fontSize = `${Math.round(size * 0.4)}px`;
    const path = profile.avatar_path;
    if (path) {
      api
        .readAvatar(path)
        .then((dataUrl) => {
          el.classList.add("has-img");
          el.textContent = "";
          el.style.backgroundImage = `url("${dataUrl}")`;
        })
        .catch((err) => console.error("read_avatar failed", err));
    }
    return el;
  }

  // ---------- 首页 ----------
  function paint(): void {
    root.innerHTML = "";

    // 状态行：已登录显示用户名（+ 已绑定手机号），未登录如实标注「未登录」。
    const statusLabel = account.loggedIn
      ? account.username || "已登录"
      : "未登录（本地）";
    const subParts = [statusLabel];
    if (profile.phone) subParts.push(profile.phone);
    subParts.push(`iTools 已陪伴你 ${profile.companion_days} 天`);

    const banner = h(
      "div",
      { class: "acc-banner" },
      avatarEl(64),
      h(
        "div",
        { class: "acc-banner-info" },
        h("div", { class: "acc-name-row" }, h("span", { class: "acc-name", text: displayName(profile.nickname) })),
        h("div", { class: "acc-sub", text: subParts.join("　|　") }),
      ),
    );

    // 云端未接入时给一条诚实提示（顶部横幅），避免用户误以为有云账号体系。
    const rows: HTMLElement[] = [accRow("修改账号", openEdit)];
    if (account.loggedIn) {
      rows.push(accRow("退出账号", openLogout));
    } else {
      rows.push(accRow(account.cloudConfigured ? "登录账号" : "登录账号（云端未接入）", openLogin));
    }
    const list = h("div", { class: "acc-list card" }, ...rows);

    root.append(banner);
    if (!account.cloudConfigured) {
      root.append(
        noticeBox(
          "info-box-warn",
          "云端账号未接入",
          "当前版本未配置云端服务，账号与数据仅保存在本地、离线可用。接入云端后即可登录并跨设备同步。",
        ),
      );
    }
    root.append(list);
  }

  function accRow(label: string, onClick: () => void): HTMLElement {
    return h(
      "button",
      { class: "acc-row", onClick },
      h("span", { text: label }),
      h("span", { class: "acc-row-chevron", html: IC_CHEVRON }),
    );
  }

  // ---------- 修改账号覆盖页 ----------
  function openEdit(): void {
    const paneWrap = h("div", { class: "edit-pane" });
    const subnav = h("div", { class: "edit-subnav" });

    const subs: Array<{ label: string; icon: string; render: (inner: HTMLElement) => void }> = [
      { label: "修改头像", icon: IC_FACE, render: renderAvatarPane },
      { label: "修改昵称", icon: IC_PEN, render: renderNickPane },
      { label: "数据同步", icon: IC_SYNC, render: renderSyncPane },
      { label: "账号注销", icon: IC_BAN, render: renderDeletePane },
    ];

    const items: HTMLButtonElement[] = [];
    subs.forEach((s, i) => {
      const item = h(
        "button",
        { class: "edit-subitem", onClick: () => selectSub(i) },
        h("span", { class: "edit-subicon", html: s.icon }),
        h("span", { text: s.label }),
      );
      items.push(item);
      subnav.appendChild(item);
    });

    function selectSub(i: number): void {
      items.forEach((it, idx) => it.classList.toggle("active", idx === i));
      clearDropZone(); // 上个子页可能注册过头像拖放区
      paneWrap.innerHTML = "";
      const inner = h("div", { class: "pane-inner" });
      subs[i].render(inner);
      paneWrap.appendChild(inner);
    }

    const overlay = h(
      "div",
      { class: "edit-overlay" },
      h(
        "div",
        { class: "edit-head" },
        h("div", { class: "edit-head-title", text: "修改账号信息" }),
        h("button", { class: "edit-close", html: IC_X, onClick: closeEdit }),
      ),
      h("div", { class: "edit-body" }, subnav, paneWrap),
    );
    document.body.appendChild(overlay);

    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.stopPropagation(); // 抢在 main.ts 的关窗监听前，只关覆盖页
        closeEdit();
      }
    };
    document.addEventListener("keydown", onKey, true);

    function closeEdit(): void {
      document.removeEventListener("keydown", onKey, true);
      clearDropZone();
      overlay.remove();
      paint(); // 刷新底层 banner/列表（昵称/头像可能已改）
    }

    // ----- 各子页 -----
    function renderAvatarPane(inner: HTMLElement): void {
      let current = avatarEl(96);
      const drop = h(
        "div",
        { class: "avatar-drop" },
        h("span", { class: "avatar-drop-hint", text: "拖放图片到这，或" }),
        h("button", { class: "btn btn-primary", text: "选择图片", onClick: pick }),
      );
      inner.appendChild(
        h(
          "div",
          { class: "pane-avatar" },
          current,
          drop,
          h("div", { class: "pane-tip", text: "支持拖入本地图片，自动裁剪为方形头像（本地保存）" }),
        ),
      );
      setDropZone(drop, (paths) => {
        if (paths[0]) void apply(paths[0]);
      });

      async function pick(): Promise<void> {
        const p = await api.pickImage();
        if (p) await apply(p);
      }
      async function apply(path: string): Promise<void> {
        try {
          profile = await api.setAvatar(path);
          const next = avatarEl(96);
          current.replaceWith(next);
          current = next; // 更新可变引用，下次替换才不会指向失效节点
          ctx.toast("头像已更新");
        } catch (err) {
          console.error("set_avatar failed", err);
          ctx.toast("头像更新失败");
        }
      }
    }

    function renderNickPane(inner: HTMLElement): void {
      let baseline = profile.nickname;
      const input = h("input", { class: "field-input", type: "text", placeholder: "输入新昵称" });
      input.value = baseline;
      const btn = h("button", { class: "btn btn-primary btn-block", text: "确定修改" });
      btn.disabled = true;
      input.addEventListener("input", () => {
        const v = input.value.trim();
        btn.disabled = v === "" || v === baseline.trim();
      });
      btn.addEventListener("click", async () => {
        const name = input.value.trim();
        if (!name) return;
        try {
          profile = await api.setNickname(name);
          baseline = profile.nickname;
          btn.disabled = true;
          ctx.toast("昵称已修改");
        } catch (err) {
          console.error("set_nickname failed", err);
          ctx.toast("修改失败");
        }
      });
      inner.appendChild(
        h("div", { class: "pane-form" }, h("label", { class: "field-label", text: "昵称" }), input, btn),
      );
    }

    function renderSyncPane(inner: HTMLElement): void {
      inner.appendChild(
        noticeBox(
          "info-box-blue",
          "数据同步",
          "iTools 采用本地优先存储：你的数据始终先保存在本机、离线可用。",
          "登录云账号后可将数据同步到云端、在多台设备间共享；未登录或云端未接入时，数据只保留在本地。",
        ),
      );

      if (!account.cloudConfigured) {
        inner.appendChild(
          noticeBox(
            "info-box-warn",
            "云端服务未接入",
            "当前版本未配置云端服务（ITOOLS_SYNC_ENDPOINT），暂无法同步，数据仅保存在本地。",
          ),
        );
        return;
      }
      if (!account.loggedIn) {
        inner.appendChild(noticeBox("info-box-warn", "未登录", "登录云账号后可开启登录同步。"));
        inner.appendChild(
          h("button", {
            class: "btn btn-primary btn-block",
            text: "去登录",
            onClick: () => {
              closeEdit();
              openLogin();
            },
          }),
        );
        return;
      }

      // 已登录 + 已配置：真实开关 + 立即同步
      const sw = makeSwitch(account.syncEnabled, async (checked) => {
        try {
          account = await api.setDataSync(checked);
          ctx.toast(checked ? "已开启登录同步" : "已关闭登录同步");
        } catch (err) {
          console.error("set_data_sync failed", err);
          ctx.toast("操作失败");
        }
      });
      const syncBtn = h("button", { class: "btn btn-primary btn-block", text: "立即同步" });
      syncBtn.addEventListener("click", async () => {
        syncBtn.disabled = true;
        syncBtn.textContent = "同步中…";
        try {
          const r = await api.syncNow();
          ctx.toast(syncResultMsg(r));
        } catch (err) {
          console.error("sync_now failed", err);
          ctx.toast("同步失败");
        } finally {
          syncBtn.disabled = false;
          syncBtn.textContent = "立即同步";
        }
      });
      inner.append(
        h("div", { class: "sync-row card" }, h("span", { text: "登录后自动同步" }), sw),
        syncBtn,
      );
    }

    function renderDeletePane(inner: HTMLElement): void {
      if (!account.cloudConfigured) {
        inner.appendChild(
          noticeBox(
            "info-box-warn",
            "云端服务未接入",
            "当前未配置云端服务，没有云端账号可注销。你的数据都在本地，可自行管理。",
          ),
        );
        return;
      }
      if (!account.loggedIn) {
        inner.appendChild(noticeBox("info-box-warn", "未登录", "注销云端账号需先登录。"));
        inner.appendChild(
          h("button", {
            class: "btn btn-primary btn-block",
            text: "去登录",
            onClick: () => {
              closeEdit();
              openLogin();
            },
          }),
        );
        return;
      }

      const warn = noticeBox(
        "info-box-warn",
        "注销云端账号",
        "将通过鉴权删除云端账号数据，本机随后切换为「游客」。该操作不可撤销，请谨慎操作！",
      );
      const user = field("用户名");
      const pass = field("密码", "password");
      const btn = h("button", { class: "btn btn-danger btn-block", text: "注销账号" });
      btn.disabled = true;
      const refresh = (): void => {
        btn.disabled = user.value.trim() === "" || pass.value.trim() === "";
      };
      user.addEventListener("input", refresh);
      pass.addEventListener("input", refresh);
      btn.addEventListener("click", async () => {
        try {
          account = await api.deleteAccount(user.value.trim(), pass.value.trim());
          ctx.toast("账号已注销，已切换为游客");
          closeEdit();
          void refreshAll();
        } catch (err) {
          console.error("delete_account failed", err);
          ctx.toast(typeof err === "string" ? err : "注销失败");
        }
      });
      inner.appendChild(
        h(
          "div",
          { class: "pane-form" },
          warn,
          h("label", { class: "field-label", text: "用户名" }),
          user,
          h("label", { class: "field-label", text: "密码" }),
          pass,
          btn,
        ),
      );
    }

    selectSub(0);
  }

  // ---------- 登录弹窗 ----------
  function openLogin(): void {
    const mask = h("div", { class: "modal-mask" });
    const content = h("div", { class: "pane-form" });

    if (!account.cloudConfigured) {
      content.appendChild(
        noticeBox(
          "info-box-blue",
          "云端服务未接入",
          "当前版本未配置云端服务（ITOOLS_SYNC_ENDPOINT），暂时只能本地使用，无法登录云账号。",
        ),
      );
    } else {
      const user = field("用户名");
      const pass = field("密码", "password");
      const btn = h("button", { class: "btn btn-primary btn-block", text: "登录" });
      btn.disabled = true;
      const refresh = (): void => {
        btn.disabled = user.value.trim() === "" || pass.value.trim() === "";
      };
      user.addEventListener("input", refresh);
      pass.addEventListener("input", refresh);
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
          account = await api.accountLogin(user.value.trim(), pass.value);
          ctx.toast("已登录");
          closeModal();
          void refreshAll();
        } catch (err) {
          console.error("account_login failed", err);
          ctx.toast(typeof err === "string" ? err : "登录失败");
          btn.disabled = false;
        }
      });
      content.append(
        h("label", { class: "field-label", text: "用户名" }),
        user,
        h("label", { class: "field-label", text: "密码" }),
        pass,
        btn,
      );
    }

    const modal = h(
      "div",
      { class: "modal" },
      h(
        "div",
        { class: "modal-title-row" },
        h("div", { class: "modal-title", text: "登录云账号" }),
        h("button", { class: "edit-close", html: IC_X, onClick: closeModal }),
      ),
      content,
    );
    mask.appendChild(modal);
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) closeModal();
    });
    document.body.appendChild(mask);

    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.stopPropagation();
        closeModal();
      }
    };
    document.addEventListener("keydown", onKey, true);

    function closeModal(): void {
      document.removeEventListener("keydown", onKey, true);
      mask.remove();
    }
  }

  // ---------- 退出账号弹窗 ----------
  function openLogout(): void {
    const allCheck = h("input", { type: "checkbox" });
    const mask = h("div", { class: "modal-mask" });
    const modal = h(
      "div",
      { class: "modal" },
      h("div", { class: "modal-title", text: "确认退出当前账号？" }),
      h("div", { class: "modal-warn", text: "退出后将切换为本地游客态；本机已缓存的账号资料会被清除。" }),
      h("label", { class: "modal-check" }, allCheck, h("span", { text: "同时退出所有已登录设备" })),
      h(
        "div",
        { class: "modal-actions" },
        h("button", { class: "btn btn-quiet", text: "取消", onClick: closeModal }),
        h("button", { class: "btn btn-danger", text: "退出账号", onClick: doLogout }),
      ),
    );
    mask.appendChild(modal);
    mask.addEventListener("mousedown", (e) => {
      if (e.target === mask) closeModal();
    });
    document.body.appendChild(mask);

    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.stopPropagation();
        closeModal();
      }
    };
    document.addEventListener("keydown", onKey, true);

    function closeModal(): void {
      document.removeEventListener("keydown", onKey, true);
      mask.remove();
    }
    async function doLogout(): Promise<void> {
      try {
        account = await api.logoutAccount(allCheck.checked);
        ctx.toast("已退出账号");
        closeModal();
        void refreshAll();
      } catch (err) {
        console.error("logout_account failed", err);
        ctx.toast("退出失败");
      }
    }
  }

  paint();
}
