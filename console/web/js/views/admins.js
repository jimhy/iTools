// 控制台账号管理（仅超级管理员可见）。
//
// 注意与「用户」页的区别：这里的停用是**真实生效**的——控制台每次校验令牌都会
// 联表查 status，停用后对方下一个请求就被拒，且会话立刻被清掉。
// 终端用户那边做不到同样的事，因为那要主服务配合。

import { Api } from '../api.js';
import * as fmt from '../fmt.js';
import {
  card, cardHead, clear, confirmDialog, h, note, renderAsync, table, tag,
  toastErr, toastOk,
} from '../ui.js';

const ROLES = [
  ['super', '超级管理员', '可管理控制台账号，拥有全部权限'],
  ['admin', '管理员', '可执行运营动作，但不能增删控制台账号'],
  ['viewer', '只读', '只能查看，任何写操作都会被拒绝'],
];

export async function render(container, ctx, param, page) {
  page.setSubtitle('控制台自己的登录账号，与 iTools 云账号完全隔离');

  const host = h('div');
  clear(container).append(host);
  page.setActions(
    h('button.btn.btn-primary', { text: '+ 新建账号', onclick: () => showCreate(reload) }),
    h('button.btn.btn-ghost', { text: '刷新', onclick: () => reload() }),
  );
  await reload();

  async function reload() {
    await renderAsync(host, async () => {
      const data = await Api.admins();
      const meName = ctx.me.username;

      const cols = [
        {
          key: 'username', label: '用户名',
          render: (r) => h('span',
            r.username,
            r.username === meName ? h('span.tag.tag-accent', { style: 'margin-left:6px', text: '当前登录' }) : null,
          ),
        },
        {
          key: 'role', label: '角色',
          render: (r) => h('select', {
            disabled: r.username === meName,
            title: r.username === meName ? '不能修改自己的角色，请让另一个超管操作' : '',
            style: 'width:auto;padding:4px 8px;font-size:13px',
            onchange: (e) => changeRole(r.username, e.target.value, reload),
          }, ROLES.map(([v, t]) => h('option', { value: v, selected: r.role === v }, t))),
        },
        {
          key: 'status', label: '状态',
          render: (r) => (r.status === 'active' ? tag('可用', 'tag-ok') : tag('已停用', 'tag-danger')),
        },
        {
          key: 'must', label: '待改密',
          render: (r) => (r.mustChangePassword ? tag('是', 'tag-warn') : '—'),
        },
        { key: 'createdAt', label: '创建时间', render: (r) => fmt.date(r.createdAt) },
        {
          key: 'lastLogin', label: '最后登录',
          render: (r) => (r.lastLoginAt
            ? h('span', { title: `${fmt.time(r.lastLoginAt)}${r.lastLoginIp ? ` · ${r.lastLoginIp}` : ''}` },
                fmt.ago(r.lastLoginAt))
            : '从未登录'),
        },
        {
          key: 'act', label: '操作', cellClass: 'actions',
          render: (r) => h('span',
            h('button.btn.btn-sm', {
              text: r.status === 'active' ? '停用' : '启用',
              disabled: r.username === meName,
              title: r.username === meName ? '不能停用自己' : '',
              onclick: () => toggleStatus(r, reload),
            }),
            h('button.btn.btn-sm', {
              text: '重置口令',
              onclick: () => showReset(r.username, reload),
            }),
            h('button.btn.btn-sm.btn-danger-soft', {
              text: '删除',
              disabled: r.username === meName,
              title: r.username === meName ? '不能删除自己' : '',
              onclick: () => remove(r.username, reload),
            }),
          ),
        },
      ];

      return h('div', { style: 'display:flex;flex-direction:column;gap:14px' },
        note(
          '停用、改角色、重置口令都会立即清掉对方的全部控制台会话。系统会阻止「删除/降级/停用最后一个可用超管」——'
          + '那会导致谁都进不来。',
        ),
        table(cols, data.items),
        card(
          cardHead('角色说明'),
          h('dl.detail-grid', ROLES.map(([, name, desc]) =>
            h('div.kv', h('dt', { text: name }), h('dd', { style: 'font-weight:400;font-size:13px', text: desc })))),
        ),
      );
    });
  }
}

// ---------- 操作 ----------

async function changeRole(username, role, reload) {
  try {
    await Api.setAdminRole(username, role);
    toastOk(`${username} 的角色已改为${fmt.roleName(role)}`);
  } catch (e) {
    toastErr(`修改角色失败：${e.message}`);
  }
  reload();
}

async function toggleStatus(admin, reload) {
  const next = admin.status === 'active' ? 'disabled' : 'active';
  if (next === 'disabled') {
    const ok = await confirmDialog({
      title: '停用控制台账号',
      text: [`将停用 ${admin.username}，其当前所有登录会话会被立即清除，之后无法再登录控制台。`],
      okText: '确认停用',
    });
    if (!ok) return;
  }
  try {
    await Api.setAdminStatus(admin.username, next);
    toastOk(next === 'disabled' ? `已停用 ${admin.username}` : `已启用 ${admin.username}`);
  } catch (e) {
    toastErr(`操作失败：${e.message}`);
  }
  reload();
}

async function remove(username, reload) {
  const ok = await confirmDialog({
    title: '删除控制台账号',
    text: [`将永久删除控制台账号 ${username} 及其全部会话。此操作不可撤销。`],
    echo: username,
    okText: '永久删除',
  });
  if (!ok) return;
  try {
    await Api.deleteAdmin(username);
    toastOk(`已删除 ${username}`);
  } catch (e) {
    toastErr(`删除失败：${e.message}`);
  }
  reload();
}

// ---------- 新建 / 重置口令 ----------

function showCreate(reload) {
  const user = h('input', { autocapitalize: 'off', spellcheck: 'false', placeholder: '字母数字与 . - _' });
  const pass = h('input', { type: 'password', autocomplete: 'new-password' });
  const roleSel = h('select', {}, ROLES.map(([v, t]) => h('option', { value: v, selected: v === 'admin' }, t)));
  const errEl = h('div.login-error', { hidden: true });

  const dlg = h('dialog.modal',
    h('form.modal-body', {
      onsubmit: async (ev) => {
        ev.preventDefault();
        errEl.hidden = true;
        try {
          await Api.createAdmin(user.value.trim(), pass.value, roleSel.value);
          toastOk(`已创建 ${user.value.trim()}，对方首次登录必须改口令`);
          dlg.close();
          dlg.remove();
          reload();
        } catch (e) {
          errEl.textContent = e.message;
          errEl.hidden = false;
        }
      },
    },
      h('h3', { text: '新建控制台账号' }),
      h('div.modal-text', { text: '新账号会被标记为「首次登录必须改口令」——你设的这个口令只是一次性的。' }),
      h('label.field', h('span', { text: '用户名' }), user),
      h('label.field', h('span', { text: '初始口令' }), pass,
        h('em.field-hint', { text: '至少 8 个字符，需同时包含字母与非字母字符' })),
      h('label.field', h('span', { text: '角色' }), roleSel),
      errEl,
      h('div.modal-actions',
        h('button.btn.btn-ghost', {
          type: 'button', text: '取消',
          onclick: () => { dlg.close(); dlg.remove(); },
        }),
        h('button.btn.btn-primary', { type: 'submit', text: '创建' }),
      ),
    ),
  );
  document.body.append(dlg);
  dlg.showModal();
  user.focus();
}

function showReset(username, reload) {
  const pass = h('input', { type: 'password', autocomplete: 'new-password' });
  const errEl = h('div.login-error', { hidden: true });

  const dlg = h('dialog.modal',
    h('form.modal-body', {
      onsubmit: async (ev) => {
        ev.preventDefault();
        errEl.hidden = true;
        try {
          await Api.resetAdminPassword(username, pass.value);
          toastOk(`已重置 ${username} 的口令，其会话已全部清除`);
          dlg.close();
          dlg.remove();
          reload();
        } catch (e) {
          errEl.textContent = e.message;
          errEl.hidden = false;
        }
      },
    },
      h('h3', { text: `重置 ${username} 的口令` }),
      h('div.modal-text', { text: '重置后对方全部会话立即失效，且下次登录必须再改一次口令。' }),
      h('label.field', h('span', { text: '新口令' }), pass,
        h('em.field-hint', { text: '至少 8 个字符，需同时包含字母与非字母字符' })),
      errEl,
      h('div.modal-actions',
        h('button.btn.btn-ghost', {
          type: 'button', text: '取消',
          onclick: () => { dlg.close(); dlg.remove(); },
        }),
        h('button.btn.btn-primary', { type: 'submit', text: '重置' }),
      ),
    ),
  );
  document.body.append(dlg);
  dlg.showModal();
  pass.focus();
}
