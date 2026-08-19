// 数据统计：流量 + 库内趋势。
//
// 「流量」与「库内趋势」是两类数据，页面上刻意分开：
// - 流量来自主服务的请求指标（traffic_hourly），只有服务端开了采集才有；
// - 库内趋势（注册、会话、同步、提审）是从业务表实时聚合的，一直都有。
//
// 服务端没开采集时，流量区块整块显示「未采集」——不画任何曲线，包括零线。
// 开了采集但某段时间还没开始记录时，那一段在图上是**断开的**并铺斜纹底，
// 与「那段时间没有请求」（贴地的 0）看起来完全不同。

import { Api } from '../api.js';
import { barList, legend, lineChart, seriesChart, stackedBar } from '../chart.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, empty, h, note, renderAsync, table, unavailable,
} from '../ui.js';

/** 库内趋势的几条曲线。 */
const METRICS = [
  { key: 'users', label: '新增注册', hourly: false },
  { key: 'sessions', label: '会话创建', hourly: true },
  { key: 'records', label: '同步数据更新', hourly: false },
  { key: 'submissions', label: '插件提审', hourly: false },
  { key: 'market', label: '插件上线', hourly: false },
];

/**
 * 区间选项。流量与库内趋势的桶粒度**分开定**：
 * 流量按小时落库，7 天以内看小时更有信息量；30 天以上换成天，否则 720 个点糊成一团。
 */
const RANGES = [
  { id: '24h', label: '最近 24 小时', dbBucket: 'hour', dbPoints: 24, trBucket: 'hour', trPoints: 24, hours: 24, days: 1 },
  { id: '7d', label: '最近 7 天', dbBucket: 'day', dbPoints: 7, trBucket: 'hour', trPoints: 168, hours: 168, days: 7 },
  { id: '30d', label: '最近 30 天', dbBucket: 'day', dbPoints: 30, trBucket: 'day', trPoints: 30, hours: 720, days: 30 },
  { id: '90d', label: '最近 90 天', dbBucket: 'day', dbPoints: 90, trBucket: 'day', trPoints: 90, hours: 2160, days: 90 },
];

const state = { range: '24h' };

export async function render(container, ctx, param, page) {
  page.setSubtitle('流量来自服务端的请求指标；库内趋势来自业务表实时聚合');

  const rangeSel = h('select', {
    onchange: (e) => { state.range = e.target.value; reload(); },
  }, RANGES.map((r) => h('option', { value: r.id, selected: state.range === r.id }, r.label)));

  page.setActions(rangeSel, h('button.btn.btn-ghost', { text: '刷新', onclick: () => reload() }));

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const range = RANGES.find((r) => r.id === state.range) || RANGES[0];
      const hourly = range.trBucket === 'hour';

      const [traffic, routes, downloads, ov] = await Promise.all([
        Api.traffic(range.trPoints, range.trBucket),
        Api.trafficRoutes(range.hours, 20),
        Api.downloads(range.days, 15),
        Api.overview(),
      ]);

      const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

      if (traffic.available) {
        wrap.append(...trafficBlocks(traffic, routes, downloads, hourly, range));
      } else {
        wrap.append(card(
          cardHead('流量', '服务端未采集'),
          note(traffic.reason || '云同步服务端还没有请求指标表。', 'warn'),
        ));
      }

      // ---- 库内趋势 ----
      wrap.append(...(await dbTrendBlocks(range)));

      // ---- 仍然拿不到的 ----
      if (ov.unavailable.length) {
        wrap.append(card(
          cardHead('仍然拿不到的指标', '结构性缺失，不是暂时没数据'),
          h('div.grid.grid-2', ov.unavailable.map((u) => unavailable(u))),
        ));
      }

      return wrap;
    });
  }
}

// ---------- 流量区块 ----------

function trafficBlocks(traffic, routes, downloads, hourly, range) {
  const labelOf = (t) => fmt.tickLabel(t, hourly);
  const titleOf = (t) => fmt.tickTitle(t, hourly);
  const covered = traffic.points.filter((p) => p.covered);
  const totalReqs = covered.reduce((a, p) => a + (p.reqs || 0), 0);
  const totalErrs = covered.reduce((a, p) => a + (p.errs || 0), 0);
  const totalOut = covered.reduce((a, p) => a + (p.bytesOut || 0), 0);
  const totalIn = covered.reduce((a, p) => a + (p.bytesIn || 0), 0);
  const durWeighted = covered.reduce((a, p) => a + (p.avgMs || 0) * (p.reqs || 0), 0);
  const avgMs = totalReqs > 0 ? durWeighted / totalReqs : 0;
  const maxMs = covered.reduce((a, p) => Math.max(a, p.maxMs || 0), 0);
  const uncoveredCount = traffic.points.length - covered.length;

  const blocks = [];

  // 汇总卡
  blocks.push(h('div.grid.grid-4',
    statTile('请求数', fmt.num(totalReqs), `${range.label.replace('最近 ', '')}内`),
    statTile('错误数', fmt.num(totalErrs),
      totalReqs > 0 ? `错误率 ${((totalErrs / totalReqs) * 100).toFixed(2)}%` : '—'),
    statTile('出/入流量', fmt.bytes(totalOut), `入站 ${fmt.bytes(totalIn)}`),
    statTile('平均耗时', `${avgMs.toFixed(1)} ms`, `最慢 ${fmt.num(maxMs)} ms`),
  ));

  // 请求量与错误
  const reqLines = [
    { key: 'reqs', label: '请求数', color: 'var(--accent)', fill: true },
    { key: 'errs', label: '错误数（4xx+5xx）', color: 'var(--warn)' },
    { key: 'serverErrs', label: '服务端错误（5xx）', color: 'var(--danger)' },
  ];
  blocks.push(card(
    cardHead('请求量与错误', hourly ? '按小时' : '按天'),
    seriesChart(traffic.points, reqLines, labelOf, titleOf, { formatValue: (v) => fmt.num(v) }),
    legend(reqLines, uncoveredCount ? `斜纹区间为未采集（${uncoveredCount} 个时段）` : ''),
    h('div', { style: 'margin-top:12px;display:flex;flex-direction:column;gap:8px' },
      note(traffic.note),
      coverageNote(traffic),
    ),
  ));

  // 带宽
  const bwLines = [
    { key: 'bytesOut', label: '出站', color: 'var(--accent)', fill: true },
    { key: 'bytesIn', label: '入站', color: 'var(--ok)' },
  ];
  blocks.push(card(
    cardHead('流量字节', '按 Content-Length 统计'),
    seriesChart(traffic.points, bwLines, labelOf, titleOf, { height: 170, formatValue: (v) => fmt.bytes(v) }),
    legend(bwLines),
    h('div', { style: 'margin-top:12px' },
      note('分块传输（无确定长度）的响应记为 0——宁可少记，也不估一个数出来。')),
  ));

  // 耗时
  const durLines = [
    { key: 'avgMs', label: '平均耗时', color: 'var(--accent)', fill: true },
    { key: 'maxMs', label: '最慢', color: 'var(--warn)' },
  ];
  blocks.push(card(
    cardHead('响应耗时', '毫秒'),
    seriesChart(traffic.points, durLines, labelOf, titleOf, {
      height: 170,
      formatValue: (v) => `${Number(v).toFixed(1)} ms`,
    }),
    legend(durLines),
    h('div', { style: 'margin-top:12px' },
      note('平均值 = 耗时总和 ÷ 请求数，是精确的。这里刻意不给 P95——从分桶插值出的百分位是估算，而这两个数不是。')),
  ));

  // 状态码与延迟分布
  if (routes.available) {
    const mixColors = { 2: 'var(--ok)', 3: 'var(--accent)', 4: 'var(--warn)', 5: 'var(--danger)' };
    const statusSegs = (routes.statusMix || []).map((m) => ({
      label: m.label, value: m.reqs, color: mixColors[m.class] || 'var(--text-faint)',
    }));
    const statusTotal = statusSegs.reduce((a, s) => a + s.value, 0);

    const lat = routes.latency || {};
    const latSegs = [
      { label: lat.labels?.[0] || '< 50ms', value: lat.fast || 0, color: 'var(--ok)' },
      { label: lat.labels?.[1] || '50~200ms', value: lat.mid || 0, color: 'var(--accent)' },
      { label: lat.labels?.[2] || '200ms~1s', value: lat.slow || 0, color: 'var(--warn)' },
      { label: lat.labels?.[3] || '≥ 1s', value: lat.verySlow || 0, color: 'var(--danger)' },
    ];
    const latTotal = latSegs.reduce((a, s) => a + s.value, 0);

    blocks.push(h('div.grid.grid-2',
      card(cardHead('状态码分布'), stackedBar(statusSegs, statusTotal)),
      card(
        cardHead('延迟分布', '精确计数，非估算'),
        stackedBar(latSegs, latTotal),
        h('div', { style: 'margin-top:12px' }, note(routes.latencyNote || '')),
      ),
    ));

    // Top 路由
    blocks.push(card(
      cardHead('按路由', `Top ${routes.items.length}`),
      routes.items.length
        ? table([
            { key: 'route', label: '路由', render: (r) => h('span.mono', { text: r.route }) },
            { key: 'method', label: '方法', render: (r) => h('span.mono', { text: r.method }) },
            { key: 'reqs', label: '请求', num: true, render: (r) => fmt.num(r.reqs) },
            {
              key: 'errs', label: '错误', num: true,
              render: (r) => h('span', {
                title: `其中 5xx：${r.serverErrs}`,
                text: r.errs ? `${fmt.num(r.errs)}（${((r.errs / r.reqs) * 100).toFixed(1)}%）` : '—',
              }),
            },
            { key: 'avgMs', label: '平均', num: true, render: (r) => `${r.avgMs.toFixed(1)} ms` },
            { key: 'maxMs', label: '最慢', num: true, render: (r) => `${fmt.num(r.maxMs)} ms` },
            { key: 'bytesOut', label: '出站', num: true, render: (r) => fmt.bytes(r.bytesOut) },
          ], routes.items)
        : empty('这段时间没有请求'),
      h('div', { style: 'margin-top:12px' }, note(routes.note || '')),
    ));
  }

  // 插件下载
  if (downloads.available) {
    blocks.push(card(
      cardHead('插件下载量', `最近 ${downloads.days} 天`),
      downloads.items.length
        ? barList(
            downloads.items.map((d) => ({ label: d.name, value: d.downloads })),
            (v) => `${fmt.num(v)} 次`,
          )
        : empty('这段时间没有插件被下载'),
      h('div', { style: 'margin-top:12px' }, note(downloads.note || '')),
    ));
  }

  return blocks;
}

/** 采集覆盖范围的如实说明。 */
function coverageNote(traffic) {
  if (!traffic.coverageFrom) {
    return note('服务端已开启采集，但还没有落库任何数据（可能刚启动，第一个窗口尚未刷新）。', 'warn');
  }
  const from = new Date(traffic.coverageFrom * 1000).toLocaleString('zh-CN', { hour12: false });
  const uncovered = traffic.points.length - traffic.coveredPoints;
  if (uncovered <= 0) {
    return note(`采集数据自 ${from} 起可用，本区间全部有数据。${traffic.flushNote || ''}`);
  }
  return note(
    `采集数据自 ${from} 起可用；本区间有 ${uncovered} 个时段在那之前，图上画成断线并铺斜纹——`
    + `那是「服务端还没开始记录」，不是「没有流量」。${traffic.flushNote || ''}`,
    'warn',
  );
}

// ---------- 库内趋势 ----------

async function dbTrendBlocks(range) {
  const hourly = range.dbBucket === 'hour';
  const metrics = hourly ? METRICS.filter((m) => m.hourly) : METRICS;

  const [seriesList, storage, namespaces] = await Promise.all([
    Promise.all(metrics.map((m) =>
      Api.series(m.key, range.dbPoints, range.dbBucket).then((d) => ({ meta: m, data: d })))),
    Api.storage(15),
    Api.namespaces(),
  ]);

  const blocks = [];

  blocks.push(h('div', { style: 'margin-top:6px' },
    h('h3', { style: 'font-size:15px', text: '库内趋势' }),
    h('p', { style: 'font-size:12px;color:var(--text-faint);margin-top:2px',
      text: '从业务表实时聚合，与服务端是否开启指标采集无关。' }),
  ));

  if (hourly) {
    blocks.push(note(
      '按小时只展示会话创建：注册、提审、插件上线都是低频事件，小时粒度下几乎全是 0，画出来没有信息量。',
    ));
  }

  for (const { meta, data } of seriesList) {
    const total = data.points.reduce((a, p) => a + p.v, 0);
    blocks.push(card(
      cardHead(meta.label, `区间合计 ${fmt.num(total)}`),
      lineChart(data.points, (t) => fmt.tickLabel(t, hourly), (t) => fmt.tickTitle(t, hourly), { height: 160 }),
      h('div', { style: 'margin-top:12px' }, note(data.note)),
    ));
  }

  blocks.push(h('div.grid.grid-2',
    card(
      cardHead('存储占用排行', 'Top 15'),
      storage.items.length
        ? barList(
            storage.items.map((r) => ({
              label: r.username, value: r.bytes, hint: `${r.username}：${fmt.num(r.count)} 条`,
            })),
            fmt.bytes,
          )
        : empty('还没有同步数据'),
      h('div', { style: 'margin-top:12px' }, note(storage.note)),
    ),
    card(
      cardHead('命名空间分布'),
      namespaces.items.length
        ? table([
            { key: 'ns', label: '命名空间' },
            { key: 'users', label: '用户数', num: true, render: (r) => fmt.num(r.users) },
            { key: 'count', label: '记录数', num: true, render: (r) => fmt.num(r.count) },
            { key: 'bytes', label: '占用', num: true, render: (r) => fmt.bytes(r.bytes) },
          ], namespaces.items)
        : empty('还没有同步数据'),
      h('div', { style: 'margin-top:12px' }, note(namespaces.note)),
    ),
  ));

  return blocks;
}

function statTile(label, value, delta) {
  return h('div.card',
    h('div.stat',
      h('span.stat-label', { text: label }),
      h('span.stat-value', { text: value }),
      delta ? h('span.stat-delta', { text: delta }) : null,
    ),
  );
}
