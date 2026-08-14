//! 开发者中心「发布」tab：把调试好的插件提交审核，并如实展示审核状态。
//!
//! # 这一页展示的每一个状态都来自服务端
//!
//! 「审核中 / 已上线 / 已驳回 / 待人工处理」是服务端提审单上的 `status`，
//! 「已上线版本号」来自市场索引。前端**一个都不推断**：没查到就显示查不到的原因，
//! 而不是拿本地信息拼一个看起来合理的状态出来。
//!
//! 特别是 `manual`（审核没能完成，需人工处理）：它**不是**通过。服务端在模型未配置、
//! 调用失败、裁决无法解析或服务重启时都会落这个状态，页面必须把它和「已上线」清楚分开——
//! 否则作者会以为自己的插件已经在市场里了。
//!
//! # 自检不是审核
//!
//! 提交前的本地自检只拦「服务端必然会拒」的那几条（缺 index.html、name 不合法、版本没升…）。
//! 通过自检**不代表**会过审：代码审核由服务端的大模型做，本地看不出结论。
//! 文案必须写清楚这层区别，不能让绿色的「自检通过」被读成「稳过」。
//!
//! # 不做假控件
//!
//! 不能提交时（未登录、有阻断项、已有在审的单子、版本号没升）按钮**禁用并写明原因**，
//! 不做「看着能点、点了没反应」的按钮。

import type { DevPluginInfo, Preflight, PublishStatus, Submission } from "../types";
import { h } from "./ui";
import * as api from "./api";
import { errText } from "./plugin-install";

/** 状态 → 展示元数据。取值即服务端 `status` 常量，多一个「本地态」用于还没提交过的情况。 */
const STATUS_META: Record<string, { label: string; cls: string; desc: string }> = {
  reviewing: {
    label: "审核中",
    cls: "dev-pub-badge-wait",
    desc: "服务端正在读你的代码。审核要花几十秒到几分钟，可以点「刷新状态」查看进展。",
  },
  approved: {
    label: "已上线",
    cls: "dev-pub-badge-ok",
    desc: "审核通过，这个版本已经发布到插件市场，用户可以在「插件市场」页装到它。",
  },
  rejected: {
    label: "已驳回",
    cls: "dev-pub-badge-bad",
    desc: "审核未通过，插件没有上线。下面是服务端给出的原因原文。",
  },
  manual: {
    label: "待人工处理",
    cls: "dev-pub-badge-warn",
    // 这句话是本页最重要的一句：manual 绝不能被读成「过了」
    desc: "自动审核没能完成（模型未接入、调用失败或服务重启），这次提交既没通过也没被驳回，插件尚未上线，需要维护者人工处理。",
  },
};

const fmtTime = (ms: number): string => {
  if (!ms || !Number.isFinite(ms)) return "—";
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? "—" : d.toLocaleString();
};

const fmtSize = (n: number): string =>
  n >= 1024 * 1024 ? `${(n / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;

/** 模型裁决里的一条问题（结构见服务端 `llm.rs::Finding`）。宽松解析：字段缺了也不崩。 */
interface Finding {
  severity?: string;
  file?: string;
  issue?: string;
  evidence?: string;
}
interface PermissionCheck {
  permission?: string;
  status?: string;
  note?: string;
}
interface Verdict {
  verdict?: string;
  riskLevel?: string;
  summary?: string;
  findings?: Finding[];
  permissionCheck?: PermissionCheck[];
}

const SEVERITY_LABEL: Record<string, string> = {
  blocker: "阻断",
  major: "严重",
  minor: "次要",
  info: "提示",
};

const PERM_STATUS_LABEL: Record<string, string> = {
  "declared-and-used": "已声明且确实用到",
  "declared-not-used": "已声明但代码里没用到",
  "used-not-declared": "代码里用到了但没声明",
};

export interface DevPublishOpts {
  plugin: DevPluginInfo;
  toast: (msg: string) => void;
}

export function renderDevPublish(host: HTMLElement, o: DevPublishOpts): void {
  const { plugin, toast } = o;
  host.innerHTML = "";

  let status: PublishStatus | null = null;
  let preflight: Preflight | null = null;
  let loading = false;
  let submitting = false;

  const body = h("div", { class: "dev-tab-body" });
  host.appendChild(body);

  const refreshBtn = h("button", { class: "btn", text: "刷新状态" });
  const submitBtn = h("button", { class: "btn btn-primary", text: "提交审核" });

  refreshBtn.addEventListener("click", () => void load());
  submitBtn.addEventListener("click", () => void doSubmit());

  // ---------- 各区块 ----------

  /** 顶部说明：这条链路到底会发生什么。 */
  function intro(): HTMLElement {
    return h(
      "div",
      { class: "dev-note" },
      h("div", { class: "dev-note-title", text: "提交审核会发生什么" }),
      h("div", {
        class: "dev-sec-desc",
        text:
          "① 客户端把这个调试目录打成 zip 上传到你配置的服务器（跳过 .git / node_modules 与点开头的文件）；" +
          "② 服务端先做机械校验（格式、清单、不含可执行文件、路径安全）；" +
          "③ 通过后由大模型通读代码，判断有无恶意行为、申请的权限是否名副其实、描述是否与实际功能相符；" +
          "④ 通过即自动发布到插件市场，用户可以直接安装。",
      }),
      h("div", {
        class: "dev-sec-desc",
        text:
          "提审需要先登录云账号 —— 插件的归属与审核结果都挂在账号上，" +
          "而且同名插件只有首次上线它的那个账号才能更新。",
      }),
    );
  }

  /** 状态卡：本地版本 / 线上版本 / 最近一次提审的结论。 */
  function statusCard(): HTMLElement {
    const box = h("div", { class: "dev-pub-card" });

    if (!status) {
      box.appendChild(h("div", { class: "dev-kv-loading", text: "正在查询发布状态…" }));
      return box;
    }

    // 查询本身出错时，先把真实原因摆出来。下面那些字段可能是不完整的，
    // 不写这一条就会把「查不到」渲染成「没有记录」。
    if (status.error) {
      box.appendChild(
        h(
          "div",
          { class: "info-box info-box-warn" },
          h("div", { text: "发布状态没能完整读取" }),
          h("div", { class: "info-box-detail", text: status.error }),
          h("div", {
            class: "info-box-detail",
            text: "下面显示的信息可能不完整。解决上面的问题后点「刷新状态」重试。",
          }),
        ),
      );
    }

    const latest = status.latest;
    // 大状态：线上态优先（它是用户真正看得到的事实），其次才是最近一次提审的进展
    let badgeLabel: string;
    let badgeCls: string;
    let badgeDesc: string;
    if (status.revoked) {
      badgeLabel = "已下架";
      badgeCls = "dev-pub-badge-bad";
      badgeDesc = `这个插件已被市场下架，用户无法再安装。下架原因：${status.revokedReason || "未给出原因"}`;
    } else if (status.onlineVersion) {
      badgeLabel = `已上线 v${status.onlineVersion}`;
      badgeCls = "dev-pub-badge-ok";
      badgeDesc = "插件市场里当前提供的就是这个版本。";
    } else if (latest && STATUS_META[latest.status]) {
      badgeLabel = STATUS_META[latest.status].label;
      badgeCls = STATUS_META[latest.status].cls;
      badgeDesc = STATUS_META[latest.status].desc;
    } else if (latest) {
      // 服务端给了个前端不认识的状态：如实显示原值，不猜
      badgeLabel = latest.status;
      badgeCls = "dev-pub-badge-warn";
      badgeDesc = "服务端返回了一个当前版本不认识的状态，已原样显示。";
    } else {
      badgeLabel = "未提审";
      badgeCls = "dev-pub-badge-idle";
      badgeDesc = "这个插件还没有提交过审核，也不在插件市场里。";
    }

    box.appendChild(
      h(
        "div",
        { class: "dev-pub-head" },
        h("span", { class: `dev-pub-badge ${badgeCls}`, text: badgeLabel }),
        h("div", { class: "dev-pub-head-text" }, h("div", { class: "dev-sec-desc", text: badgeDesc })),
      ),
    );

    // 版本对照
    const rows = h("div", { class: "dev-pub-rows" });
    rows.appendChild(row("提审名称", status.name, "服务端按它归属插件与授权；改名等于换一个插件"));
    rows.appendChild(row("本地版本", `v${status.localVersion}`, "plugin.json 里写的那个"));
    rows.appendChild(
      row(
        "线上版本",
        status.onlineVersion ? `v${status.onlineVersion}` : "（还没上线过）",
        "市场索引里这个插件当前的版本",
      ),
    );

    // 「审核中」那一版是哪个版本 —— 作者最常问的问题
    if (latest && latest.status === "reviewing") {
      rows.appendChild(row("在审版本", `v${latest.version}`, `提交于 ${fmtTime(latest.createdAt)}`));
    }
    box.appendChild(rows);

    // 最近一次提审的结论原文（驳回原因 / 人工处理原因都在这里）
    if (latest && latest.message) {
      const cls =
        latest.status === "rejected" || latest.status === "manual"
          ? "info-box info-box-warn"
          : "info-box";
      box.appendChild(
        h(
          "div",
          { class: cls },
          h("div", { text: `最近一次提审（v${latest.version} · ${fmtTime(latest.updatedAt)}）` }),
          // 服务端原文，逐行展示，不改写
          ...latest.message.split("\n").map((line) => h("div", { class: "info-box-detail", text: line })),
        ),
      );
    }

    return box;
  }

  function row(label: string, value: string, desc: string): HTMLElement {
    return h(
      "div",
      { class: "dev-pub-row" },
      h(
        "div",
        { class: "dev-pub-row-text" },
        h("span", { class: "dev-pub-row-label", text: label }),
        h("span", { class: "dev-pub-row-desc", text: desc }),
      ),
      h("code", { class: "dev-pub-row-value", text: value }),
    );
  }

  /** 自检结果。 */
  function preflightCard(): HTMLElement {
    const box = h("div", { class: "dev-pub-card" });
    box.appendChild(h("div", { class: "dev-sec-title", text: "提交前自检" }));
    box.appendChild(
      h("div", {
        class: "dev-sec-desc",
        text:
          "只检查「服务端一定会拒」的那几条，帮你省一次上传。" +
          "自检通过不代表会过审 —— 代码审核在服务端，本地看不出结论。",
      }),
    );

    if (!preflight) {
      box.appendChild(h("div", { class: "dev-kv-loading", text: "自检中…" }));
      return box;
    }

    // 版本号那一条由**后端自检**给出（它会现查一次线上版本），前端不再自己拼——
    // 两处各判一次迟早分叉，而且提交时后端还会再查一遍，以它为准最准确。
    const blockers = preflight.blockers;

    if (!blockers.length && !preflight.warnings.length) {
      box.appendChild(h("div", { class: "dev-issues-ok", text: "没有发现会导致提交失败的问题。" }));
    }
    for (const b of blockers) {
      box.appendChild(
        h(
          "div",
          { class: "dev-issue dev-issue-error" },
          h("span", { class: "dev-issue-level", text: "阻断" }),
          h("span", { class: "dev-issue-msg", text: b }),
        ),
      );
    }
    for (const w of preflight.warnings) {
      box.appendChild(
        h(
          "div",
          { class: "dev-issue dev-issue-warn" },
          h("span", { class: "dev-issue-level", text: "提醒" }),
          h("span", { class: "dev-issue-msg", text: w }),
        ),
      );
    }
    return box;
  }

  /** 提交区：按钮 + 禁用原因。 */
  function submitCard(): HTMLElement {
    const box = h("div", { class: "dev-pub-card" });
    const reason = disabledReason();
    submitBtn.disabled = reason !== null || submitting || loading;
    submitBtn.textContent = submitting ? "正在上传…" : "提交审核";
    submitBtn.title = reason ?? "把当前目录打包上传，交给服务端审核";

    box.appendChild(
      h(
        "div",
        { class: "dev-run-actions" },
        submitBtn,
        refreshBtn,
        reason ? h("span", { class: "dev-blocked", text: reason }) : null,
      ),
    );
    return box;
  }

  /** 给出「比它高一档」的版本号建议（末段 +1）。纯提示用，作者当然可以填别的。 */
function bumpHint(online: string): string {
  const parts = online.trim().replace(/^v/i, "").split(".");
  const last = Number(parts[parts.length - 1]);
  if (!Number.isFinite(last)) return `${online}.1`;
  parts[parts.length - 1] = String(last + 1);
  return parts.join(".");
}

/** 不能提交时的**具体**原因；能提交返回 null。 */
  function disabledReason(): string | null {
    if (loading) return "正在读取状态…";
    if (!plugin.runnable) return "这个插件当前跑不起来（见上方的清单问题），修好再提交";
    if (!preflight) return "自检还没跑完";
    if (preflight.blockers.length) return "自检有阻断项，修好再提交";
    if (!status) return "还没读到发布状态";
    // 未登录 / 服务器不可达时，提交必然失败 —— 与其让用户点了吃一记报错，不如直接说清楚
    if (status.error) {
      return status.error.includes("登录")
        ? "需要先登录云账号（插件归属与审核结果都挂在账号上）"
        : "发布状态读取失败，先解决上面的问题";
    }
    if (status.latest?.status === "reviewing") {
      return `v${status.latest.version} 还在审核中，出结果后才能提交下一版`;
    }
    if (status.onlineVersion && !status.canSubmitNewVersion) {
      // 说清楚「现在是多少、要改成什么、为什么」——只说「版本号不对」等于没说
      return (
        `本地版本 v${status.localVersion} 不高于已上线的 v${status.onlineVersion}，不能提交。` +
        `请把 plugin.json 的 version 升上去（例如 v${bumpHint(status.onlineVersion)}）再提交` +
        `—— 客户端的更新检查只比这一个值，不升版本号用户收不到更新。`
      );
    }
    return null;
  }

  /** 提审历史（可展开看模型裁决原文）。 */
  function historyCard(): HTMLElement {
    const box = h("div", { class: "dev-pub-card" });
    box.appendChild(h("div", { class: "dev-sec-title", text: "提审记录" }));
    const list = status?.history ?? [];
    if (!list.length) {
      box.appendChild(
        h("div", {
          class: "dev-sec-desc",
          text: status?.error
            ? "没能读到提审记录（原因见上方）。"
            : "这个插件还没有提交过审核。",
        }),
      );
      return box;
    }
    for (const s of list) box.appendChild(historyRow(s));
    return box;
  }

  function historyRow(s: Submission): HTMLElement {
    const meta = STATUS_META[s.status];
    const detail = h("div", { class: "dev-pub-detail" });
    let expanded = false;
    let loaded = false;

    const toggle = h("button", { class: "btn btn-sm", text: "查看审核详情" });
    toggle.addEventListener("click", async () => {
      expanded = !expanded;
      detail.style.display = expanded ? "" : "none";
      toggle.textContent = expanded ? "收起" : "查看审核详情";
      if (!expanded || loaded) return;
      detail.replaceChildren(h("div", { class: "dev-kv-loading", text: "读取中…" }));
      try {
        const full = await api.devSubmissionDetail(s.id);
        loaded = true;
        detail.replaceChildren(verdictBlock(full));
      } catch (err) {
        detail.replaceChildren(
          h("div", { class: "dev-kv-error", text: `读取审核详情失败：${errText(err, "未知错误")}` }),
        );
      }
    });
    detail.style.display = "none";

    return h(
      "div",
      { class: "dev-pub-hist" },
      h(
        "div",
        { class: "dev-pub-hist-head" },
        h("span", { class: `dev-pub-badge ${meta?.cls ?? "dev-pub-badge-warn"}`, text: meta?.label ?? s.status }),
        h("span", { class: "dev-pub-hist-ver", text: `v${s.version}` }),
        h("span", { class: "dev-pub-hist-time", text: fmtTime(s.createdAt) }),
        h("span", {
          class: "dev-pub-hist-size",
          text: `${s.fileCount} 个文件 · ${fmtSize(s.sizeBytes)}`,
        }),
        h("div", { class: "dev-pub-hist-acts" }, toggle),
      ),
      s.message
        ? h("div", { class: "dev-pub-hist-msg", text: s.message })
        : null,
      detail,
    );
  }

  /** 模型裁决原文的展开区。 */
  function verdictBlock(s: Submission): HTMLElement {
    const box = h("div");
    const v = (s.review ?? null) as Verdict | null;
    if (!v || typeof v !== "object" || !v.verdict) {
      box.appendChild(
        h("div", {
          class: "dev-sec-desc",
          text:
            "这次提审没有模型裁决可看 —— 可能是自动审核未接入或调用失败（结论见上面那行原文）。",
        }),
      );
      return box;
    }

    box.appendChild(
      h("div", {
        class: "dev-sec-desc",
        text: `裁决：${v.verdict === "approve" ? "通过" : "不通过"}${v.riskLevel ? `　·　风险等级 ${v.riskLevel}` : ""}`,
      }),
    );
    if (v.summary) box.appendChild(h("div", { class: "dev-pub-hist-msg", text: v.summary }));

    const findings = Array.isArray(v.findings) ? v.findings : [];
    if (findings.length) {
      box.appendChild(h("div", { class: "dev-sec-title", text: "逐条问题" }));
      for (const f of findings) {
        const level = f.severity === "blocker" || f.severity === "major" ? "dev-issue-error" : "dev-issue-warn";
        box.appendChild(
          h(
            "div",
            { class: `dev-issue ${level}` },
            h("span", { class: "dev-issue-level", text: SEVERITY_LABEL[f.severity ?? ""] ?? f.severity ?? "—" }),
            h(
              "div",
              { class: "dev-issue-msg" },
              f.file ? h("code", { class: "dev-issue-field", text: f.file }) : null,
              h("div", { text: f.issue ?? "（模型没给描述）" }),
              f.evidence ? h("pre", { class: "dev-log-pre", text: f.evidence }) : null,
            ),
          ),
        );
      }
    }

    const perms = Array.isArray(v.permissionCheck) ? v.permissionCheck : [];
    if (perms.length) {
      box.appendChild(h("div", { class: "dev-sec-title", text: "权限声明核对" }));
      for (const p of perms) {
        box.appendChild(
          h(
            "div",
            { class: "dev-perm-row" },
            h("span", { class: "dev-perm-name", text: p.permission ?? "—" }),
            h("span", {
              class: "dev-perm-text",
              text: `${PERM_STATUS_LABEL[p.status ?? ""] ?? p.status ?? "—"}${p.note ? `（${p.note}）` : ""}`,
            }),
          ),
        );
      }
    }
    return box;
  }

  // ---------- 数据装载 ----------

  function paint(): void {
    body.replaceChildren(intro(), statusCard(), preflightCard(), submitCard(), historyCard());
  }

  async function load(): Promise<void> {
    if (loading) return;
    loading = true;
    refreshBtn.disabled = true;
    paint();
    // 两个请求互不依赖，并发发出；各自的失败各自呈现，不互相掩盖
    const [st, pf] = await Promise.allSettled([
      api.devPublishStatus(plugin.id),
      api.devPreflight(plugin.id),
    ]);
    if (st.status === "fulfilled") {
      status = st.value;
    } else {
      status = {
        name: plugin.name,
        localVersion: plugin.version,
        onlineVersion: null,
        revoked: false,
        revokedReason: "",
        canSubmitNewVersion: false,
        latest: null,
        history: [],
        error: errText(st.reason, "查询发布状态失败"),
      };
    }
    preflight =
      pf.status === "fulfilled"
        ? pf.value
        : { blockers: [`自检没能执行：${errText(pf.reason, "未知错误")}`], warnings: [] };
    loading = false;
    refreshBtn.disabled = false;
    paint();
  }

  async function doSubmit(): Promise<void> {
    if (submitting) return;
    submitting = true;
    paint();
    try {
      const sub = await api.devSubmitPlugin(plugin.id);
      toast(`已提交 v${sub.version}，服务端正在审核`);
    } catch (err) {
      // 服务端的拒绝原因是写给作者看的，原样弹出来
      toast(errText(err, "提交审核失败"));
    } finally {
      submitting = false;
      await load();
    }
  }

  void load();
}
