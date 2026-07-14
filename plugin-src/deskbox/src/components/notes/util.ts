/** 笔记界面的纯文本工具：HTML 转义、搜索高亮、时间格式化、摘要 / 字数 / 标签提取、稳定配色。 */

/** HTML 转义（预览 / 高亮前统一转义，杜绝注入）。 */
export function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c] as string);
}

/** 先转义再包 <mark>，返回可安全 dangerouslySetInnerHTML 的 html。 */
export function highlight(text: string, q: string): string {
  const t = esc(text);
  if (!q) return t;
  const re = new RegExp("(" + q.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "gi");
  return t.replace(re, "<mark>$1</mark>");
}

/** 时间戳 → yyyy-MM-dd HH:mm；空值返回空串。 */
export function fmt(ts: number): string {
  if (!ts) return "";
  const d = new Date(ts);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 时间戳 → MM-DD HH:mm（卡片 / 元信息用的紧凑格式）；空值返回空串。 */
export function fmtShort(ts: number): string {
  if (!ts) return "";
  const d = new Date(ts);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 提取错误信息文本。 */
export function errMsg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 剥掉 HTML 标签取纯文本（用正则而非 innerHTML，杜绝任何副作用/脚本）；解码常见实体。 */
export function htmlToText(html: string): string {
  return html
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/\s+/g, " ")
    .trim();
}

/** 正文（HTML 或旧版 Markdown）→ 可见纯文本。HTML 剥标签；Markdown 剥常见记号。 */
export function plainText(body: string): string {
  if (/<[a-z!/][^>]*>/i.test(body)) return htmlToText(body);
  return body
    .replace(/```[\s\S]*?```/g, " ") // 代码块整体去掉
    .replace(/^\s{0,3}#{1,6}\s+/gm, "") // 标题记号
    .replace(/^\s{0,3}>\s?/gm, "") // 引用记号
    .replace(/^\s{0,3}(?:[-*+]|\d+\.)\s+(?:\[[ xX]\]\s+)?/gm, "") // 列表 / 任务记号
    .replace(/[*_~`#>]/g, "") // 行内强调记号
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1") // 链接 / 图片保留可见文字
    .replace(/\s+/g, " ")
    .trim();
}

/** 正文 → 侧栏卡片预览摘要：取可见纯文本，截断到约 60 字。 */
export function makeExcerpt(body: string): string {
  const plain = plainText(body);
  return plain.length > 60 ? plain.slice(0, 60) + "…" : plain;
}

/** 正文字数：按可见纯文本的「非空白字符」计（中英文都合理）。 */
export function wordCount(body: string): number {
  return plainText(body).replace(/\s+/g, "").length;
}

/**
 * 从正文提取行内 `#标签`：`#` 后接 1+ 个字母/数字/下划线/汉字，且不是行首标题（行首 `# ` 视为标题）。
 * 去重、保序，最多返回 8 个。纯展示（反映正文真实内容），不做假控件。
 */
export function extractTags(body: string): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  const text = plainText(body);
  const re = /(^|[^#\w一-龥])#([\w一-龥]{1,24})/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text))) {
    // 排除 Markdown 标题：`#` 位于行首且其后紧跟空格的情况不会命中此正则（要求 # 后是词字符），
    // 但形如「# 标题」中的「#标题」仍可能来自正文，交由「# 后无空格」的规则天然区分。
    const tag = m[2];
    if (!seen.has(tag)) {
      seen.add(tag);
      out.push(tag);
      if (out.length >= 8) break;
    }
  }
  return out;
}

/** 卡片彩色圆点 / 文件夹图标的稳定配色：由 id 哈希落到固定调色板（确定性、装饰性）。 */
const DOT_PALETTE = ["#38E1C9", "#5B8CFF", "#F59E0B", "#EF4444", "#A78BFA"];
export function colorFromId(id: string): string {
  let h = 0;
  for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) >>> 0;
  return DOT_PALETTE[h % DOT_PALETTE.length];
}
