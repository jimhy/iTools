//! 零依赖、安全的轻量 Markdown 渲染器（用于插件详情页展示 README.md）。
//!
//! 安全策略：全程用 `document.createElement` + `textContent` 构建 DOM，**从不** innerHTML 注入原始文本，
//! 故无 XSS 面（即便插件作者恶意）。URL 经 `safeUrl` 白名单校验（http/https/mailto/相对路径）。
//!
//! 支持：ATX 标题、粗/斜体、行内代码、围栏代码块、有序/无序列表、引用、链接、图片、
//! 分隔线、GFM 表格、段落。不追求全 CommonMark，覆盖插件 README 常用语法即可。

/** 校验 URL：只放行 http/https/mailto 与相对路径；危险协议（javascript:/data: 等）归一为 "#"。 */
function safeUrl(url: string): string {
  const u = url.trim();
  // 剥离所有控制字符与空白（码点 <= 0x20，以及 0x7f-0x9f 的 C1 区）后再判定协议：
  // WebView2/Chromium 解析前会去掉前导控制字符，故 "\x01javascript:" 会被当成 javascript: 执行——
  // 必须以剥离后的形态过滤，否则 trim()（不去 C0 控制字符）会让它落到「无协议→放行」分支绕过白名单。
  let probe = "";
  for (let i = 0; i < u.length; i++) {
    const c = u.charCodeAt(i);
    if (c > 0x20 && !(c >= 0x7f && c <= 0x9f)) probe += u[i];
  }
  probe = probe.toLowerCase();
  if (probe.startsWith("http:") || probe.startsWith("https:") || probe.startsWith("mailto:")) {
    return u;
  }
  // 带任何其它协议（javascript:/data:/vbscript:/file: …）→ 阻断
  if (/^[a-z][a-z0-9+.-]*:/.test(probe)) return "#";
  // 无协议：相对路径 / 锚点，放行
  return u;
}

/** 行内解析：把一段文本切成 文本/代码/强调/链接/图片 节点。code 内部不再解析。 */
function parseInline(text: string): Node[] {
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
      const img = document.createElement("img");
      img.src = safeUrl(src);
      img.alt = alt;
      img.loading = "lazy";
      nodes.push(img);
    } else if (tok.startsWith("[")) {
      const label = tok.slice(1, tok.indexOf("]"));
      const href = tok.slice(tok.indexOf("(") + 1, -1);
      const a = document.createElement("a");
      a.href = safeUrl(href);
      a.target = "_blank";
      a.rel = "noopener noreferrer";
      a.append(...parseInline(label));
      nodes.push(a);
    } else if (tok.startsWith("**") || tok.startsWith("__")) {
      const strong = document.createElement("strong");
      strong.append(...parseInline(tok.slice(2, -2)));
      nodes.push(strong);
    } else {
      const em = document.createElement("em");
      em.append(...parseInline(tok.slice(1, -1)));
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

/** 渲染 markdown 文本为一个 `div.markdown-body`。 */
export function renderMarkdown(md: string): HTMLElement {
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
      h.append(...parseInline(hm[2].replace(/\s+#+\s*$/, "")));
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
      bq.appendChild(renderMarkdown(buf.join("\n")));
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
        th.append(...parseInline(cell));
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
          td.append(...parseInline(cells[c] ?? ""));
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
        li.append(...parseInline(im[3]));
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
        p.append(...parseInline(ln));
      });
      root.appendChild(p);
    }
  }

  return root;
}
