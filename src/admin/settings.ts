//! 设置面板：使用偏好 / 主题样式 / 高级设置 / 关于。
//! 网络相关（云同步服务器、网络代理）已迁至「网络设置」页（admin/network.ts）。
//! 改动即写入本地 settings 副本并防抖保存；主题切换即时应用。

import type { AdminCtx } from "./main";
import type { AppSettings, Theme, UpdateInfo } from "../types";
import { AUTO_CLEAR_NEVER } from "../types";
import { h, makeSwitch, bindHotkeyRecorder, formatHotkey, panelError } from "./ui";
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

export async function renderSettings(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  let settings: AppSettings;
  try {
    settings = await api.getSettings();
  } catch (err) {
    console.error("get_settings failed", err);
    panelError(root, "设置加载失败", () => void renderSettings(root, ctx));
    return;
  }

  // ---------- 防抖保存 ----------
  let saveTimer: number | undefined;
  function scheduleSave(): void {
    window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => {
      saveTimer = undefined;
      void save();
    }, 300);
  }
  async function save(): Promise<void> {
    try {
      await api.saveSettings(settings);
    } catch (err) {
      console.error("save_settings failed", err);
      // 后端给的是可照做的中文原因（「已开启网络代理，但没有填写代理地址（形如 127.0.0.1:7897）」
      // 「代理端口不合法：应为 1~65535 的数字」…），吞成「保存失败」等于把唯一的修正指引丢掉。
      ctx.toast(errText(err));
    }
  }

  // 切走本面板时，把还在 300ms 防抖窗口里的改动立刻落盘。
  // 否则「刚改完代理地址就点『去账号页修改』」会静默丢掉最后一次编辑 ——
  // 本页新增的跳转按钮正是这样一条「编辑完立刻离开」的路径。
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

  // 发行线：官网版与开源版是两条**独立**的发行线，更新路径不交叉。
  // 把它明明白白摆在界面上——用户装了哪一份、更新会从哪拿包，不该靠猜。
  const channelDesc = h("div", { class: "set-row-desc", text: "正在读取发行线…" });
  const channelRow = h("div", { class: "set-row" }, channelDesc);
  // 被跨线覆盖过时的告警（如官网版被开源版顶掉，云服务接入会静默失效）
  const channelWarn = h("div", { class: "info-box info-box-warn" });
  channelWarn.style.display = "none";

  let latestUrl = "";
  let latestInstaller: string | null = null;

  // 「前往下载」：在系统浏览器打开 release 页（手动下载，始终可用）。
  const downloadBtn = h("button", {
    class: "btn",
    text: "前往下载",
    onClick: () => {
      if (latestUrl) void api.openReleasePage(latestUrl);
    },
  });
  downloadBtn.style.display = "none";

  // 「立即更新」：自动下载安装包并调起安装向导（随后退出 app）。仅当 release 附带安装包直链时出现。
  const installBtn = h("button", { class: "btn btn-primary", text: "立即更新" });
  installBtn.style.display = "none";
  installBtn.addEventListener("click", async () => {
    if (!latestInstaller) return;
    installBtn.disabled = true;
    installBtn.textContent = "下载中…";
    try {
      const path = await api.downloadUpdate(latestInstaller);
      ctx.toast("下载完成，即将启动安装并退出 iTools");
      await api.launchInstaller(path); // 调起 NSIS 安装向导并退出当前进程
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
        renderResult(info);
        ctx.toast(info.hasUpdate ? `发现新版本 v${info.latestVersion}` : "已是最新版本");
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

  /** 把一次检查结果渲染到状态行 + 决定要不要露出更新按钮。
   *
   *  手动检查与「进页面时读到的自动检查结果」共用它——两处各写一遍，
   *  迟早出现「手动说有新版、自动那边还显示已是最新」的分叉。 */
  function renderResult(info: UpdateInfo): void {
    versionBadge.textContent = `v${info.currentVersion}`;
    if (info.hasUpdate) {
      updateStatus.textContent = `发现新版本 v${info.latestVersion}，建议更新（来自 ${info.source}）`;
      latestUrl = info.releaseUrl;
      latestInstaller = info.installerUrl;
      downloadBtn.style.display = "";
      if (latestInstaller) installBtn.style.display = "";
    } else {
      updateStatus.textContent = `已是最新版本（v${info.currentVersion}）`;
      downloadBtn.style.display = "none";
      installBtn.style.display = "none";
    }
    statusRow.style.display = "";
  }

  /** 「N 分钟前」——用于说明这个结论是什么时候得出的。 */
  function agoText(ms: number): string {
    const diff = Date.now() - ms;
    if (diff < 0) return "";
    const mins = Math.floor(diff / 60000);
    if (mins < 1) return "刚刚";
    if (mins < 60) return `${mins} 分钟前`;
    if (mins < 1440) return `${Math.floor(mins / 60)} 小时前`;
    return new Date(ms).toLocaleString();
  }

  // 进页面就把**自动检查**的结果显示出来（本地瞬时、不发请求）。
  // 不做这一步的话，「查过了没新版」与「压根没查成」在界面上完全一样，
  // 用户只能反复手动点——这正是本轮要修的问题。
  void api
    .updateStatus()
    .then((st) => {
      versionBadge.textContent = `v${st.currentVersion}`;
      channelDesc.textContent = st.channelDesc;
      if (st.channelSwitchNote) {
        // 跨线覆盖是静默发生的，不主动说没人会发现——这里必须显眼
        channelWarn.replaceChildren(
          h("div", { text: "这台机器的安装来源变过" }),
          h("div", { class: "info-box-detail", text: st.channelSwitchNote }),
        );
        channelWarn.style.display = "";
      }
      if (st.checkedAt === 0) {
        // 启动后还没到第一次自动检查（20 秒）——如实说，不要让人以为「检查过且没新版」
        updateStatus.textContent = "启动后还没有自动检查过，可点右侧手动检查。";
        statusRow.style.display = "";
        return;
      }
      const when = agoText(st.checkedAt);
      if (st.error) {
        updateStatus.textContent = `${when}自动检查失败：${st.error}`;
        statusRow.style.display = "";
        return;
      }
      if (st.info) {
        renderResult(st.info);
        updateStatus.textContent += `（${when}自动检查）`;
      }
    })
    .catch((err) => {
      console.error("update_status failed", err);
      channelDesc.textContent = "没能读到发行线信息（更新源未知）。";
    });

  // ---------- 运行日志 ----------
  // iTools 会把运行日志写成一个文件（release 在 %LOCALAPPDATA%\itools\itools.log，2MB 轮转），
  // 但在此之前**界面上没有任何地方告诉用户它在哪**——「出问题我们拿得到证据」就没闭环：
  // 用户想反馈问题时压根不知道该发什么给我们。这一行就是那个入口。
  //
  // 路径一律**从后端取**：debug 与 release 的落点本来就不同，数据根还可能因为取不到
  // LOCALAPPDATA 而回退到临时目录，前端写死一份必然有对不上的那天。
  const logPathDesc = h("div", { class: "set-row-desc", text: "正在定位日志文件…" });
  const openLogBtn = h("button", { class: "btn", text: "打开日志目录" });
  // 路径还没拿到之前先禁用：这时点了后端也定位不出目录，不做「看着能点、点了没反应」的控件
  openLogBtn.disabled = true;
  openLogBtn.addEventListener("click", async () => {
    try {
      await api.openLogDir();
    } catch (err) {
      console.error("open_log_dir failed", err);
      // 后端会说清是「目录不存在」还是「打开失败」，原样透出比自造文案有用
      ctx.toast(errText(err));
    }
  });
  void api
    .logFilePath()
    .then((p) => {
      logPathDesc.textContent = `遇到问题时，请把这个文件发给我们：${p}`;
      openLogBtn.disabled = false;
    })
    .catch((err) => {
      console.error("log_file_path failed", err);
      // 定位不出来就如实说，并让按钮保持禁用——宁可少一个按钮，也不给一个点了没用的
      logPathDesc.textContent = `定位日志文件失败：${errText(err)}`;
    });
  const logRow = h(
    "div",
    { class: "set-row" },
    h(
      "div",
      { class: "set-row-text" },
      h("div", { class: "set-row-label", text: "运行日志" }),
      logPathDesc,
    ),
    h("div", { class: "set-row-control" }, openLogBtn),
  );

  const about = group(
    "关于 iTools",
    row("当前版本", "每小时自动检查一次新版本；也可以随时手动检查", versionBadge, checkBtn, downloadBtn, installBtn),
    statusRow,
    channelRow,
    channelWarn,
    logRow,
  );

  root.appendChild(
    h("div", { class: "settings-scroll" }, usage, theme, advanced, about),
  );
  void refreshThumb();
}
