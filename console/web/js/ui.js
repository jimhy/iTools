// DOM 构造与通用组件。
//
// 零框架、零构建：这套后台的页面结构不复杂，命令式 DOM 足够，
// 换来的是「改完 scp 就生效」的秒级发布（对照：改 Rust 要重发 30MB 镜像 + 一分钟中断）。

/**
 * 元素工厂。`h('div.card', {...}, child, child)`。
 *
 * **所有文本都走 textContent**，没有任何 innerHTML 路径——
 * 用户名、插件描述、驳回原因都是外部输入，一个 innerHTML 就是存储型 XSS。
 */
export function h(spec, props, ...children) {
  const [tagAndId, ...classes] = String(spec).split('.');
  const [tag, id] = tagAndId.split('#');
  const el = document.createElement(tag || 'div');
  if (id) el.id = id;
  if (classes.length) el.className = classes.join(' ');

  if (props && typeof props === 'object' && !(props instanceof Node) && !Array.isArray(props)) {
    for (const [k, v] of Object.entries(props)) {
      if (v === undefined || v === null || v === false) continue;
      if (k === 'class') el.className = [el.className, v].filter(Boolean).join(' ');
      else if (k === 'text') el.textContent = String(v);
      else if (k === 'html') throw new Error('禁止 innerHTML：所有文本走 text');
      else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, String(v));
    }
  } else if (props !== undefined && props !== null) {
    children.unshift(props);
  }

  for (const c of children.flat(Infinity)) {
    if (c === undefined || c === null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

// ---------- Toast ----------

const toastHost = () => document.getElementById('toasts');

export function toast(message, kind = 'info', ms = 4000) {
  const el = h(`div.toast.toast-${kind}`, { text: message });
  toastHost().append(el);
  setTimeout(() => {
    el.style.opacity = '0';
    el.style.transition = 'opacity .2s';
    setTimeout(() => el.remove(), 220);
  }, ms);
  return el;
}

export const toastOk = (m) => toast(m, 'ok');
/** 错误 toast 停留久一点——出错时人往往正在别处看 */
export const toastErr = (m) => toast(m, 'err', 7000);

// ---------- 确认对话框 ----------

/**
 * 危险操作确认。`echo` 非空时要求用户原样输入该串才能确认（防手滑删错人）。
 * 返回 Promise<boolean>。
 */
export function confirmDialog({ title, text, echo, okText = '确认', danger = true }) {
  const dlg = document.getElementById('confirm-dialog');
  const okBtn = document.getElementById('confirm-ok');
  const echoWrap = document.getElementById('confirm-echo-wrap');
  const echoInput = document.getElementById('confirm-echo');
  const echoLabel = document.getElementById('confirm-echo-label');

  document.getElementById('confirm-title').textContent = title;
  const textEl = clear(document.getElementById('confirm-text'));
  for (const line of Array.isArray(text) ? text : [text]) {
    textEl.append(line instanceof Node ? line : h('p', { text: String(line) }));
  }

  okBtn.textContent = okText;
  okBtn.className = danger ? 'btn btn-danger' : 'btn btn-primary';

  if (echo) {
    echoWrap.hidden = false;
    echoInput.value = '';
    echoLabel.textContent = `请输入「${echo}」以确认`;
    okBtn.disabled = true;
    const check = () => { okBtn.disabled = echoInput.value !== echo; };
    echoInput.addEventListener('input', check);
    dlg.addEventListener('close', () => echoInput.removeEventListener('input', check), { once: true });
  } else {
    echoWrap.hidden = true;
    okBtn.disabled = false;
  }

  return new Promise((resolve) => {
    dlg.addEventListener('close', () => resolve(dlg.returnValue === 'ok'), { once: true });
    dlg.showModal();
    if (echo) echoInput.focus();
  });
}

// ---------- 常用块 ----------

export const card = (...children) => h('section.card', ...children);

export function cardHead(title, sub, ...actions) {
  return h('div.card-head',
    h('div', h('h3', { text: title }), sub ? h('div.sub', { text: sub }) : null),
    actions.length ? h('div.toolbar', ...actions) : null,
  );
}

export function stat(label, value, delta) {
  return h('div.card',
    h('div.stat',
      h('span.stat-label', { text: label }),
      h('span.stat-value', { text: value }),
      delta ? h('span.stat-delta', { text: delta }) : null,
    ),
  );
}

export function tag(text, cls = '') {
  return h(`span.tag${cls ? '.' + cls : ''}`, { text });
}

/** 说明条。用来把「这个数字到底是什么」讲清楚。 */
export function note(text, kind = '') {
  return h(`div.note${kind ? '.note-' + kind : ''}`,
    h('span.ico', { text: kind === 'warn' ? '⚠' : 'ⓘ' }),
    h('div', { text }),
  );
}

/**
 * 「未采集 / 未接入」占位块。
 *
 * 刻意不画成图表：一条零线会被理解成「这段时间没有流量」，
 * 而事实是「这个东西从来没被记录过」。两者必须看起来完全不同。
 */
export function unavailable({ label, reason, unblockedBy }) {
  return h('div.unavail',
    h('h4', { text: `${label} · 未采集` }),
    h('p', { text: reason }),
    unblockedBy ? h('p.why', { text: `接入条件：${unblockedBy}` }) : null,
  );
}

export const loading = (text = '加载中…') => h('div.loading', { text });
export const empty = (text = '没有数据') => h('div.empty', { text });

/**
 * 表格。`columns` 每项：{ key, label, num?, sortable?, render?(row) }。
 * `render` 返回 Node 或字符串。
 */
export function table(columns, rows, { sort, order, onSort, emptyText } = {}) {
  if (!rows.length) return h('div.table-wrap', empty(emptyText));

  const headCells = columns.map((c) => {
    const active = sort && c.key === sort;
    const cell = h(`th${c.num ? '.num' : ''}${c.sortable ? '.sortable' : ''}`,
      c.label,
      active ? h('span.arrow', { text: order === 'asc' ? ' ↑' : ' ↓' }) : null,
    );
    if (c.sortable && onSort) cell.addEventListener('click', () => onSort(c.key));
    return cell;
  });

  const body = rows.map((row) =>
    h('tr', columns.map((c) => {
      const v = c.render ? c.render(row) : row[c.key];
      const cls = c.cellClass ? `.${c.cellClass}` : (c.num ? '.num' : '');
      return h(`td${cls}`, v === undefined || v === null ? '—' : v);
    })),
  );

  return h('div.table-wrap',
    h('table',
      h('thead', h('tr', headCells)),
      h('tbody', body),
    ),
  );
}

/** 分页条。`onPage(n)` 切页。 */
export function pager({ page, size, total, onPage }) {
  const pages = Math.max(1, Math.ceil(total / size));
  const cur = Math.min(page, pages);
  const from = total === 0 ? 0 : (cur - 1) * size + 1;
  const to = Math.min(cur * size, total);

  return h('div.pager',
    h('div.info', { text: `第 ${from}–${to} 条，共 ${total} 条 · 第 ${cur}/${pages} 页` }),
    h('div.ctrl',
      h('button.btn.btn-sm', { text: '上一页', disabled: cur <= 1, onclick: () => onPage(cur - 1) }),
      h('button.btn.btn-sm', { text: '下一页', disabled: cur >= pages, onclick: () => onPage(cur + 1) }),
    ),
  );
}

/** 把异步渲染包起来：先显示加载中，出错时如实显示错误而不是空白。 */
export async function renderAsync(container, fn) {
  clear(container).append(loading());
  try {
    const node = await fn();
    clear(container).append(node);
  } catch (e) {
    clear(container).append(
      h('div.card',
        h('div.note.note-danger',
          h('span.ico', { text: '⚠' }),
          h('div',
            h('strong', { text: '加载失败：' }),
            e?.message || String(e),
            e?.code ? h('div.mono', { text: `code=${e.code}${e.status ? ` status=${e.status}` : ''}` }) : null,
          ),
        ),
      ),
    );
  }
}
