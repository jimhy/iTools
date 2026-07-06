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

## 账号态（只读）

查询用户的云账号登录态，用于决定「是否走云同步 / 是否引导登录」。**只暴露状态，不含用户名/token**（隐私）。

- `itools.account.state(): Promise<{ loggedIn: boolean; cloudConfigured: boolean; syncEnabled: boolean }>`
  - `loggedIn` — 是否已登录云账号。
  - `cloudConfigured` — 云端服务是否已接入（`false` = 未接入，只能本地）。
  - `syncEnabled` — 用户是否开启了「登录后自动同步」。
- `itools.account.isLoggedIn(): Promise<boolean>` — 便捷判断是否已登录。

## 同步型数据（本地优先 + 云同步）

与 `db` 一样是按插件隔离的 KV（值自动 JSON 序列化），**区别是 `data` 参与云同步**：写入**先落本地**（离线始终可用），用户已登录且云端已接入时可经 `sync()` 上行云端并回拉合并。

- `itools.data.get(key: string): Promise<any | null>`
- `itools.data.set(key: string, value: any): Promise<void>` — 先写本地、标记待同步。
- `itools.data.remove(key: string): Promise<void>`
- `itools.data.keys(prefix?: string): Promise<string[]>`
- `itools.data.sync(): Promise<{ synced: boolean; reason?: string; pushed: number; pulled: number; message?: string }>`
  - **诚实降级**：`synced=false` 时 `reason ∈ "cloud_not_configured" | "not_logged_in" | "offline" | "error"`，数据仍安全保留在本地。

```js
// 本地优先：随时读写，离线也可用
await itools.data.set("note", { text: "hello" });
// 登录后可同步；未登录/未接云端时诚实返回 reason，不谎报
const r = await itools.data.sync();
if (!r.synced) itools.showToast(r.reason === "not_logged_in" ? "登录后可云端同步" : "已保存在本地");
```

> `db` = 纯本地永久 KV；`data` = 本地优先 + 可云同步。只在本机用选 `db`，想跨设备同步选 `data`。

## 设置（settings，只读）

读取用户在 iTools「插件管理 → 点开插件 → 设置」里为本插件配置的值。设置项由插件目录根的可选文件 `settings.json` 声明（schema），iTools 据此自动渲染设置界面并保存用户改动。运行时读到的**值 = schema 默认 + 用户覆盖**（iTools 已合并好）。**只读**：值只能由用户在管理中心改，插件没有 `set`。

- `itools.settings.get(key: string): Promise<any | null>` — 读单项；`key` 是 `settings.json` 里某项的 `key`，不存在返回 `null`。
- `itools.settings.all(): Promise<Record<string, any>>` — 读全部，返回 `{ key: value, ... }`。
- `itools.settings.onChange(cb: (values: Record<string, any>) => void): void` — 用户在管理中心改了本插件设置时回调，`cb` 收到最新的**全量**设置对象；用于实时重新应用（如启停全局热键）。

```js
const cfg = await itools.settings.all();        // { instantShot: true, ... }
const prefix = await itools.settings.get("filenamePrefix"); // 不存在 → null
itools.settings.onChange((v) => { /* 重新应用最新配置 */ });
```

> ⚠️ 插件自己的**业务状态**（历史记录、上次选中项等）用 `db` / `data`，**不要**塞进 settings。
> `settings.json` 的完整写法、控件类型（text/textarea/number/boolean/select/path/color/hotkey）见 `plugin-settings-spec.md`。

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
