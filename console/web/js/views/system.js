// 系统状态。
//
// 每个绿灯都来自一次真实探测。探不到就显示「未知 / 未配置」，
// 绝不默认成健康——假绿灯比没有这一页更糟。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import { card, cardHead, h, note, renderAsync, tag } from '../ui.js';

export async function render(container, ctx, param, page) {
  page.setSubtitle('控制台自身、数据库与云同步服务端的实时状态');
  page.setActions(h('button.btn.btn-ghost', { text: '重新探测', onclick: () => render(container, ctx, param, page) }));

  await renderAsync(container, async () => {
    const s = await Api.system();
    const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

    // ---- 三个状态卡 ----
    wrap.append(h('div.grid.grid-4',
      statusCard('控制台', true, `v${s.console.version} · 端口 ${s.console.port}`,
        s.console.tls ? 'HTTPS（本进程终结 TLS）' : '⚠ 明文 HTTP'),
      statusCard('数据库', s.database.ok, s.database.ok ? `响应 ${s.database.latencyMs} ms` : '连接失败',
        s.database.target),
      upstreamCard(s.upstream),
      statusCard('登录限流', s.limits.loginLimiterEnabled,
        s.limits.loginLimiterEnabled
          ? `${s.console.loginRateMax} 次 / ${s.console.loginRateWindowSec} 秒`
          : '未启用',
        `当前跟踪 ${s.limits.loginBuckets} 个来源`),
    ));

    // ---- 主服务表检查 ----
    if (s.database.missingUpstreamTables?.length) {
      wrap.append(note(
        `目标库里缺少云同步服务端的表：${s.database.missingUpstreamTables.join('、')}。`
        + '相关页面会是空的——请确认控制台连的是主服务在用的那个库。',
        'danger',
      ));
    }

    // ---- 服务端能力 ----
    wrap.append(card(
      cardHead('服务端能力', '决定哪些功能可用'),
      h('dl.detail-grid',
        capRow('终端用户禁用位', s.database.caps.usersStatus,
          'users 表是否有 status 列。没有它，「禁用账号」无法真实生效'),
        capRow('插件下架归属', s.database.caps.marketRevokedBy,
          'market_entries 是否有 revoked_by 列，用于区分作者自助下架与维护者处置'),
        capRow('HTTP 指标采集', false,
          '主服务是否记录请求量、带宽、耗时。当前没有，故流量面板显示「未采集」'),
        capRow('插件下架接口', false,
          '主服务是否提供可调用的下架端点。当前没有，控制台对插件只读'),
      ),
      h('div', { style: 'margin-top:12px' },
        note(
          '打叉的项不是故障，是云同步服务端还没有对应能力。控制台不会用「改数据库」的方式'
          + '绕过它们——那样按钮会点得动但不生效。',
        ),
      ),
    ));

    // ---- 配置 ----
    wrap.append(card(
      cardHead('控制台配置', '敏感项不回显'),
      h('dl.detail-grid',
        kv('版本', s.console.version),
        kv('监听端口', String(s.console.port)),
        kv('TLS', s.console.tls ? '已启用' : '未启用'),
        kv('会话有效期', `${Math.round(s.console.sessionTtlSec / 3600)} 小时`),
        kv('统计时区偏移', `${s.console.tzOffsetMin >= 0 ? '+' : ''}${s.console.tzOffsetMin / 60} 小时`),
        kv('信任反向代理', s.console.trustProxy ? '是（认 X-Forwarded-For）' : '否'),
        kv('前端托管', s.console.webDir ? '已配置' : '未配置'),
        kv('服务端时间', fmt.time(s.console.serverTime)),
      ),
      !s.console.tls
        ? h('div', { style: 'margin-top:12px' },
            note('未启用 TLS：登录口令会明文过网。除非控制台只监听在受信内网上，否则请配置证书。', 'danger'))
        : null,
      !s.console.trustProxy
        ? h('div', { style: 'margin-top:12px' },
            note('未信任反向代理：如果控制台位于 nginx / frp 之后，登录限流会把所有来访算成同一个 IP。', 'warn'))
        : null,
    ));

    return wrap;
  });
}

function statusCard(title, ok, line1, line2) {
  return h('div.card',
    h('div', { style: 'display:flex;align-items:center;justify-content:space-between;gap:8px;margin-bottom:6px' },
      h('span.stat-label', { text: title }),
      ok ? tag('正常', 'tag-ok') : tag('异常', 'tag-danger'),
    ),
    h('div', { style: 'font-size:14px;font-weight:600', text: line1 }),
    line2 ? h('div.stat-delta', { style: 'word-break:break-all', text: line2 }) : null,
  );
}

function upstreamCard(u) {
  if (!u.configured) {
    return h('div.card',
      h('div', { style: 'display:flex;align-items:center;justify-content:space-between;gap:8px;margin-bottom:6px' },
        h('span.stat-label', { text: '云同步服务端' }),
        tag('未探测', ''),
      ),
      h('div', { style: 'font-size:14px;font-weight:600', text: '未配置探测地址' }),
      h('div.stat-delta', { text: '这不代表主服务有问题，只是控制台没被告知去哪儿探' }),
    );
  }
  return h('div.card',
    h('div', { style: 'display:flex;align-items:center;justify-content:space-between;gap:8px;margin-bottom:6px' },
      h('span.stat-label', { text: '云同步服务端' }),
      u.ok ? tag('正常', 'tag-ok') : tag('异常', 'tag-danger'),
    ),
    h('div', { style: 'font-size:14px;font-weight:600', text: u.ok ? `响应 ${u.latencyMs} ms` : (u.error || '探测失败') }),
    h('div.stat-delta', {
      text: u.ok
        ? `${u.service || '—'}${u.allowRegister === undefined ? '' : ` · 开放注册：${u.allowRegister ? '是' : '否'}`}`
        : `HTTP ${u.status ?? '—'}`,
    }),
  );
}

function capRow(label, ok, desc) {
  return h('div.kv',
    h('dt', { text: label }),
    h('dd', { style: 'display:flex;align-items:center;gap:8px' },
      ok ? tag('可用', 'tag-ok') : tag('不可用', ''),
    ),
    h('div', { style: 'font-size:12px;color:var(--text-faint);margin-top:2px', text: desc }),
  );
}

function kv(label, value) {
  return h('div.kv', h('dt', { text: label }), h('dd', { text: value }));
}
