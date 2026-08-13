//! 零依赖、安全的轻量 Markdown 渲染器（用于插件详情页展示 README.md）。
//!
//! 安全策略：全程用 `document.createElement` + `textContent` 构建 DOM，**从不** innerHTML 注入原始文本，
//! 故无 XSS 面（即便插件作者恶意）。URL 经 `safeUrl` 白名单校验（http/https/mailto/相对路径）。
//!
//! 隐私策略：`allowRemoteImages=false` 时不加载任何远端图片（见 `RenderOptions`）。
//!
//! 支持：ATX 标题、粗/斜体、行内代码、围栏代码块、有序/无序列表、引用、链接、图片、
//! 分隔线、GFM 表格、段落。不追求全 CommonMark，覆盖插件 README 常用语法即可。

/** 渲染选项 */
export interface RenderOptions {
  /**
   * 是否允许加载 README 里的**远端**图片（http/https 以及协议相对的 `//host/…`）。
   *
   * 默认 true（已安装插件的详情页：包已由用户信任并落地，维持原有观感）。
   * 传 false 用于**尚未安装、来源不可信**的场景（安装预览）：管理中心窗口没有 CSP
   * （tauri.conf.json 的 security.csp = null），`img.src` 会立刻发起真实请求，
   * 于是攻击者只要在 README 里写 `![](https://attacker.tld/px.png)`，用户「粘贴 URL →
   * 获取信息 → 看完取消」这条从未同意安装的路径就已经把 IP / UA / 访问时间回传给了攻击者。
   * 关闭后远端图片降级为可见的占位文本（不静默丢弃，用户能知道这里原本有张图）。
   */
  allowRemoteImages?: boolean;
}

/** 内部使用的规范化选项（避免每层递归都做默认值判断） */
interface ResolvedOptions {
  allowRemoteImages: boolean;
}

/** 剥离所有控制字符与空白（码点 <= 0x20，以及 0x7f-0x9f 的 C1 区）并转小写，用于协议判定。
 *  WebView2/Chromium 解析前会去掉前导控制字符，故 "\x01javascript:" 会被当成 javascript: 执行——
 *  必须以剥离后的形态判定，否则 trim()（不去 C0 控制字符）会让它落到「无协议→放行」分支绕过白名单。 */
function urlProbe(url: string): string {
  let probe = "";
  for (let i = 0; i < url.length; i++) {
    const c = url.charCodeAt(i);
    if (c > 0x20 && !(c >= 0x7f && c <= 0x9f)) probe += url[i];
  }
  return probe.toLowerCase();
}

/** 校验 URL：只放行 http/https/mailto 与相对路径；危险协议（javascript:/data: 等）归一为 "#"。 */
function safeUrl(url: string): string {
  const u = url.trim();
  const probe = urlProbe(u);
  if (probe.startsWith("http:") || probe.startsWith("https:") || probe.startsWith("mailto:")) {
    return u;
  }
  // 带任何其它协议（javascript:/data:/vbscript:/file: …）→ 阻断
  if (/^[a-z][a-z0-9+.-]*:/.test(probe)) return "#";
  // 无协议：相对路径 / 锚点，放行
  return u;
}

/** 是否会向外网发起请求：显式 http/https，或协议相对的 `//host/path`（safeUrl 视其为「无协议」放行，
 *  但浏览器会按当前协议补全后真的联网，故必须一并算作远端）。 */
function isRemoteUrl(url: string): boolean {
  const probe = urlProbe(url.trim());
  return probe.startsWith("http:") || probe.startsWith("https:") || probe.startsWith("//");
}

/** 被拦截的远端图片 → 明确的占位文本（诚实告知「这里本来有张图，我们没加载」）。 */
function blockedImage(alt: string): HTMLElement {
  const span = document.createElement("span");
  span.className = "md-img-blocked";
  span.textContent = alt.trim() ? `[已拦截远端图片：${alt.trim()}]` : "[已拦截远端图片]";
  span.title = "安装预览不加载来自未知仓库的远端图片，避免在你确认安装前向对方暴露访问行为";
  return span;
}

/** 行内解析：把一段文本切成 文本/代码/强调/链接/图片 节点。code 内部不再解析。 */
function parseInline(text: string, opts: ResolvedOptions): Node[] {
  const nodes: Node[] = [];
  // 依次匹配：行内代码 | 图片 | 链接 | 加粗(**/__) | 斜体(*/_)
  const re =
    /(`[^`]+`)|(!\[[^\]]*\]\([^)\s]+\))|(\[[^\]]+\]\([^)\s]+\))|(\*\*[^*]+\*\*)|(__[^_]+__)|(\*[^*]+\*)|(_[^_]+_)/;
  let rest = text;
  while (rest.length) {
    const m = re.exec(rest);
    if (!m) {
      nodes.push(document.createTextNode(rest));
      break;
    }
    if (m.index > 0) nodes.push(document.createTextNode(rest.slice(0, m.index)));
    const tok = m[0];
    if (tok.startsWith("`")) {
      const code = document.createElement("code");
      code.textContent = tok.slice(1, -1);
      nodes.push(code);
    } else if (tok.startsWith("![")) {
      const alt = tok.slice(2, tok.indexOf("]"));
      const src = tok.slice(tok.indexOf("(") + 1, -1);
      if (!opts.allowRemoteImages && isRemoteUrl(src)) {
        // 不创建 img 元素：只要 src 一落到 DOM 上请求就发出去了，无法事后撤销
        nodes.push(blockedImage(alt));
      } else {
        const img = document.createElement("img");
        img.src = safeUrl(src);
        img.alt = alt;
        img.loading = "lazy";
        nodes.push(img);
      }
    } else if (tok.startsWith("[")) {
      const label = tok.slice(1, tok.indexOf("]"));
      const href = tok.slice(tok.indexOf("(") + 1, -1);
      const a = document.createElement("a");
      a.href = safeUrl(href);
      a.target = "_blank";
      a.rel = "noopener noreferrer";
      a.append(...parseInline(label, opts));
      nodes.push(a);
    } else if (tok.startsWith("**") || tok.startsWith("__")) {
      const strong = document.createElement("strong");
      strong.append(...parseInline(tok.slice(2, -2), opts));
      nodes.push(strong);
    } else {
      const em = document.createElement("em");
      em.append(...parseInline(tok.slice(1, -1), opts));
      nodes.push(em);
    }
    rest = rest.slice(m.index + tok.length);
  }
  return nodes;
}

/** 判断是否 GFM 表格分隔行，如 `|---|:--:|`。 */
function isTableSep(line: string): boolean {
  return /^\s*\|?\s*:?-{1,}:?\s*(\|\s*:?-{1,}:?\s*)*\|?\s*$/.test(line) && line.includes("-");
}

/** 把 `| a | b |` 拆成单元格文本。 */
function splitRow(line: string): string[] {
  return line
    .trim()
    .replace(/^\||\|$/g, "")
    .split("|")
    .map((c) => c.trim());
}

/**
 * 渲染 markdown 文本为一个 `div.markdown-body`。
 *
 * @param opts 渲染选项；安装预览等**来源不可信**的场景必须传 `{ allowRemoteImages: false }`。
 */
export function renderMarkdown(md: string, opts?: RenderOptions): HTMLElement {
  return renderBlocks(md, { allowRemoteImages: opts?.allowRemoteImages !== false });
}

/** 块级渲染主体（选项已规范化，递归内部直接透传）。 */
function renderBlocks(md: string, opts: ResolvedOptions): HTMLElement {
  const root = document.createElement("div");
  root.className = "markdown-body";
  const lines = md.replace(/\r\n/g, "\n").split("\n");
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];
    const trimmed = line.trim();

    // 围栏代码块 ```lang
    if (/^```/.test(trimmed)) {
      const buf: string[] = [];
      i++;
      while (i < lines.length && !/^```/.test(lines[i].trim())) {
        buf.push(lines[i]);
        i++;
      }
      i++; // 跳过闭合 ```
      const pre = document.createElement("pre");
      const code = document.createElement("code");
      code.textContent = buf.join("\n");
      pre.appendChild(code);
      root.appendChild(pre);
      continue;
    }

    // 分隔线
    if (/^(-{3,}|\*{3,}|_{3,})$/.test(trimmed)) {
      root.appendChild(document.createElement("hr"));
      i++;
      continue;
    }

    // ATX 标题
    const hm = line.match(/^(#{1,6})\s+(.*)$/);
    if (hm) {
      const h = document.createElement(`h${hm[1].length}`);
      h.append(...parseInline(hm[2].replace(/\s+#+\s*$/, ""), opts));
      root.appendChild(h);
      i++;
      continue;
    }

    // 引用（连续 > 行 → 递归渲染内部）
    if (/^>\s?/.test(line)) {
      const buf: string[] = [];
      while (i < lines.length && /^>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^>\s?/, ""));
        i++;
      }
      const bq = document.createElement("blockquote");
      bq.appendChild(renderBlocks(buf.join("\n"), opts));
      root.appendChild(bq);
      continue;
    }

    // 表格（当前行含 | 且下一行是分隔行）
    if (line.includes("|") && i + 1 < lines.length && isTableSep(lines[i + 1])) {
      const header = splitRow(line);
      i += 2; // 跳过表头 + 分隔行
      const table = document.createElement("table");
      const thead = document.createElement("thead");
      const htr = document.createElement("tr");
      for (const cell of header) {
        const th = document.createElement("th");
        th.append(...parseInline(cell, opts));
        htr.appendChild(th);
      }
      thead.appendChild(htr);
      table.appendChild(thead);
      const tbody = document.createElement("tbody");
      while (i < lines.length && lines[i].includes("|") && lines[i].trim() !== "") {
        const cells = splitRow(lines[i]);
        const tr = document.createElement("tr");
        for (let c = 0; c < header.length; c++) {
          const td = document.createElement("td");
          td.append(...parseInline(cells[c] ?? "", opts));
          tr.appendChild(td);
        }
        tbody.appendChild(tr);
        i++;
      }
      table.appendChild(tbody);
      root.appendChild(table);
      continue;
    }

    // 列表（连续的 - / * / + / 1. 行）
    const listM = line.match(/^(\s*)([-*+]|\d+\.)\s+(.*)$/);
    if (listM) {
      const ordered = /\d+\./.test(listM[2]);
      const listEl = document.createElement(ordered ? "ol" : "ul");
      while (i < lines.length) {
        const im = lines[i].match(/^(\s*)([-*+]|\d+\.)\s+(.*)$/);
        if (!im) break;
        const li = document.createElement("li");
        li.append(...parseInline(im[3], opts));
        listEl.appendChild(li);
        i++;
      }
      root.appendChild(listEl);
      continue;
    }

    // 空行：跳过
    if (trimmed === "") {
      i++;
      continue;
    }

    // 段落（收集到下一个空行/块起始）
    const buf: string[] = [];
    while (
      i < lines.length &&
      lines[i].trim() !== "" &&
      !/^```/.test(lines[i].trim()) &&
      !/^#{1,6}\s+/.test(lines[i]) &&
      !/^>\s?/.test(lines[i]) &&
      !/^(\s*)([-*+]|\d+\.)\s+/.test(lines[i]) &&
      !/^(-{3,}|\*{3,}|_{3,})$/.test(lines[i].trim())
    ) {
      buf.push(lines[i]);
      i++;
    }
    if (buf.length) {
      const p = document.createElement("p");
      // 段内换行保留为 <br>
      buf.forEach((ln, idx) => {
        if (idx > 0) p.appendChild(document.createElement("br"));
        p.append(...parseInline(ln, opts));
      });
      root.appendChild(p);
    }
  }

  return root;
}
