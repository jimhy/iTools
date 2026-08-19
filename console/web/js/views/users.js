// 用户管理：列表 / 详情 / 强制下线 / 删除。
//
// 「禁用账号」在这里是一个**禁用状态的开关**，旁边写明为什么点不了。
// 这不是偷懒：主服务的登录与鉴权根本不读任何禁用位，做一个能点的开关
// 只会让运营以为封停了，实际对方下一秒就能登回来。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, confirmDialog, empty, h, note, pager, renderAsync,
  table, tag, toastErr, toastOk,
} from '../ui.js';

const state = {
  q: '',
  page: 1,
  size: 30,
  sort: 'created_at',
  order: 'desc',
};

export async function render(container, ctx, param, page) {
  if (param) return renderDetail(container, ctx, param, page);
  return renderList(container, ctx, page);
}

// ---------- 列表 ----------

async function renderList(container, ctx, page) {
  page.setTitle('用户');
  page.setSubtitle('iTools 云账号');

  const search = h('input', {
    type: 'search', placeholder: '搜索用户名…', value: state.q,
    onkeydown: (e) => { if (e.key === 'Enter') { state.q = e.target.value.trim(); state.page = 1; reload(); } },
  });

  page.setActions(
    search,
    h('button.btn', {
      text: '搜索',
      onclick: () => { state.q = search.value.trim(); state.page = 1; reload(); },
    }),
    h('button.btn.btn-ghost', {
      text: '刷新',
      onclick: () => reload(),
    }),
  );

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const data = await Api.users({
        q: state.q, page: state.page, size: state.size,
        sort: state.sort, order: state.order,
      });

      const canWrite = !!ctx.me?.capabilities?.canWrite;
      const canDisable = !!ctx.me?.capabilities?.usersStatusColumn;

      const cols = [
        {
          key: 'username', label: '用户名', sortable: true,
          render: (r) => h('a', { href: `#/users/${encodeURIComponent(r.username)}`, text: r.username }),
        },
        { key: 'created_at', label: '注册时间', sortable: true, render: (r) => h('span', { title: fmt.time(r.createdAt), text: fmt.date(r.createdAt) }) },
        { key: 'sessions', label: '会话', num: true, sortable: true, render: (r) => fmt.num(r.sessionCount) },
        { key: 'records', label: '记录数', num: true, sortable: true, render: (r) => fmt.num(r.recordCount) },
        { key: 'bytes', label: '占用', num: true, sortable: true, render: (r) => fmt.bytes(r.bytes) },
        {
          key: 'plugin', label: '插件', num: true,
          render: (r) => h('span', {
            title: r.pluginCount ? '该用户在市场上线的插件数（含已下架）' : '该用户没有上线过插件',
            text: r.pluginCount ? fmt.num(r.pluginCount) : '—',
          }),
        },
        {
          key: 'act', label: '操作', cellClass: 'actions',
          render: (r) => h('span',
            h('button.btn.btn-sm', {
              text: '下线',
              disabled: !canWrite || r.sessionCount === 0,
              title: !canWrite ? '只读账号不能执行' : (r.sessionCount === 0 ? '该用户当前没有会话' : '删除其全部登录态，立即生效'),
              onclick: () => kick(r.username, reload),
            }),
            h('button.btn.btn-sm', {
              text: '禁用',
              disabled: true,
              title: canDisable
                ? '服务端已支持禁用位，但控制台此版本尚未接入'
                : '云同步服务端的 users 表没有禁用位，登录与鉴权也不检查它——做成可点的开关会是假功能',
            }),
            h('button.btn.btn-sm.btn-danger-soft', {
              text: '删除',
              disabled: !canWrite,
              title: canWrite ? '删除账号、会话与全部同步数据' : '只读账号不能执行',
              onclick: () => removeUser(r.username, reload),
            }),
          ),
        },
      ];

      const onSort = (key) => {
        const sortable = { username: 'username', created_at: 'created_at', sessions: 'sessions', records: 'records', bytes: 'bytes' };
        if (!sortable[key]) return;
        if (state.sort === key) state.order = state.order === 'desc' ? 'asc' : 'desc';
        else { state.sort = key; state.order = 'desc'; }
        state.page = 1;
        reload();
      };

      return h('div', { style: 'display:flex;flex-direction:column;gap:14px' },
        note(
          '「禁用账号」需要云同步服务端支持：users 表目前没有禁用位，主服务的登录与鉴权也不会读它。'
          + '现在能做的是「强制下线」——删掉全部会话，立即生效，但不阻止对方重新登录。',
          'warn',
        ),
        table(cols, data.items, {
          sort: state.sort, order: state.order, onSort,
          emptyText: state.q ? `没有匹配「${state.q}」的用户` : '还没有任何用户',
        }),
        pager({
          page: data.page, size: data.size, total: data.total,
          onPage: (p) => { state.page = p; reload(); },
        }),
      );
    });
  }
}

async function kick(username, reload) {
  const ok = await confirmDialog({
    title: '强制下线',
    text: [
      `将删除 ${username} 在云同步服务端的全部会话，其所有设备上的登录态立即失效。`,
      '注意：这不阻止对方用原口令重新登录。',
    ],
    okText: '确认下线',
  });
  if (!ok) return;
  try {
    const r = await Api.kickUser(username);
    toastOk(`已下线 ${username}，删除 ${r.removed} 条会话`);
    reload();
  } catch (e) {
    toastErr(`下线失败：${e.message}`);
  }
}

async function removeUser(username, reload) {
  const ok = await confirmDialog({
    title: '删除用户',
    text: [
      `将永久删除账号 ${username}、其全部会话与全部云端同步数据。此操作不可撤销。`,
      '该用户已上线的插件会保持原状（与主服务销号行为一致），如需处置请单独下架。',
    ],
    echo: username,
    okText: '永久删除',
  });
  if (!ok) return;
  try {
    await Api.deleteUser(username);
    toastOk(`已删除 ${username}`);
    reload();
  } catch (e) {
    toastErr(`删除失败：${e.message}`);
  }
}

// ---------- 详情 ----------

async function renderDetail(container, ctx, username, page) {
  page.setTitle(username);
  page.setSubtitle('用户详情');
  page.setActions(
    h('a.btn.btn-ghost', { href: '#/users', text: '← 返回列表' }),
  );

  await renderAsync(container, async () => {
    const u = await Api.user(username);
    const canWrite = !!ctx.me?.capabilities?.canWrite;

    const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

    wrap.append(card(
      cardHead('基本信息'),
      h('dl.detail-grid',
        kv('用户名', u.username),
        kv('注册时间', fmt.time(u.createdAt)),
        kv('现存会话', fmt.num(u.sessionCount)),
        kv('最近会话创建', u.lastSessionAt ? `${fmt.time(u.lastSessionAt)}（${fmt.ago(u.lastSessionAt)}）` : '—'),
        kv('同步记录', fmt.num(u.recordCount)),
        kv('占用空间', fmt.bytes(u.bytes)),
      ),
      h('div', { style: 'margin-top:14px;display:flex;gap:8px' },
        h('button.btn', {
          text: '强制下线',
          disabled: !canWrite || u.sessionCount === 0,
          onclick: () => kick(u.username, () => location.reload()),
        }),
        h('button.btn.btn-danger', {
          text: '删除账号',
          disabled: !canWrite,
          onclick: async () => {
            await removeUser(u.username, () => {});
            location.hash = '#/users';
          },
        }),
      ),
      h('div', { style: 'margin-top:12px' }, note(u.notes.sessions)),
    ));

    wrap.append(card(
      cardHead('同步数据用量', '按命名空间'),
      note(u.notes.storage),
      h('div', { style: 'margin-top:12px' },
        u.namespaces.length
          ? table([
              { key: 'ns', label: '命名空间' },
              { key: 'count', label: '记录数', num: true, render: (r) => fmt.num(r.count) },
              { key: 'bytes', label: '占用', num: true, render: (r) => fmt.bytes(r.bytes) },
              { key: 'last', label: '最后更新', render: (r) => h('span', { title: fmt.time(r.lastUpdatedAt), text: fmt.ago(r.lastUpdatedAt) }) },
            ], u.namespaces, { emptyText: '该用户还没有同步任何数据' })
          : empty('该用户还没有同步任何数据'),
      ),
    ));

    wrap.append(h('div.grid.grid-2',
      card(
        cardHead('会话', `最多显示最近 100 条`),
        u.sessions.length
          ? table([
              { key: 'createdAt', label: '创建时间', render: (r) => fmt.time(r.createdAt) },
              { key: 'ago', label: '距今', render: (r) => fmt.ago(r.createdAt) },
            ], u.sessions)
          : empty('当前没有会话'),
      ),
      card(
        cardHead('名下插件', '含已下架'),
        u.plugins.length
          ? table([
              { key: 'name', label: '插件', render: (r) => h('a', { href: `#/plugins/${encodeURIComponent(r.name)}`, text: r.name }) },
              { key: 'version', label: '版本', render: (r) => h('span.mono', { text: r.version }) },
              { key: 'state', label: '状态', render: (r) => (r.revoked ? tag('已下架', 'tag-danger') : tag('在架', 'tag-ok')) },
              { key: 'publishedAt', label: '上线', render: (r) => fmt.date(r.publishedAt) },
            ], u.plugins)
          : empty('该用户没有上线过插件'),
      ),
    ));

    return wrap;
  });
}

function kv(label, value) {
  return h('div.kv', h('dt', { text: label }), h('dd', { text: value }));
}
