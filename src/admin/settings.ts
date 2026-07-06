//! 设置面板：使用偏好 / 主题样式 / 高级设置 / 网络代理。
//! 改动即写入本地 settings 副本并防抖保存；主题切换即时应用。

import type { AdminCtx } from "./main";
import type { AppSettings, Theme } from "../types";
import { AUTO_CLEAR_NEVER } from "../types";
import { h, makeSwitch, bindHotkeyRecorder, formatHotkey } from "./ui";
import * as api from "./api";

export async function renderSettings(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let settings: AppSettings;
  try {
    settings = await api.getSettings();
  } catch (err) {
    console.error("get_settings failed", err);
    root.appendChild(h("div", { class: "panel-error", text: "设置加载失败" }));
    return;
  }

  // ---------- 防抖保存 ----------
  let saveTimer: number | undefined;
  function scheduleSave(): void {
    window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(save, 300);
  }
  async function save(): Promise<void> {
    try {
      await api.saveSettings(settings);
    } catch (err) {
      console.error("save_settings failed", err);
      ctx.toast("保存失败");
    }
  }

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
  /** 数值滑块 + 数值徽章 */
  function rangeControl(
    value: number,
    min: number,
    max: number,
    onChange: (v: number) => void,
    fmt: (v: number) => string,
  ): HTMLElement {
    const badge = h("span", { class: "value-badge", text: fmt(value) });
    const input = h("input", { type: "range", class: "slider-range" });
    input.min = String(min);
    input.max = String(max);
    input.value = String(value);
    input.addEventListener("input", () => {
      const v = Number(input.value);
      badge.textContent = fmt(v);
      onChange(v);
    });
    return h("div", { class: "slider-wrap" }, input, badge);
  }
  function select(current: string, options: Array<[string, string]>, onChange: (v: string) => void): HTMLSelectElement {
    const sel = h("select", { class: "select" });
    options.forEach(([v, t]) => sel.appendChild(h("option", { value: v, text: t })));
    sel.value = current;
    sel.addEventListener("change", () => onChange(sel.value));
    return sel;
  }
  function hotkeyRecorder(get: () => string, set: (hk: string) => void): HTMLInputElement {
    const input = h("input", { class: "hotkey-input", type: "text" });
    bindHotkeyRecorder(input, get, (hk) => {
      set(hk);
      scheduleSave();
    });
    return input;
  }

  // ---------- 使用偏好 ----------
  const usage = group(
    "使用偏好",
    row(
      "搜索框快捷键",
      "全局唤起 iTools 的组合键（至少含一个修饰键）",
      hotkeyRecorder(() => settings.hotkey, (hk) => (settings.hotkey = hk)),
    ),
    row(
      "截图快捷键",
      "内置原生截图（PixPin 风格：框选 · 就地标注 · 复制/保存/贴图/OCR），无需插件",
      hotkeyRecorder(() => settings.screenshot_hotkey, (hk) => (settings.screenshot_hotkey = hk)),
    ),
    row(
      "贴图快捷键",
      "把剪贴板里的图片钉成置顶浮窗（支持单功能键如 F3）",
      hotkeyRecorder(() => settings.pin_hotkey, (hk) => (settings.pin_hotkey = hk)),
    ),
    row(
      "自动清除搜索内容",
      "失焦后多久清空搜索框",
      select(
        String(settings.auto_clear_seconds),
        [
          ["0", "立即清除"],
          ["60", "1 分钟后"],
          ["180", "3 分钟后"],
          ["300", "5 分钟后"],
          ["600", "10 分钟后"],
          [String(AUTO_CLEAR_NEVER), "从不"],
        ],
        (v) => {
          settings.auto_clear_seconds = Number(v);
          scheduleSave();
        },
      ),
    ),
  );

  // ---------- 主题样式 ----------
  const bgThumb = h("div", { class: "bg-thumb" });
  async function refreshThumb(): Promise<void> {
    if (settings.background_image) {
      try {
        const url = await api.readImage(settings.background_image);
        bgThumb.style.backgroundImage = `url("${url}")`;
      } catch {
        bgThumb.style.backgroundImage = "";
      }
    } else {
      bgThumb.style.backgroundImage = "";
    }
  }

  const theme = group(
    "主题样式",
    row(
      "主题",
      "跟随系统或强制浅色 / 深色",
      select(
        settings.theme,
        [
          ["system", "跟随系统"],
          ["light", "浅色"],
          ["dark", "深色"],
        ],
        (v) => {
          settings.theme = v as Theme;
          ctx.applyTheme(settings.theme);
          scheduleSave();
        },
      ),
    ),
    row(
      "启用背景图",
      "关闭后保留已选图片但不渲染",
      makeSwitch(settings.background_enabled, (checked) => {
        settings.background_enabled = checked;
        scheduleSave();
      }),
    ),
    row(
      "背景图片",
      "选择本地图片作为搜索面板背景",
      bgThumb,
      h("button", {
        class: "btn",
        text: "选择图片",
        onClick: async () => {
          const p = await api.pickImage();
          if (p) {
            settings.background_image = p;
            scheduleSave();
            await refreshThumb();
          }
        },
      }),
      h("button", {
        class: "btn btn-quiet",
        text: "清除",
        onClick: async () => {
          settings.background_image = null;
          scheduleSave();
          await refreshThumb();
        },
      }),
    ),
    row(
      "背景暗化",
      "叠加暗色蒙版提升前景可读性",
      rangeControl(
        settings.background_dim,
        0,
        100,
        (v) => {
          settings.background_dim = v;
          scheduleSave();
        },
        (v) => `${v}%`,
      ),
    ),
    row(
      "搜索框不透明度",
      "毛玻璃底色的不透明程度",
      rangeControl(
        settings.opacity,
        1,
        255,
        (v) => {
          settings.opacity = v;
          scheduleSave();
        },
        (v) => `${Math.round((v / 255) * 100)}%`,
      ),
    ),
  );

  // ---------- 高级设置 ----------
  const placeholderInput = h("input", { class: "field-input-sm", type: "text", placeholder: "如 Hi, 输入以搜索" });
  placeholderInput.value = settings.search_placeholder;
  placeholderInput.addEventListener("input", () => {
    settings.search_placeholder = placeholderInput.value;
    scheduleSave();
  });

  const advanced = group(
    "高级设置",
    row("搜索框占位符", "搜索框空置时的提示文字（留空用默认问候）", placeholderInput),
    row(
      "开机启动",
      "登录 Windows 后自动运行 iTools",
      makeSwitch(settings.autostart, (checked) => {
        settings.autostart = checked;
        scheduleSave();
      }),
    ),
    // 分离独立窗口尚未实现：诚实标注「开发中，暂未生效」并禁用录制，不做「看着能用点了没反应」的控件。
    row(
      "分离独立窗口快捷键",
      "把当前功能分离为独立窗口的组合键（开发中，暂未生效）",
      h("span", {
        class: "value-badge value-badge-muted",
        text: formatHotkey(settings.separate_hotkey) || "未设置",
      }),
    ),
  );

  // ---------- 网络代理 ----------
  const proxyAddr = h("input", { class: "field-input-sm", type: "text", placeholder: "如 127.0.0.1:7890" });
  proxyAddr.value = settings.proxy_address;
  proxyAddr.addEventListener("input", () => {
    settings.proxy_address = proxyAddr.value;
    scheduleSave();
  });

  const proxy = group(
    "网络代理",
    row(
      "启用代理",
      "为插件网络请求走代理（演示）",
      makeSwitch(settings.proxy_enabled, (checked) => {
        settings.proxy_enabled = checked;
        scheduleSave();
      }),
    ),
    row("代理地址", "host:port 形式", proxyAddr),
  );

  // ---------- 关于 iTools ----------
  // 进入即用 get_app_version 显示当前版本（本地瞬时）；「检查更新」再联网比对 Gitee Releases。
  const versionBadge = h("span", { class: "value-badge", text: "…" });
  api
    .getAppVersion()
    .then((v) => (versionBadge.textContent = `v${v}`))
    .catch(() => (versionBadge.textContent = "未知"));

  const updateStatus = h("div", { class: "set-row-desc", text: "" });
  const statusRow = h("div", { class: "set-row" }, updateStatus);
  statusRow.style.display = "none";

  let latestUrl = "";
  let latestMsi: string | null = null;

  // 「前往下载」：在系统浏览器打开 release 页（手动下载，始终可用）。
  const downloadBtn = h("button", {
    class: "btn",
    text: "前往下载",
    onClick: () => {
      if (latestUrl) void api.openReleasePage(latestUrl);
    },
  });
  downloadBtn.style.display = "none";

  // 「立即更新」：自动下载 msi 并调起安装（随后退出 app）。仅当 release 附带 msi 直链时出现。
  const installBtn = h("button", { class: "btn btn-primary", text: "立即更新" });
  installBtn.style.display = "none";
  installBtn.addEventListener("click", async () => {
    if (!latestMsi) return;
    installBtn.disabled = true;
    installBtn.textContent = "下载中…";
    try {
      const path = await api.downloadUpdate(latestMsi);
      ctx.toast("下载完成，即将启动安装并退出 iTools");
      await api.launchInstaller(path); // 调起 msi 安装向导并退出当前进程
    } catch (err) {
      console.error("update install failed", err);
      ctx.toast(typeof err === "string" ? err : "更新失败");
      installBtn.disabled = false;
      installBtn.textContent = "立即更新";
    }
  });

  const checkBtn = h("button", {
    class: "btn",
    text: "检查更新",
    onClick: async () => {
      checkBtn.disabled = true;
      checkBtn.textContent = "检查中…";
      statusRow.style.display = "none";
      downloadBtn.style.display = "none";
      installBtn.style.display = "none";
      try {
        const info = await api.checkUpdate();
        versionBadge.textContent = `v${info.currentVersion}`;
        if (info.hasUpdate) {
          updateStatus.textContent = `发现新版本 v${info.latestVersion}，建议更新`;
          latestUrl = info.releaseUrl;
          latestMsi = info.msiUrl;
          downloadBtn.style.display = "";
          if (latestMsi) installBtn.style.display = "";
          ctx.toast(`发现新版本 v${info.latestVersion}`);
        } else {
          updateStatus.textContent = `已是最新版本（v${info.currentVersion}）`;
          ctx.toast("已是最新版本");
        }
        statusRow.style.display = "";
      } catch (err) {
        console.error("check_update failed", err);
        updateStatus.textContent = "检查失败，请检查网络后重试";
        statusRow.style.display = "";
        ctx.toast("检查更新失败");
      } finally {
        checkBtn.disabled = false;
        checkBtn.textContent = "检查更新";
      }
    },
  });

  const about = group(
    "关于 iTools",
    row("当前版本", "从 Gitee 检查是否有新版本", versionBadge, checkBtn, downloadBtn, installBtn),
    statusRow,
  );

  root.appendChild(h("div", { class: "settings-scroll" }, usage, theme, advanced, proxy, about));
  void refreshThumb();
}
