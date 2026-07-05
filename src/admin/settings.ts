//! 设置面板：使用偏好 / 主题样式 / 高级设置 / 网络代理。
//! 改动即写入本地 settings 副本并防抖保存；主题切换即时应用。

import type { AdminCtx } from "./main";
import type { AppSettings, Theme } from "../types";
import { AUTO_CLEAR_NEVER } from "../types";
import { h, makeSwitch, bindHotkeyRecorder } from "./ui";
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
    row(
      "分离独立窗口快捷键",
      "把当前功能分离为独立窗口的组合键",
      hotkeyRecorder(() => settings.separate_hotkey, (hk) => (settings.separate_hotkey = hk)),
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

  root.appendChild(h("div", { class: "settings-scroll" }, usage, theme, advanced, proxy));
  void refreshThumb();
}
