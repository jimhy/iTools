// 数据统计。
//
// 这一页的定位要说清楚：它**不是流量面板**。云同步服务端没有任何请求量 / 带宽 /
// 耗时的采集，那些指标在页面底部以「未采集」的形式如实列出。
// 上面的曲线全部来自数据库的真实聚合，每条都标注了它到底在数什么。

import { Api } from '../api.js';
import { barList, lineChart } from '../chart.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, empty, h, note, renderAsync, table, unavailable,
} from '../ui.js';

const METRICS = [
  { key: 'users', label: '新增注册', hourly: false },
  { key: 'sessions', label: '会话创建', hourly: true },
  { key: 'records', label: '同步数据更新', hourly: false },
  { key: 'submissions', label: '插件提审', hourly: false },
  { key: 'market', label: '插件上线', hourly: false },
];

const state = { points: 30, bucket: 'day' };

export async function render(container, ctx, param, page) {
  page.setSubtitle('全部来自数据库聚合；服务端未采集的指标在页面底部如实列出');

  const rangeSel = h('select', {
    onchange: (e) => {
      const [points, bucket] = e.target.value.split(':');
      state.points = Number(points);
      state.bucket = bucket;
      reload();
    },
  },
    h('option', { value: '24:hour', selected: state.bucket === 'hour' && state.points === 24 }, '最近 24 小时'),
    h('option', { value: '7:day', selected: state.bucket === 'day' && state.points === 7 }, '最近 7 天'),
    h('option', { value: '30:day', selected: state.bucket === 'day' && state.points === 30 }, '最近 30 天'),
    h('option', { value: '90:day', selected: state.bucket === 'day' && state.points === 90 }, '最近 90 天'),
  );

  page.setActions(rangeSel, h('button.btn.btn-ghost', { text: '刷新', onclick: () => reload() }));

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const hourly = state.bucket === 'hour';
      // 按小时只有会话这条曲线有意义（其余都是低频事件，小时桶几乎全是 0）
      const metrics = hourly ? METRICS.filter((m) => m.hourly) : METRICS;

      const [seriesList, storage, namespaces, ov] = await Promise.all([
        Promise.all(metrics.map((m) =>
          Api.series(m.key, state.points, state.bucket).then((d) => ({ meta: m, data: d })))),
        Api.storage(15),
        Api.namespaces(),
        Api.overview(),
      ]);

      const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

      if (hourly) {
        wrap.append(note(
          '按小时只展示会话创建：注册、提审、插件上线都是低频事件，小时粒度下几乎全是 0，画出来没有信息量。',
        ));
      }

      // ---- 各条曲线 ----
      for (const { meta, data } of seriesList) {
        const total = data.points.reduce((a, p) => a + p.v, 0);
        wrap.append(card(
          cardHead(meta.label, `区间合计 ${fmt.num(total)}`),
          lineChart(
            data.points,
            (t) => fmt.tickLabel(t, hourly),
            (t) => fmt.tickTitle(t, hourly),
            { height: 170 },
          ),
          h('div', { style: 'margin-top:12px' }, note(data.note)),
        ));
      }

      // ---- 存储排行与命名空间 ----
      wrap.append(h('div.grid.grid-2',
        card(
          cardHead('存储占用排行', 'Top 15'),
          storage.items.length
            ? barList(
                storage.items.map((r) => ({
                  label: r.username,
                  value: r.bytes,
                  hint: `${r.username}：${fmt.num(r.count)} 条`,
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

      // ---- 未采集 ----
      wrap.append(card(
        cardHead('未采集的指标', '需要云同步服务端支持'),
        note(
          '这些是运营后台通常会有、但当前确实拿不到的数据。它们不是「这段时间没有流量」，'
          + '而是服务端从来没有记录过——所以这里不画任何曲线。',
          'warn',
        ),
        h('div.grid.grid-2', { style: 'margin-top:12px' },
          ov.unavailable.map((u) => unavailable(u)),
        ),
      ));

      return wrap;
    });
  }
}
