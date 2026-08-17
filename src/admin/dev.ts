//! 开发者中心：正在开发的插件的**独立调试环境**面板。
//!
//! 为什么要有它：正在写的插件不该和已安装的正式插件共用一套环境 —— 调试时随手写进去的垃圾数据
//! 会进正式库、甚至被云同步推到服务器；改错一个字段插件就从列表里消失、无从排查。
//! 所以调试插件跑在一整套独立环境里：
//!   · 加载源：固定的 dev-plugins/ 目录 + 用户手动添加的任意本地目录（可直接指向构建产物）
//!   · 存储　：独立的 SQLite 测试文件（物理隔离，云同步扫不到它 → 调试数据永不上云）
//!   · 文件　：独立的沙盒根
//!   · 授权　：独立的调试授权表（不影响正式安装后的授权状态）
//!   · 同步　：本地 Mock（插件调 itools.data.sync() 拿到这里配置的结果，不联网）
//!   · 窗口　：独立的调试窗口，可与正式插件窗口并存
//!
//! 诚信约束（doc/开发准则.md）：
//! - 本面板不产生任何假数据：插件清单、校验问题、日志、存储、授权态全部来自后端命令的真实返回；
//! - 校验问题（DevIssue）逐条如实列出，`runnable=false` 的插件「运行」按钮**禁用并写明原因**，
//!   不做「看着能点、点了没反应」的控件；
//! - 「调试数据不会上云」这句话敢写死，是因为它由独立数据库文件**物理保证**，不是前端过滤；
//! - 正常降级（没有调试插件 / 目录为空 / 没有日志）一律渲染成引导或空态，**不渲染成故障**。

import type { AdminCtx } from "./main";
import type {
  DevConfig,
  DevFeature,
  DevIssue,
  DevMockConfig,
  DevPluginInfo,
  AiClient,
  AiClientsStatus,
} from "../types";
import { h, makeSwitch, panelError } from "./ui";
import * as api from "./api";
import { errText, permLabel } from "./plugin-install";
import { createDevLogView, type DevLogView } from "./dev-log";
import { renderDevStorage } from "./dev-storage";
import { renderDevPublish } from "./dev-publish";

const IC_REFRESH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/></svg>';
const IC_FOLDER =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V17a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>';
const IC_MINUS =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round"><line x1="6" y1="12" x2="18" y2="12"/></svg>';
const PLUGIN_GLYPH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';

type TabId = "run" | "log" | "storage" | "perm" | "mock" | "publish";

const TABS: Array<{ id: TabId; label: string }> = [
  { id: "run", label: "运行" },
  { id: "log", label: "日志" },
  { id: "storage", label: "存储" },
  { id: "perm", label: "权限" },
  { id: "mock", label: "同步 Mock" },
  { id: "publish", label: "发布" },
];

/** 同步 Mock 的模式取值与说明（取值即后端 `DevMockConfig.mode`）。 */
const MOCK_MODES: Array<{ mode: string; title: string; desc: string }> = [
  { mode: "success", title: "同步成功", desc: "返回 synced=true，并带上下面伪造的上行/下行条数。" },
  { mode: "fail", title: "同步失败", desc: "返回失败，reason 用下面填的文本 —— 用来测插件的错误分支。" },
  { mode: "offline", title: "离线", desc: "模拟连不上服务器（正式环境里对应 reason=offline）。" },
  { mode: "notLoggedIn", title: "未登录", desc: "模拟用户没登录云账号（正式环境里对应 reason=not_logged_in）。" },
];

/** 触发方式（对应 `dev_open_plugin` 的 kind 参数）。 */
const TRIGGER_KINDS: Array<{ kind: string; label: string; desc: string }> = [
  { kind: "keyword", label: "关键字", desc: "模拟用户在主搜索里输入关键字命中该 feature。" },
  { kind: "regex", label: "正则", desc: "模拟输入被该 feature 声明的正则匹配命中。" },
  { kind: "text", label: "文本", desc: "模拟把一段文本（如选中的长文本）投给该 feature。" },
];

// ---------- 跨渲染保留的界面状态（切页回来还停在原来的插件/tab） ----------
let selectedId: string | null = null;
let activeTab: TabId = "run";

/** 当前挂载的面板实例（用于切页时解绑日志轮询与事件监听）。 */
interface ActiveDev {
  logView: DevLogView;
  offVisibility: (() => void) | null;
}
let active: ActiveDev | null = null;
/** 渲染代数：renderDev 里每个 await 之后都要核对，避免「切走后异步才返回」的实例把自己挂上去。 */
let generation = 0;

/** 卸载当前实例：停日志轮询、解事件监听。切换左侧导航项时由 main.ts 调用。 */
function disposeDev(): void {
  generation++;
  const prev = active;
  active = null;
  if (!prev) return;
  try {
    prev.logView.dispose();
  } catch (err) {
    console.error("dispose dev log view failed", err);
  }
  if (prev.offVisibility) {
    try {
      prev.offVisibility();
    } catch (err) {
      console.error("remove dev visibility listener failed", err);
    }
  }
}

/** 目录路径的末段（用于展示插件所在目录名）。 */
function baseName(p: string): string {
  const parts = p.replace(/[\\/]+$/, "").split(/[\\/]/);
  return parts[parts.length - 1] || p;
}

/** 校验结论：有 error / 只有 warn / 干净。 */
function issueLevel(issues: DevIssue[]): "error" | "warn" | "ok" {
  if (issues.some((i) => i.level === "error")) return "error";
  if (issues.length) return "warn";
  return "ok";
}

export async function renderDev(root: HTMLElement, ctx: AdminCtx): Promise<void> {
  disposeDev(); // 上一个实例（若有）就此作废：保证全局至多一个日志轮询
  ctx.registerDispose(disposeDev);
  const gen = generation;

  root.innerHTML = "";
  const scroll = h("div", { class: "dev-scroll" });
  root.appendChild(scroll);
  scroll.appendChild(h("div", { class: "detail-loading", text: "加载调试环境…" }));

  let config: DevConfig;
  let plugins: DevPluginInfo[];
  try {
    [config, plugins] = await Promise.all([api.devGetConfig(), api.devListPlugins()]);
  } catch (err) {
    if (gen !== generation) return;
    console.error("dev center load failed", err);
    panelError(scroll, "开发者中心加载失败：" + errText(err, "未知错误"), () => {
      root.innerHTML = "";
      void renderDev(root, ctx);
    });
    return;
  }
  if (gen !== generation) return; // 加载期间已切走：不要把界面挂到已被清空的容器上

  // 日志视图**只创建一次**并常驻（切 tab 只是把它从 DOM 摘下/挂回）：
  // 轮询若随 tab 停摆，后端缓冲区滚掉的日志就永远看不到了。
  const logView = createDevLogView({ getPlugins: () => plugins, toast: ctx.toast });
  const instance: ActiveDev = { logView, offVisibility: null };
  active = instance;

  // ---------- 顶部说明 ----------
  // ==================== AI 助手接入 ====================
  //
  // 这一段是开发者中心的门面。设计上只讲一件事：**让你的 AI 会写 iTools 插件**。
  //
  // 早先这里是三张并列的卡片（MCP 连接配置 / skill 安装 / 调试环境说明），
  // 每张都带大段解释文字，光是读完就得半分钟——而用户真正要做的只有一个动作。
  // 现在按**客户端**组织：一行一个 AI 助手，一个按钮同时装好 MCP 与 skill；
  // 每处说明压到一句；不影响操作的背景知识（调试环境怎么隔离的）折叠起来，想看再看。

  /** AI 接入状态（异步读到后重绘）。 */
  let ai: AiClientsStatus | null = null;

  /** 页头：一句话说清这页是干什么的 + 右上角环境状态。 */
  function aiHeader(): HTMLElement {
    const box = h("div", { class: "dev-hero" });
    const left = h(
      "div",
      {},
      h("div", { class: "dev-hero-kicker", text: "AI 插件创作" }),
      h("h2", { class: "dev-hero-title", text: "让 AI 帮你写插件" }),
      h("div", {
        class: "dev-hero-sub",
        text: "连接、规范、调试都由 iTools 准备好，你只需要描述想法。",
      }),
    );
    box.appendChild(left);

    // 右上角状态：来自后端实际状态，不是写死的装饰
    const ok = !!ai?.mcpRunning;
    const pill = h(
      "div",
      { class: "dev-env-pill" + (ok ? "" : " dev-env-pill-off") },
      h("span", { class: "dev-env-dot" }),
      h("span", {
        text: !ai
          ? "正在检查环境…"
          : ok
            ? "本地开发环境已就绪"
            : "MCP 服务器未运行",
      }),
    );
    if (ai && !ai.mcpRunning) pill.title = ai.mcpUrl || "开发者中心的 MCP 服务器没有起来";
    box.appendChild(pill);
    return box;
  }

  /** 「AI 助手接入」主卡片。 */
  function aiClientsCard(): HTMLElement {
    const box = h("div", { class: "dev-ai-card" });

    const head = h(
      "div",
      { class: "dev-ai-head" },
      h("span", { class: "dev-ai-icon", text: "✦" }),
      h(
        "div",
        { class: "dev-ai-head-text" },
        h("strong", { text: "AI 助手接入" }),
        h("small", { text: "选择你使用的 AI 助手，iTools 会同时装好 MCP 连接和插件开发 Skill。" }),
      ),
    );
    if (ai) {
      head.appendChild(
        h("span", {
          class: "dev-ai-count",
          text: ai.detectedCount > 0 ? `已检测到 ${ai.detectedCount} 个客户端` : "未检测到客户端",
        }),
      );
    }
    box.appendChild(head);

    if (!ai) {
      box.appendChild(h("div", { class: "dev-ai-loading", text: "正在读取接入状态…" }));
      return box;
    }

    // 拿不到接入地址 / 没有 skill 源时，按钮一律不可用——并且**说清楚为什么**，
    // 而不是摆一个点了必然失败的按钮
    const blocker = !ai.mcpRunning
      ? "MCP 服务器没有运行，拿不到接入地址。"
      : !ai.skillSourceAvailable
        ? ai.skillSourceError ?? "这个安装包里没带 skill 源文件。"
        : null;
    if (blocker) {
      box.appendChild(
        h("div", { class: "info-box info-box-warn" }, h("div", { class: "info-box-detail", text: blocker })),
      );
    }

    const list = h("div", { class: "dev-ai-list" });
    for (const c of ai.clients) list.appendChild(aiClientRow(c, blocker));
    box.appendChild(list);

    box.appendChild(
      h(
        "div",
        { class: "dev-ai-tip" },
        h("span", { class: "dev-ai-tip-mark", text: "✓" }),
        h("span", { text: "无需复制配置；装完重启 AI 助手，就可以直接让它创建 iTools 插件。" }),
      ),
    );
    return box;
  }

  /** 客户端图标：用首字母 + 品牌色块，避免为三个 logo 引入图片资源。 */
  function clientMark(id: string): HTMLElement {
    const text = id === "claude" ? "C" : id === "codex" ? "◇" : "◌";
    return h("span", { class: `dev-ai-mark dev-ai-mark-${id}`, text });
  }

  /** 一行一个 AI 助手。 */
  function aiClientRow(c: AiClient, blocker: string | null): HTMLElement {
    const row = h("div", { class: "dev-ai-row" + (c.ready ? " dev-ai-row-on" : "") });

    row.append(
      clientMark(c.id),
      h(
        "div",
        { class: "dev-ai-name" },
        h("strong", { text: c.label }),
        h("small", {
          text: c.detected ? "已检测到客户端" : "未检测到，装了也不影响",
          title: `MCP 配置：${c.configPath}\nSkill 目录：${c.skillPath}`,
        }),
      ),
    );

    // 两个能力标签：各自如实反映状态，而不是笼统一个「已安装」
    const tags = h("div", { class: "dev-ai-tags" });
    const tag = (label: string, on: boolean, warn: boolean, tip: string) =>
      h("span", {
        class: "dev-ai-tag" + (warn ? " dev-ai-tag-warn" : on ? " dev-ai-tag-on" : ""),
        text: label,
        title: tip,
      });
    tags.append(
      tag(
        "MCP",
        c.mcpReady,
        c.mcpStale,
        c.mcpStale ? "配着的地址与当前监听的不一致，点「重新安装」可修正" : c.mcpReady ? "已接入" : "未接入",
      ),
      tag(
        "SKILL",
        c.skillReady,
        c.skillOutdated,
        c.skillOutdated
          ? `已装 v${c.skillVersion ?? "?"}，随包版本是 v${ai?.bundledVersion ?? "?"}`
          : c.skillReady
            ? `已装 v${c.skillVersion ?? "?"}`
            : "未安装",
      ),
    );
    row.appendChild(tags);

    const stateText = c.ready
      ? "已接入"
      : c.mcpStale || c.skillOutdated
        ? "需更新"
        : c.mcpReady || c.skillReady
          ? "未完成"
          : "未接入";
    row.appendChild(
      h(
        "div",
        { class: "dev-ai-state" },
        h("span", { class: "dev-ai-state-dot" + (c.ready ? " on" : "") }),
        h("span", { text: stateText }),
      ),
    );

    const actions = h("div", { class: "dev-ai-actions" });
    const disabled = !!blocker || !c.cliReady;
    const btn = h("button", {
      class: "btn btn-sm" + (c.ready ? "" : " btn-primary"),
      text: c.ready ? "重新安装" : c.mcpReady || c.skillReady ? "补全接入" : "一键安装",
    }) as HTMLButtonElement;
    if (disabled) {
      btn.disabled = true;
      btn.title = blocker ?? c.note ?? "缺少必要的命令行工具";
    }
    btn.addEventListener("click", () => void runAi(btn, () => api.aiClientInstall(c.id), `已为 ${c.label} 完成接入`));
    actions.appendChild(btn);

    if (c.mcpReady || c.skillReady) {
      const off = h("button", { class: "btn btn-sm", text: "断开" }) as HTMLButtonElement;
      off.addEventListener("click", () =>
        void runAi(off, () => api.aiClientUninstall(c.id), `已断开 ${c.label}`),
      );
      actions.appendChild(off);
    }
    row.appendChild(actions);

    // note 里是「为什么不能装 / 为什么状态不对」，属于要看见的信息，但不该常驻——
    // 只有真的有问题时才占一行
    if (c.note) {
      const wrap = h("div", { class: "dev-ai-row-wrap" }, row);
      wrap.appendChild(h("div", { class: "dev-ai-note", text: c.note }));
      return wrap;
    }
    return h("div", { class: "dev-ai-row-wrap" }, row);
  }

  /** 安装/卸载的公共执行体：禁用按钮 → 调命令 → 用后端原文报结果。 */
  async function runAi(
    btn: HTMLButtonElement,
    op: () => Promise<AiClientsStatus>,
    okMsg: string,
  ): Promise<void> {
    btn.disabled = true;
    const before = btn.textContent;
    btn.textContent = "处理中…";
    try {
      ai = await op();
      ctx.toast(okMsg);
      paint();
    } catch (err) {
      console.error("ai client op failed", err);
      // 后端会说清「skill 成了但 MCP 没成」这类半成功情况，原文透给用户
      ctx.toast(errText(err, "操作失败"));
      btn.disabled = false;
      btn.textContent = before;
      // 失败后也要刷新状态：半成功时界面必须反映真实结果
      void api.aiClientsStatus().then((st) => {
        ai = st;
        paint();
      }).catch(() => {});
    }
  }

  /** 底部两栏：左边示例提示词，右边 AI 的自动流程。 */
  function aiShowcase(): HTMLElement {
    const box = h("div", { class: "dev-ai-showcase" });
    box.append(
      h(
        "div",
        { class: "dev-ai-prompt" },
        h("span", { class: "dev-ai-prompt-kicker", text: "试试这样说" }),
        h("p", { text: "“给 iTools 做个字数统计插件，输入文字后显示字数和阅读时间。”" }),
      ),
      h(
        "div",
        { class: "dev-ai-flow" },
        h("strong", { text: "AI 会自动完成" }),
        h(
          "div",
          { class: "dev-ai-flow-steps" },
          ...["读取规范", "创建插件", "运行调试"].flatMap((s) => [
            h("span", { class: "dev-ai-step", text: s }),
            h("i", { class: "dev-ai-arrow", text: "→" }),
          ]),
          h("span", { class: "dev-ai-step dev-ai-step-done", text: "完成" }),
        ),
      ),
    );
    return box;
  }

  /** 调试环境的背景说明。**默认折叠**——它是「需要时才查」的知识，不是每次进页面都要读的。
   *
   *  但不能删：里面两条是**行为差异**（name 校验放宽、settings 只给默认值），
   *  不写出来开发者会当成 bug 去查（见开发准则：行为差异必须在 UI 上告知）。 */
  function envNote(): HTMLElement {
    const box = h("details", { class: "dev-envnote" });
    box.appendChild(h("summary", { text: "调试环境是怎么隔离的？（点开查看）" }));
    for (const t of [
      "正在开发的插件跑在独立的加载目录、独立的测试数据库、独立的沙盒与授权表里，云同步也被 Mock 掉了。调试怎么折腾都碰不到已安装的正式插件，也不会把调试数据传上云端。",
      "正式环境要求 plugin.json 的 name 与目录名一致，不一致直接跳过；调试环境刻意放宽了这条 —— 照样加载，但会在下面把问题如实标出来。所以「调试能跑」不等于「能发布」。",
      "调试会话里 itools.settings.get() / all() 只返回 settings.json 声明的 default，读不到用户设置值 —— 这是刻意隔离。想验证不同取值，请直接改 settings.json 里的 default 再重新运行。",
    ]) {
      box.appendChild(h("p", { text: t }));
    }
    return box;
  }

  function dirsCard(): HTMLElement {
    const list = h("div", { class: "dev-dir-list" });
    for (const d of config.dirs) {
      const tags = h("div", { class: "dev-dir-tags" });
      if (d.fixed) {
        tags.appendChild(
          h("span", {
            class: "plugin-badge plugin-badge-muted",
            text: "固定",
            title: "随程序约定的调试目录，不可移除",
          }),
        );
      }
      if (!d.exists) {
        tags.appendChild(
          h("span", {
            class: "plugin-badge dev-badge-err",
            text: "目录不存在",
            title: "该路径当前不存在（可能被删除或改名），本次扫描跳过了它",
          }),
        );
      }
      tags.appendChild(
        h("span", { class: "dev-dir-count", text: `${d.pluginCount} 个插件` }),
      );

      const row = h(
        "div",
        { class: "dev-dir-row" + (d.exists ? "" : " dev-dir-missing") },
        h("span", { class: "dev-dir-ic", html: IC_FOLDER }),
        h("code", { class: "dev-dir-path", text: d.path, title: d.path }),
        tags,
      );

      if (!d.fixed) {
        const rm = h("button", { class: "icon-btn", html: IC_MINUS, title: "移除这个调试目录" });
        rm.addEventListener("click", async () => {
          rm.disabled = true;
          try {
            config = await api.devRemoveDir(d.path);
            await reloadPlugins();
            ctx.toast("已移除调试目录");
          } catch (err) {
            console.error("dev_remove_dir failed", err);
            ctx.toast(errText(err, "移除失败"));
            rm.disabled = false;
          }
        });
        row.appendChild(rm);
      }
      list.appendChild(row);
    }

    if (!config.dirs.length) {
      // 后端理论上至少给出固定的 dev-plugins/；真的一个都没有时如实说明，不假装有
      list.appendChild(
        h("div", { class: "dev-dir-empty", text: "后端没有返回任何调试目录。" }),
      );
    }

    const addBtn = h("button", { class: "btn btn-sm", text: "添加调试目录" });
    addBtn.addEventListener("click", async () => {
      addBtn.disabled = true;
      try {
        const picked = await api.devPickDir();
        if (picked) {
          config = await api.devAddDir(picked);
          await reloadPlugins();
          ctx.toast("已添加调试目录");
        }
      } catch (err) {
        console.error("dev_add_dir failed", err);
        ctx.toast(errText(err, "添加失败"));
      } finally {
        addBtn.disabled = false;
      }
    });

    const rescanBtn = h("button", { class: "icon-btn", html: IC_REFRESH, title: "重新扫描全部调试目录" });
    rescanBtn.addEventListener("click", async () => {
      rescanBtn.disabled = true;
      try {
        // dev_rescan 的返回值是后端的统计口径（条命令/个插件语义未在契约里定死），
        // 这里不拿它当文案 —— 改用重新拉到的插件数报告，说的每个数字都确定是什么意思。
        await api.devRescan();
        config = await api.devGetConfig();
        await reloadPlugins();
        ctx.toast(`已重新扫描：${plugins.length} 个调试插件`);
      } catch (err) {
        console.error("dev_rescan failed", err);
        ctx.toast(errText(err, "重新扫描失败"));
      } finally {
        rescanBtn.disabled = false;
      }
    });

    const searchSw = makeSwitch(config.searchVisible, async (checked) => {
      try {
        await api.devSetSearchVisible(checked);
        config.searchVisible = checked;
        ctx.toast(checked ? "调试插件已加入主搜索" : "调试插件已退出主搜索");
      } catch (err) {
        console.error("dev_set_search_visible failed", err);
        ctx.toast(errText(err, "设置失败"));
        paint(); // 后端没改成功，界面要退回真实状态，不能停在用户以为生效的位置
      }
    });

    const searchRow = h(
      "div",
      { class: "dev-search-row" },
      h(
        "div",
        { class: "dev-search-text" },
        h("div", { class: "dev-search-title", text: "调试插件参与主搜索" }),
        h("div", {
          class: "dev-search-desc",
          // ⚠ 别再写成「关闭时只能从『运行』tab 启动」：本开关只管**主搜索索引**，
          // 管不了热键。调试插件自己注册的全局热键是进程内的绑定，本开关、关闭调试窗口、
          // 甚至在「插件管理」里停用同名正式插件都不会注销它（见 hotkey.rs 的 arbitrate 注释），
          // 按下去照样会把调试窗口拉起来。
          text: "默认关闭。打开后，调试中的插件会和正式插件一起出现在主搜索结果里 —— 同名插件会出现两份，容易分不清点开的是哪个。关闭时，调试插件不再出现在主搜索里，从下面「运行」tab 启动即可；但有一个例外：调试插件自己注册过的全局热键仍然有效，按下它照样会拉起调试窗口（热键只登记在进程内，关掉调试窗口也不会注销，需插件自己注销或重启 iTools）。",
        }),
      ),
      searchSw,
    );

    return h(
      "div",
      { class: "card dev-dirs" },
      h(
        "div",
        { class: "dev-sec-head" },
        h("span", { class: "dev-sec-title", text: "调试目录（加载源）" }),
        h("div", { class: "launch-actions" }, addBtn, rescanBtn),
      ),
      h("div", {
        class: "dev-sec-desc",
        text: "这些目录下的每个子目录（含 plugin.json 与 index.html）都会被当作一个调试插件加载。可以直接指向构建产物目录，例如 <你的插件源码>/dist。",
      }),
      list,
      searchRow,
    );
  }

  // ---------- 左侧插件列表 ----------
  function pluginLogo(p: DevPluginInfo): HTMLElement {
    return p.logo
      ? h("img", { class: "dev-item-logo", src: p.logo, alt: "" })
      : h("span", { class: "dev-item-logo plugin-logo-fallback", html: PLUGIN_GLYPH });
  }

  function listPane(): HTMLElement {
    const pane = h("div", { class: "dev-list" });
    for (const p of plugins) {
      const lv = issueLevel(p.issues);
      const badge =
        lv === "error"
          ? h("span", { class: "plugin-badge dev-badge-err", text: "有错误", title: "清单校验发现 error 级问题" })
          : lv === "warn"
            ? h("span", { class: "plugin-badge dev-badge-warn", text: "有警告", title: "清单校验发现 warn 级问题" })
            : h("span", { class: "plugin-badge dev-badge-ok", text: "正常", title: "清单校验未发现问题" });

      const item = h(
        "div",
        { class: "dev-item" + (p.id === selectedId ? " active" : "") },
        pluginLogo(p),
        h(
          "div",
          { class: "dev-item-meta" },
          h(
            "div",
            { class: "dev-item-name-row" },
            h("span", { class: "dev-item-name", text: p.name || p.id }),
            h("span", { class: "dev-item-ver", text: "v" + p.version }),
            badge,
          ),
          h("div", {
            class: "dev-item-dir",
            text: `${baseName(p.dir)}　·　来自 ${baseName(p.root)}`,
            title: `插件目录：${p.dir}\n调试目录：${p.root}`,
          }),
        ),
      );
      item.addEventListener("click", () => {
        if (selectedId === p.id) return;
        selectedId = p.id;
        paint();
      });
      pane.appendChild(item);
    }
    return pane;
  }

  // ---------- 空态引导 ----------
  function emptyGuide(): HTMLElement {
    const fixed = config.dirs.find((d) => d.fixed);
    const box = h(
      "div",
      { class: "dev-empty" },
      h("div", { class: "dev-empty-title", text: "还没有可调试的插件" }),
      h("div", { class: "dev-empty-desc", text: "两种方式都行：" }),
      h(
        "ol",
        { class: "dev-empty-steps" },
        h(
          "li",
          {},
          h("span", { text: "把插件目录（至少含 plugin.json 与 index.html）放进固定的调试目录：" }),
          fixed
            ? h("code", { class: "dev-path", text: fixed.path, title: fixed.path })
            : h("span", { class: "dev-muted", text: "（后端未返回固定调试目录）" }),
        ),
        h("li", {}, h("span", {
          text: "或者点上面的「添加调试目录」，指向任意本地目录 —— 包括构建产物，例如 <你的插件源码>/dist，这样改完代码重新构建就能直接调试。",
        })),
      ),
      h("div", {
        class: "dev-empty-tip",
        text: "还没有插件骨架？在 Claude Code 里用 /itools-plugin-dev 让 AI 直接生成一个可运行的插件目录，放进上面的调试目录即可。",
      }),
    );
    if (fixed && !fixed.exists) {
      box.appendChild(
        h("div", {
          class: "dev-empty-warn",
          text: `固定调试目录当前不存在：${fixed.path}　—— 手工建一个同名目录即可。`,
        }),
      );
    }
    return box;
  }

  // ---------- 校验问题 ----------
  function issuesBlock(p: DevPluginInfo): HTMLElement {
    if (!p.issues.length) {
      return h("div", { class: "dev-issues dev-issues-ok", text: "清单校验通过：没有发现问题。" });
    }
    const box = h("div", { class: "dev-issues" });
    box.appendChild(
      h("div", {
        class: "dev-issues-title",
        text: `清单校验发现 ${p.issues.length} 个问题`,
      }),
    );
    for (const it of p.issues) {
      box.appendChild(
        h(
          "div",
          { class: `dev-issue dev-issue-${it.level === "error" ? "error" : "warn"}` },
          h("span", { class: "dev-issue-level", text: it.level === "error" ? "错误" : "警告" }),
          h("code", { class: "dev-issue-field", text: it.field }),
          h("span", { class: "dev-issue-msg", text: it.message }),
        ),
      );
    }
    return box;
  }

  // ---------- tab：运行（触发模拟器） ----------
  function runTab(p: DevPluginInfo): HTMLElement {
    const box = h("div", { class: "dev-tab-body" });

    box.appendChild(
      h(
        "div",
        { class: "dev-note" },
        h("div", { class: "dev-note-title", text: "触发模拟器" }),
        h("div", {
          text: "模拟「用户在主搜索里命中某个 feature 然后进入插件」这件事：选 feature、选触发方式、填一段 query，点运行，插件会在独立的调试窗口里打开，收到的正是这里填的内容。",
        }),
      ),
    );

    if (!p.features.length) {
      box.appendChild(
        h("div", {
          class: "dev-blocked",
          text: "该插件的 plugin.json 没有声明任何 feature，触发模拟器没有可用的入口。请先在 plugin.json 的 features 里加一条（至少要有 code）。",
        }),
      );
      return box;
    }

    const featSel = h("select", { class: "select dev-select" });
    for (const f of p.features) {
      featSel.appendChild(
        h("option", { value: f.code, text: f.explain ? `${f.code} — ${f.explain}` : f.code }),
      );
    }

    const kindSel = h("select", { class: "select dev-select" });
    for (const k of TRIGGER_KINDS) {
      kindSel.appendChild(h("option", { value: k.kind, text: k.label }));
    }

    const queryInput = h("input", { class: "field-input-sm dev-query", placeholder: "模拟 query" });
    const cmdsBox = h("div", { class: "dev-cmds" });
    const kindNote = h("div", { class: "dev-kind-note", text: "" });

    const currentFeature = (): DevFeature => p.features.find((f) => f.code === featSel.value) ?? p.features[0];

    /** feature / 触发方式变化后，刷新「该 feature 声明了什么」与「这种触发方式它声明了没有」。 */
    function syncFeature(fillQuery: boolean): void {
      const f = currentFeature();
      cmdsBox.innerHTML = "";
      cmdsBox.appendChild(h("span", { class: "dev-cmds-label", text: "该 feature 声明的 cmds：" }));
      if (!f.keywords.length && !f.hasRegex && !f.hasText) {
        cmdsBox.appendChild(h("span", { class: "dev-muted", text: "（一条都没有 —— 正式环境里它不会被任何输入命中）" }));
      } else {
        for (const k of f.keywords) cmdsBox.appendChild(h("span", { class: "dev-chip", text: k }));
        if (f.hasRegex) cmdsBox.appendChild(h("span", { class: "dev-chip dev-chip-type", text: "regex" }));
        if (f.hasText) cmdsBox.appendChild(h("span", { class: "dev-chip dev-chip-type", text: "文本" }));
      }

      const kind = kindSel.value;
      const declared =
        kind === "keyword" ? f.keywords.length > 0 : kind === "regex" ? f.hasRegex : f.hasText;
      const meta = TRIGGER_KINDS.find((k) => k.kind === kind);
      kindNote.className = "dev-kind-note" + (declared ? "" : " dev-kind-note-warn");
      kindNote.textContent = declared
        ? `${meta?.desc ?? ""}`
        : `${meta?.desc ?? ""} 注意：该 feature 并未声明这种匹配方式，正式环境里它不会以这种方式被触发 —— 仍可用它调试插件的处理分支。`;

      if (fillQuery) queryInput.value = f.keywords[0] ?? "";
    }

    featSel.addEventListener("change", () => syncFeature(true));
    kindSel.addEventListener("change", () => syncFeature(false));

    const runBtn = h("button", { class: "btn btn-primary", text: "运行" });
    if (!p.runnable) {
      runBtn.disabled = true;
      const why = p.issues.filter((i) => i.level === "error").map((i) => `${i.field}：${i.message}`);
      runBtn.title = why.length ? why.join("\n") : "后端判定该插件当前无法运行";
      box.appendChild(
        h("div", {
          class: "dev-blocked",
          text:
            "该插件当前无法运行，「运行」已禁用。原因：" +
            (why.length ? why.join("；") : "后端判定 runnable=false（详见上面的校验问题）"),
        }),
      );
    } else {
      runBtn.addEventListener("click", async () => {
        runBtn.disabled = true;
        runBtn.textContent = "启动中…";
        try {
          await api.devOpenPlugin(p.id, featSel.value, queryInput.value, kindSel.value);
          ctx.toast("已在调试窗口打开");
        } catch (err) {
          console.error("dev_open_plugin failed", err);
          ctx.toast(errText(err, "启动失败"));
        } finally {
          runBtn.disabled = false;
          runBtn.textContent = "运行";
        }
      });
    }

    box.append(
      h(
        "div",
        { class: "dev-form" },
        h("label", { class: "dev-form-label", text: "feature" }),
        featSel,
        h("label", { class: "dev-form-label", text: "触发方式" }),
        kindSel,
        h("label", { class: "dev-form-label", text: "模拟 query" }),
        queryInput,
      ),
      kindNote,
      cmdsBox,
      h("div", { class: "dev-run-actions" }, runBtn),
      h("div", {
        class: "dev-hint",
        text: "调试插件在独立的调试窗口里打开，可与正式插件窗口同时存在。它这次会话里的存储读写、文件读写、权限判定与 itools.data.sync() 全部走调试环境，不会碰到正式数据。运行过程中它发出的调用可以在「日志」tab 里实时看到。",
      }),
    );

    syncFeature(true);
    return box;
  }

  // ---------- tab：权限 ----------
  function permTab(p: DevPluginInfo): HTMLElement {
    const box = h("div", { class: "dev-tab-body" });
    box.appendChild(
      h(
        "div",
        { class: "dev-note" },
        h("div", { class: "dev-note-title", text: "调试授权（只在调试环境生效）" }),
        h("div", {
          // 这里是 textContent（ui.ts 的 text 分支），不渲染 Markdown —— 星号会原样显示成 **，
          // 要强调只能靠文案本身或 CSS 类。
          text: "这里的开关写的是调试专用的授权表，只影响调试窗口里的这个插件。它既不会改变正式安装后的授权状态，也不会被正式插件读到。用户真正安装你的插件时，仍然要在插件管理里自己授权一次。",
        }),
      ),
    );

    if (!p.permissions.length) {
      box.appendChild(
        h("div", {
          class: "dev-empty-inline",
          text: "该插件未在 plugin.json 里申请任何高危能力（permissions 为空），没有可授权的项目。",
        }),
      );
      return box;
    }

    const list = h("div", { class: "dev-perm-list" });
    for (const perm of p.permissions) {
      const granted = p.granted.includes(perm);
      const sw = makeSwitch(granted, async (checked) => {
        try {
          await api.devSetPermission(p.id, perm, checked);
          // 本地视图同步，保证切 tab 回来显示的还是真实状态
          if (checked) {
            if (!p.granted.includes(perm)) p.granted.push(perm);
          } else {
            p.granted = p.granted.filter((x) => x !== perm);
          }
          ctx.toast((checked ? "已授权（调试环境）" : "已撤销（调试环境）") + permLabel(perm));
        } catch (err) {
          console.error("dev_set_permission failed", err);
          ctx.toast(errText(err, "操作失败"));
          paint(); // 后端没生效，开关必须退回真实状态
        }
      });
      list.appendChild(
        h(
          "div",
          { class: "dev-perm-row" },
          h(
            "div",
            { class: "dev-perm-text" },
            h("div", { class: "dev-perm-name", text: permLabel(perm) }),
            h("div", { class: "dev-perm-key", text: perm }),
          ),
          sw,
        ),
      );
    }
    box.appendChild(list);
    return box;
  }

  // ---------- tab：同步 Mock ----------
  function mockTab(): HTMLElement {
    const box = h("div", { class: "dev-tab-body" });
    box.appendChild(
      h(
        "div",
        { class: "dev-note" },
        h("div", { class: "dev-note-title", text: "云同步 Mock（全局，对所有调试插件生效）" }),
        h("div", {
          text: "插件在调试环境里调 itools.data.sync() 时，拿到的就是这里配置的结果 —— 不会真的联网、也不会碰到你的云账号。用它把「同步成功 / 失败 / 离线 / 未登录」四条分支都走一遍。",
        }),
      ),
    );

    const form = h("div", { class: "dev-mock-form" });
    box.appendChild(form);
    const status = h("div", { class: "dev-hint", text: "读取当前 Mock 配置…" });
    box.appendChild(status);

    /** 用后端**真实返回**的配置绘制表单（不拿本地缓存冒充后端状态）。 */
    function paintForm(cfg: DevMockConfig): void {
      form.innerHTML = "";

      const modeBox = h("div", { class: "dev-mock-modes" });
      let mode = cfg.mode;
      for (const m of MOCK_MODES) {
        const radio = h("input", { type: "radio", name: "dev-mock-mode" });
        radio.checked = cfg.mode === m.mode;
        radio.addEventListener("change", () => {
          if (!radio.checked) return;
          mode = m.mode;
          syncVisible();
        });
        modeBox.appendChild(
          h(
            "label",
            { class: "dev-mock-mode" },
            radio,
            h(
              "div",
              { class: "dev-mock-mode-text" },
              h("div", { class: "dev-mock-mode-title", text: m.title }),
              h("div", { class: "dev-mock-mode-desc", text: m.desc }),
            ),
          ),
        );
      }
      // 后端返回了一个界面上没有的模式：如实告知，不要静默改成 success
      if (!MOCK_MODES.some((m) => m.mode === cfg.mode)) {
        modeBox.appendChild(
          h("div", {
            class: "dev-mock-unknown",
            text: `后端当前的模式是「${cfg.mode}」，不在本面板已知的四种里。保存会把它改成上面选中的那一种。`,
          }),
        );
      }

      const delayInput = h("input", { class: "field-input-sm", type: "number", min: "0", step: "50" });
      delayInput.value = String(cfg.delayMs);
      const reasonInput = h("input", { class: "field-input-sm dev-wide", placeholder: "如：服务器返回 500" });
      reasonInput.value = cfg.reason;
      const pushedInput = h("input", { class: "field-input-sm", type: "number", min: "0" });
      pushedInput.value = String(cfg.pushed);
      const pulledInput = h("input", { class: "field-input-sm", type: "number", min: "0" });
      pulledInput.value = String(cfg.pulled);

      const reasonRow = h(
        "div",
        { class: "dev-mock-row" },
        h("label", { class: "dev-form-label", text: "失败原因 reason" }),
        reasonInput,
      );
      const countRow = h(
        "div",
        { class: "dev-mock-row" },
        h("label", { class: "dev-form-label", text: "伪造条数" }),
        h(
          "div",
          { class: "dev-mock-counts" },
          h("span", { class: "dev-muted", text: "上行 pushed" }),
          pushedInput,
          h("span", { class: "dev-muted", text: "下行 pulled" }),
          pulledInput,
        ),
      );

      function syncVisible(): void {
        reasonRow.hidden = mode !== "fail";
        countRow.hidden = mode !== "success";
      }
      syncVisible();

      const saveBtn = h("button", { class: "btn btn-primary btn-sm", text: "保存 Mock 配置" });
      saveBtn.addEventListener("click", async () => {
        saveBtn.disabled = true;
        const next: DevMockConfig = {
          mode,
          delayMs: Math.max(0, Math.round(Number(delayInput.value) || 0)),
          reason: reasonInput.value,
          pushed: Math.max(0, Math.round(Number(pushedInput.value) || 0)),
          pulled: Math.max(0, Math.round(Number(pulledInput.value) || 0)),
        };
        try {
          await api.devSetMock(next);
          // 回读后端真实存下来的值再绘制：后端若做了归一化/夹取，界面显示的必须是它的结果
          const saved = await api.devGetMock();
          config.mock = saved;
          paintForm(saved);
          status.textContent = "已保存。以下显示的是后端回读的当前配置。";
          ctx.toast("已保存 Mock 配置");
        } catch (err) {
          console.error("dev_set_mock failed", err);
          status.textContent = "保存失败：" + errText(err, "未知错误");
          ctx.toast(errText(err, "保存失败"));
          saveBtn.disabled = false;
        }
      });

      form.append(
        h("label", { class: "dev-form-label", text: "同步结果" }),
        modeBox,
        h(
          "div",
          { class: "dev-mock-row" },
          h("label", { class: "dev-form-label", text: "人为延迟（毫秒）" }),
          delayInput,
        ),
        reasonRow,
        countRow,
        h("div", { class: "dev-run-actions" }, saveBtn),
      );
    }

    // 优先用后端**当下**的值：config.mock 是进页面时的快照，可能已被别处改过
    api
      .devGetMock()
      .then((cfg) => {
        config.mock = cfg;
        status.textContent = "以下是后端当前生效的 Mock 配置。";
        paintForm(cfg);
      })
      .catch((err) => {
        console.error("dev_get_mock failed", err);
        status.textContent =
          "读取当前 Mock 配置失败：" + errText(err, "未知错误") + "（下面显示的是进入本页时拿到的快照，可能已过期）";
        paintForm(config.mock);
      });

    return box;
  }

  // ---------- 右侧详情 ----------
  function detailPane(): HTMLElement {
    const p = plugins.find((x) => x.id === selectedId);
    if (!p) return h("div", { class: "dev-detail" }, emptyGuide());

    const openDirBtn = h("button", { class: "btn btn-sm", text: "打开目录" });
    openDirBtn.addEventListener("click", async () => {
      try {
        await api.devOpenDir(p.id);
      } catch (err) {
        console.error("dev_open_dir failed", err);
        ctx.toast(errText(err, "打开目录失败"));
      }
    });

    const header = h(
      "div",
      { class: "dev-detail-head" },
      pluginLogo(p),
      h(
        "div",
        { class: "dev-detail-meta" },
        h(
          "div",
          { class: "dev-detail-name-row" },
          h("span", { class: "dev-detail-name", text: p.name || p.id }),
          h("span", { class: "dev-detail-ver", text: "v" + p.version }),
          p.runnable
            ? null
            : h("span", {
                class: "plugin-badge dev-badge-err",
                text: "无法运行",
                title: "后端判定 runnable=false，见下面的校验问题",
              }),
        ),
        h("div", { class: "dev-detail-desc", text: p.description || "（无描述）" }),
        h("div", {
          class: "dev-detail-sub",
          text: `id ${p.id}　·　${p.author ? "作者 " + p.author + "　·　" : ""}${p.featureCount} 个 feature`,
        }),
        h("code", { class: "dev-detail-path", text: p.dir, title: p.dir }),
      ),
      h("div", { class: "dev-detail-acts" }, openDirBtn),
    );

    const tabBar = h("div", { class: "detail-tabs dev-tabs" });
    const tabBody = h("div", { class: "dev-tab-host" });

    // ⚠ 必须写成 const 箭头函数而不是 function 声明：函数声明会被提升到作用域顶部，
    //   TS 认为它可能在上面 `if (!p) return` 之前被调用，于是丢掉对 p 的非空收窄。
    const paintTab = (): void => {
      // innerHTML="" 只是把子节点摘下来。logView.el 的引用仍在本模块手里，
      // 它的监听器与轮询都不受影响 —— 这正是日志视图能常驻的原因。
      tabBody.innerHTML = "";
      switch (activeTab) {
        case "run":
          tabBody.appendChild(runTab(p));
          break;
        case "log":
          tabBody.appendChild(logView.el);
          break;
        case "storage":
          renderDevStorage(tabBody, { plugin: p, config, toast: ctx.toast });
          break;
        case "perm":
          tabBody.appendChild(permTab(p));
          break;
        case "mock":
          tabBody.appendChild(mockTab());
          break;
        case "publish":
          renderDevPublish(tabBody, { plugin: p, toast: ctx.toast });
          break;
      }
    };

    for (const t of TABS) {
      const btn = h("button", {
        class: "detail-tab" + (t.id === activeTab ? " active" : ""),
        text: t.label,
      });
      btn.addEventListener("click", () => {
        if (activeTab === t.id) return;
        activeTab = t.id;
        Array.from(tabBar.children).forEach((c, i) =>
          (c as HTMLElement).classList.toggle("active", TABS[i].id === activeTab),
        );
        paintTab();
      });
      tabBar.appendChild(btn);
    }

    paintTab();
    return h("div", { class: "dev-detail" }, header, issuesBlock(p), tabBar, tabBody);
  }

  // ---------- 整页绘制 ----------
  function paint(): void {
    // 选中项失效（插件被删/改名）时回落到第一个，避免右侧停在不存在的插件上
    if (selectedId && !plugins.some((p) => p.id === selectedId)) selectedId = null;
    if (!selectedId && plugins.length) selectedId = plugins[0].id;

    scroll.innerHTML = ""; // 注意：logView.el 只是被摘下，实例与轮询都还在
    const main = plugins.length
      ? h("div", { class: "dev-main" }, listPane(), detailPane())
      : h("div", { class: "dev-main dev-main-empty" }, emptyGuide());
    scroll.append(aiHeader(), aiClientsCard(), aiShowcase(), envNote(), dirsCard(), main);
  }

  /** 列表指纹：只覆盖会影响界面的字段。用来判断「后台刷新到底有没有变化」，
   *  没变化就不重绘 —— 重绘会清掉用户正在填的 query / Mock 表单。 */
  function listSig(list: DevPluginInfo[]): string {
    return list
      .map((p) =>
        [
          p.id,
          p.version,
          p.name,
          p.runnable ? "1" : "0",
          String(p.issues.length),
          p.permissions.join(","),
          p.granted.join(","),
          String(p.featureCount),
        ].join(""),
      )
      .join("");
  }

  /**
   * 重新拉插件列表。
   * @param silent 后台刷新（窗口重新显示时）：失败不弹 toast —— 此时界面继续显示上一份**真实**数据，
   *               没有任何虚假内容被渲染出来；下一次显式操作会把错误暴露给用户。
   * @returns 列表是否发生了变化
   */
  async function fetchPlugins(silent: boolean): Promise<boolean> {
    const before = listSig(plugins);
    try {
      plugins = await api.devListPlugins();
    } catch (err) {
      console.error("dev_list_plugins failed", err);
      // 保留旧列表：宁可显示上一份真实数据，也不清空成「没有插件」误导用户
      if (!silent) ctx.toast(errText(err, "调试插件列表刷新失败"));
      return false;
    }
    logView.refreshPlugins();
    return listSig(plugins) !== before;
  }

  /** 显式刷新（目录增删 / 重新扫描之后）：无论有没有变化都重绘。 */
  async function reloadPlugins(): Promise<void> {
    await fetchPlugins(false);
    paint();
  }

  paint();

  // 接入状态要问后端（MCP 是否在监听、各家配没配、skill 装没装），拿到后重绘这一块。
  // 读失败也如实呈现：把「查不到」显示成「未接入」会让用户一直点安装、一直失败。
  void api
    .aiClientsStatus()
    .then((st) => {
      if (active !== instance) return;
      ai = st;
      paint();
    })
    .catch((err) => {
      if (active !== instance) return;
      console.error("ai_clients_status failed", err);
      ai = {
        mcpRunning: false,
        mcpUrl: "",
        skillSourceAvailable: false,
        skillSourceError: `读取接入状态失败：${errText(err, "未知错误")}`,
        bundledVersion: "",
        detectedCount: 0,
        clients: [],
      };
      paint();
    });

  // ---------- 窗口重新可见时对账一次 ----------
  // 关闭管理中心走的是 close_admin_window → win.hide()（窗口只隐藏不销毁、也不触发 switchView），
  // 所以 registerDispose 管不到这条路径。开发者一直在改插件文件，窗口重新显示时对一次账，
  // 避免长期停在过期视图上；但**只有真的变了才重绘**，否则会把用户填到一半的表单清掉。
  // 日志侧的可见性门控在 dev-log.ts 里。
  const onVisible = (): void => {
    if (document.visibilityState !== "visible") return;
    if (active !== instance) return;
    void fetchPlugins(true).then((changed) => {
      if (changed && active === instance) paint();
    });
  };
  document.addEventListener("visibilitychange", onVisible);
  instance.offVisibility = (): void => {
    document.removeEventListener("visibilitychange", onVisible);
  };
}
