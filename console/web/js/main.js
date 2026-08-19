// 入口：启动流程、hash 路由、登录/改密/登出、主题。

import { Api, ApiError, getToken, setToken, setHandlers } from './api.js';
import { clear, h, toastErr, toastOk } from './ui.js';
import * as overview from './views/overview.js';
import * as users from './views/users.js';
import * as plugins from './views/plugins.js';
import * as submissions from './views/submissions.js';
import * as trends from './views/trends.js';
import * as admins from './views/admins.js';
import * as audit from './views/audit.js';
import * as system from './views/system.js';

/** 路由表。`needSuper` 为真的项只对超级管理员显示。 */
const ROUTES = {
  overview: { title: '概览', icon: '▦', mod: overview },
  users: { title: '用户', icon: '☺', mod: users },
  plugins: { title: '插件市场', icon: '⬢', mod: plugins },
  submissions: { title: '提审单', icon: '✎', mod: submissions },
  trends: { title: '数据统计', icon: '◵', mod: trends },
  audit: { title: '审计日志', icon: '☰', mod: audit },
  admins: { title: '控制台账号', icon: '⚿', mod: admins, needSuper: true },
  system: { title: '系统状态', icon: '⚙', mod: system },
};

const DEFAULT_ROUTE = 'overview';

/** 全局上下文，交给各视图使用。 */
const ctx = {
  me: null,      // whoami 的结果
  meta: null,    // /api/meta 的结果
  navigate,
};

// ---------- 启动 ----------

const el = {
  login: document.getElementById('login'),
  pwchange: document.getElementById('pwchange'),
  app: document.getElementById('app'),
  view: document.getElementById('view'),
  nav: document.getElementById('nav'),
  title: document.getElementById('page-title'),
  sub: document.getElementById('page-sub'),
  actions: document.getElementById('page-actions'),
};

setHandlers({
  unauthorized() {
    ctx.me = null;
    showLogin('登录已失效，请重新登录');
  },
  mustChangePassword() {
    showPasswordChange();
  },
});

initTheme();
boot();

async function boot() {
  // 先探服务端能力：登录页也要用它显示版本与状态。
  // 探不到就如实说「连不上」，而不是让人在登录页干等。
  try {
    ctx.meta = await Api.meta();
    document.getElementById('login-sub').textContent = `版本 v${ctx.meta.version}`;
    document.getElementById('side-version').textContent = `v${ctx.meta.version}`;
  } catch (e) {
    document.getElementById('login-sub').textContent =
      e instanceof ApiError && e.code === 'network'
        ? '连不上控制台服务端'
        : `服务端异常：${e.message}`;
  }

  if (!getToken()) {
    showLogin();
    return;
  }
  try {
    ctx.me = await Api.whoami();
    if (ctx.me.mustChangePassword) {
      showPasswordChange();
      return;
    }
    showApp();
  } catch (e) {
    // whoami 失败但不是 401（401 已由 api.js 处理）→ 服务端有问题，别默默回登录页
    if (!(e instanceof ApiError) || e.status !== 401) {
      showLogin(`无法确认登录状态：${e.message}`);
    }
  }
}

// ---------- 页面切换 ----------

function showLogin(errorText) {
  el.app.hidden = true;
  el.pwchange.hidden = true;
  el.login.hidden = false;
  const errEl = document.getElementById('login-error');
  if (errorText) {
    errEl.textContent = errorText;
    errEl.hidden = false;
  } else {
    errEl.hidden = true;
  }
  document.getElementById('login-user').focus();
}

function showPasswordChange() {
  el.app.hidden = true;
  el.login.hidden = true;
  el.pwchange.hidden = false;
  document.getElementById('pw-old').focus();
}

function showApp() {
  el.login.hidden = true;
  el.pwchange.hidden = true;
  el.app.hidden = false;

  document.getElementById('who-name').textContent = ctx.me.username;
  document.getElementById('who-role').textContent =
    { super: '超级管理员', admin: '管理员', viewer: '只读账号' }[ctx.me.role] || ctx.me.role;

  renderNav();
  route();
}

function renderNav() {
  const nav = clear(el.nav);
  for (const [key, r] of Object.entries(ROUTES)) {
    if (r.needSuper && !ctx.me?.capabilities?.canManageAdmins) continue;
    nav.append(h('a.nav-item', {
      href: `#/${key}`,
      dataset: { route: key },
    }, h('span.ico', { text: r.icon }), r.title));
  }
}

// ---------- 路由 ----------

function navigate(path) {
  location.hash = path.startsWith('#') ? path : `#${path}`;
}

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [name, ...rest] = raw.split('/').filter(Boolean);
  return { name: ROUTES[name] ? name : DEFAULT_ROUTE, param: rest.map(decodeURIComponent).join('/') };
}

async function route() {
  if (el.app.hidden) return;
  const { name, param } = parseHash();
  const r = ROUTES[name];

  if (r.needSuper && !ctx.me?.capabilities?.canManageAdmins) {
    navigate(`/${DEFAULT_ROUTE}`);
    return;
  }

  for (const item of el.nav.querySelectorAll('.nav-item')) {
    item.classList.toggle('active', item.dataset.route === name);
  }

  el.title.textContent = r.title;
  el.sub.textContent = '';
  clear(el.actions);
  clear(el.view);

  try {
    await r.mod.render(el.view, ctx, param, {
      setSubtitle: (t) => { el.sub.textContent = t || ''; },
      setActions: (...nodes) => { clear(el.actions).append(...nodes.filter(Boolean)); },
      setTitle: (t) => { el.title.textContent = t; },
    });
  } catch (e) {
    clear(el.view).append(
      h('div.card',
        h('div.note.note-danger',
          h('span.ico', { text: '⚠' }),
          h('div',
            h('strong', { text: '页面加载失败：' }),
            e?.message || String(e),
          ),
        ),
      ),
    );
  }
}

window.addEventListener('hashchange', route);

// ---------- 登录 ----------

document.getElementById('login-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = document.getElementById('login-submit');
  const errEl = document.getElementById('login-error');
  const username = document.getElementById('login-user').value.trim();
  const password = document.getElementById('login-pass').value;

  btn.disabled = true;
  btn.textContent = '登录中…';
  errEl.hidden = true;
  try {
    const r = await Api.login(username, password);
    setToken(r.token);
    document.getElementById('login-pass').value = '';
    ctx.me = await Api.whoami();
    if (r.mustChangePassword) showPasswordChange();
    else showApp();
  } catch (e) {
    errEl.textContent = e.message;
    errEl.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = '登录';
  }
});

// ---------- 强制改密 ----------

document.getElementById('pw-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const btn = document.getElementById('pw-submit');
  const errEl = document.getElementById('pw-error');
  const oldPw = document.getElementById('pw-old').value;
  const newPw = document.getElementById('pw-new').value;
  const newPw2 = document.getElementById('pw-new2').value;

  errEl.hidden = true;
  if (newPw !== newPw2) {
    errEl.textContent = '两次输入的新口令不一致';
    errEl.hidden = false;
    return;
  }

  btn.disabled = true;
  btn.textContent = '保存中…';
  try {
    const r = await Api.changePassword(oldPw, newPw);
    // 服务端改密后会重签令牌（旧的全部作废），这里必须换上新的
    setToken(r.token);
    for (const id of ['pw-old', 'pw-new', 'pw-new2']) document.getElementById(id).value = '';
    ctx.me = await Api.whoami();
    toastOk('口令已更新，其它设备上的登录已失效');
    showApp();
  } catch (e) {
    errEl.textContent = e.message;
    errEl.hidden = false;
  } finally {
    btn.disabled = false;
    btn.textContent = '保存并继续';
  }
});

document.getElementById('pw-logout').addEventListener('click', doLogout);
document.getElementById('logout').addEventListener('click', doLogout);

async function doLogout() {
  try {
    await Api.logout();
  } catch (e) {
    // 登出失败（比如会话已经没了）不该卡住用户，本地清掉就行
    if (!(e instanceof ApiError) || e.code === 'network') toastErr(`登出请求失败：${e.message}`);
  }
  setToken('');
  ctx.me = null;
  showLogin();
}

// ---------- 主题 ----------

function initTheme() {
  const saved = localStorage.getItem('itools-console-theme');
  const prefersDark = window.matchMedia?.('(prefers-color-scheme: dark)').matches;
  document.documentElement.dataset.theme = saved || (prefersDark ? 'dark' : 'light');
  document.getElementById('theme-toggle').addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    localStorage.setItem('itools-console-theme', next);
  });
}
