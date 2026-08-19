// 插件市场：列表 / 详情。**只读**。
//
// 下架按钮是禁用状态：直接改数据库确实能改到 market_entries.revoked，
// 但主服务的市场索引是进程内缓存、插件包直接读盘返回——不重启主服务，
// 客户端那边一个字节都不会变。放一个「改了库但没生效」的按钮比没有按钮更糟。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, h, note, pager, renderAsync, table, tag,
} from '../ui.js';

const state = { q: '', page: 1, size: 30, includeRevoked: true };

export async function render(container, ctx, param, page) {
  if (param) return renderDetail(container, ctx, param, page);
  return renderList(container, ctx, page);
}

async function renderList(container, ctx, page) {
  page.setTitle('插件市场');
  page.setSubtitle('已上线的插件条目');

  const search = h('input', {
    type: 'search', placeholder: '搜索插件名或作者…', value: state.q,
    onkeydown: (e) => { if (e.key === 'Enter') { state.q = e.target.value.trim(); state.page = 1; reload(); } },
  });
  const revokedToggle = h('label', { style: 'display:flex;align-items:center;gap:6px;font-size:13px;color:var(--text-sub)' },
    h('input', {
      type: 'checkbox', checked: state.includeRevoked, style: 'width:auto',
      onchange: (e) => { state.includeRevoked = e.target.checked; state.page = 1; reload(); },
    }),
    '含已下架',
  );

  page.setActions(
    search,
    h('button.btn', { text: '搜索', onclick: () => { state.q = search.value.trim(); state.page = 1; reload(); } }),
    revokedToggle,
  );

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const data = await Api.plugins({
        q: state.q, page: state.page, size: state.size,
        include_revoked: state.includeRevoked,
      });

      const cols = [
        {
          key: 'name', label: '插件',
          render: (r) => h('div',
            h('a', { href: `#/plugins/${encodeURIComponent(r.name)}`, text: r.title || r.name }),
            r.title ? h('div.mono', { style: 'color:var(--text-faint)', text: r.name }) : null,
          ),
        },
        { key: 'owner', label: '作者', render: (r) => h('a', { href: `#/users/${encodeURIComponent(r.owner)}`, text: r.owner }) },
        { key: 'version', label: '版本', render: (r) => h('span.mono', { text: r.version }) },
        { key: 'state', label: '状态', render: (r) => revokedTag(r, data.capabilities.revokedBy) },
        { key: 'publishedAt', label: '上线', render: (r) => h('span', { title: fmt.time(r.publishedAt), text: fmt.date(r.publishedAt) }) },
        { key: 'updatedAt', label: '更新', render: (r) => h('span', { title: fmt.time(r.updatedAt), text: fmt.ago(r.updatedAt) }) },
        {
          key: 'act', label: '操作', cellClass: 'actions',
          render: () => h('button.btn.btn-sm', {
            text: '下架',
            disabled: true,
            title: '需要云同步服务端提供下架接口：直接改库不会刷新主服务的市场索引缓存，客户端看不到变化',
          }),
        },
      ];

      return h('div', { style: 'display:flex;flex-direction:column;gap:14px' },
        note(
          '本页只读。下架 / 恢复需要调用云同步服务端的接口——控制台直接改数据库无法刷新主服务的'
          + '市场索引缓存（它只在发布与下架时重建），客户端拿到的索引和插件包都不会变。',
          'warn',
        ),
        table(cols, data.items, {
          emptyText: state.q ? `没有匹配「${state.q}」的插件` : '市场里还没有插件',
        }),
        pager({ page: data.page, size: data.size, total: data.total, onPage: (p) => { state.page = p; reload(); } }),
      );
    });
  }
}

function revokedTag(r, hasRevokedBy) {
  if (!r.revoked) return tag('在架', 'tag-ok');
  // 主服务旧版本没有 revoked_by 列时，空串不代表「作者下架」——不能瞎猜
  if (!hasRevokedBy || !r.revokedBy) return tag('已下架', 'tag-danger');
  return tag(r.revokedBy === 'admin' ? '维护者下架' : '作者下架', 'tag-danger');
}

async function renderDetail(container, ctx, name, page) {
  page.setTitle(name);
  page.setSubtitle('插件详情');
  page.setActions(h('a.btn.btn-ghost', { href: '#/plugins', text: '← 返回列表' }));

  await renderAsync(container, async () => {
    const p = await Api.plugin(name);
    const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

    wrap.append(card(
      cardHead(p.title || p.name, p.title ? p.name : ''),
      p.description ? h('p', { style: 'color:var(--text-sub);margin-bottom:14px', text: p.description }) : null,
      h('dl.detail-grid',
        kv('作者', p.owner),
        kv('当前版本', p.version),
        kv('状态', p.revoked ? '已下架' : '在架'),
        kv('上线时间', fmt.time(p.publishedAt)),
        kv('最后更新', fmt.time(p.updatedAt)),
        kv('包文件', p.packageFile),
      ),
      p.revoked
        ? h('div', { style: 'margin-top:14px' },
            note(`下架原因：${p.revokedReason || '（未填写）'}`, 'danger'))
        : null,
      h('div', { style: 'margin-top:14px' },
        h('div.kv',
          h('dt', { text: '内容哈希' }),
          h('dd.mono', { text: p.contentHash }),
        ),
      ),
    ));

    wrap.append(card(
      cardHead('提审历史', `${p.submissions.length} 次`),
      p.submissions.length
        ? table([
            { key: 'version', label: '版本', render: (r) => h('span.mono', { text: r.version }) },
            {
              key: 'status', label: '结果',
              render: (r) => { const s = fmt.submissionStatus(r.status); return tag(s.text, s.cls); },
            },
            { key: 'message', label: '结论', cellClass: 'trunc', render: (r) => h('span', { title: r.message, text: r.message || '—' }) },
            { key: 'createdAt', label: '提交时间', render: (r) => fmt.time(r.createdAt) },
            {
              key: 'act', label: '', cellClass: 'actions',
              render: (r) => h('a.btn.btn-sm', { href: `#/submissions/${encodeURIComponent(r.id)}`, text: '详情' }),
            },
          ], p.submissions)
        : h('div.empty', { text: '没有提审记录（可能是早期直接入库的条目）' }),
    ));

    return wrap;
  });
}

function kv(label, value) {
  return h('div.kv', h('dt', { text: label }), h('dd', { text: value }));
}
