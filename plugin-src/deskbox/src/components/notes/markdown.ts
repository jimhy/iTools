/**
 * Markdown → HTML 迁移器：把「旧版以 Markdown 文本存储」的笔记正文转换成 TipTap 可解析的 HTML，
 * 供切换到富文本编辑器后一次性迁移（新笔记直接以 HTML 存储，不再经此）。
 *
 * 安全：先整体转义，再套白名单语法产出标签（杜绝 XSS）；链接只放行 http(s)。
 * 产出结构对齐 TipTap 扩展的解析规则（任务清单用 data-type=taskList/taskItem）。
 */
import { esc } from "./util";

/** 正文是否已是 HTML（新笔记）——首字符为标签即视为 HTML，无需迁移。 */
export function isHtml(body: string): boolean {
  return /^\s*<[a-z!/]/i.test(body);
}

function inline(src: string): string {
  let s = esc(src);
  s = s.replace(/`([^`]+)`/g, (_, c) => `<code>${c}</code>`);
  s = s.replace(/!\[([^\]]*)\]\(([^)\s]+)\)/g, (m, alt, url) =>
    /^(https?:\/\/|data:image\/|itasset:\/\/)/i.test(url) ? `<img src="${url}" alt="${alt}">` : m,
  );
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (m, txt, url) =>
    /^https?:\/\//i.test(url) ? `<a href="${url}">${txt}</a>` : m,
  );
  s = s.replace(/~~([^~]+)~~/g, "<s>$1</s>");
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  s = s.replace(/&lt;u&gt;/g, "<u>").replace(/&lt;\/u&gt;/g, "</u>");
  return s;
}

const TASK_RE = /^(\s*)[-*]\s+\[([ xX])\]\s+(.*)$/;

export function mdToHtml(src: string): string {
  if (!src.trim()) return "";
  if (isHtml(src)) return src;
  const lines = src.replace(/\r\n?/g, "\n").split("\n");
  let html = "";
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (/^```/.test(line)) {
      i++;
      let code = "";
      while (i < lines.length && !/^```/.test(lines[i])) {
        code += lines[i] + "\n";
        i++;
      }
      i++;
      html += `<pre><code>${esc(code.replace(/\n$/, ""))}</code></pre>`;
      continue;
    }

    let m: RegExpMatchArray | null;
    if ((m = line.match(/^(#{1,3})\s+(.*)$/))) {
      html += `<h${m[1].length}>${inline(m[2])}</h${m[1].length}>`;
      i++;
      continue;
    }

    if (/^\s*>\s?/.test(line)) {
      let q = "";
      while (i < lines.length && /^\s*>\s?/.test(lines[i])) {
        q += lines[i].replace(/^\s*>\s?/, "") + "\n";
        i++;
      }
      html += `<blockquote><p>${inline(q.trim())}</p></blockquote>`;
      continue;
    }

    if (TASK_RE.test(line)) {
      html += '<ul data-type="taskList">';
      while (i < lines.length && (m = lines[i].match(TASK_RE))) {
        const checked = m[2] !== " ";
        html += `<li data-type="taskItem" data-checked="${checked}"><p>${inline(m[3])}</p></li>`;
        i++;
      }
      html += "</ul>";
      continue;
    }

    if (/^\s*[-*]\s+/.test(line)) {
      html += "<ul>";
      while (i < lines.length && /^\s*[-*]\s+/.test(lines[i]) && !TASK_RE.test(lines[i])) {
        html += `<li>${inline(lines[i].replace(/^\s*[-*]\s+/, ""))}</li>`;
        i++;
      }
      html += "</ul>";
      continue;
    }

    if (/^\s*\d+\.\s+/.test(line)) {
      html += "<ol>";
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        html += `<li>${inline(lines[i].replace(/^\s*\d+\.\s+/, ""))}</li>`;
        i++;
      }
      html += "</ol>";
      continue;
    }

    if (/^\s*(-{3,}|\*{3,})\s*$/.test(line)) {
      html += "<hr>";
      i++;
      continue;
    }

    if (line.trim() === "") {
      i++;
      continue;
    }

    html += `<p>${inline(line)}</p>`;
    i++;
  }

  return html;
}
