---
name: itools-plugin-dev
description: 开发 iTools 插件——iTools 是一款 macOS 风格的效率启动器，插件是「关键词触发 → 弹出 HTML 面板」的小工具。当用户想为 iTools 制作/编写/生成插件、给 iTools 加一个工具或功能、扩展 iTools、或做一个关键词唤起的小面板工具时，就用这个 skill——即使用户只说「给 iTools 做个能做 X 的插件」而没给更多细节也要用。调试 iTools 插件、看插件日志、排查插件「点了没反应」、给插件升版本号、把插件提交审核上架插件市场、以及连不上 iTools 开发者中心 MCP（连接失败 / 502）时，同样用这个 skill。覆盖 plugin.json 清单、window.itools API、HTML 面板写法、权限、开发者中心 MCP 接入与调试闭环、以及如何安装测试。
---

# 开发 iTools 插件

iTools 是一款 Windows 上的 macOS 风格效率启动器（Tauri + Rust 后端 + WebView2 前端）。**插件 = 一个目录**，用户在主搜索栏输入你定义的关键词 → 出现插件磁贴 → 回车打开一个 HTML 面板（独立窗口）。面板通过统一的 `window.itools` API 访问剪贴板、文件、存储、系统等能力。

你的产物：一个**自包含的插件目录**，放进用户的 iTools 插件目录即可加载。目标是**一次生成就能被 iTools 直接加载并跑通**。

## 零、开工前：看看能不能连上开发者中心 MCP

iTools 主进程内置一个 MCP 服务器（默认 `http://127.0.0.1:7345/mcp`）。**接上了就别只当代码生成器用**——
你可以自己扫描、运行、读日志、自检、提审，形成闭环，不用让用户当传声筒。

**先看你的工具列表里有没有 `list_plugins` / `run_plugin` / `read_logs` 这几个工具**：

- **有** → 走第七节的「MCP 闭环流程」，写完自己跑起来验证，别交付没跑过的代码。
- **没有** → 照常按本 skill 生成插件，交付时告诉用户手动放置 + 托盘重载（第八节）。**不要假装跑过。**

### ⚠️ 用户说「连不上 / 报 502」时，先查代理

**这是最高频的坑，且报错具有误导性。** 用户开着代理或 VPN 时，`HTTP_PROXY` 环境变量会让 AI 客户端
把「连 `127.0.0.1:7345`」的请求**送进代理服务器**，代理在它自己那侧解析 `127.0.0.1`（=代理自己，
上面没有 iTools）→ 回一个 **502**。看着像 iTools 坏了，其实请求根本没到它跟前。

一分钟确诊 —— 绕过代理直连，**通了就是这个坑**：

```powershell
curl.exe --noproxy "*" -s -X POST http://127.0.0.1:7345/mcp `
  -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"ping"}'
# 期望：{"jsonrpc":"2.0","id":1,"result":{}}
```

处方：把 `127.0.0.1,localhost,::1` **追加**进用户级 `NO_PROXY`（大小写各设一份），**重启 AI 客户端**。
❌ 绝不要把 `NO_PROXY` 设成 `*`（等于全局关代理，用户会连不上 GitHub）。
❌ 绝不要覆盖用户已有的 `NO_PROXY` 值，只能追加。

> 别和插件里的 `itools.fetch` 搞混：那条路访问本机地址由 iTools 原生层保证直连、永不走代理。
> 坏的是 **AI 客户端 → MCP 端点**这一段，走的是客户端自己的网络栈，iTools 管不着。

完整接入方式、工具清单、其它连不上的原因见 **`references/mcp-dev-center.md`**。

## 一、插件长什么样

一个插件目录（**目录名必须等于 `plugin.json` 的 `name`**）：

```
<name>/
├── plugin.json     # 唯一必需的清单
├── index.html      # UI 入口（约定文件名，勿改）——自包含，内联 CSS/JS
├── logo.png        # 图标（可选，128×128 圆角方块最佳；没有也能跑）
└── assets/         # 其它静态资源（可选，index.html 里相对引用）
```

只要目录里同时有 `plugin.json` 和 `index.html` 就能加载。

## 二、最小可用例子（照抄改）

**`json-escape/plugin.json`**
```json
{
  "name": "json-escape",
  "version": "1.0.0",
  "description": "JSON 字符串转义 / 反转义",
  "author": "you",
  "features": [
    { "code": "main", "explain": "JSON 转义 / 反转义", "cmds": ["json转义", "escape", "转义"] }
  ]
}
```

**`json-escape/index.html`**
```html
<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="UTF-8" />
<title>JSON 转义</title>
<style>
  * { box-sizing: border-box; margin: 0; }
  :root { --bg:#f6f7f9; --panel:#fff; --line:#e6e8ec; --text:#1d2129; --accent:#3b6cf6; }
  @media (prefers-color-scheme: dark){ :root{ --bg:#1b1c1e; --panel:#242628; --line:#34373b; --text:#e8eaed; } }
  body{ background:var(--bg); color:var(--text); font:14px system-ui,"Segoe UI","Microsoft YaHei",sans-serif;
        display:flex; flex-direction:column; gap:12px; padding:16px; height:100vh; }
  textarea{ flex:1; resize:none; background:var(--panel); color:var(--text); border:1px solid var(--line);
            border-radius:10px; padding:12px; font:13px Consolas,ui-monospace,monospace; outline:none; }
  .row{ display:flex; gap:8px; }
  button{ border:1px solid var(--line); background:var(--panel); color:var(--text); border-radius:8px;
          padding:8px 14px; cursor:pointer; }
  button.primary{ background:var(--accent); border-color:var(--accent); color:#fff; }
</style></head>
<body>
  <textarea id="io" placeholder="输入文本…"></textarea>
  <div class="row">
    <button class="primary" id="esc">转义</button>
    <button id="unesc">反转义</button>
    <button id="copy">复制</button>
  </div>
  <script>
    const io = document.getElementById("io");
    // 进入插件：自动读剪贴板预填、聚焦
    window.itools.onEnter(async () => {
      try { const t = await window.itools.readText(); if (t) io.value = t; } catch(_){}
      io.focus();
    });
    document.getElementById("esc").onclick = () => {
      io.value = JSON.stringify(io.value).slice(1, -1);
      window.itools.showToast("已转义");
    };
    document.getElementById("unesc").onclick = () => {
      try { io.value = JSON.parse('"' + io.value + '"'); window.itools.showToast("已反转义"); }
      catch(e){ window.itools.showToast("反转义失败：" + e.message); }
    };
    document.getElementById("copy").onclick = async () => {
      await window.itools.copyText(io.value); window.itools.showToast("已复制");
    };
    // 约定：Esc 收起面板
    window.addEventListener("keydown", e => { if (e.key === "Escape") window.itools.hide(); });
  </script>
</body></html>
```

这就是一个完整可加载的插件。**面板是普通网页**：内联 `<style>`/`<script>` 最省事，所有系统能力都经 `window.itools` 调用。

## 三、plugin.json 要点

必填仅 4 项：`name`（=目录名，小写字母数字连字符）、`version`、`description`、`features`（≥1）。
每个 feature：`code`（插件内唯一，进入时回传）、`cmds`（≥1 个触发方式）、`explain`（可选，作搜索标题）。

**cmds 触发方式**（详见 `references/plugin-spec.md`）——**关键字=裸字符串，text/regex=对象**：
- **关键字 = 裸字符串**：`"base64"`（支持模糊/拼音）。⚠️ 不要写成 `{"type":"keyword","label":...}` 这种对象——会被**静默忽略、搜不到**。
- **regex = 对象**：`{ "type": "regex", "match": "^https?://" }`。`match` 写正则源串，**元字符在 JSON 里要双反斜杠**：匹配 `rgb(` 写 `"^rgba?\\("`、匹配数字写 `"\\d+"`、`{3}` 直接写。
- **text = 对象**：`{ "type": "text" }`（任意非空输入都命中，低优先级）。**转换/翻译/搜索/计算类优先用它**，让用户直接输内容就唤起，输入经 `onEnter` 的 `info.query` 传入。

一个 feature 可混用，如 `"cmds": ["颜色", { "type": "text" }]`（搜"颜色"能唤起，直接输 `#8b5cf6` 也能唤起）。

**选关键字**：2-4 个有辨识度的词即可；避免 `ip`/`hex`/`rgb`/`color` 这类过短/过通用的单词——模糊匹配会让它在无关搜索里被召回、污染结果。宁可配 `{type:"text"}` 让面板自己识别输入，也别堆一串泛词。

## 四、window.itools API（面板里可用）

完整签名见 `references/window-itools-api.md` 与 `assets/itools.d.ts`。常用：

| 分类 | 方法 |
|---|---|
| 生命周期 | `onEnter(cb)`（cb 收 `{code,type,query}`，读剪贴板/初始化放这）· `onExit(cb)` |
| 窗口 | `hide()` · `exit()` · `setHeight(px)` |
| 剪贴板 | `copyText(s)` · `readText()` |
| 文件（限插件沙盒，相对路径） | `readFile(path)` · `writeFile(path, content)` |
| 存储（按插件隔离 KV，值自动 JSON，**纯本地**） | `db.get(k)` · `db.set(k,v)` · `db.remove(k)` · `db.keys(prefix?)` |
| 账号态（只读） | `account.state()`→`{loggedIn,cloudConfigured,syncEnabled}` · `account.isLoggedIn()` |
| 同步型数据（本地优先 + 可云同步） | `data.get/set/remove/keys` · `data.sync()`（未登录/未接云端时诚实返回 reason，数据留本地） |
| 设置（只读） | `settings.get(k)`（不存在返回 null）· `settings.all()` · `settings.onChange(cb)`（值由用户在管理中心配置，schema 见插件目录 `settings.json`，详见 `references/plugin-settings-spec.md`） |
| 系统 | `openExternal(url)`(仅 http/https/mailto) · `openPath(path)` · `notify(body)`(真系统通知) |
| 剪贴板图片 | `readImage()`→ArrayBuffer(PNG) · `writeImage(data)`（data 收 ArrayBuffer/Uint8Array/base64/dataURL） |
| 截屏（授权 `screen-capture`） | `captureFull(displayId?)`→ArrayBuffer · `captureRegion()`→ArrayBuffer\|null(取消) · `listDisplays()` |
| 图片输出 | `saveImage(data,name?)`→路径\|null(原生另存为) · `createPin(data,opacity?)`→pinId(贴图置顶浮窗) · `ocr(data,lang?)`→文本(离线OCR) |
| 全局热键（授权 `hotkey`） | `registerHotkey(accel,code?)` · `unregisterHotkey(accel)` · `onHotkey(cb)` |
| 录音（授权 `audio-capture`） | `startAudioRecord()` · `stopAudioRecord()`→ArrayBuffer(WAV) |
| 录屏（授权 `screen-capture`） | `startGifRecord()` · `stopGifRecord()`→ArrayBuffer(GIF) |
| 高危（需授权，见第六节） | `runCommand(program, args?)` · `fetch(url, init?)` |
| UI/平台 | `showToast(msg)`（同步）· `platform.{isWindows,isMacOS,isLinux,isDev}` |
| **执行程序拿输出**（授权 `runCommand`） | `exec(prog,args,opts?)`→`{code,stdout,stderr,truncated}` · `execStream(...)` + `execKill/execQuit`（GBK 自动解码） |
| **用户选择即授权的文件**（授权 `fs-user-scope`） | `fs.pickDir()/pickFile()`→scope · `fs.list/stat/hash/read/readChunk/write` · `fs.zipCreate/unzip` · `fs.watchStart/watchStop` · `fs.getFileIcon` |
| **系统位置 / 回收站**（授权 `fs-named-path`/`fs-trash`） | `paths.resolve(name)` · `paths.scan(name,opts?)` · `trash(paths)`（送回收站，**不提供真删**） |
| **上下文感知**（授权 `context-read`） | `context.activeWindow()` · `context.browserUrl()` · `context.folderPath()`（拿不到返回 null，必须判空） |
| **窗口管理**（授权 `window-manage`） | `win.list/focus/move/resize/setRect/minimize/maximize/restore/close/setTopmost` |
| **输入注入**（授权 `input-inject`） | `input.typeString(text)`（支持 Emoji）· `input.pasteText/pasteFile/pasteImage` · `input.keyTap` · `input.mouse*` |
| **剪贴板监听**（授权 `clipboard-watch`） | `clipboard.watchStart/watchStop/onChange` |
| **托管运行时**（授权 `runtime`） | `runtime.ensure("ffmpeg")` · `runtime.exec/execStream`（含 ffmpeg 进度解析）· `runtime.kill/quit` |
| **本地服务 / 局域网**（授权 `local-server`） | `serve.start(opts)`→`{url,port}` · `serve.stop` · `lan.announce/discover` |
| **系统信息 / 管理** | `sys.info()` · `sys.usage()` · `sys.getPath(n)` · `showItemInFolder(p)`；`proc.*`/`installedApps`/`startup.*`/`power.*`（各需对应授权） |
| **数据** | `sqlite.open/exec/query/batch/close`（按插件隔离，禁 ATTACH）· `crypto.*`（DPAPI 加密）· `attach.*`（附件，32MB） |
| **图像 / 屏幕** | `image.resize/crop/convert/compress/info` · `screen.cursorPoint/pickColorAt/toDip/toPhysical` |
| **通知 / 托盘** | `notifyShow(opts)` + `onNotifyClick/onNotifyAction` · `tray.set/remove/onClick/onMenu`（授权 `tray`） |
| **定时任务**（授权 `background`） | `schedule.add({everySecs,code})` · `schedule.remove/list` · `schedule.onFire(cb)` |
| **搜索结果注入** | `onMainPush(getList)`（feature 声明 `mainPush:true`；**宿主只等 250ms**，回调别做慢活） |
| **暴露给外部 AI** | `registerTool(name, handler)`（`plugin.json` 声明 `tools`；**必须在初始化时注册，不能写在 onEnter 里**） |
| **插件间 / 多窗口** | `redirect(label,payload?)` · `createWindow(page,opts?)` · `closeWindow(label)` |
| **动态指令** | `setFeature(f)` · `removeFeature(code)` · `getFeatures(codes?)`（运行时增删触发条目） |
| **下载** | `download(url,dest,id,onProgress?)` · `downloadCancel(id)`；`fetch(url,{responseType:"binary"})` 取二进制 |

> 截图/录制/贴图等原生能力由 iTools **原生 Rust** 实现（xcap 抓屏 / arboard 剪贴板 / WinRT OCR / cpal 录音），**不要再 shell 出去调 PowerShell**——那会被杀软按「隐藏编码 PowerShell 抓屏」的木马指纹误报（Bearfoos!ml），且慢、脆。图片经 base64 走 IPC、桥接层解回 ArrayBuffer；显示用 `URL.createObjectURL(new Blob([buf],{type:'image/png'}))`（CSP 已放行 blob:）。

除 `showToast`/`platform` 外均返回 `Promise`。

> ⚠️ **浏览器级存储（`localStorage`/`sessionStorage`/`IndexedDB`/Cache）每次进插件都会被清空**——
> 所有插件页同源，不清就等于插件之间互相能读。它们对插件而言是**会话级**的，
> 持久化一律用 `db` / `data` / `sqlite`。这是新手最容易踩的坑。

## 五、铁律（违反 = 插件跑不起来）

面板运行在 WebView2（Chromium）里，是**普通网页的安全上下文**，但**不是 Node、不能外链外网**：

1. **标准浏览器 API 随便用**：`crypto`、`TextEncoder`/`TextDecoder`、`btoa`/`atob`、`URL`、`Intl`、`Date`、`JSON`、`structuredClone`、DOM/Canvas 等 Chromium 内置的都能用。（造随机用 `crypto.getRandomValues(...)`——别用可能不可用的 `crypto.randomUUID()`。）系统能力（剪贴板/文件/存储/系统/联网）才走 `window.itools`。
2. **禁止**的只有这些：Node / `require` / `import 外部` / `fs` / `process`、`window.__TAURI__`、npm 包、**外链外部 URL**（`<script src="https://…">`、外部 CSS/字体/图片/CDN——严格 CSP 会拦，全部内联或放 `assets/` 相对引用）、**直连外网**（浏览器 `fetch`/`XHR` 访问外网被 CSP 掐断）。
3. **联网**：默认不能。需在 `plugin.json` 声明 `"permissions":["network"]` + 用户授权，然后用 `window.itools.fetch(url)`（原生代理，**不是**浏览器 fetch），支持 http/https。
4. **纯前端能算的就前端算**（编解码、格式化、进制、颜色、正则、时间、UUID、哈希等都是纯 JS，不需要任何权限，别声明）。
5. **目录名 == `plugin.json.name`**；必须有 `index.html`；**关键字用裸字符串，text/regex 用对象**（见第三节）。
6. 适配深浅色（`prefers-color-scheme`），进入时在 `onEnter` 里初始化（见下方惯用模式，**不必**总是读剪贴板）。
7. **顶层别与宿主注入的全局同名声明**。首选裸引用 `itools.xxx` 或起别名 `const api = window.itools`。背景：旧版 iTools 曾用 `Object.defineProperty(window,'itools',{configurable:false})` 注入，顶层 `const itools = …` 会让**整个 `<script>` 抛 `SyntaxError: Identifier 'itools' has already been declared`、一行都不执行**（页面渲染正常但按钮全灭，普通浏览器里测不出来）；现已改为普通属性注入，`const itools = window.itools;` 不再致命，但为兼容旧版 iTools 仍建议避开。`__TAURI_INTERNALS__` 等 Tauri 自有全局照旧**禁止**同名顶层声明。另外 `itools` 对象是 `Object.freeze` 过的，别给 `itools.xxx` 赋值（严格模式下抛 TypeError）。
8. **可配置项统一用 `settings.json` 声明，别自绘设置 UI**：插件目录放可选的 `settings.json` 声明设置项，iTools 在「插件管理 → 设置」tab 自动渲染并存值；运行时用 `itools.settings`（只读）读取（值 = schema 默认 + 用户覆盖）。声明了就要真读取生效（假控件违反诚信红线）。写法见 `references/plugin-settings-spec.md`。

## 五点五、onEnter 与面板尺寸

`onEnter` **保证进入插件时恰好触发一次**（无需 `setTimeout`/`DOMContentLoaded` 兜底）。按插件类型初始化：

```js
window.itools.onEnter(async (info) => {
  // info = { code, type, query }
  // ⚠️ query 仅在 text/regex 触发时是「用户输入的内容」；keyword 触发时 query 是关键词本身，别当内容填进去
  if ((info.type === "text" || info.type === "regex") && info.query) {
    input.value = info.query;             // text/regex 触发：用户输入直接进来
  } else {                                // keyword 触发/编解码类：读剪贴板预填
    try { const t = await window.itools.readText(); if (t) input.value = t; } catch (_) {}
  }
  input.focus();
  // 生成器类（UUID/密码/lorem）：无内容可填，直接 generate() 产出一批，不必读剪贴板
});
```

**面板尺寸**：默认窗口约 **760×560**。内容多时用可滚动容器（如 `overflow:auto` 的区域）兜住；需按内容调高度时 `window.itools.setHeight(document.body.scrollHeight)`（别硬编码魔法数）。

## 六、高危能力与授权（`runCommand` / `network`）

这些能力**默认禁用**，要在 `plugin.json` 顶层声明 `"permissions": [...]`，用户在 iTools「插件管理」页按插件授权后才可用：

- `runCommand`：执行程序（不经 shell）。声明 `"permissions":["runCommand"]`。
- `network`：联网。声明 `"permissions":["network"]`，用 `window.itools.fetch(url, {...})` → `{status, ok, body}`。
- `screen-capture`：截屏 / 录屏 GIF（`captureFull`/`captureRegion`/`listDisplays`/`startGifRecord`）。
- `hotkey`：注册全局热键（`registerHotkey`/`onHotkey`）。
- `audio-capture`：录音（`startAudioRecord`/`stopAudioRecord`）。

> 未门控（无需声明）的原生能力：`readImage`/`writeImage`（读写剪贴板图片，与 readText 同级）、`saveImage`（原生另存为对话框本身即用户授权）、`createPin`（贴图，仅显示插件自己给的图）、`ocr`（识别插件自己给的图）。

未声明/未授权时这两个调用会被拒绝——所以**只在真需要时才声明**，能纯前端做就别声明。

## 七、生成一个插件的步骤

1. 想清楚：关键词是什么、面板做什么、是否需要高危能力（多数不需要）。
2. 建目录 `<name>/`（小写连字符），写 `plugin.json`（4 必填 + features + cmds 裸字符串关键字）。
3. 写自包含的 `index.html`（内联 CSS/JS，只用 `window.itools`，深浅色适配，`onEnter` 初始化，Esc `hide()`）。
4. 对照第五节铁律逐条自检。
5.（可选）配 `logo.png`。
6. 参照 `assets/examples/` 里的完整示例（base64 / json-format）对齐风格。

**逐条核对清单**：目录名==name ✓ · 有 index.html ✓ · 关键字是裸字符串 ✓ · 只用 window.itools、无 Node/外链/外网 ✓ · 高危能力仅在需要时声明 permissions ✓ · 深浅色适配 ✓。

### 七点五、接了 MCP 就走这个闭环（写完自己跑起来验证）

工具列表里有 `list_plugins` 时，别停在「生成文件」——**跑起来看结果，再交付**：

```
list_plugins                 → 拿调试目录路径（往 fixed=true 的那个目录下建）+ 看现有插件
（用文件工具写插件目录）
rescan → list_plugins        → 确认扫到了，且 issues 里没有 level=error（有就先修）
run_plugin(id, code)         → 实际打开调试窗口跑一次
read_logs                    → 看它发出的调用与报错
   ↑ 有问题就改 → rescan → run_plugin → read_logs，直到干净
```

要提审时再往下走（**用户要求了才做，别自作主张往市场传东西**）：

```
（改 plugin.json 的 version —— iTools 不会替你升，已上线过的必须严格高于线上版本）
preflight(id)                → 自检；版本号被拦时返回的 suggestedVersion 可直接用
submit(id) → publish_status  → 提审、查结论；被驳回时里面是模型给出的逐条理由
revoke(id, reason="…")      → 把已上线的插件下架（revoked=false 恢复上架）
                              对线上真实用户生效，**用户明确要求才调**
```

几条要记住的：

- **改完文件必须 `rescan`**，否则 `list_plugins` 给你的是旧清单，白排查半天。
- **`read_logs` 是「点了没反应」的首选**——被后端拒绝的调用会带上拒绝原因。日志不落盘，重启 iTools 即清零。
- **`preflight` 通过 ≠ 会过审**。代码审核在服务端的大模型那侧，本地看不出结论，别向用户承诺能过。
- **调试环境刻意放宽了部分校验**（如 `name` ≠ 目录名只报告警）。「调试能跑」不等于「能发布」，提审前把 `issues` 清到零。
- 汇报时**如实区分**「跑过了」和「只是写完了」。没调 `run_plugin` 就别说验证过。

## 八、安装与测试（没接 MCP 时，告诉用户怎么用）

1. 把插件目录放进 iTools 的插件目录：
   - 开发/项目内：iTools 项目根的 `plugins/` 目录。
   - 安装版：`%LOCALAPPDATA%\iTools\plugins\`（Windows）。
   - 或设环境变量 `ITOOLS_PLUGINS_DIR` 指定。
2. 在 iTools **托盘图标 → 「重新加载插件」**（无需重启）。
3. 主搜索栏输入你的关键词 → 出现插件磁贴 → 回车打开面板。
4. 若声明了高危能力：到「插件管理」页把对应授权开关打开。
5. 搜不到就看 `itools.log`（搜「插件」有加载/告警日志）。**位置随构建走**：装好的正式版在 `%LOCALAPPDATA%\itools\itools.log`（安装目录通常在 Program Files，普通用户写不进去，所以**不在 exe 旁边**）；自己 `cargo run` / 打 debug 包跑的才在 exe 同目录（`src-tauri\target\debug\itools.log`）。单文件超 2 MiB 会轮转成 `itools.log.1`（只留一代），想找的那段翻不到就去 `.1` 里看。

## 八点五、复杂插件：React + Vite 脚手架（进阶）

**何时用**：功能复杂（多状态、多可复用组件，如 deskbox 的笔记树 / 待办 / 密码库）时，vanilla 单文件会失控——**严禁把 HTML+CSS+JS 全堆进一个文件**。改用 React + Vite 组件化 + 分层。简单工具（编解码 / 格式化）仍用第二节的 vanilla 单文件，更省。

**关键约束**：插件页是严格 CSP（`script-src 'self'`），**不能用 CDN**。用 Vite 把 React 打包成 bundle 放进插件目录，`index.html` 用相对路径引用（`'self'` 放行）。已验证：module script + CSS 在插件 CSP 下正常加载运行（自定义协议 `serve()` 返回 `Access-Control-Allow-Origin:*` 满足 `crossorigin` 的 CORS）。

**脚手架结构**（构建型插件的典型布局）：
```
plugin-src/<id>/            # 源码（与加载目录分离；node_modules 走全局 .gitignore）
  package.json              # react + react-dom + zustand；devDeps: vite + @vitejs/plugin-react + typescript
  vite.config.ts            # base:'./' + build.outDir 指向 ../../plugins/<id> + emptyOutDir:false + 固定 bundle 名
  tsconfig.json             # strict
  index.html                # 只有 <div id="root"> + <script type="module" src="/src/main.tsx">
  src/
    services/               # 唯一接触 window.itools / 底层的层（itools 封装、加密…）；非宿主环境降级 mock 便于浏览器 dev
    state/                  # zustand store（UI / 领域状态 + 持久化经 services）
    components/             # shared/（外壳）+ 各功能子目录；样式用 CSS Modules（组件同名 *.module.css）
    types.ts · App.tsx · main.tsx · styles/global.css（设计令牌 :root 变量）
```

**vite.config.ts 要点**：
```ts
base: "./",                                     // 插件在 itplugin://<id>/ 下，assets 必须相对路径
build: {
  outDir: resolve(__dirname, "../../plugins/<id>"),
  emptyOutDir: false,                           // 保留插件目录里的 plugin.json / logo.png / README.md
  rollupOptions: { output: { entryFileNames: "assets/<id>.js", assetFileNames: "assets/<id>.[ext]" } },
}
```

**构建**：`cd plugin-src/<id> && npm install && npm run build` → 产物落在 `plugins/<id>/`（index.html + assets/）。插件加载只读 index.html，与源码 / node_modules 无关。改代码后重新 build 即可（插件 HTML 热加载，无需重启 iTools）。

**分层规范（设计模式）**：services（底层封装，唯一碰全局 API）→ state（zustand）→ components（只做展示 + 交互）。数据 / 持久化 / 加密逻辑放 store 或 services，**别塞进组件**。TypeScript 严格、CSS Modules、每个组件独立文件。

**完整范例**：本仓库不再内置插件源码（插件在各自的仓库里独立开发）。
需要参考真实实现时，看 `plugins/` 下已构建的插件产物，或用「开发者中心 → MCP」让 AI 直接读规范生成骨架。

## 九、参考文件（按需读）

- `references/mcp-dev-center.md` — **开发者中心 MCP**：接入配置、8 个工具详解、调试闭环、**代理/VPN 连不上的完整诊断与处方**、其它连不上的原因。
- `references/plugin-spec.md` — plugin.json 完整字段、所有 cmd 类型、权限、目录约定、加载机制。
- `references/window-itools-api.md` — `window.itools` 每个方法的完整签名、参数、返回、注意事项。
- `references/plugin-settings-spec.md` — 插件设置 `settings.json` 规范（声明设置项、控件类型、`itools.settings` 只读读取）。
- `assets/itools.d.ts` — TypeScript 类型定义（生成时可当契约参照）。
- `assets/templates/minimal/` — 最小骨架（plugin.json + index.html），可复制起步。
- `assets/examples/base64/`、`assets/examples/json-format/` — 关键字触发 + 剪贴板处理的完整示例。
- `assets/examples/word-count/` — **text 触发**（关键字 + `{type:"text"}` 混用）+ `info.query` 预填的完整示例，转换/搜索类照它。
