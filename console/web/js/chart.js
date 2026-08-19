// 手写 SVG 图表。
//
// 不引任何图表库：CSP 只允许 'self'，NAS 上也不该依赖 CDN；而这里要画的东西
// 就是折线和柱状两种，几十行足够，还能完全跟着主题变量走（换暗色不用改代码）。
//
// tooltip 用原生 `<title>`：零 JS、零定位计算，鼠标悬停浏览器自己弹。

const NS = 'http://www.w3.org/2000/svg';

function svgEl(tag, attrs = {}, ...children) {
  const el = document.createElementNS(NS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null) continue;
    el.setAttribute(k, String(v));
  }
  for (const c of children.flat()) {
    if (c === undefined || c === null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

/**
 * 纵轴刻度：把最大值向上取整到一个「好看」的数，并给出至多 4 条网格线。
 *
 * 刻度数会跟着量级收缩：峰值只有 1 时若硬画 4 条线，格值是 0.25/0.5/0.75，
 * 取整后显示成 0,0,1,1,1——一串重复的数字，比没有刻度还糟。
 * 计数类指标不存在小数，所以每格至少跨 1。
 */
function niceScale(max) {
  if (max <= 0) return { top: 1, ticks: [0, 1] };
  const exp = Math.floor(Math.log10(max));
  const base = 10 ** exp;
  const n = max / base;
  const step = n <= 1 ? 1 : n <= 2 ? 2 : n <= 5 ? 5 : 10;
  const top = step * base;
  // top 小于 4 时按 top 本身定格数，保证每格至少跨 1
  const lines = top < 4 ? Math.max(1, Math.round(top)) : 4;
  const ticks = [];
  for (let i = 0; i <= lines; i += 1) ticks.push((top / lines) * i);
  return { top, ticks };
}

/** 横轴标签的抽稀：点太多时只标若干个，避免糊成一片。 */
function tickIndexes(count, want = 6) {
  if (count <= want) return [...Array(count).keys()];
  const step = Math.ceil(count / want);
  const out = [];
  for (let i = 0; i < count; i += step) out.push(i);
  if (out[out.length - 1] !== count - 1) out.push(count - 1);
  return out;
}

/**
 * 折线图（带面积填充）。
 *
 * @param {{t:number,v:number}[]} points 已补齐的连续序列，`t` 是 Unix 秒
 * @param {(sec:number)=>string} labelOf 横轴短标签
 * @param {(sec:number)=>string} titleOf tooltip 用的完整时间
 */
export function lineChart(points, labelOf, titleOf, { height = 200 } = {}) {
  const W = 720;
  const H = height;
  const padL = 44;
  const padR = 12;
  const padT = 12;
  const padB = 26;
  const innerW = W - padL - padR;
  const innerH = H - padT - padB;

  const values = points.map((p) => p.v);
  const { top, ticks } = niceScale(Math.max(...values, 0));
  const n = points.length;
  const x = (i) => (n === 1 ? padL + innerW / 2 : padL + (innerW * i) / (n - 1));
  const y = (v) => padT + innerH - (innerH * v) / top;

  const kids = [];

  // 网格线 + 纵轴刻度
  for (const t of ticks) {
    const yy = y(t);
    kids.push(svgEl('line', { class: 'grid-line', x1: padL, y1: yy, x2: W - padR, y2: yy }));
    kids.push(svgEl('text', {
      class: 'axis-text', x: padL - 6, y: yy + 3, 'text-anchor': 'end',
    }, formatTick(t)));
  }

  // 面积 + 折线
  const line = points.map((p, i) => `${i === 0 ? 'M' : 'L'}${x(i).toFixed(1)},${y(p.v).toFixed(1)}`).join(' ');
  const area = `${line} L${x(n - 1).toFixed(1)},${y(0).toFixed(1)} L${x(0).toFixed(1)},${y(0).toFixed(1)} Z`;
  kids.push(svgEl('path', { class: 'area', d: area }));
  kids.push(svgEl('path', { class: 'line', d: line }));

  // 数据点：只在点数不多时画圆点，否则太密
  if (n <= 40) {
    points.forEach((p, i) => {
      kids.push(svgEl('circle', { class: 'dot', cx: x(i), cy: y(p.v), r: 2.5 },
        svgEl('title', {}, `${titleOf(p.t)}：${p.v}`)));
    });
  }

  // 覆盖整列的透明热区，让「哪一天都能悬停看到数值」，不必精确对准圆点
  points.forEach((p, i) => {
    const w = n === 1 ? innerW : innerW / (n - 1);
    kids.push(svgEl('rect', {
      x: x(i) - w / 2, y: padT, width: w, height: innerH, fill: 'transparent',
    }, svgEl('title', {}, `${titleOf(p.t)}：${p.v}`)));
  });

  // 横轴标签
  for (const i of tickIndexes(n)) {
    kids.push(svgEl('text', {
      class: 'axis-text', x: x(i), y: H - 8, 'text-anchor': 'middle',
    }, labelOf(points[i].t)));
  }

  return svgEl('svg', {
    class: 'chart', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none',
    role: 'img', 'aria-label': '时间序列折线图',
  }, kids);
}

/**
 * 横向柱状图（用于排行榜：存储占用、命名空间分布）。
 *
 * @param {{label:string,value:number,hint?:string}[]} rows
 * @param {(v:number)=>string} formatValue
 */
export function barList(rows, formatValue) {
  if (!rows.length) return null;
  const max = Math.max(...rows.map((r) => r.value), 1);
  const wrap = document.createElement('div');
  wrap.style.display = 'flex';
  wrap.style.flexDirection = 'column';
  wrap.style.gap = '8px';

  for (const r of rows) {
    const row = document.createElement('div');
    row.style.display = 'grid';
    row.style.gridTemplateColumns = 'minmax(80px, 160px) 1fr auto';
    row.style.gap = '10px';
    row.style.alignItems = 'center';
    row.style.fontSize = '13px';

    const label = document.createElement('span');
    label.textContent = r.label;
    label.style.overflow = 'hidden';
    label.style.textOverflow = 'ellipsis';
    label.style.whiteSpace = 'nowrap';
    if (r.hint) label.title = r.hint;

    const track = document.createElement('div');
    track.style.height = '8px';
    track.style.borderRadius = '999px';
    track.style.background = 'var(--bg-hover)';
    track.style.overflow = 'hidden';

    const fill = document.createElement('div');
    fill.style.height = '100%';
    fill.style.width = `${Math.max(2, (r.value / max) * 100)}%`;
    fill.style.background = 'var(--accent)';
    fill.style.borderRadius = '999px';
    track.append(fill);

    const val = document.createElement('span');
    val.textContent = formatValue(r.value);
    val.style.fontVariantNumeric = 'tabular-nums';
    val.style.color = 'var(--text-sub)';

    row.append(label, track, val);
    wrap.append(row);
  }
  return wrap;
}

/** 纵轴刻度的紧凑写法：1200 → 1.2k */
function formatTick(v) {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(v % 1_000_000 === 0 ? 0 : 1)}M`;
  if (v >= 1000) return `${(v / 1000).toFixed(v % 1000 === 0 ? 0 : 1)}k`;
  return String(Math.round(v));
}
