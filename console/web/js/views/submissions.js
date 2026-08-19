// 提审单：列表 / 详情。**只读**。
//
// `manual`（待人工处理）在主服务里目前是死胡同——全仓没有任何端点能把它改判为通过。
// 这个事实必须显示出来，否则运营会一直等一个不存在的按钮。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, h, note, pager, renderAsync, table, tag,
} from '../ui.js';

const state = { q: '', status: '', page: 1, size: 30 };

const STATUS_OPTIONS = [
  ['', '全部状态'],
  ['reviewing', '审核中'],
  ['approved', '已上线'],
  ['rejected', '已驳回'],
  ['manual', '待人工处理'],
  ['failed', '校验未通过'],
];

export async function render(container, ctx, param, page) {
  if (param) return renderDetail(container, ctx, param, page);
  return renderList(container, ctx, page);
}

async function renderList(container, ctx, page) {
  page.setTitle('提审单');
  page.setSubtitle('插件提交与审核记录（一次提审一行，永不覆盖）');

  const search = h('input', {
    type: 'search', placeholder: '搜索插件名或作者…', value: state.q,
    onkeydown: (e) => { if (e.key === 'Enter') { state.q = e.target.value.trim(); state.page = 1; reload(); } },
  });
  const statusSel = h('select', {
    onchange: (e) => { state.status = e.target.value; state.page = 1; reload(); },
  }, STATUS_OPTIONS.map(([v, t]) => h('option', { value: v, selected: state.status === v }, t)));

  page.setActions(
    search,
    h('button.btn', { text: '搜索', onclick: () => { state.q = search.value.trim(); state.page = 1; reload(); } }),
    statusSel,
  );

  const host = h('div');
  clear(container).append(host);
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const data = await Api.submissions({
        q: state.q, status: state.status, page: state.page, size: state.size,
      });

      const cols = [
        {
          key: 'name', label: '插件',
          render: (r) => h('a', { href: `#/submissions/${encodeURIComponent(r.id)}`, text: r.name }),
        },
        { key: 'version', label: '版本', render: (r) => h('span.mono', { text: r.version }) },
        { key: 'author', label: '作者', render: (r) => h('a', { href: `#/users/${encodeURIComponent(r.author)}`, text: r.author }) },
        {
          key: 'status', label: '状态',
          render: (r) => { const s = fmt.submissionStatus(r.status); return tag(s.text, s.cls); },
        },
        { key: 'size', label: '包体积', num: true, render: (r) => fmt.bytes(r.sizeBytes) },
        { key: 'files', label: '文件数', num: true, render: (r) => fmt.num(r.fileCount) },
        { key: 'message', label: '结论', cellClass: 'trunc', render: (r) => h('span', { title: r.message, text: r.message || '—' }) },
        { key: 'createdAt', label: '提交时间', render: (r) => h('span', { title: fmt.time(r.createdAt), text: fmt.ago(r.createdAt) }) },
      ];

      const manualCount = data.items.filter((s) => s.status === 'manual').length;

      return h('div', { style: 'display:flex;flex-direction:column;gap:14px' },
        note(data.capabilities.manualNote, manualCount ? 'warn' : ''),
        table(cols, data.items, {
          emptyText: state.status || state.q ? '没有匹配的提审单' : '还没有任何提审记录',
        }),
        pager({ page: data.page, size: data.size, total: data.total, onPage: (p) => { state.page = p; reload(); } }),
      );
    });
  }
}

async function renderDetail(container, ctx, id, page) {
  page.setTitle('提审单详情');
  page.setActions(h('a.btn.btn-ghost', { href: '#/submissions', text: '← 返回列表' }));

  await renderAsync(container, async () => {
    const s = await Api.submission(id);
    page.setSubtitle(`${s.name} ${s.version}`);
    const st = fmt.submissionStatus(s.status);

    const wrap = h('div', { style: 'display:flex;flex-direction:column;gap:18px' });

    wrap.append(card(
      cardHead('提审信息', '', tag(st.text, st.cls)),
      h('dl.detail-grid',
        kv('插件名', s.name),
        kv('版本', s.version),
        kv('作者', s.author),
        kv('文件数', fmt.num(s.fileCount)),
        kv('包体积', fmt.bytes(s.sizeBytes)),
        kv('提交时间', fmt.time(s.createdAt)),
        kv('更新时间', fmt.time(s.updatedAt)),
      ),
      h('div', { style: 'margin-top:14px' },
        h('div.kv', h('dt', { text: '内容哈希' }), h('dd.mono', { text: s.contentHash })),
      ),
      s.message
        ? h('div', { style: 'margin-top:14px' },
            h('div.kv', h('dt', { text: '给作者的结论' }), h('dd', { text: s.message })))
        : null,
      s.status === 'manual'
        ? h('div', { style: 'margin-top:14px' },
            note('本状态在云同步服务端没有处置入口——控制台无法改判，请让作者重新提交。', 'warn'))
        : null,
    ));

    wrap.append(card(
      cardHead('模型审核结论', s.review ? '原文' : ''),
      s.review
        ? h('pre.code', { text: prettyJson(s.review) })
        : h('div.empty', { text: '这次提审没有留下模型裁决（未配置 LLM、审核失败或尚未审核）' }),
    ));

    wrap.append(card(
      cardHead('plugin.json', '提交时的清单原文'),
      s.manifest
        ? h('pre.code', { text: prettyJson(s.manifest) })
        : h('div.empty', { text: '没有清单内容' }),
    ));

    return wrap;
  });
}

/** JSON 美化。解析不了就原样显示——绝不吞掉内容。 */
function prettyJson(text) {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

function kv(label, value) {
  return h('div.kv', h('dt', { text: label }), h('dd', { text: value }));
}
