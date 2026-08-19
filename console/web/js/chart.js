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


/**
 * 多系列折线图，支持「未采集」断线。
 *
 * 与 lineChart 的关键区别：每个点带 `covered`。未采集的点**不画**——
 * 那一段是断开的，而不是贴着 0 的一条线。一条零线会被读成「这段时间没有流量」，
 * 而事实是「服务端那时还没开始记录」，两者在图上必须看起来完全不同。
 * 未采集区间另外铺一层斜纹底，让人一眼看出是数据缺失而非低谷。
 *
 * @param {{t:number,covered:boolean}[]} points 每个点还带各系列的键
 * @param {{key:string,label:string,color:string,fill?:boolean}[]} lines
 */
export function seriesChart(points, lines, labelOf, titleOf, { height = 210, formatValue } = {}) {
  const W = 720;
  const H = height;
  const padL = 48;
  const padR = 12;
  const padT = 12;
  const padB = 26;
  const innerW = W - padL - padR;
  const innerH = H - padT - padB;

  const fmtV = formatValue || ((v) => String(v));
  const all = [];
  for (const p of points) {
    if (!p.covered) continue;
    for (const l of lines) {
      const v = Number(p[l.key]);
      if (Number.isFinite(v)) all.push(v);
    }
  }
  const { top, ticks } = niceScale(Math.max(...all, 0));
  const n = points.length;
  const x = (i) => (n === 1 ? padL + innerW / 2 : padL + (innerW * i) / (n - 1));
  const y = (v) => padT + innerH - (innerH * v) / top;

  const kids = [];

  // 未采集区间的斜纹底（用 pattern 定义一次，复用）
  const defs = svgEl('defs', {},
    svgEl('pattern', {
      id: 'uncovered-hatch', width: 8, height: 8,
      patternUnits: 'userSpaceOnUse', patternTransform: 'rotate(45)',
    },
      svgEl('rect', { width: 8, height: 8, fill: 'var(--bg-hover)' }),
      svgEl('line', { x1: 0, y1: 0, x2: 0, y2: 8, stroke: 'var(--border)', 'stroke-width': 3 }),
    ),
  );
  kids.push(defs);

  // 把连续的未采集段合并成矩形，别一个点画一块
  let runStart = -1;
  const halfStep = n > 1 ? innerW / (n - 1) / 2 : innerW / 2;
  for (let i = 0; i <= n; i += 1) {
    const uncovered = i < n && !points[i].covered;
    if (uncovered && runStart < 0) runStart = i;
    if (!uncovered && runStart >= 0) {
      const x0 = Math.max(padL, x(runStart) - halfStep);
      const x1 = Math.min(W - padR, x(i - 1) + halfStep);
      kids.push(svgEl('rect', {
        x: x0, y: padT, width: Math.max(1, x1 - x0), height: innerH,
        fill: 'url(#uncovered-hatch)', opacity: 0.7,
      }, svgEl('title', {}, '未采集：服务端那时还没开始记录（不是没有流量）')));
      runStart = -1;
    }
  }

  // 网格线与纵轴
  for (const t of ticks) {
    const yy = y(t);
    kids.push(svgEl('line', { class: 'grid-line', x1: padL, y1: yy, x2: W - padR, y2: yy }));
    kids.push(svgEl('text', { class: 'axis-text', x: padL - 6, y: yy + 3, 'text-anchor': 'end' }, formatTick(t)));
  }

  // 每条系列：把 covered 的连续段各画一条 path
  for (const l of lines) {
    let seg = [];
    const flush = () => {
      if (seg.length === 0) return;
      const d = seg.map((s, i) => `${i === 0 ? 'M' : 'L'}${s[0].toFixed(1)},${s[1].toFixed(1)}`).join(' ');
      if (l.fill && seg.length > 1) {
        const area = `${d} L${seg[seg.length - 1][0].toFixed(1)},${y(0).toFixed(1)} L${seg[0][0].toFixed(1)},${y(0).toFixed(1)} Z`;
        kids.push(svgEl('path', { d: area, fill: l.color, opacity: 0.1 }));
      }
      // 单点段画不出线，补一个圆点，否则这个点在图上会凭空消失
      if (seg.length === 1) {
        kids.push(svgEl('circle', { cx: seg[0][0], cy: seg[0][1], r: 2.5, fill: l.color }));
      } else {
        kids.push(svgEl('path', {
          d, fill: 'none', stroke: l.color, 'stroke-width': 2,
          'stroke-linejoin': 'round', 'stroke-linecap': 'round',
          'stroke-dasharray': l.dash || undefined,
        }));
      }
      seg = [];
    };
    points.forEach((p, i) => {
      if (!p.covered) { flush(); return; }
      const v = Number(p[l.key]);
      if (!Number.isFinite(v)) { flush(); return; }
      seg.push([x(i), y(v)]);
    });
    flush();
  }

  // 悬停热区：整列一块，显示该时刻所有系列的值
  points.forEach((p, i) => {
    const w = n === 1 ? innerW : innerW / (n - 1);
    const lines_text = p.covered
      ? lines.map((l) => `${l.label}：${fmtV(Number(p[l.key]) || 0, l.key)}`).join('\n')
      : '未采集（服务端那时还没开始记录）';
    kids.push(svgEl('rect', {
      x: x(i) - w / 2, y: padT, width: w, height: innerH, fill: 'transparent',
    }, svgEl('title', {}, `${titleOf(p.t)}\n${lines_text}`)));
  });

  // 横轴
  for (const i of tickIndexes(n)) {
    kids.push(svgEl('text', { class: 'axis-text', x: x(i), y: H - 8, 'text-anchor': 'middle' }, labelOf(points[i].t)));
  }

  return svgEl('svg', {
    class: 'chart', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none',
    role: 'img', 'aria-label': '流量时间序列',
  }, kids);
}

/** 图例。与 seriesChart 的 lines 同构。 */
export function legend(lines, extra) {
  const wrap = document.createElement('div');
  wrap.className = 'chart-legend';
  for (const l of lines) {
    const item = document.createElement('span');
    item.style.display = 'inline-flex';
    item.style.alignItems = 'center';
    item.style.gap = '5px';
    const dot = document.createElement('span');
    dot.style.width = '10px';
    dot.style.height = '3px';
    dot.style.borderRadius = '2px';
    dot.style.background = l.color;
    item.append(dot, document.createTextNode(l.label));
    wrap.append(item);
  }
  if (extra) {
    const e = document.createElement('span');
    e.style.color = 'var(--text-faint)';
    e.textContent = extra;
    wrap.append(e);
  }
  return wrap;
}

/**
 * 横向占比条（状态码分布、延迟分布）。
 * 段为 0 时不渲染——0 宽度的段只会让图例对不上。
 */
export function stackedBar(segments, total) {
  const wrap = document.createElement('div');
  if (!total) {
    wrap.className = 'empty';
    wrap.textContent = '这段时间没有请求';
    return wrap;
  }
  const bar = document.createElement('div');
  bar.style.display = 'flex';
  bar.style.height = '14px';
  bar.style.borderRadius = '999px';
  bar.style.overflow = 'hidden';
  bar.style.background = 'var(--bg-hover)';

  for (const seg of segments) {
    if (!seg.value) continue;
    const el = document.createElement('div');
    el.style.width = `${(seg.value / total) * 100}%`;
    el.style.background = seg.color;
    el.title = `${seg.label}：${seg.value}（${((seg.value / total) * 100).toFixed(1)}%）`;
    bar.append(el);
  }

  const legendEl = document.createElement('div');
  legendEl.className = 'chart-legend';
  legendEl.style.flexWrap = 'wrap';
  for (const seg of segments) {
    if (!seg.value) continue;
    const item = document.createElement('span');
    item.style.display = 'inline-flex';
    item.style.alignItems = 'center';
    item.style.gap = '5px';
    const dot = document.createElement('span');
    dot.style.width = '8px';
    dot.style.height = '8px';
    dot.style.borderRadius = '50%';
    dot.style.background = seg.color;
    item.append(dot, document.createTextNode(
      `${seg.label} ${seg.value}（${((seg.value / total) * 100).toFixed(1)}%）`,
    ));
    legendEl.append(item);
  }

  wrap.append(bar, legendEl);
  return wrap;
}

/** 纵轴刻度的紧凑写法：1200 → 1.2k */
function formatTick(v) {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(v % 1_000_000 === 0 ? 0 : 1)}M`;
  if (v >= 1000) return `${(v / 1000).toFixed(v % 1000 === 0 ? 0 : 1)}k`;
  return String(Math.round(v));
}
