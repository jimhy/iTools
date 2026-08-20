//! 「从 Git 安装插件」弹窗：输入仓库 URL → 后端下载解包并返回预览 → 确认落地。
//!
//! 诚信约束（doc/开发准则.md）：
//! - 预览里展示的名称/版本/权限/文件数/体积全部来自后端对**真实下载包**的解析，前端不编造、不兜底假值；
//! - 后端返回的错误原样呈现（不吞成「失败」两个字），失败时不显示任何看似成功的内容；
//! - 权限区明确写「安装后默认不授权」——这与后端「安装不写 plugin_permissions」的真实行为一致；
//! - 离开弹窗的**任何**路径（取消/关闭/Esc/点遮罩/重新获取）都会回传 token 调 plugin_install_cancel
//!   清理后端暂存目录，避免残留。
//!
//! 本模块同时导出若干插件相关的展示辅助（权限文案 / 来源文案 / 错误文案），供插件列表与详情页复用。

import type { GitSource, InstallPreview } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { renderMarkdown } from "./markdown";

const PLUGIN_GLYPH =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M9 2v4M15 2v4"/><path d="M7 6h10v4a5 5 0 0 1-10 0z"/><path d="M12 15v5"/></svg>';
const IC_X =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="6" y1="6" x2="18" y2="18"/><line x1="18" y1="6" x2="6" y2="18"/></svg>';

// ---------- 共用展示辅助 ----------

/** 高危能力名 → 中文文案（插件列表、安装弹窗共用） */
export const PERM_LABELS: Record<string, string> = {
  runCommand: "执行程序",
  network: "联网",
  "fs-user-scope": "读写你选择的文件夹",
  "fs-named-path": "访问系统位置（临时目录 / 下载 / 浏览器缓存等）",
  "fs-trash": "把文件送进回收站",
  "context-read": "读取当前活动窗口信息",
  "input-inject": "模拟键盘鼠标输入",
  "clipboard-watch": "监听剪贴板变化",
  "window-manage": "管理其它程序的窗口",
  "local-server": "在局域网开放本机服务",
  "process-manage": "查看并结束进程",
  "system-read": "读取已安装软件等系统信息",
  "system-manage": "修改启动项 / 关机重启",
  "runtime": "使用 iTools 托管的外部程序（ffmpeg 等）",
  "background": "后台常驻与定时任务",
  "camera": "使用摄像头",
  "tray": "在系统托盘显示图标",
};

/** 高危能力的展示文案（未知能力原样显示，不隐瞒）。 */
export const permLabel = (perm: string): string => PERM_LABELS[perm] ?? perm;

/** 后端错误 → 可读文案：Tauri 命令的 `Err(String)` 会原样 reject，优先原样呈现。 */
export function errText(err: unknown, fallback: string): string {
  if (typeof err === "string" && err.trim()) return err;
  if (err instanceof Error && err.message) return err.message;
  return fallback;
}

/**
 * host 标识（gitee/github）→ 展示域名；未知值原样返回。
 *
 * ⚠ `host` 是**接口风格名**而非域名：自建 Gitea/Gitee 私有部署（ITOOLS_PLUGIN_HOSTS=git.corp.com）
 * 的 host 同样是 "gitee"，硬映射会显示成 gitee.com——与真实下载来源不符。
 * 因此这里只作为最后的兜底，展示请一律走 {@link sourceDomain}。
 */
export function hostDomain(host: string): string {
  if (host === "gitee") return "gitee.com";
  if (host === "github") return "github.com";
  return host;
}

/** 从 URL 取主机名；解析不了返回 ""（不猜、不编造）。 */
function domainFromUrl(url: string): string {
  if (!url) return "";
  try {
    return new URL(url).hostname;
  } catch {
    return "";
  }
}

/**
 * 来源的真实展示域名。
 *
 * 优先用后端给出的 `domain`（真实下载域名，自建站也准确）；老锁文件没有该字段（Rust 侧
 * `#[serde(default)]` 兼容，反序列化得空串），退而从 `pageUrl` 解析主机名；再退到风格名映射。
 */
export function sourceDomain(src: GitSource): string {
  return src.domain || domainFromUrl(src.pageUrl) || hostDomain(src.host);
}

/** 来源的一行展示：`git.corp.com/owner/repo`（不含子目录与 revision）。 */
export function sourceLabel(src: GitSource): string {
  return `${sourceDomain(src)}/${src.owner}/${src.repo}`;
}

/** revision 的展示文案：空 = 跟随默认分支；完整 commit sha 截前 7 位。 */
export function revisionLabel(revision: string): string {
  if (!revision) return "跟随默认分支";
  return isCommitSha(revision) ? revision.slice(0, 7) : revision;
}

/** 是否是完整 commit sha（40 位十六进制）——与后端「锁定版本」的判定一致。 */
export function isCommitSha(revision: string): boolean {
  return /^[0-9a-fA-F]{40}$/.test(revision);
}

/** 字节数 → 友好体积文案。 */
export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(2)} MB`;
}

// ---------- 弹窗 ----------

/** 安装弹窗需要的外部能力 */
export interface InstallModalCtx {
  /** 顶部轻提示 */
  toast: (msg: string) => void;
  /** 安装成功后回调（插件名），用于刷新列表 */
  onInstalled: (name: string) => void;
  /** **预置来源**（插件市场用）：给定后不显示 URL 输入框，打开即按这个来源取预览。
   *
   *  市场安装与手动粘 URL 走的是同一套弹窗，为的是让两条路的展示与确认逻辑完全一致——
   *  权限清单、「安装后默认不授权」、实际下载源、是否经镜像、是否做过哈希校验，
   *  这些如实告知的逻辑只写一份，另起一个简化弹窗迟早会漏掉其中几条。 */
  source?: {
    /** 弹窗标题（如「从市场安装 xxx」） */
    title: string;
    /** 取预览的方式（市场走 market_install_preview，会带上索引给的内容哈希校验） */
    fetchPreview: () => Promise<InstallPreview>;
  };
}

/** 弹窗阶段：输入 → 获取中 → 预览 → 安装中 */
type Stage = "idle" | "fetching" | "preview" | "installing";

/**
 * 打开「从 Git 安装插件」弹窗。
 * 关闭时若存在未确认的预览 token，会 fire-and-forget 调 plugin_install_cancel 清理后端暂存。
 */
export function openInstallModal(ctx: InstallModalCtx): void {
  let stage: Stage = "idle";
  /** 当前预览（含后端暂存 token），null = 尚未获取或已失效 */
  let preview: InstallPreview | null = null;

  // ---------- 结构 ----------
  const urlInput = h("input", {
    class: "field-input",
    type: "text",
    placeholder: "https://github.com/user/repo.git?path=/plugins/foo#v1.0.0",
  });

  /** URL 输入行。市场安装时来源已经定了，把它整行藏起来，
   *  免得用户以为还能在这里改地址（改了也不会生效——预览走的是市场条目）。 */
  const urlRow = h(
    "div",
    { class: "install-url-row" },
    h("label", { class: "field-label", text: "仓库地址" }),
    urlInput,
  );

  const hint = h(
    "div",
    { class: "install-hint" },
    h("div", { text: "插件生态基于 GitHub，语法与 Unity 包管理器一致：" }),
    h(
      "ul",
      { class: "install-hint-list" },
      h("li", {}, h("code", { text: "https://github.com/user/repo.git" }), " 仓库根目录即插件"),
      h(
        "li",
        {},
        h("code", { text: "https://github.com/user/repo.git?path=/plugins/foo" }),
        " ?path= 指定仓库内子目录",
      ),
      h(
        "li",
        {},
        h("code", { text: "https://github.com/user/repo.git#v1.2.3" }),
        " #号后指定分支 / 标签 / 完整 commit",
      ),
    ),
    h("div", {
      text: "不写 #revision 时跟随仓库默认分支；写完整 commit 会锁定版本，之后不再跟随更新。",
    }),
    h("div", {
      text: "Gitee / 自建 Gitea 风格站点的地址仍可安装（旧插件不受影响），但不再是推荐来源。",
    }),
  );

  /** 下载源提示：**事前**的预期告知（还没下载，只能按模式说「可能」）。
   *  一旦拿到预览，后端已经知道本次**实际**走了哪个源，就必须让位给
   *  {@link downloadFactBlock} 的事后事实——两者同时挂着会让用户以为「可能」就是结论。
   *  读不到模式就什么也不说（不猜、不吓唬），由下载源面板自身承担告知责任。 */
  const mirrorHint = h("div", { class: "install-notice install-notice-warn" });
  mirrorHint.style.display = "none";
  /** 事前提示是否有内容可显示（取到模式且该模式确实可能走镜像） */
  let mirrorHintReady = false;
  void api
    .pluginMirrorConfig()
    .then((v) => {
      if (v.mode === "official") return;
      // 「自动」的真实行为是官方源抢跑 400ms、官方可用时镜像收不到请求（mirror.rs HEAD_START），
      // 不是「官方与镜像一起竞速」——但仍**可能**走镜像（官方不通时），所以下面的提示照发
      const modeText = v.mode === "mirror" ? "镜像优先" : "自动（官方源优先抢跑）";
      mirrorHint.textContent =
        `当前下载源模式为「${modeText}」：插件包可能经第三方镜像站中转，镜像有能力篡改传输内容；` +
        `手动粘贴 URL 安装没有可信的哈希来源，iTools 无法校验内容是否被篡改。` +
        `介意可到插件页「下载源」里切换为「仅官方」。点「获取信息」取回插件包后，这里会替换成本次实际用了哪个源的确切结论。`;
      mirrorHintReady = true;
      syncMirrorHint();
    })
    .catch((err) => console.error("plugin_mirror_config failed", err));

  /** 有预览时隐藏事前提示（预览里已有确切事实），无预览时恢复。 */
  function syncMirrorHint(): void {
    mirrorHint.style.display = mirrorHintReady && preview === null ? "" : "none";
  }

  /** 错误框：只在有错时显示，内容为后端原文 */
  const errBox = h("div", { class: "install-error" });
  errBox.style.display = "none";

  /** 预览区容器 */
  const previewBox = h("div", { class: "install-preview" });
  previewBox.style.display = "none";

  const cancelBtn = h("button", { class: "btn btn-quiet", text: "取消" });
  const mainBtn = h("button", { class: "btn btn-primary", text: "获取信息" });
  const closeBtn = h("button", { class: "edit-close", html: IC_X });

  const modal = h(
    "div",
    { class: "modal modal-install" },
    h(
      "div",
      { class: "modal-title-row" },
      h("div", { class: "modal-title", text: ctx.source ? ctx.source.title : "从 Git 安装插件" }),
      closeBtn,
    ),
    h(
      "div",
      { class: "install-body" },
      urlRow,
      hint,
      mirrorHint,
      errBox,
      previewBox,
    ),
    h("div", { class: "modal-actions" }, cancelBtn, mainBtn),
  );

  const mask = h("div", { class: "modal-mask" }, modal);

  // ---------- 状态渲染 ----------

  /** 主按钮文案/可用性 = 当前阶段 + 预览内容的真实反映 */
  function syncButtons(): void {
    if (stage === "fetching") {
      mainBtn.textContent = "获取中…";
      mainBtn.disabled = true;
    } else if (stage === "installing") {
      mainBtn.textContent = preview?.alreadyInstalled ? "更新中…" : "安装中…";
      mainBtn.disabled = true;
    } else if (stage === "preview" && preview) {
      if (preview.builtinBlocked) {
        mainBtn.textContent = "无法安装";
        mainBtn.disabled = true;
      } else if (preview.alreadyInstalled) {
        mainBtn.textContent = `覆盖更新到 v${preview.version}`;
        mainBtn.disabled = false;
      } else {
        mainBtn.textContent = "安装";
        mainBtn.disabled = false;
      }
    } else {
      mainBtn.textContent = "获取信息";
      mainBtn.disabled = urlInput.value.trim() === "";
    }
    cancelBtn.disabled = stage === "installing";
    urlInput.disabled = stage === "fetching" || stage === "installing";
    syncMirrorHint();
  }

  function showError(msg: string): void {
    errBox.textContent = msg;
    errBox.style.display = "";
  }
  function clearError(): void {
    errBox.textContent = "";
    errBox.style.display = "none";
  }

  /** 只清预览 UI，不触碰后端暂存（token 已被消费时用它，避免拿死 token 再发 cancel）。 */
  function clearPreviewUi(): void {
    previewBox.innerHTML = "";
    previewBox.style.display = "none";
  }

  /** 丢弃当前预览：清 UI，并 fire-and-forget 通知后端删除暂存目录。 */
  function discardPreview(): void {
    const token = preview?.token;
    preview = null;
    clearPreviewUi();
    if (token) {
      api
        .pluginInstallCancel(token)
        .catch((err) => console.error("plugin_install_cancel failed", err));
    }
  }

  // ---------- 预览渲染 ----------

  function metaRow(label: string, value: string, valueTitle?: string): HTMLElement {
    return h(
      "div",
      { class: "install-meta-row" },
      h("span", { class: "install-meta-label", text: label }),
      h("span", { class: "install-meta-value", text: value, title: valueTitle ?? value }),
    );
  }

  /**
   * 本次下载的**事后事实**：包体到底从哪个源下来的、是否经第三方镜像、是否做过 sha256 校验。
   *
   * 后端在下载完成后已经知道确切答案（install.rs 的 `downloadSource` / `viaMirror` / `hashVerified`），
   * 所以这里不允许再用「可能经第三方镜像中转」这种事前的泛化说法糊弄过去；
   * `hashVerified=false` 必须明写「未经哈希校验」并说明原因（见 install.rs 同名字段注释、
   * 以及插件分发规范 §8.5「客户端会在走镜像时于界面上如实告知」）。
   *
   * 措辞克制：不吓唬用户，但也不含糊——走官方且未校验时不加 warn 底色，走镜像且未校验时才加。
   */
  function downloadFactBlock(p: InstallPreview): HTMLElement {
    const src = p.downloadSource || "（后端未给出源名）";
    const risky = p.viaMirror && !p.hashVerified;
    const box = h("div", {
      class: risky ? "install-notice install-notice-warn" : "install-notice",
    });
    box.appendChild(h("div", { class: "install-notice-title", text: "本次下载" }));
    box.appendChild(
      h("div", {
        text: p.viaMirror
          ? `包体经第三方镜像「${src}」中转下载。镜像有能力篡改传输内容。`
          : `包体经 GitHub 官方源「${src}」直连下载，未经第三方中转。`,
      }),
    );
    box.appendChild(
      h("div", {
        text: p.hashVerified
          ? "已按可信哈希校验通过：下载到的内容与预期一致。"
          : p.viaMirror
            ? "本次下载未经哈希校验：手动粘贴仓库 URL 安装没有可信的哈希来源，iTools 无法确认内容与仓库原文一致。请确认你信任该仓库与上述镜像；介意可到「下载源」切到「仅官方」后重试。"
            : "本次下载未经哈希校验：手动粘贴仓库 URL 安装没有可信的哈希来源。内容直接来自 GitHub，未经第三方中转。",
      }),
    );
    return box;
  }

  function renderPreview(p: InstallPreview): void {
    previewBox.innerHTML = "";

    const logo = p.logo
      ? h("img", { class: "install-logo", src: p.logo, alt: "" })
      : h("span", { class: "install-logo plugin-logo-fallback", html: PLUGIN_GLYPH });

    previewBox.appendChild(
      h(
        "div",
        { class: "install-head" },
        logo,
        h(
          "div",
          { class: "install-head-meta" },
          h(
            "div",
            { class: "install-name-row" },
            h("span", { class: "install-name", text: p.name }),
            h("span", { class: "install-ver", text: "v" + p.version }),
          ),
          p.author ? h("div", { class: "install-author", text: "作者：" + p.author }) : null,
          h("div", { class: "install-desc", text: p.description || "（无描述）" }),
        ),
      ),
    );

    // 来源 / 内容体量
    const metas = h("div", { class: "install-metas" });
    metas.appendChild(metaRow("来源仓库", sourceLabel(p.source), p.source.pageUrl));
    if (p.source.subPath) metas.appendChild(metaRow("仓库子目录", "/" + p.source.subPath));
    metas.appendChild(
      metaRow(
        "版本引用",
        revisionLabel(p.source.revision),
        p.source.revision || "未指定 revision，安装时取仓库默认分支的最新提交",
      ),
    );
    metas.appendChild(
      metaRow("功能与关键字", `${p.featureCount} 个功能 · ${p.cmds.length} 个关键字`),
    );
    if (p.cmds.length) metas.appendChild(metaRow("关键字", p.cmds.join("  ·  ")));
    metas.appendChild(metaRow("包体内容", `${p.fileCount} 个文件 · ${formatBytes(p.totalBytes)}`));
    previewBox.appendChild(metas);

    // 本次实际走了哪个源 / 有没有校验过——事后事实，紧跟在来源信息之后
    previewBox.appendChild(downloadFactBlock(p));

    // 权限文案必须与后端真实行为一致：
    //   全新安装 / 换来源覆盖 → 后端清空 plugin_permissions，安装后确实默认不授权；
    //   同源覆盖升级        → 后端**保留**历史授权，装完 runCommand/network 立刻可用。
    // 「覆盖更新到 vX」最常见的用法恰恰是同源覆盖，一律写「安装后默认不授权」就是对用户撒谎。
    const permBox = h("div", { class: "install-perm-box" });
    if (p.permissions.length) {
      permBox.classList.add("install-perm-box-warn");
      const keepsGrants = p.alreadyInstalled && p.sameSource;
      permBox.append(
        h("div", { class: "install-perm-title", text: "该插件申请了高危能力" }),
        h(
          "div",
          { class: "install-perm-chips" },
          ...p.permissions.map((perm) =>
            h("span", { class: "install-perm-chip", text: permLabel(perm) }),
          ),
        ),
      );
      if (keepsGrants) {
        const note = h("div", {
          class: "install-perm-note",
          text: "同源升级：该插件已有的授权会被保留，更新后立即生效（无需重新授权）。",
        });
        permBox.appendChild(note);
        // 已授权项来自本机真实记录；取不到就不显示这一行，绝不臆测
        void api
          .listPlugins()
          .then((all) => {
            const cur = all.find((x) => x.name === p.name);
            if (!cur || !note.isConnected) return;
            const granted = cur.granted.filter((g) => p.permissions.includes(g));
            note.textContent = granted.length
              ? `同源升级：已授权的「${granted.map(permLabel).join("、")}」会被保留，更新后立即生效；其余能力仍需到插件详情开启。`
              : "同源升级：会保留已有授权。当前该插件尚未获得任何授权，更新后仍需到插件详情逐项开启。";
          })
          .catch((err) => console.error("list_plugins failed", err));
      } else if (p.alreadyInstalled) {
        permBox.appendChild(
          h("div", {
            class: "install-perm-note",
            text: "安装来源与已安装版本不同：历史授权会被清空，安装后默认不授权，需到插件详情逐项开启。",
          }),
        );
      } else {
        permBox.appendChild(
          h("div", {
            class: "install-perm-note",
            text: "安装后默认不授权，需到插件详情逐项开启后才会生效。",
          }),
        );
      }
    } else {
      permBox.appendChild(h("div", { class: "install-perm-title", text: "未申请高危能力" }));
    }
    previewBox.appendChild(permBox);

    // 已安装 / 内置提示
    if (p.builtinBlocked) {
      previewBox.appendChild(
        h("div", {
          class: "install-notice install-notice-warn",
          text:
            `「${p.name}」是随安装包分发的内置插件，不可被 Git 包覆盖。` +
            `它的更新请走插件市场 —— 那条路的每个版本都过了服务端审核。`,
        }),
      );
    } else if (p.isBuiltin) {
      // 市场安装内置插件：允许，但要说清楚会发生什么
      previewBox.appendChild(
        h("div", {
          class: "install-notice",
          text:
            `「${p.name}」随安装包内置了一份，本次将用市场版 v${p.version} 覆盖它。` +
            `之后启动不会再被内置版本盖回去（插件的用户数据不在插件目录内，不会被清除）。`,
        }),
      );
    } else if (p.alreadyInstalled) {
      previewBox.appendChild(
        h("div", {
          class: p.sameSource ? "install-notice" : "install-notice install-notice-warn",
          text:
            `本机已安装 v${p.installedVersion ?? "?"}，继续将整目录替换为 v${p.version}（插件的用户数据不在插件目录内，不会被清除）。` +
            (p.sameSource
              ? ""
              : "　本次安装来源与已安装版本不同，等同于换源重装。"),
        }),
      );
    }

    // README（折叠）
    // 这里的 README 来自**尚未安装、来源不可信**的仓库：禁止加载其中的远端图片。
    // 管理中心窗口没有 CSP（tauri.conf.json security.csp = null），img.src 会立即发出真实请求，
    // 用户只是「粘贴 URL → 获取信息 → 取消」就会把 IP / UA / 访问时间泄露给仓库作者。
    // 被拦截的图片降级为可见占位文本，不静默丢弃。（已安装插件的详情页不受此限制。）
    if (p.readme && p.readme.trim()) {
      const details = h("details", { class: "install-readme" });
      details.appendChild(h("summary", { text: "查看说明文档（README）" }));
      details.appendChild(renderMarkdown(p.readme, { allowRemoteImages: false }));
      previewBox.appendChild(details);
    }

    previewBox.style.display = "";
  }

  // ---------- 动作 ----------

  async function fetchPreview(): Promise<void> {
    if (stage === "fetching" || stage === "installing") return;
    const url = urlInput.value.trim();
    if (!ctx.source && !url) return;
    discardPreview(); // 重新获取前，先清掉上一次的后端暂存
    clearError();
    stage = "fetching";
    syncButtons();
    try {
      const p = ctx.source ? await ctx.source.fetchPreview() : await api.pluginInstallPreview(url);
      preview = p;
      stage = "preview";
      renderPreview(p);
    } catch (err) {
      console.error("plugin_install_preview failed", err);
      stage = "idle";
      showError(errText(err, "获取插件信息失败"));
    } finally {
      syncButtons();
    }
  }

  async function doInstall(): Promise<void> {
    if (!preview || preview.builtinBlocked || stage !== "preview") return;
    const p = preview;
    clearError();
    stage = "installing";
    syncButtons();
    try {
      await api.pluginInstallConfirm(p.token);
      preview = null; // token 已被后端消费，离开时不再取消
      ctx.toast(p.alreadyInstalled ? `已更新 ${p.name} 到 v${p.version}` : `已安装 ${p.name} v${p.version}`);
      close();
      ctx.onInstalled(p.name);
    } catch (err) {
      console.error("plugin_install_confirm failed", err);
      // 后端 plugin_install_confirm 一进函数就把暂存记录（含 token）摘走，之后无论是内置插件拒绝
      // 还是落地失败，这个 token 都已不存在。所以失败后不能停在「安装」态：再点只会得到
      // 「安装会话已失效」这种与真实原因无关的二次错误，关窗时 discardPreview 还会拿死 token
      // 再发一次 cancel。这里直接丢掉预览退回「获取信息」，并保留后端的真实错误原文。
      preview = null; // token 已被消费，不可再 cancel，故走 clearPreviewUi 而非 discardPreview
      stage = "idle";
      clearPreviewUi();
      showError(errText(err, "安装失败") + "\n安装会话已结束，请重新点击「获取信息」。");
      syncButtons();
    }
  }

  // ---------- 关闭 ----------

  function close(): void {
    document.removeEventListener("keydown", onKey, true);
    discardPreview(); // 未确认的暂存必须清理
    mask.remove();
  }

  /** 安装写盘过程中不允许中途关闭，避免落地做到一半 */
  function tryClose(): void {
    if (stage === "installing") return;
    close();
  }

  const onKey = (e: KeyboardEvent): void => {
    if (e.key !== "Escape") return;
    e.stopPropagation(); // 抢在 main.ts 的关窗监听前，只关弹窗
    tryClose();
  };

  // ---------- 事件 ----------
  urlInput.addEventListener("input", () => {
    if (stage === "preview") {
      // URL 改了，旧预览不再对应输入内容：丢弃并回到「获取信息」
      stage = "idle";
      discardPreview();
      clearError();
    }
    syncButtons();
  });
  urlInput.addEventListener("keydown", (e) => {
    if (e.key !== "Enter") return;
    e.preventDefault();
    if (stage === "preview") void doInstall();
    else void fetchPreview();
  });
  mainBtn.addEventListener("click", () => {
    if (stage === "preview") void doInstall();
    else void fetchPreview();
  });
  cancelBtn.addEventListener("click", tryClose);
  closeBtn.addEventListener("click", tryClose);
  mask.addEventListener("mousedown", (e) => {
    if (e.target === mask) tryClose();
  });
  document.addEventListener("keydown", onKey, true);

  document.body.appendChild(mask);
  syncButtons();
  if (ctx.source) {
    urlRow.style.display = "none";
    hint.style.display = "none";
    void fetchPreview(); // 来源已定，打开即取预览
  } else {
    urlInput.focus();
  }
}
