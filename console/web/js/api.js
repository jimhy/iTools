// 与控制台服务端通信。
//
// 错误处理的原则：**绝不把不同原因折叠成一句「网络异常」**。
// 「没登录」「权限不够」「端点不存在」「服务端 500」「真的连不上」是五件事，
// 运营看到哪一句就知道该找谁，含糊一句只会让人瞎猜。

/** 令牌放 sessionStorage：关掉标签页即失效，比 localStorage 少一份长期泄露面。 */
const TOKEN_KEY = 'itools-console-token';

/** 会话失效时的回调，由 main.js 注入（把界面切回登录页）。 */
let onUnauthorized = () => {};
/** 需要强制改密时的回调。 */
let onMustChangePassword = () => {};

export function setHandlers({ unauthorized, mustChangePassword }) {
  if (unauthorized) onUnauthorized = unauthorized;
  if (mustChangePassword) onMustChangePassword = mustChangePassword;
}

export function getToken() {
  return sessionStorage.getItem(TOKEN_KEY) || '';
}

export function setToken(token) {
  if (token) sessionStorage.setItem(TOKEN_KEY, token);
  else sessionStorage.removeItem(TOKEN_KEY);
}

/** 带 code 的错误。UI 据 code 决定怎么呈现，不靠匹配中文文案。 */
export class ApiError extends Error {
  constructor(message, code, status) {
    super(message);
    this.name = 'ApiError';
    this.code = code;
    this.status = status;
  }
}

async function request(method, path, { query, body } = {}) {
  const url = new URL(path, location.origin);
  if (query) {
    for (const [k, v] of Object.entries(query)) {
      if (v === undefined || v === null || v === '') continue;
      url.searchParams.set(k, String(v));
    }
  }

  const headers = {};
  const token = getToken();
  if (token) headers['Authorization'] = `Bearer ${token}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';

  let res;
  try {
    res = await fetch(url, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      // 控制台与 API 同源，不需要也不应该带跨域凭据
      credentials: 'same-origin',
    });
  } catch (e) {
    // 这里才是真正的「连不上」：DNS、TLS、断网、被浏览器拦截
    throw new ApiError('无法连接控制台服务端（网络中断或服务未运行）', 'network', 0);
  }

  // 204 之类没有响应体的情况
  if (res.status === 204) return null;

  const text = await res.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      // 服务端返回了非 JSON——多半是被中间的反代/网关截了胡。
      // 如实说出来，而不是假装成业务错误。
      throw new ApiError(
        `服务端返回了非 JSON 内容（HTTP ${res.status}），请求可能被中间网关拦截`,
        'bad_response',
        res.status,
      );
    }
  }

  if (res.ok) return data;

  const code = data?.code || 'server_error';
  const message = data?.error || `请求失败（HTTP ${res.status}）`;

  if (res.status === 401) {
    setToken('');
    onUnauthorized(message);
  } else if (code === 'must_change_password') {
    onMustChangePassword();
  }
  throw new ApiError(message, code, res.status);
}

export const api = {
  get: (path, query) => request('GET', path, { query }),
  post: (path, body, query) => request('POST', path, { body: body ?? {}, query }),
  del: (path, query) => request('DELETE', path, { query }),
};

// ---- 具体端点 ----

export const Api = {
  meta: () => api.get('/api/meta'),
  login: (username, password) => api.post('/api/login', { username, password }),
  logout: () => api.post('/api/logout'),
  whoami: () => api.get('/api/whoami'),
  changePassword: (oldPassword, newPassword) =>
    api.post('/api/password', { old_password: oldPassword, new_password: newPassword }),

  overview: () => api.get('/api/overview'),
  series: (metric, points, bucket) => api.get('/api/stats/series', { metric, points, bucket }),
  storage: (limit) => api.get('/api/stats/storage', { limit }),
  namespaces: () => api.get('/api/stats/namespaces'),

  users: (params) => api.get('/api/users', params),
  user: (name) => api.get(`/api/users/${encodeURIComponent(name)}`),
  kickUser: (name) => api.post(`/api/users/${encodeURIComponent(name)}/kick`),
  deleteUser: (name) => api.del(`/api/users/${encodeURIComponent(name)}`, { confirm: name }),

  plugins: (params) => api.get('/api/plugins', params),
  plugin: (name) => api.get(`/api/plugins/${encodeURIComponent(name)}`),
  submissions: (params) => api.get('/api/submissions', params),
  submission: (id) => api.get(`/api/submissions/${encodeURIComponent(id)}`),

  admins: () => api.get('/api/admins'),
  createAdmin: (username, password, role) => api.post('/api/admins', { username, password, role }),
  deleteAdmin: (name) => api.del(`/api/admins/${encodeURIComponent(name)}`),
  setAdminRole: (name, role) => api.post(`/api/admins/${encodeURIComponent(name)}/role`, { role }),
  setAdminStatus: (name, status) => api.post(`/api/admins/${encodeURIComponent(name)}/status`, { status }),
  resetAdminPassword: (name, password) =>
    api.post(`/api/admins/${encodeURIComponent(name)}/password`, { password }),

  audit: (params) => api.get('/api/audit', params),
  system: () => api.get('/api/system'),
};
