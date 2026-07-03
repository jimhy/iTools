# window.itools API 参考

iTools 在插件页任何脚本前注入全局 `window.itools`。**所有系统能力都经它调用**；面板拿不到 Node、`window.__TAURI__`、npm 包。除 `showToast` / `platform` 外都返回 `Promise`。类型定义见同目录 `../assets/itools.d.ts`。

## 生命周期

### `itools.onEnter(cb)`
进入插件时回调（初始化都放这）。**保证进入插件时恰好触发一次，无需 `setTimeout` / `DOMContentLoaded` 兜底**（晚注册也会补触发）。
```js
itools.onEnter((info) => {
  // info = { code, type, query }
  // code : 命中的 feature.code（区分同插件多功能）
  // type : "keyword" | "regex" | "text"（本次触发类型）
  // query: 触发时搜索框里的文本。⚠️ text/regex 触发时是「用户输入的内容」（可预填）；
  //        keyword 触发时是关键词本身（别当内容填进输入框——做预填要判 info.type）
});
```

### `itools.onExit(cb)`
页面卸载/隐藏时回调。

## 窗口

- `itools.hide(): Promise<void>` — 隐藏插件窗口（保留，下次秒开）。约定：面板监听 Esc → `hide()`。
- `itools.exit(): Promise<void>` — 关闭插件窗口。
- `itools.setHeight(px: number): Promise<void>` — 设窗口高度（宽度不变），按内容自适应高度时用。

## 剪贴板

- `itools.copyText(text: string): Promise<void>` — 写剪贴板。
- `itools.readText(): Promise<string>` — 读剪贴板文本。常在 `onEnter` 里读来预填输入。

## 文件（限插件沙盒）

`readFile`/`writeFile` 只在插件自己的沙盒目录内，**path 为相对路径**（禁绝对路径与 `..`）。适合插件持久化自己的小文件；**不能读写用户任意路径**。

- `itools.readFile(path: string): Promise<string>`
- `itools.writeFile(path: string, content: string): Promise<void>`

> 需要持久化配置/数据，优先用 `db`（更简单，KV + 自动 JSON）。

## 存储（KV，按插件隔离）

值自动 JSON 序列化/反序列化，可直接存对象/数组。

- `itools.db.get(key: string): Promise<any | null>`
- `itools.db.set(key: string, value: any): Promise<void>`
- `itools.db.remove(key: string): Promise<void>`
- `itools.db.keys(prefix?: string): Promise<string[]>`

```js
await itools.db.set("opts", { len: 16, symbol: true });
const opts = await itools.db.get("opts"); // → { len: 16, symbol: true } 或 null
```

## 系统

- `itools.openExternal(url: string): Promise<void>` — 用默认浏览器打开网址（**仅 http/https/mailto**）。
- `itools.openPath(path: string): Promise<void>` — 用默认程序打开本地路径（拒绝可执行/脚本类文件）。
- `itools.notify(body: string): Promise<void>` — 通知（当前落日志）。

## 高危（需声明 permissions + 用户授权，见 plugin-spec.md）

- `itools.runCommand(program: string, args?: string[]): Promise<void>` — 执行程序（不经 shell）。需 `"permissions":["runCommand"]` + 授权，否则报错。
- `itools.fetch(url: string, init?: { method?: string; headers?: Record<string,string>; body?: string }): Promise<{ status: number; ok: boolean; body: string }>` — 联网（原生代理，**非浏览器 fetch**，支持 **http/https**，返回文本体）。需 `"permissions":["network"]` + 用户授权。
  - **成功** resolve `{ status, ok, body }`（body 是响应文本；要 JSON 自己 `JSON.parse(r.body)`）。
  - **失败**（未授权 / 断网 / DNS / 超时）会 **reject（throw）**——必须 `try/catch`。HTTP 4xx/5xx **不 throw**，resolve 成 `{ ok:false, status }`。
```js
// 标准写法：loading / 未授权 / 错误 三态
async function load() {
  setLoading(true);
  try {
    const r = await itools.fetch("https://api.example.com/x");
    if (r.ok) render(JSON.parse(r.body));
    else showError("请求失败 HTTP " + r.status);
  } catch (e) {
    // 最常见是未授权：提示用户去 iTools「插件管理」给本插件打开 network 授权
    showError("联网失败（若首次使用，请在「插件管理」授权本插件联网）：" + e);
  } finally { setLoading(false); }
}
```

## UI / 平台

- `itools.showToast(msg: string): void` — 轻量提示（**同步，无需 await**）。
- `itools.platform` — 只读属性：`{ isWindows, isMacOS, isLinux, isDev }`。

## 惯用模式

```js
// 进入即初始化：text/regex 触发用 info.query，否则读剪贴板；生成器类直接产出不必读剪贴板
itools.onEnter(async (info) => {
  if ((info.type === "text" || info.type === "regex") && info.query) input.value = info.query;
  else { try { const t = await itools.readText(); if (t && t.trim()) input.value = t; } catch (_) {} }
  input.focus(); input.select();
});
// 复制结果 + 提示
copyBtn.onclick = async () => { await itools.copyText(out.value); itools.showToast("已复制"); };
// Esc 收起
window.addEventListener("keydown", (e) => { if (e.key === "Escape") itools.hide(); });
```
