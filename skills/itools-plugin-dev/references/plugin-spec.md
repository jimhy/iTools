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

### feature 对象

| 字段 | 必填 | 说明 |
|---|---|---|
| `code` | ✅ | 插件内唯一；进入插件时经 `onEnter(info)` 的 `info.code` 回传，用于区分是哪个功能 |
| `explain` | — | 功能说明，进入搜索结果标题（缺省用 `description`） |
| `cmds` | ✅ | 触发方式数组，≥1 |

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

### 4. files / img（占位，暂不触发）

`{ "type": "files", "ext": ["png"] }` / `{ "type": "img" }` —— 当前仅解析、**暂不触发**（向前兼容占位，写了也搜不到）。

## 触发类型在 onEnter 中的回传

进入插件时 `onEnter(info)` 的 `info.type` 会是本次实际触发类型：`"keyword"` / `"regex"` / `"text"`。`info.query` 是触发时搜索框里的文本。可据此分支（如 regex 触发走不同逻辑）。

## 高危能力与授权

`runCommand`、`network` **默认禁用**，需：
1. 在 `plugin.json` 顶层声明：`"permissions": ["runCommand"]` 或 `["network"]` 或两者。
2. 用户在 iTools「插件管理」页把该插件对应的授权开关打开。

未声明或未授权时，对应 API 调用返回错误。**只在真正需要时声明**——纯前端能做的（编解码、格式化、计算等）不要声明任何权限。

- `runCommand`：`itools.runCommand(program, args?)` 执行程序（后端直接 spawn，不经 shell，无注入面）。
- `network`：联网。用 `itools.fetch(url, init?)`（原生代理，非浏览器 fetch）访问 http/https。

## 加载与热重载

- iTools **启动时**扫描插件目录一次。
- 新增/修改插件后，用户在 **托盘 →「重新加载插件」** 即时生效，无需重启。
- 插件目录定位：`ITOOLS_PLUGINS_DIR` 环境变量 > 项目内 `plugins/`（开发）> `%LOCALAPPDATA%\iTools\plugins`（安装版）。

## 安全沙盒（了解即可，影响你能做什么）

- 插件页是受限的自定义协议页：只能内联脚本/样式或引用同目录 `assets/`，**不能引外部 URL、默认不能联网**（严格 CSP）。
- `readFile`/`writeFile` 限插件自己的沙盒目录（相对路径，不能读写任意磁盘路径）。
- `db` 存储按插件隔离。
- 因此：面板要自包含，联网走 `itools.fetch` + `network` 授权，读写用户任意文件的场景当前不支持。
