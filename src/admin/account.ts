//! 我的账号（纯本地模拟）：首页 banner + 账号操作列表；
//! 「修改账号」覆盖页（6 子导航）与「退出账号」居中弹窗。

import type { AdminCtx } from "./main";
import type { ProfileView } from "../types";
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

export async function renderAccount(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let profile: ProfileView;
  try {
    profile = await api.getProfile();
  } catch (err) {
    console.error("get_profile failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "账号信息加载失败" }));
    return;
  }

  /** 构造一个圆形头像元素：有头像路径则异步载入，否则显示首字母。 */
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

    const banner = h(
      "div",
      { class: "acc-banner" },
      h("div", { class: "acc-watermark", text: "¥" }),
      avatarEl(64),
      h(
        "div",
        { class: "acc-banner-info" },
        h("div", { class: "acc-name-row" }, h("span", { class: "acc-name", text: displayName(profile.nickname) })),
        h("div", {
          class: "acc-sub",
          text: `${profile.phone}　|　iTools 已陪伴你 ${profile.companion_days} 天`,
        }),
      ),
    );

    const list = h(
      "div",
      { class: "acc-list card" },
      accRow("修改账号", openEdit),
      accRow("退出账号", openLogout),
    );

    root.append(banner, list);
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
      { label: "关闭数据同步", icon: IC_SYNC, render: renderSyncPane },
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
          h("div", { class: "pane-tip", text: "支持拖入本地图片，自动裁剪为方形头像" }),
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
      const info = h(
        "div",
        { class: "info-box info-box-blue" },
        h("div", { class: "info-title", text: "数据同步" }),
        h("div", {
          text: "1. 本地磁盘存储的数据，重装操作系统或使用安全软件清理误删文件等行为会导致数据丢失。开启数据同步可将本地磁盘数据备份到 iTools 云端服务器，将很大提高您数据的安全性！",
        }),
        h("div", { text: "2. 开启数据同步，数据可在您多台电脑之间无缝同步协同！" }),
        h("div", { text: "3. 数据同步是 iTools 会员专属权益！" }),
      );
      const sw = makeSwitch(profile.data_sync_enabled, async (checked) => {
        try {
          profile = await api.setDataSync(checked);
          ctx.toast(checked ? "已开启数据同步" : "已关闭数据同步");
        } catch (err) {
          console.error("set_data_sync failed", err);
          ctx.toast("操作失败");
        }
      });
      inner.append(info, h("div", { class: "sync-row card" }, h("span", { text: "账号数据同步功能" }), sw));
    }

    function renderDeletePane(inner: HTMLElement): void {
      const warn = h("div", {
        class: "info-box info-box-warn",
        text: "删除服务器上的账号数据，同时 iTools 将切换到「游客」数据，该行为无法撤销，请谨慎操作！",
      });
      const user = h("input", { class: "field-input", type: "text", placeholder: "用户名" });
      const pass = h("input", { class: "field-input", type: "password", placeholder: "密码" });
      const btn = h("button", { class: "btn btn-danger btn-block", text: "注销账号" });
      btn.disabled = true;
      const refresh = (): void => {
        btn.disabled = user.value.trim() === "" || pass.value.trim() === "";
      };
      user.addEventListener("input", refresh);
      pass.addEventListener("input", refresh);
      btn.addEventListener("click", async () => {
        try {
          profile = await api.deleteAccount(user.value.trim(), pass.value.trim());
          ctx.toast("账号已注销，已切换为游客");
          closeEdit();
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

  // ---------- 退出账号弹窗（无 profile 参数：noUnusedParameters） ----------
  function openLogout(): void {
    const allCheck = h("input", { type: "checkbox" });
    const mask = h("div", { class: "modal-mask" });
    const modal = h(
      "div",
      { class: "modal" },
      h("div", { class: "modal-title", text: "确认退出当前账号？" }),
      h("div", { class: "modal-warn", text: "退出后，本设备上的该账号数据将无法继续访问。" }),
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
        profile = await api.logoutAccount(allCheck.checked);
        ctx.toast("已退出账号");
        closeModal();
        paint();
      } catch (err) {
        console.error("logout_account failed", err);
        ctx.toast("退出失败");
      }
    }
  }

  paint();
}
