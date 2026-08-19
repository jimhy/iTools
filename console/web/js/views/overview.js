// 概览页。
//
// 每个卡片下面都写清「这个数字是什么」。服务端返回的 note 字段原样显示，
// 不做二次概括——概括的过程最容易把语义弄丢。

import { Api } from '../api.js';
import { lineChart } from '../chart.js';
import * as fmt from '../fmt.js';
import { card, cardHead, clear, h, note, stat, tag, unavailable } from '../ui.js';

export async function render(container, ctx, param, page) {
  page.setSubtitle('全部数字来自数据库实时聚合');

  const [ov, series] = await Promise.all([
    Api.overview(),
    Api.series('users', 30),
  ]);

  const wrap = clear(container);

  // ---- 汇总卡 ----
  wrap.append(h('div.grid.grid-4',
    stat('注册用户', fmt.num(ov.users.total), `近 7 天 +${ov.users.new7d} · 近 30 天 +${ov.users.new30d}`),
    stat('现存会话', fmt.num(ov.sessions.total), `近 24 小时新增 ${ov.sessions.new1d}`),
    stat('市场插件', fmt.num(ov.market.online), `已下架 ${ov.market.revoked} · 累计 ${ov.market.total}`),
    stat('同步数据', fmt.bytes(ov.data.bytes), `${fmt.num(ov.data.records)} 条记录`),
  ));

  // ---- 用户增长 ----
  wrap.append(card(
    cardHead('用户增长', '近 30 天'),
    lineChart(
      series.points,
      (t) => fmt.tickLabel(t, false),
      (t) => fmt.tickTitle(t, false),
    ),
    h('div', { style: 'margin-top:12px' }, note(series.note)),
  ));

  // ---- 提审与会话 ----
  wrap.append(h('div.grid.grid-2',
    card(
      cardHead('提审单', `累计 ${fmt.num(ov.submissions.total)} 次`),
      ov.submissions.byStatus.length
        ? h('div', { style: 'display:flex;flex-wrap:wrap;gap:8px' },
            ov.submissions.byStatus.map((s) => {
              const st = fmt.submissionStatus(s.status);
              return h('div', { style: 'display:flex;align-items:center;gap:6px' },
                tag(st.text, st.cls),
                h('strong', { text: fmt.num(s.count) }),
              );
            }))
        : h('div.empty', { text: '还没有任何提审记录' }),
      h('div', { style: 'margin-top:12px' },
        note('一次提审一行、永不覆盖，是本库里唯一真正意义上的事件流。')),
    ),
    card(
      cardHead('会话与存储'),
      h('dl.detail-grid',
        kv('现存会话', fmt.num(ov.sessions.total)),
        kv('近 24 小时新会话', fmt.num(ov.sessions.new1d)),
        kv('近 24 小时数据更新', fmt.num(ov.data.updated1d)),
        kv('数据库占用', ov.storage.dbBytes < 0 ? '读不到' : fmt.bytes(ov.storage.dbBytes)),
        kv('控制台在线会话', ov.console.activeSessions < 0 ? '—' : fmt.num(ov.console.activeSessions)),
      ),
      h('div', { style: 'margin-top:12px;display:flex;flex-direction:column;gap:8px' },
        note(ov.sessions.note),
        note(ov.storage.note),
      ),
    ),
  ));

  // ---- 未采集的指标 ----
  wrap.append(card(
    cardHead('流量指标', '云同步服务端尚未采集，控制台无法凭空生成'),
    note(
      '下面这些是「后台管理系统」通常会有、但当前确实拿不到的指标。它们不是暂时没数据，'
      + '而是服务端从来没有记录过——所以这里不画任何曲线，包括零线。',
      'warn',
    ),
    h('div.grid.grid-2', { style: 'margin-top:12px' },
      ov.unavailable.map((u) => unavailable(u)),
    ),
  ));
}

function kv(label, value) {
  return h('div.kv', h('dt', { text: label }), h('dd', { text: value }));
}
