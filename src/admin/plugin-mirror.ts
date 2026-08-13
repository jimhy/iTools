//! 「插件下载源」面板：模式切换（自动竞速 / 仅官方 / 镜像优先）、镜像配置来源与刷新、候选源就地测速。
//!
//! 背景：插件生态只用 GitHub，而 GitHub 在国内可达性差。解决办法不是自建反代，而是服务端维护一份
//! 第三方镜像站列表并定时探测，客户端定期拉取；下载时在候选源之间竞速，由胜出的一路完整下载。
//!
//! ⚠ 竞速**不是**「谁先 2xx 谁赢」的裸竞速（那是已被堵掉的漏洞：一个响应极快的恶意镜像能稳定压过官方源）。
//!   真实模型见 mirror.rs `HEAD_START` / `race_groups` / `race_staged`：**分两组、后组延迟入场**——
//!   优先组先单独发请求，它在 400ms 抢跑窗口内跑出成功，另一组**一个请求都收不到**；
//!   优先组全部失败（不必等满窗口）或窗口到期仍无结果，后组才入场（此时优先组仍在赛道上，晚到的成功也算数）。
//!   `auto` 的优先组是官方源（这是**安全**约束），`mirror` 是用户显式反转成镜像优先、官方延迟兜底。
//!   面板里所有关于「怎么选源」的文案都必须描述这个模型，不得再写成「一起竞速 / 谁先响应用谁」。
//!
//! 诚信约束（doc/开发准则.md）：
//! - 面板里的健康/延迟**严格区分**「服务端探测结论（可能已过期，标注其时间）」与「本机刚测的实测值」，
//!   没测过就显示「未测速」，不拿服务端结论冒充本机可达性；
//! - raw 与归档（zip）**分两行**展示：实测存在「raw 通、归档 403」的镜像（ghfast.top 即是），
//!   塌成一行就抹掉了「究竟哪一段不通」这个唯一有用的信息；
//! - 测速失败原样展示后端给出的可区分原因（DNS / 超时 / 403 / 404 / 校验失败），不吞成「失败」；
//! - 镜像由第三方运营、可篡改传输内容，这一点必须常驻显示，不能因为「不好看」而省略；
//! - 拉取配置失败、无任何可用源，都如实呈现并给出可操作建议，不静默降级；
//!   但**告警分级同样是诚信问题**：拉配置失败时客户端会退到内置/缓存镜像继续正常工作，
//!   把这种正常降级画成两个并排的红框，等于告诉用户「出故障了」——同样是与真实行为不符的表述。
//!   分级规则见 {@link fetchFailureNote}：功能仍可用 → 信息级（中性色），实测确无可用源 → 错误级（红）。
//!   后端给出的失败原文一律**原样透传**，前端不另写一套解释（否则会出现两套口径）；
//! - 每个源「当前模式下到底参不参与下载」必须与 mirror.rs `build_candidates` + `race_groups` 的真实行为一致：
//!   mode=`mirror` 时官方源**仍在候选列表末尾兜底**，且只等 400ms 抢跑窗口就入场（不必等镜像全败），
//!   写成「仅镜像 / 官方已被排除 / 镜像全挂才轮到官方」都是假的，所以这里的模式名用「镜像优先」；
//!   被判 unhealthy 的镜像则是真的不参与候选，也要说出来；
//! - 服务端下发的、主机不在客户端内置清单里的镜像（`MirrorEntry.unlisted`）必须在面板上标注：
//!   这是一条内置清单未背书的新代码分发路径，用户有权看见；展示主机名一律用后端给的 `MirrorEntry.host`
//!   （前端自己 `new URL(tpl).hostname` 会丢掉端口，与后端判定 unlisted 的字符串不是同一个）。
//!
//! ⚠ 后端契约见 `src/types.ts` 的镜像小节：`MirrorConfigView` 是**扁平**结构，
//!   `plugin_mirror_test` 每个源返回 raw / archive **两条**。别按印象改字段名。

import type {
  MirrorConfigView,
  MirrorEntry,
  MirrorMode,
  MirrorTestResult,
  MirrorTestTarget,
} from "../types";
import { h } from "./ui";
import * as api from "./api";
import { errText } from "./plugin-install";

const IC_X =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>';

/** 官方源在候选/测速结果里的固定 id（与 mirror.rs `OFFICIAL_ID` 一致）。 */
const OFFICIAL_ID = "official";

/** 官方源两种形态的固定主机（与 mirror.rs `OFFICIAL_RAW_TPL` / `OFFICIAL_ARCHIVE_TPL` 一致）。 */
const OFFICIAL_HOSTS = "raw.githubusercontent.com · codeload.github.com";

/** 两种路径形态的展示顺序与文案。
 *  说明写清「谁在用它」——用户才知道某一行挂了会影响什么。 */
const TARGETS: Array<{ key: MirrorTestTarget; text: string; note: string }> = [
  {
    key: "raw",
    text: "raw 单文件",
    note: "检查插件更新时读远端 plugin.json 走这条；测速会完整取回 13 字节的探测文件并比对内容哈希",
  },
  {
    key: "archive",
    text: "归档 zip",
    note: "安装 / 更新插件下载整包走这条；测速只发一个 Range: bytes=0-0 的极小请求探可达性",
  },
];

/** 模式文案：名称 + **真实**含义（必须与 mirror.rs `build_candidates` + `race_groups` + `HEAD_START` 一致）。 */
const MODE_ITEMS: Array<{ mode: MirrorMode; title: string; desc: string }> = [
  {
    mode: "auto",
    title: "自动（推荐）",
    desc: "GitHub 官方源先单独试 400ms：官方在这个窗口内成功，第三方镜像一个请求都收不到；官方失败或 400ms 内没响应，镜像才入场竞速（此时官方仍在赛道上，它晚一点的成功照样算数）。",
  },
  {
    mode: "official",
    title: "仅官方",
    desc: "只走 raw.githubusercontent.com / codeload.github.com，内容不经第三方中转；国内网络不通时会直接失败。",
  },
  {
    mode: "mirror",
    title: "镜像优先",
    desc: "反过来让第三方镜像先试 400ms：窗口内有镜像成功就用镜像、官方不会被请求；镜像全部失败或 400ms 窗口一到期，官方源就入场兜底（不必等镜像全挂）。本模式下官方源不再享有抢跑保护，请知悉镜像有能力篡改内容。",
  },
];

/** 配置来源（后端字段 `origin`）→ 展示文案（如实说明这层意味着什么）。 */
const ORIGIN_LABEL: Record<string, { text: string; note: string }> = {
  server: { text: "来自服务端", note: "最近一次从服务端成功拉取（或被服务端确认仍有效）的镜像配置" },
  cache: { text: "本地缓存", note: "上次成功拉取后落盘的配置，本次未能从服务端取到新配置" },
  builtin: { text: "内置默认", note: "编译进客户端的兜底镜像列表，从未成功从服务端拉到配置" },
};

/** RFC3339 → 本地「MM-DD HH:mm」；解析不了返回空串（不编造时间）。 */
function formatTime(iso: string | null | undefined): string {
  if (!iso) return "";
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return "";
  const p = (n: number): string => String(n).padStart(2, "0");
  return `${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}`;
}

/** 镜像条目的主机展示：**只用后端给的 `m.host`**，前端绝不自己解析模板。
 *
 *  理由（这不是洁癖）：`m.host` 由 mirror.rs `tpl_host` 解析，**保留端口**
 *  （`https://evil.tld:8443/…` → `evil.tld:8443`，mirror.rs 有单测钉死），
 *  而 JS 的 `new URL(tpl).hostname` **不含端口**，会把上面这个源显示成 `evil.tld`——
 *  用户看到的主机名就与后端判定「是不是内置主机」所用的字符串不是同一个，非标准端口被悄悄藏掉。
 *  raw 与 archive 的主机可能不同（`m.host` 取自 raw 模板），差异不靠前端解析来暴露，
 *  而是把两条完整模板原样放进 title：既不丢信息，也不引入第二套解析口径。 */
function hostTitle(m: MirrorEntry): string {
  return `主机名由客户端从 raw 模板现场解析，不采信服务端下发的 label\nraw：${m.raw}\n归档：${m.archive}`;
}

/** 延迟展示；未知返回 null（由调用方决定显示「—」还是不显示）。 */
function latencyText(ms: number | null | undefined): string | null {
  return typeof ms === "number" && Number.isFinite(ms) ? `${Math.round(ms)} ms` : null;
}

/** 测速结果的键：**必须**带 target —— 后端每个源返回 raw / archive 两条，按 id 存会互相覆盖。 */
function testKey(id: string, target: MirrorTestTarget): string {
  return `${id}:${target}`;
}

/** 面板需要的外部能力 */
export interface MirrorModalCtx {
  toast: (msg: string) => void;
}

/**
 * 打开「插件下载源」面板。
 * 面板内的所有状态都来自后端命令的真实返回；任一命令失败都会在面板内显示原文错误。
 */
export function openMirrorModal(ctx: MirrorModalCtx): void {
  /** 当前后端返回的配置视图；null = 尚未取到（加载中或加载失败） */
  let view: MirrorConfigView | null = null;
  /** 本机就地测速结果（`id:target` → 结果）；空 = 本次尚未测速，界面不得显示任何「本机实测」结论 */
  const tested = new Map<string, MirrorTestResult>();
  /** 是否测速过（用于区分「没测过」与「测过但全挂」） */
  let testedOnce = false;
  /** 本轮 refresh 失败的原文。后端拉取失败时命令是 reject 的，`view.lastError` 拿不到本次原因，
   *  只能由前端记着——但必须与后端的 lastError 分开显示，不能伪装成后端返回值。 */
  let refreshError: string | null = null;
  let busy = false;

  const closeBtn = h("button", { class: "edit-close", html: IC_X });
  const body = h("div", { class: "mirror-body" });
  const doneBtn = h("button", { class: "btn btn-quiet", text: "关闭" });

  const modal = h(
    "div",
    { class: "modal modal-mirror" },
    h(
      "div",
      { class: "modal-title-row" },
      h("div", { class: "modal-title", text: "插件下载源" }),
      closeBtn,
    ),
    body,
    h("div", { class: "modal-actions" }, doneBtn),
  );
  const mask = h("div", { class: "modal-mask" }, modal);

  // ---------- 渲染 ----------

  /** 顶部说明（为什么存在这个面板 + 选源到底怎么选的）。 */
  function introBlock(): HTMLElement {
    return h(
      "div",
      { class: "mirror-intro" },
      h("div", {
        text: "插件从 GitHub 仓库安装。GitHub 在部分网络下不可达，iTools 会在官方源与第三方镜像站之间选一路完整下载。",
      }),
      h("div", {
        text: "选源不是「谁先响应用谁」：优先组先单独发请求，它在 400ms 抢跑窗口内成功，另一组一个请求都收不到；优先组失败或窗口到期，另一组才入场。默认（自动）的优先组是 GitHub 官方源 —— 官方可直连时，镜像根本联系不上你。",
      }),
      h("div", {
        text: "决出的赢家会被记住 30 分钟，其间同类下载直接复用、不重复竞速（raw 与归档各记各的；一旦该源下载失败即作废并重新竞速）。",
      }),
    );
  }

  /** 模式选择区。 */
  function modeBlock(v: MirrorConfigView): HTMLElement {
    const box = h("div", { class: "mirror-modes" });
    for (const item of MODE_ITEMS) {
      const radio = h("input", { type: "radio", name: "mirror-mode" });
      radio.checked = v.mode === item.mode;
      radio.disabled = busy;
      radio.addEventListener("change", () => {
        if (!radio.checked) return;
        void applyMode(item.mode);
      });
      box.appendChild(
        h(
          "label",
          { class: "mirror-mode" + (v.mode === item.mode ? " active" : "") },
          radio,
          h(
            "span",
            { class: "mirror-mode-text" },
            h("span", { class: "mirror-mode-title", text: item.title }),
            h("span", { class: "mirror-mode-desc", text: item.desc }),
          ),
        ),
      );
    }
    return box;
  }

  /** 配置来源与时间 + 立即刷新。 */
  function configBlock(v: MirrorConfigView): HTMLElement {
    const meta = ORIGIN_LABEL[v.origin] ?? {
      text: v.origin || "未知",
      note: "后端返回了未知的配置来源标识，已原样显示",
    };
    const parts = [`配置来源：${meta.text}`, `${v.mirrors.length} 个镜像`];
    const upd = formatTime(v.updatedAt);
    if (upd) parts.push(`配置更新于 ${upd}`);
    const fetched = formatTime(v.fetchedAt);
    parts.push(fetched ? `上次拉取成功 ${fetched}` : "尚未成功从服务端拉取过");

    const refreshBtn = h("button", { class: "btn btn-sm", text: "立即刷新" });
    refreshBtn.disabled = busy;
    refreshBtn.addEventListener("click", () => void refresh());

    // row 单独拎出来：本次刷新的结果要就地贴在「立即刷新」按钮下面（它是动作反馈，不是全局状态）
    const row = h(
      "div",
      { class: "mirror-config-row" },
      h("span", { class: "mirror-config-text", text: parts.join("　·　"), title: meta.note }),
      refreshBtn,
    );
    const box = h("div", { class: "mirror-config" }, row);

    // 服务端地址未配置：这不是错误，但用户有权知道「刷新」为什么拉不到东西
    if (!v.serverUrl) {
      box.appendChild(
        h("div", {
          class: "mirror-note mirror-note-plain",
          text: "尚未配置服务器地址，无法从服务端拉取镜像列表，当前使用客户端内置/缓存的列表。可在「账号 → 数据同步」里填写服务器地址。",
        }),
      );
    }
    // 拉取失败的两条信息分属**两个层级**，不能并排堆两个同样的红框：
    //   · refreshError = 本次点「立即刷新」的结果 → 就地贴在按钮下方（动作反馈）
    //   · v.lastError  = 后端记录的历史状态（可能是很久以前那次）→ 归入下面的说明块
    // 两者往往是同一次失败的两种措辞（refreshError 通常把 lastError 整段包在里面），
    // 此时只显示更近的那条 —— 合并的前提是「一条完整包含另一条」，一个字都不会丢。
    const historical = sameFailure(refreshError, v.lastError) ? null : v.lastError;
    if (refreshError) {
      const usable = hasUsableSource(v);
      row.appendChild(
        h("div", {
          class: "mirror-refresh-result" + (usable ? "" : " mirror-refresh-result-bad"),
          // 后端原文原样透传，前端不再补一套「可能是什么原因」的解释
          text: `刚才的刷新失败：${refreshError}`,
        }),
      );
    }
    if (refreshError || historical) box.appendChild(fetchFailureNote(v, historical));
    return box;
  }

  /** 两条失败文案是不是同一次失败：一条完整包含另一条即认为是（合并展示不丢信息）。 */
  function sameFailure(a: string | null, b: string | null): boolean {
    if (!a || !b) return false;
    return a === b || a.includes(b) || b.includes(a);
  }

  /** 当前配置在当前模式下**还有没有能用的下载源** —— 决定「拉取配置失败」该用信息级还是错误级。
   *
   *  口径与 {@link participates} 同源，不另立一套：
   *  - 已就地测速：以本机实测为准。参与下载的源里 raw 与归档**两种形态都没有一个通**，才算「没有可用源」；
   *    只通一种形态属于「部分不可用」，由 {@link deadEndBlock} 单独说清楚是「装不上」还是「查不了更新」，
   *    不在这里重复报一次。
   *  - 未测速：「不知道」不等于「不可用」，只看候选集合是否为空（官方源三种模式下都在候选里，
   *    所以正常情况恒非空）。绝不拿没测过的状态把正常降级渲染成故障。 */
  function hasUsableSource(v: MirrorConfigView): boolean {
    const rawOk = anyOk(v, "raw");
    const archiveOk = anyOk(v, "archive");
    if (rawOk !== null || archiveOk !== null) return rawOk === true || archiveOk === true;
    return [OFFICIAL_ID, ...v.mirrors.map((m) => m.id)].some((id) => participates(v, id));
  }

  /** 「没能从服务端取到镜像配置」的说明块。
   *
   *  这**不是**把错误藏起来：失败原文照常展示（本次的贴在刷新按钮下，历史的在这里）。
   *  变的是严重度与措辞 —— 拉配置失败时客户端退到内置/缓存镜像照常工作，此时用 danger 红
   *  等于谎报故障；只有实测确实没有可用源时才升到错误级。 */
  function fetchFailureNote(v: MirrorConfigView, historical: string | null): HTMLElement {
    const usable = hasUsableSource(v);
    // 来源文案与上面那行「配置来源：…」同源，避免同一份配置在同一个面板里出现两种叫法
    const originText = ORIGIN_LABEL[v.origin]?.text ?? (v.origin || "当前");
    const inUse =
      v.mirrors.length > 0
        ? `${originText}的 ${v.mirrors.length} 个镜像 + GitHub 官方源`
        : `${originText}的配置（没有镜像）+ GitHub 官方源`;
    const box = h("div", {
      class: usable ? "mirror-note mirror-note-info" : "mirror-note mirror-note-warn",
    });
    box.append(
      h("div", {
        class: "mirror-note-title",
        // 措辞对「刚才失败」与「历史失败」都成立，免得同一个块要写两套时态
        text: usable
          ? "从服务端拉取镜像配置未成功 —— 不影响插件的安装与更新"
          : "从服务端拉取镜像配置未成功，且当前模式下没有一个实测可用的源",
      }),
      h("div", {
        text: usable
          ? `当前生效的仍是${inUse}，下方候选源照常参与下载；拉取失败不会改动已生效的配置。`
          : `当前生效的仍是${inUse}，拉取失败不会改动它；此刻装不上的具体形态见下方提示。`,
      }),
    );
    // 历史失败原因：与本次刷新不是同一条时才出现（同一条已贴在刷新按钮下，不重复）
    if (historical) {
      box.appendChild(
        h("div", {
          class: "mirror-note-detail",
          text: `此前记录的拉取失败原因：${historical}`,
        }),
      );
    }
    return box;
  }

  /** 本机实测的一条形态（raw / archive）状态行。 */
  function probeLine(target: (typeof TARGETS)[number], t: MirrorTestResult): HTMLElement {
    const lat = latencyText(t.latencyMs);
    const line = h(
      "div",
      { class: "mirror-probe" },
      h("span", { class: "mirror-probe-target", text: target.text, title: target.note }),
      h("span", {
        class: t.ok ? "plugin-badge plugin-badge-ok" : "plugin-badge plugin-badge-warn",
        text: t.ok ? (lat ? `可用 · ${lat}` : "可用") : "不可用",
        title: t.ok
          ? `本机刚刚实测：请求成功\n${t.url}`
          : `本机实测失败：${t.error ?? "后端未给出原因"}\n${t.url}`,
      }),
    );

    const detail: string[] = [];
    if (!t.ok) detail.push(t.error ?? "本机实测失败（后端未给出原因）");
    // 内容哈希：判断镜像是否被劫持的关键信息。有就说结论，没有就明说没比对——绝不含糊带过
    if (t.hashChecked) {
      detail.push(t.hashOk ? "内容哈希已校验，与预期一致" : "内容哈希不匹配（内容与官方不一致）");
    } else if (t.ok && target.key === "raw") {
      detail.push("未比对内容哈希（镜像配置未提供探测文件的 sha256）");
    } else if (t.ok) {
      detail.push("仅探测首字节可达性，未取回内容、未比对哈希");
    }

    const box = h("div", { class: "mirror-probe-box" }, line);
    for (const d of detail) box.appendChild(h("div", { class: "mirror-row-detail", text: d }));
    return box;
  }

  /** 尚未本机测速时的状态：服务端探测结论是**整条镜像的整体结论**，不区分 raw / 归档，
   *  所以只出一行，绝不复制成两行假装「两种形态各自测过」。 */
  function untestedStatus(entry: MirrorEntry | null): HTMLElement {
    if (entry === null) {
      return h(
        "div",
        { class: "mirror-probe-box" },
        h(
          "div",
          { class: "mirror-probe" },
          h("span", {
            class: "plugin-badge plugin-badge-muted",
            text: "未测速",
            title: "本机尚未测速；官方源不参与服务端探测",
          }),
        ),
        h("div", { class: "mirror-row-detail", text: "点「测速」实测 raw 与归档两种形态" }),
      );
    }
    const lat = latencyText(entry.latencyMs);
    const ok = formatTime(entry.lastOkAt);
    return h(
      "div",
      { class: "mirror-probe-box" },
      h(
        "div",
        { class: "mirror-probe" },
        h("span", {
          class: entry.healthy ? "plugin-badge plugin-badge-muted" : "plugin-badge plugin-badge-warn",
          text: entry.healthy
            ? lat
              ? `服务端探测可用 · ${lat}`
              : "服务端探测可用"
            : "服务端探测不可用",
          title:
            "这是服务端上次探测该镜像的整体结论，既不区分 raw / 归档，也不代表你本机此刻能否访问；点「测速」可就地实测",
        }),
      ),
      h("div", {
        class: "mirror-row-detail",
        text: ok ? `服务端上次探测成功 ${ok}（未区分形态）` : "服务端从未探测成功",
      }),
    );
  }

  /** 单个候选源行。`entry` 为 null 表示官方源（内置固定候选，不在 mirrors 列表里）。
   *  `role` 如实说明当前模式下这个源到底参不参与下载（必须与 build_candidates 一致）。 */
  function sourceRow(
    id: string,
    label: string,
    host: string,
    hostTip: string | null,
    entry: MirrorEntry | null,
    role: { dim: boolean; note: string | null },
  ): HTMLElement {
    const row = h("div", { class: "mirror-row" + (role.dim ? " mirror-row-off" : "") });
    const nameCol = h(
      "div",
      { class: "mirror-row-main" },
      h(
        "div",
        { class: "mirror-row-name" },
        h("span", { class: "mirror-row-label", text: label }),
        entry === null
          ? h("span", {
              class: "plugin-badge plugin-badge-muted",
              text: "官方",
              title: "GitHub 官方源，客户端内置的固定候选，不在服务端下发的镜像列表里",
            })
          : null,
        // 服务端下发的新主机：内置清单未背书的一条新代码分发路径，用户有权看见。
        // 判定在后端（mirror.rs sanitize，raw 与归档两个模板的主机都在内置清单里才算内置），
        // 且该字段只出不进——服务端无法把自己标成「内置」。
        entry?.unlisted
          ? h("span", {
              class: "plugin-badge mirror-badge-unlisted",
              text: "服务端新增源",
              title:
                "这个源的主机不在 iTools 客户端内置的镜像清单里，是由同步服务端下发引入的。\n" +
                "它仍会参与下载（镜像失效后能换新的正是这套机制的用途），但竞速排序排在内置主机之后：内置主机可用时通常不会被请求到。\n" +
                "插件代码会经它中转，请确认你信任当前配置的服务器。",
            })
          : null,
      ),
      h("div", {
        class: "mirror-row-host",
        text: host || "（后端未给出主机名）",
        title: hostTip ?? undefined, // 无 tip 时不设 title 属性（h() 会跳过 null/undefined）
      }),
      role.note ? h("div", { class: "mirror-row-detail", text: role.note }) : null,
    );

    const status = h("div", { class: "mirror-row-status" });
    const hits = TARGETS.map((t) => ({ target: t, res: tested.get(testKey(id, t.key)) }));
    if (hits.some((x) => x.res)) {
      // 本机实测优先，且 raw / 归档**各占一行**：ghfast.top 就是「raw 通、归档 403」，
      // 塌成一行会把这个唯一有用的信息抹掉
      for (const { target, res } of hits) {
        status.appendChild(
          res
            ? probeLine(target, res)
            : h(
                "div",
                { class: "mirror-probe-box" },
                h(
                  "div",
                  { class: "mirror-probe" },
                  h("span", { class: "mirror-probe-target", text: target.text, title: target.note }),
                  h("span", {
                    class: "plugin-badge plugin-badge-muted",
                    text: "无结果",
                    title: "本次测速没有返回这个形态的结果",
                  }),
                ),
              ),
        );
      }
    } else {
      status.appendChild(untestedStatus(entry));
    }

    row.append(nameCol, status);
    return row;
  }

  /** 官方源在当前模式下的真实角色（对齐 mirror.rs `race_groups` + `HEAD_START`）。 */
  function officialRole(mode: MirrorMode): { dim: boolean; note: string | null } {
    if (mode === "official") return { dim: false, note: "当前模式下唯一的下载源" };
    if (mode === "mirror") {
      return {
        dim: false,
        note: "「镜像优先」模式下延迟入场：镜像抢跑的 400ms 里没有一个成功（或全部失败），它立刻入场兜底 —— 不必等镜像全挂",
      };
    }
    return {
      dim: false,
      note: "抢跑组：先单独试 400ms，成功就直接用它，镜像一个请求都收不到",
    };
  }

  /** 某个镜像在当前模式下的真实角色（对齐 mirror.rs `build_candidates` 的过滤与排序）。 */
  function mirrorRole(mode: MirrorMode, m: MirrorEntry): { dim: boolean; note: string | null } {
    if (mode === "official") return { dim: true, note: "「仅官方」模式下不参与下载" };
    if (!m.healthy) {
      return { dim: true, note: "服务端探测判定失效，当前不参与下载候选（测速仍会测它）" };
    }
    const parts = [
      mode === "mirror"
        ? "抢跑组：先于官方源发起请求，400ms 内成功则官方不会被请求"
        : "官方源抢跑的 400ms 内没有成功时，才轮到它入场竞速",
    ];
    if (m.unlisted) {
      parts.push("主机不在客户端内置清单内，竞速排序排在内置主机之后（内置主机可用时通常轮不到它）");
    }
    return { dim: false, note: parts.join("；") };
  }

  /** 候选源列表 + 测速按钮。 */
  function sourcesBlock(v: MirrorConfigView): HTMLElement {
    const testBtn = h("button", { class: "btn btn-sm", text: busy ? "测速中…" : "测速" });
    testBtn.disabled = busy;
    testBtn.title =
      "实测本机对每个候选源的可达性与延迟（含被判失效的镜像）：raw 形态完整取回 13 字节的探测文件并比对 sha256，" +
      "能发现该源在传输中改了内容；归档形态只发一个极小请求探首字节，不比对哈希（包太大）";
    testBtn.addEventListener("click", () => void runTest());

    const box = h(
      "div",
      { class: "mirror-sources" },
      h(
        "div",
        { class: "mirror-sec-head" },
        h("span", { class: "mirror-sec-title", text: "候选下载源" }),
        testBtn,
      ),
      h("div", {
        class: "mirror-sec-hint",
        text: "raw 与归档分开列：实测存在「raw 能通、归档被 403」的镜像，这会让「检查更新」正常但「安装 / 更新」失败。",
      }),
    );

    const list = h("div", { class: "mirror-list" });

    // 官方源：内置固定候选，永远列在最前（后端 id 恒为 official）
    list.appendChild(
      sourceRow(
        OFFICIAL_ID,
        v.officialLabel || "GitHub 官方",
        OFFICIAL_HOSTS,
        "官方源的两个固定主机：raw 单文件走 raw.githubusercontent.com，归档 zip 走 codeload.github.com（编译进客户端，服务端改不了）",
        null,
        officialRole(v.mode),
      ),
    );

    if (!v.mirrors.length) {
      list.appendChild(
        h("div", {
          class: "mirror-empty",
          text: v.serverUrl
            ? "当前配置里没有任何镜像站，只能走 GitHub 官方源。点「立即刷新」向服务端重新拉取镜像列表。"
            : "当前配置里没有任何镜像站，只能走 GitHub 官方源。",
        }),
      );
    } else {
      for (const m of v.mirrors) {
        // 主机名一律取后端的 m.host（与后端判定 unlisted 的字符串同源、含端口），不在前端另解析一遍
        list.appendChild(
          sourceRow(m.id, m.label || m.id, m.host, hostTitle(m), m, mirrorRole(v.mode, m)),
        );
      }
    }

    box.appendChild(list);
    return box;
  }

  /** 某个源在当前模式下是否**真的会被用来下载**（与 mirror.rs `build_candidates` 一致）。
   *  测速会把所有源（含被判失效的镜像）都测一遍，但「此刻能不能装上」只能看真正参与的那些。 */
  function participates(v: MirrorConfigView, id: string): boolean {
    if (id === OFFICIAL_ID) return true; // 三种模式下官方都在候选里（mirror 模式排最后兜底）
    if (v.mode === "official") return false;
    return v.mirrors.some((m) => m.id === id && m.healthy);
  }

  /** 本次测速里，某种形态是否至少有一个**当前模式下会被使用的**源可用。
   *  没测过返回 null —— 「不知道」不等于「不可用」，不得据此弹警告。 */
  function anyOk(v: MirrorConfigView, target: MirrorTestTarget): boolean | null {
    if (!testedOnce || !tested.size) return null;
    return Array.from(tested.values()).some(
      (r) => r.target === target && r.ok && participates(v, r.id),
    );
  }

  /** 「无可用源」的如实提示。全部基于**本机实测**或**服务端探测**的真实结论，不臆测。 */
  function deadEndBlock(v: MirrorConfigView): HTMLElement | null {
    const rawOk = anyOk(v, "raw");
    const archiveOk = anyOk(v, "archive");

    // 一、测过：按形态分别给结论——归档挂了才是「装不上」，raw 挂了是「查不了更新」
    if (rawOk !== null && archiveOk !== null) {
      if (!rawOk && !archiveOk) {
        return h(
          "div",
          { class: "mirror-note mirror-note-warn" },
          h("div", {
            text: "本次测速中，当前模式会用到的源，raw 与归档形态全都不可用 —— 此刻无法从任何源安装 / 更新插件。",
          }),
          h("div", { text: modeTip(v.mode) }),
        );
      }
      if (!archiveOk) {
        return h(
          "div",
          { class: "mirror-note mirror-note-warn" },
          h("div", {
            text: "本次测速中，当前模式会用到的源没有一个归档（zip）形态可用：可以检查更新，但下载插件包会失败 —— 安装 / 更新此刻装不上。",
          }),
          h("div", { text: modeTip(v.mode) }),
        );
      }
      if (!rawOk) {
        return h(
          "div",
          { class: "mirror-note mirror-note-warn" },
          h("div", {
            text: "本次测速中，当前模式会用到的源没有一个 raw 形态可用：可以下载插件包，但「检查更新」（读远端 plugin.json）会失败。",
          }),
          h("div", { text: modeTip(v.mode) }),
        );
      }
      return null; // 实测有源可用：不摆任何警告，实测比服务端的历史探测更可信
    }

    // 二、没测过：只能依据服务端探测。mode=mirror 下没有一个健康镜像时，会退化成官方兜底——如实说
    if (v.mode === "mirror" && v.mirrors.length && !v.mirrors.some((m) => m.healthy)) {
      return h(
        "div",
        { class: "mirror-note mirror-note-warn" },
        h("div", {
          text: "当前是「镜像优先」模式，但配置里没有任何一个被判定为健康的镜像 —— 实际会直接退回 GitHub 官方源下载。",
        }),
        h("div", { text: "点「测速」可就地实测各源此刻的真实可达性；也可点「立即刷新」拉取新的镜像列表。" }),
      );
    }
    return null;
  }

  /** 按当前模式给出可操作建议（只说真的有用的那几条）。 */
  function modeTip(mode: MirrorMode): string {
    if (mode === "official") return "建议：切到「自动」让镜像在官方失败后入场竞速；或检查网络 / 代理设置。";
    if (mode === "mirror")
      return "建议：切到「自动」把 GitHub 官方源换回抢跑组（官方可直连时镜像收不到请求）；或点「立即刷新」拉取新的镜像列表；或检查网络。";
    return "建议：点「立即刷新」拉取新的镜像列表；确认网络与代理可用；若公司网络限制第三方镜像，可切到「仅官方」。";
  }

  /** 常驻的第三方风险告知（不可省略）。 */
  function riskBlock(): HTMLElement {
    return h(
      "div",
      { class: "mirror-note mirror-note-risk" },
      h("div", { class: "mirror-note-title", text: "关于镜像的风险，请务必知悉" }),
      h("div", {
        text: "镜像站由第三方运营，iTools 无法控制其行为，它们有能力篡改中转的文件内容。",
      }),
      h("div", {
        text: "手动粘贴仓库 URL 安装的插件没有可信的哈希来源，经镜像下载时 iTools 无法校验内容是否被篡改；安装弹窗会如实告知本次到底走了哪个源、有没有校验过。",
      }),
      h("div", {
        text: "带访问令牌的请求一律直连 GitHub 官方，绝不经过镜像。",
      }),
    );
  }

  function renderLoading(): void {
    body.innerHTML = "";
    body.appendChild(h("div", { class: "mirror-loading", text: "加载下载源配置…" }));
  }

  function renderError(msg: string): void {
    body.innerHTML = "";
    const retry = h("button", { class: "btn btn-sm", text: "重试" });
    retry.addEventListener("click", () => void load());
    body.append(
      h("div", { class: "install-error", text: msg }),
      h("div", { class: "mirror-config-row" }, retry),
      riskBlock(),
    );
  }

  function render(): void {
    if (!view) return;
    const v = view;
    body.innerHTML = "";
    body.append(introBlock(), modeBlock(v), configBlock(v), sourcesBlock(v));
    const dead = deadEndBlock(v);
    if (dead) body.appendChild(dead);
    body.appendChild(riskBlock());
  }

  // ---------- 动作 ----------

  async function load(): Promise<void> {
    renderLoading();
    refreshError = null;
    try {
      view = await api.pluginMirrorConfig();
      render();
    } catch (err) {
      console.error("plugin_mirror_config failed", err);
      view = null;
      renderError(errText(err, "读取下载源配置失败"));
    }
  }

  async function refresh(): Promise<void> {
    if (busy) return;
    busy = true;
    refreshError = null;
    render();
    try {
      view = await api.pluginMirrorRefresh();
      // 配置换了，旧的测速结论不再对应当前源列表
      tested.clear();
      testedOnce = false;
      ctx.toast("已从服务端刷新镜像配置");
    } catch (err) {
      console.error("plugin_mirror_refresh failed", err);
      const msg = errText(err, "刷新镜像配置失败");
      ctx.toast(msg);
      // 拉取失败不改变当前生效配置：把真实原因单独记下来，与后端的 lastError 分开显示
      refreshError = msg;
    } finally {
      busy = false;
      if (view) render();
      else renderError(refreshError ?? "刷新镜像配置失败");
    }
  }

  async function runTest(): Promise<void> {
    if (busy) return;
    busy = true;
    render();
    try {
      const res = await api.pluginMirrorTest();
      tested.clear();
      // 每个源有 raw / archive 两条，**必须**按 id+target 存：按 id 存会让后到的那条吃掉前一条，
      // 「raw 通、归档 403」这种最关键的现象就此消失
      res.forEach((r) => tested.set(testKey(r.id, r.target), r));
      testedOnce = true;
      ctx.toast(testSummary(res));
    } catch (err) {
      console.error("plugin_mirror_test failed", err);
      // 整体失败不等于「都不可用」：清掉结论，只报错，不落任何假状态
      tested.clear();
      testedOnce = false;
      ctx.toast(errText(err, "测速失败"));
    } finally {
      busy = false;
      render();
    }
  }

  /** 测速汇总：按**源数**统计（不是按条数），并如实区分「两种形态都通」与「只通一种」。 */
  function testSummary(res: MirrorTestResult[]): string {
    if (!res.length) return "没有候选源可测";
    const byId = new Map<string, MirrorTestResult[]>();
    for (const r of res) {
      const arr = byId.get(r.id);
      if (arr) arr.push(r);
      else byId.set(r.id, [r]);
    }
    let full = 0;
    let partial = 0;
    for (const arr of byId.values()) {
      if (arr.every((r) => r.ok)) full += 1;
      else if (arr.some((r) => r.ok)) partial += 1;
    }
    const total = byId.size;
    const parts = [`${full}/${total} 个源 raw 与归档均可用`];
    if (partial) parts.push(`${partial} 个仅一种形态可用`);
    return parts.join("，");
  }

  async function applyMode(mode: MirrorMode): Promise<void> {
    if (busy || !view || view.mode === mode) return;
    const prev = view.mode;
    busy = true;
    view = { ...view, mode };
    render();
    try {
      await api.pluginMirrorSetMode(mode);
      ctx.toast(`下载源模式：${MODE_ITEMS.find((m) => m.mode === mode)?.title ?? mode}`);
    } catch (err) {
      console.error("plugin_mirror_set_mode failed", err);
      ctx.toast(errText(err, "切换下载源模式失败"));
      if (view) view = { ...view, mode: prev }; // 后端没改成，界面必须退回真实状态
    } finally {
      busy = false;
      render();
    }
  }

  // ---------- 关闭 ----------
  function close(): void {
    document.removeEventListener("keydown", onKey, true);
    mask.remove();
  }
  const onKey = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    e.stopPropagation(); // 抢在 main.ts 的关窗监听前，只关面板
    close();
  };
  closeBtn.addEventListener("click", close);
  doneBtn.addEventListener("click", close);
  mask.addEventListener("mousedown", (e) => {
    if (e.target === mask) close();
  });
  document.addEventListener("keydown", onKey, true);

  document.body.appendChild(mask);
  void load();
}
