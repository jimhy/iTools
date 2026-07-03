---
name: itools-plugin-dev
description: 开发 iTools 插件——iTools 是一款 macOS 风格的效率启动器，插件是「关键词触发 → 弹出 HTML 面板」的小工具。当用户想为 iTools 制作/编写/生成插件、给 iTools 加一个工具或功能、扩展 iTools、或做一个关键词唤起的小面板工具时，就用这个 skill——即使用户只说「给 iTools 做个能做 X 的插件」而没给更多细节也要用。覆盖 plugin.json 清单、window.itools API、HTML 面板写法、权限、以及如何安装测试。
---

# 开发 iTools 插件

iTools 是一款 Windows 上的 macOS 风格效率启动器（Tauri + Rust 后端 + WebView2 前端）。**插件 = 一个目录**，用户在主搜索栏输入你定义的关键词 → 出现插件磁贴 → 回车打开一个 HTML 面板（独立窗口）。面板通过统一的 `window.itools` API 访问剪贴板、文件、存储、系统等能力。

你的产物：一个**自包含的插件目录**，放进用户的 iTools 插件目录即可加载。目标是**一次生成就能被 iTools 直接加载并跑通**。

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
| 存储（按插件隔离 KV，值自动 JSON） | `db.get(k)` · `db.set(k,v)` · `db.remove(k)` · `db.keys(prefix?)` |
| 系统 | `openExternal(url)`(仅 http/https/mailto) · `openPath(path)` · `notify(body)` |
| 高危（需授权，见第六节） | `runCommand(program, args?)` · `fetch(url, init?)` |
| UI/平台 | `showToast(msg)`（同步）· `platform.{isWindows,isMacOS,isLinux,isDev}` |

除 `showToast`/`platform` 外均返回 `Promise`。

## 五、铁律（违反 = 插件跑不起来）

面板运行在 WebView2（Chromium）里，是**普通网页的安全上下文**，但**不是 Node、不能外链外网**：

1. **标准浏览器 API 随便用**：`crypto`、`TextEncoder`/`TextDecoder`、`btoa`/`atob`、`URL`、`Intl`、`Date`、`JSON`、`structuredClone`、DOM/Canvas 等 Chromium 内置的都能用。（造随机用 `crypto.getRandomValues(...)`——别用可能不可用的 `crypto.randomUUID()`。）系统能力（剪贴板/文件/存储/系统/联网）才走 `window.itools`。
2. **禁止**的只有这些：Node / `require` / `import 外部` / `fs` / `process`、`window.__TAURI__`、npm 包、**外链外部 URL**（`<script src="https://…">`、外部 CSS/字体/图片/CDN——严格 CSP 会拦，全部内联或放 `assets/` 相对引用）、**直连外网**（浏览器 `fetch`/`XHR` 访问外网被 CSP 掐断）。
3. **联网**：默认不能。需在 `plugin.json` 声明 `"permissions":["network"]` + 用户授权，然后用 `window.itools.fetch(url)`（原生代理，**不是**浏览器 fetch），支持 http/https。
4. **纯前端能算的就前端算**（编解码、格式化、进制、颜色、正则、时间、UUID、哈希等都是纯 JS，不需要任何权限，别声明）。
5. **目录名 == `plugin.json.name`**；必须有 `index.html`；**关键字用裸字符串，text/regex 用对象**（见第三节）。
6. 适配深浅色（`prefers-color-scheme`），进入时在 `onEnter` 里初始化（见下方惯用模式，**不必**总是读剪贴板）。

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

这两类**默认禁用**，要在 `plugin.json` 顶层声明 `"permissions": [...]`，用户在 iTools「插件管理」页按插件授权后才可用：

- `runCommand(program, args?)`：执行程序（不经 shell，program+参数数组）。声明 `"permissions":["runCommand"]`。
- `network`：联网。声明 `"permissions":["network"]`，用 `window.itools.fetch(url, {method,headers,body})` → 返回 `{status, ok, body}`（文本）。

未声明/未授权时这两个调用会被拒绝——所以**只在真需要时才声明**，能纯前端做就别声明。

## 七、生成一个插件的步骤

1. 想清楚：关键词是什么、面板做什么、是否需要高危能力（多数不需要）。
2. 建目录 `<name>/`（小写连字符），写 `plugin.json`（4 必填 + features + cmds 裸字符串关键字）。
3. 写自包含的 `index.html`（内联 CSS/JS，只用 `window.itools`，深浅色适配，`onEnter` 初始化，Esc `hide()`）。
4. 对照第五节铁律逐条自检。
5.（可选）配 `logo.png`。
6. 参照 `assets/examples/` 里的完整示例（base64 / json-format）对齐风格。

**逐条核对清单**：目录名==name ✓ · 有 index.html ✓ · 关键字是裸字符串 ✓ · 只用 window.itools、无 Node/外链/外网 ✓ · 高危能力仅在需要时声明 permissions ✓ · 深浅色适配 ✓。

## 八、安装与测试（告诉用户怎么用）

1. 把插件目录放进 iTools 的插件目录：
   - 开发/项目内：iTools 项目根的 `plugins/` 目录。
   - 安装版：`%LOCALAPPDATA%\iTools\plugins\`（Windows）。
   - 或设环境变量 `ITOOLS_PLUGINS_DIR` 指定。
2. 在 iTools **托盘图标 → 「重新加载插件」**（无需重启）。
3. 主搜索栏输入你的关键词 → 出现插件磁贴 → 回车打开面板。
4. 若声明了高危能力：到「插件管理」页把对应授权开关打开。
5. 搜不到就看 iTools exe 同目录的 `itools.log`（搜「插件」有加载/告警日志）。

## 九、参考文件（按需读）

- `references/plugin-spec.md` — plugin.json 完整字段、所有 cmd 类型、权限、目录约定、加载机制。
- `references/window-itools-api.md` — `window.itools` 每个方法的完整签名、参数、返回、注意事项。
- `assets/itools.d.ts` — TypeScript 类型定义（生成时可当契约参照）。
- `assets/templates/minimal/` — 最小骨架（plugin.json + index.html），可复制起步。
- `assets/examples/base64/`、`assets/examples/json-format/` — 关键字触发 + 剪贴板处理的完整示例。
- `assets/examples/word-count/` — **text 触发**（关键字 + `{type:"text"}` 混用）+ `info.query` 预填的完整示例，转换/搜索类照它。
