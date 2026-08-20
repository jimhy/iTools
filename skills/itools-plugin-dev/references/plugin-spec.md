# iTools 插件规范（完整）

## 目录结构

```
<name>/
├── plugin.json     # 唯一必需的清单
├── index.html      # UI 入口（约定文件名，勿改；缺它则该目录不被视为插件）
├── logo.png        # 图标（约定名，可选）；也可用 plugin.json 的 icon 字段指定其它文件名
├── settings.json   # 插件设置声明（可选）；有它则「插件管理 → 设置」tab 自动渲染配置界面
└── assets/         # 静态资源（可选，index.html 里相对路径引用）
```

- **目录名必须等于 `plugin.json` 的 `name`**（加载器按此校验；不一致会被跳过并告警）。
- 只要目录含 `plugin.json` + `index.html` 就会被加载。
- `settings.json` 是**可选**文件：声明本插件有哪些可配置项，iTools 自动渲染设置界面并存值，插件运行时用 `itools.settings`（只读）读取。写法详见 `plugin-settings-spec.md`。
- 坏插件（清单解析失败 / 缺文件 / 无有效触发）只告警跳过，不影响其它插件。

## plugin.json 字段

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | ✅ | 唯一 id，小写字母数字连字符（如 `json-format`）；**必须等于目录名** |
| `version` | ✅ | 语义化版本，如 `1.0.0` |
| `description` | ✅ | 一句话描述，进入搜索结果副标题 |
| `features` | ✅ | 功能命令数组，≥1 |
| `author` | — | 作者，缺省空 |
| `icon` | — | 图标文件名（相对插件目录），缺省 `logo.png` |
| `permissions` | — | 声明所需高危能力数组，如 `["runCommand","network"]`；用户按插件授权后才可用 |
| `tools` | — | 暴露给**外部 AI**（Claude Code / Cursor 经 MCP 连入）的工具声明：`{ "工具名": { "description": "...", "inputSchema": {...} } }`。还需插件页调 `itools.registerTool` 才算就绪 |
| `background` | — | `true` 表示本插件**支持后台常驻**（随 iTools 启动）。声明后用户才能在「插件管理 → 设置」里看到「随 iTools 启动」开关；不声明则没有该开关。详见下节 |

### feature 对象

| 字段 | 必填 | 说明 |
|---|---|---|
| `code` | ✅ | 插件内唯一；进入插件时经 `onEnter(info)` 的 `info.code` 回传，用于区分是哪个功能 |
| `explain` | — | 功能说明，进入搜索结果标题（缺省用 `description`） |
| `cmds` | ✅ | 触发方式数组，≥1 |
| `mainPush` | — | `true` 表示本 feature 参与**搜索结果注入**（用户边打字，插件的结果直接进主搜索列表）。声明后插件会被**自动后台常驻**，不需要用户开自启动开关 |

## cmds 触发方式

`cmds` 数组的每个元素是下列之一：

### 1. 关键字（最常用）

**裸字符串**：`"base64"`、`"编码"`。搜索框输入该词（支持模糊匹配、拼音由 iTools 处理）即命中。

> ⚠️ **关键字只能写裸字符串**。写成 `{ "type": "keyword", "label": "base64" }` 这种对象形式**不受支持、会被静默忽略**（该 feature 搜不到，只在日志里告警）。

### 2. regex（正则）

```json
{ "type": "regex", "match": "^https?://" }
```

输入匹配该正则即命中。`match` 写正则**源串**，不要 `/.../ ` 包裹。命中优先级高于关键字。

⚠️ **元字符在 JSON 字符串里要双反斜杠**（JSON 先吃一层反斜杠）：
- 匹配 `rgb(` / `rgba(`：`"^rgba?\\("`
- 匹配数字：`"\\d+"`、匹配单词边界 `"\\bfoo\\b"`
- 十六进制颜色：`"^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$"`（`{3}`/`[]` 不用转义，只有 `\` 要）

### 3. text（任意输入）

```json
{ "type": "text" }
```

任意非空输入都命中该插件（排在关键字/应用之后，低优先级，不喧宾夺主）。进入时用户输入经 `onEnter` 的 `info.query` 传入——**翻译 / 搜索 / 计算 / 转换类**插件用（用户直接输内容就唤起）。

一个 feature 可混用多种，如 `["翻译", { "type": "text" }]`。

### 4. window（上下文门控）

```json
{ "type": "window", "app": "chrome.exe", "title": "^GitHub", "class": "Chrome_WidgetWin_1" }
```

**只在匹配的软件处于前台时，这个 feature 才出现在搜索结果里。** 用来做上下文感知的工具：
「在 Chrome 里唤起才出现『下载此页视频』」「在资源管理器里唤起才出现『清理此目录』」。

- `app` — 进程可执行文件名，大小写不敏感精确匹配
- `title` — 窗口标题的**正则**（`regex` 语法，JSON 里注意双反斜杠）
- `class` — 窗口类名，大小写不敏感精确匹配
- 三个都可选，但**不能全省**（全空的 window 条件会被忽略并告警）

⚠️ **window 是门控，不是独立触发方式**：它决定 feature 要不要出现，出现之后仍按同一 feature 里的
关键字 / regex / text 匹配用户输入。所以**必须搭配至少一种其它触发方式**——只写 window 的 feature
会被跳过并告警。

理由：若允许「只有 window」也能命中，它就得对任意输入都出现，等于一个只在特定软件下生效的
text 触发，会严重喧宾夺主；而真实需求本来就带关键字。

判定用的是**你唤起 iTools 之前**的那个前台窗口（iTools 一唤起自己就成了前台）。

### 5. files / img（拖放触发）

```json
{ "type": "files", "ext": ["apk", "zip"] }
{ "type": "img" }
```

**把文件拖进 iTools 主面板**即命中：`files` 按扩展名白名单（`ext` 不写表示不限类型），`img` 只收图片
（png/jpg/jpeg/gif/bmp/webp/ico/tiff）。

- 判定**从严**：拖入的**所有**文件都要满足扩展名要求才命中。只要有一个不满足就不出现——
  否则插件按声明以为只会拿到 png，结果混进来一个 exe，要么报错要么误处理。
- 命中后 `onEnter` 的 `info.type` 是 `"files"`，`info.files` 是**真实绝对路径数组**。
  （网页里的 File 对象拿不到路径，这正是这类插件过去做不了的根本原因；拖放是在原生层接住的。）

## 触发类型在 onEnter 中的回传

进入插件时 `onEnter(info)` 的 `info.type` 是本次实际触发类型，取值：

| type | 含义 |
|---|---|
| `keyword` / `regex` / `text` | 用户在搜索框输入命中 |
| `files` | 用户把文件拖进主面板，`info.files` 是真实绝对路径数组 |
| `background` | 随 iTools 启动的后台加载（窗口隐藏，只该做注册，见「后台常驻」） |
| `detached` | 插件自己用 `itools.createWindow` 开的独立窗口（**不叫 `window`**——那个名字已被 cmds 的窗口门控占用，两者含义完全不同） |

`info.query` 是触发时搜索框里的文本。可据此分支（如 regex 触发走不同逻辑）。

## 高危能力与授权

当前可声明的权限：

| 权限 | 能力 |
|---|---|
| `runCommand` | 执行外部程序（`exec` / `execStream`） |
| `network` | 联网（`fetch` / `download`） |
| `screen-capture` | 屏幕截图、录屏、屏幕取色 |
| `audio-capture` | 录音、系统内录（`itools.record.*`） |
| `camera` | 摄像头取帧与预览流（`itools.camera.*`） |
| `hotkey` | 全局热键 |
| `fs-user-scope` | 读写**用户亲自选中**的文件夹/文件（`itools.fs.*`） |
| `fs-named-path` | 访问系统命名位置（临时目录 / 下载 / 浏览器缓存等，`itools.paths.*`） |
| `fs-trash` | 把文件送进回收站（`itools.trash`） |
| `context-read` | 读取当前活动窗口信息（`itools.context.*`） |
| `input-inject` | 模拟键盘鼠标、向其它窗口输入文本（`itools.input.*`） |
| `clipboard-watch` | 监听剪贴板变化（`itools.clipboard.*`） |
| `window-manage` | 管理**其它程序**的窗口（`itools.win.*`） |
| `local-server` | 在局域网开放本机服务、局域网发现（`itools.serve.*` / `itools.lan.*`） |
| `process-manage` | 查看并结束进程（`itools.proc.*`） |
| `system-read` | 读取已安装软件等系统信息（`itools.installedApps`） |
| `system-manage` | 修改启动项、锁屏/关机/重启（`itools.startup.*` / `itools.power.*`） |
| `runtime` | 使用 iTools 托管的外部程序 ffmpeg / adb 等（`itools.runtime.*`） |
| `background` | 后台常驻与定时任务（`itools.schedule.*`） |
| `tray` | 插件自己的托盘图标（`itools.tray.*`） |

以上**默认全部禁用**，需：
1. 在 `plugin.json` 顶层声明：`"permissions": ["runCommand"]` 或 `["network"]` 或两者。
2. 用户在 iTools「插件管理」页把该插件对应的授权开关打开。

未声明或未授权时，对应 API 调用返回错误。**只在真正需要时声明**——纯前端能做的（编解码、格式化、计算等）不要声明任何权限。

分级理解（申请时请按最小必要选）：

- **只读类**（`context-read` / `system-read` / `clipboard-watch`）：看得到，改不了。
- **用户点头类**（`fs-user-scope`）：范围由用户在对话框里亲自选，插件碰不到没选过的地方。
- **改变系统状态类**（`runCommand` / `input-inject` / `process-manage` / `system-manage` / `fs-named-path` / `fs-trash`）：
  用户会在授权时看到明确的中文说明。**`runCommand` 尤其要慎重**——它能执行任意程序，
  一旦授权，本文档后面讲的各种隔离在文件层面就都不成立了（`cmd /c type 别的插件的数据文件` 就能读走）。
  能用 `runtime`（只能跑宿主校验过的白名单程序）解决的，不要申请 `runCommand`。
- **对外开放类**（`local-server`）：会在局域网暴露服务，默认强制带访问令牌。

## 后台常驻（随 iTools 启动）

截图、剪贴板监听这类插件需要**在用户还没唤起过它时就已经在跑**（否则全局热键根本没注册上）。
为此在 `plugin.json` 顶层声明：

```json
{ "background": true }
```

声明后，用户可在「插件管理 → 点开插件 → 设置 → 随 iTools 启动」打开开关。开启后 iTools 启动时
会在一个**隐藏窗口**里加载这个插件页。

**插件要做的适配**（不做就会出问题）：

```js
itools.onEnter(async (info) => {
  if (info.type === "background") {
    // 后台启动：窗口是隐藏的，只做注册，别弹界面、别读剪贴板
    itools.registerHotkey("ctrl+shift+s", "shot");
    return;
  }
  // 用户主动唤起：这时才渲染界面
  render(info);
});
```

要点：

- `info.type === "background"` 是后台启动的标志（普通触发是 `keyword` / `regex` / `text`）。
- 用户之后主动唤起时**不会新开一个实例**，而是复用这个后台实例：**页面不重载**（你在后台攒的内存状态、监听器都还在），但 **`onEnter` 会再触发一次**，这次带真实的触发类型与 query。
  → 所以 `onEnter` **必须能被多次调用而不出错**。这是后台常驻插件与普通插件最大的行为差异。
- 关掉开关 / 禁用插件 / 卸载插件，后台实例都会被关闭，它注册的全局热键随之失效。
- 没声明 `background` 的插件不会显示这个开关（后端也会拒绝设置），因为一个没为后台设计的插件在后台跑着什么也不做，那种开关是骗人的。

## 锁屏时哪些能力用不了（后台常驻插件必读）

用户锁屏后，Windows 会把交互桌面隔离掉，下列能力**一定会失败**（真机验收实测）：

| 能力 | 锁屏时的报错 |
|---|---|
| `screen.cursorPoint` / `input.mouseMove` 等 | 获取光标位置失败：拒绝访问 (0x80070005) |
| `captureFull` / `record.videoStart` | 截屏失败：句柄无效 / 创建屏幕捕获会话失败：拒绝访问 |
| `win.getForeground` / `context.activeWindow` | 此刻没有前台窗口 |
| `screen.pickColorAt` | 坐标不在任何屏幕范围内 |
| `fs.pickDir` / `pickFile` | 对话框根本弹不出来 |

这不是 iTools 的限制，是 Windows 的会话隔离。**后台常驻插件（`background` / `mainPush` / 定时任务 /
被外部 AI 调起的 MCP 工具）尤其要注意**：它们在用户不在电脑前时照样在跑，
不能假设屏幕和输入随时可用——该判空的判空、该 try/catch 的 catch，别让一次锁屏把整个插件卡死。

反过来，纯计算、文件、网络、数据库这些能力在锁屏下**完全正常**。

## 调试环境的能力差异（开发者中心里跑不通的几项）

大部分能力在开发者中心的调试窗口里与正式环境**完全一致**。但下面这几项**在调试会话里被明确拒绝**，
调用会返回中文错误（不是静默失败）：

| 能力 | 为什么调试时不给 |
|---|---|
| 动态指令（`setFeature` / `removeFeature`） | 动态指令按插件 id 落进**正式插件的数据目录**。调试插件与已装的同名插件共用 id，调试时随手加的条目会污染用户真在用的那个插件 |
| 搜索结果注入（`onMainPush`） | 同理——注入的结果会混进用户的真实搜索框 |
| MCP 工具注册（`registerTool`） | 会把调试中的半成品工具暴露给外部 AI |
| 独立窗口（`createWindow`） | 窗口归属与调试窗的生命周期尚未打通 |

这几项请在**正式安装后**验证。其余能力（exec、fs、runtime、窗口管理、输入注入、录屏…）
调试环境与正式环境行为一致，可以放心在开发者中心里调。

## 加载与热重载

- iTools **启动时**扫描插件目录一次。
- 新增/修改插件后，用户在 **托盘 →「重新加载插件」** 即时生效，无需重启。
- 插件目录定位：`ITOOLS_PLUGINS_DIR` 环境变量 > 项目内 `plugins/`（开发）> `%LOCALAPPDATA%\iTools\plugins`（安装版）。

## 安全沙盒（了解即可，影响你能做什么）

- 插件页是受限的自定义协议页：只能内联脚本/样式或引用同目录 `assets/`，**不能引外部 URL、默认不能联网**（严格 CSP）。
- `readFile`/`writeFile` 限插件自己的沙盒目录（相对路径，不能读写任意磁盘路径）。
- **要读写用户的真实文件**，走 `itools.fs.*`：由**用户亲自在对话框里选中**目录/文件，插件才拿到该范围的句柄（`fs-user-scope` 授权）。插件永远只能碰用户点过头的范围，且 iTools 自身的数据目录一律拒绝——用户选中了也不行。
- `db` / `data` / `settings` / `sqlite` 与沙盒文件都**按插件隔离**，由后端按调用窗口判定身份，插件无法伪装成别的插件。
- ⚠️ **但浏览器级存储不隔离**：所有插件页同源，`localStorage` / `sessionStorage` / `IndexedDB` / Cache Storage 是共享的。因此 iTools 在**每次进入插件时会把它们全部清空**——对插件而言这些是会话级的，退出即失效。**持久化一律用 `db` / `data` / `sqlite`**，别用浏览器存储。
- 因此：面板要自包含，联网走 `itools.fetch` + `network` 授权。
