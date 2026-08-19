// 审计日志。控制台的每一个写操作都在这里，成功与失败都记。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import { clear, h, note, pager, renderAsync, table, tag } from '../ui.js';

const state = { actor: '', action: '', page: 1, size: 50 };

const ACTIONS = [
  ['', '全部动作'],
  ['login', '登录'],
  ['login_failed', '登录失败'],
  ['logout', '登出'],
  ['change_password', '修改口令'],
  ['user_kick', '强制用户下线'],
  ['user_delete', '删除用户'],
  ['admin_create', '创建控制台账号'],
  ['admin_delete', '删除控制台账号'],
  ['admin_set_role', '修改角色'],
  ['admin_set_status', '修改账号状态'],
  ['admin_reset_password', '重置口令'],
];

export async function render(container, ctx, param, page) {
  page.setSubtitle('控制台操作留痕');

  const actorInput = h('input', {
    type: 'search', placeholder: '按操作者筛选…', value: state.actor,
    onkeydown: (e) => { if (e.key === 'Enter') { state.actor = e.target.value.trim(); state.page = 1; reload(); } },
  });
  const actionSel = h('select', {
    onchange: (e) => { state.action = e.target.value; state.page = 1; reload(); },
  }, ACTIONS.map(([v, t]) => h('option', { value: v, selected: state.action === v }, t)));

  page.setActions(
    actorInput,
    h('button.btn', { text: '筛选', onclick: () => { state.actor = actorInput.value.trim(); state.page = 1; reload(); } }),
    actionSel,
    h('button.btn.btn-ghost', { text: '刷新', onclick: () => reload() }),
  );

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const data = await Api.audit({
        actor: state.actor, action: state.action, page: state.page, size: state.size,
      });

      const cols = [
        { key: 'at', label: '时间', render: (r) => h('span', { title: fmt.ago(r.at), text: fmt.time(r.at) }) },
        { key: 'actor', label: '操作者' },
        { key: 'action', label: '动作', render: (r) => fmt.auditAction(r.action) },
        { key: 'target', label: '对象', render: (r) => r.target || '—' },
        { key: 'detail', label: '详情', cellClass: 'trunc', render: (r) => h('span', { title: r.detail, text: r.detail || '—' }) },
        { key: 'ip', label: '来源 IP', render: (r) => h('span.mono', { text: r.ip || '—' }) },
        { key: 'ok', label: '结果', render: (r) => (r.ok ? tag('成功', 'tag-ok') : tag('失败', 'tag-danger')) },
      ];

      return h('div', { style: 'display:flex;flex-direction:column;gap:14px' },
        note(data.note),
        // IP 列在 frp 透传下不是真实来访地址，这句话必须显示出来
        data.ipNote ? note(data.ipNote, 'warn') : null,
        table(cols, data.items, { emptyText: '没有匹配的审计记录' }),
        pager({ page: data.page, size: data.size, total: data.total, onPage: (p) => { state.page = p; reload(); } }),
      );
    });
  }
}
