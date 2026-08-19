// 格式化工具。
//
// 时间戳统一是**毫秒**（与服务端一致）。分桶接口返回的 `t` 是**秒**，
// 两者在这里各有各的函数，别混用——混一次整条曲线就错位了。

/** 毫秒时间戳 → 本地时间字符串。0 或非法值显示成「—」而不是 1970。 */
export function time(ms) {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return '—';
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return '—';
  return d.toLocaleString('zh-CN', { hour12: false });
}

/** 毫秒时间戳 → 只到天。 */
export function date(ms) {
  if (!ms || !Number.isFinite(ms) || ms <= 0) return '—';
  const d = new Date(ms);
  if (Number.isNaN(d.getTime())) return '—';
  return d.toLocaleDateString('zh-CN');
}

/** Unix 秒 → 短标签（用于图表横轴）。 */
export function tickLabel(sec, hourly) {
  const d = new Date(sec * 1000);
  if (Number.isNaN(d.getTime())) return '';
  if (hourly) return `${String(d.getHours()).padStart(2, '0')}:00`;
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

/** Unix 秒 → 完整时间（图表 tooltip）。 */
export function tickTitle(sec, hourly) {
  const d = new Date(sec * 1000);
  if (Number.isNaN(d.getTime())) return '';
  return hourly
    ? d.toLocaleString('zh-CN', { hour12: false })
    : d.toLocaleDateString('zh-CN');
}

/** 相对时间：3 分钟前 / 2 天前。未来时间显示成「刚刚」而不是负数。 */
export function ago(ms) {
  if (!ms || ms <= 0) return '—';
  const diff = Date.now() - ms;
  if (diff < 0) return '刚刚';
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return '刚刚';
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min} 分钟前`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour} 小时前`;
  const day = Math.floor(hour / 24);
  if (day < 30) return `${day} 天前`;
  const month = Math.floor(day / 30);
  if (month < 12) return `${month} 个月前`;
  return `${Math.floor(month / 12)} 年前`;
}

const UNITS = ['B', 'KB', 'MB', 'GB', 'TB'];

/** 字节数 → 人类可读。负数表示「读不到」，交给调用方处理。 */
export function bytes(n) {
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n === 0) return '0 B';
  let v = n;
  let i = 0;
  while (v >= 1024 && i < UNITS.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${UNITS[i]}`;
}

/** 千分位。 */
export function num(n) {
  if (!Number.isFinite(n)) return '—';
  return n.toLocaleString('zh-CN');
}

/** 提审单状态 → 中文标签与配色。未知状态原样显示，不吞掉。 */
export function submissionStatus(s) {
  switch (s) {
    case 'reviewing': return { text: '审核中', cls: 'tag-accent' };
    case 'approved': return { text: '已上线', cls: 'tag-ok' };
    case 'rejected': return { text: '已驳回', cls: 'tag-danger' };
    case 'manual': return { text: '待人工处理', cls: 'tag-warn' };
    case 'failed': return { text: '校验未通过', cls: 'tag-danger' };
    default: return { text: s || '—', cls: '' };
  }
}

/** 审计动作 → 中文。 */
export function auditAction(a) {
  const map = {
    login: '登录',
    login_failed: '登录失败',
    logout: '登出',
    change_password: '修改口令',
    admin_create: '创建控制台账号',
    admin_delete: '删除控制台账号',
    admin_set_role: '修改角色',
    admin_set_status: '修改账号状态',
    admin_reset_password: '重置口令',
    user_kick: '强制用户下线',
    user_delete: '删除用户',
  };
  return map[a] || a;
}

/** 角色 → 中文。 */
export function roleName(r) {
  return { super: '超级管理员', admin: '管理员', viewer: '只读' }[r] || r;
}
