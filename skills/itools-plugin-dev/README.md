# itools-plugin-dev — iTools 插件开发 Skill

一个 Claude Code / Claude Agent **Skill**：装上后，你的 AI 就能按 iTools 的插件规范，帮你一次生成可加载的 iTools 插件。

## 这是什么

iTools 是 macOS 风格的效率启动器，插件是「关键词触发 → 弹出 HTML 面板」的轻量小工具。这个 skill 把插件规范、`window.itools` API、模板和完整示例都打包好，让 AI **不需要 iTools 源码**就能正确生成插件。

## 安装

把整个 `itools-plugin-dev/` 目录放到你的 skills 目录（`SKILL.md` 是开放格式，各家客户端通用）：

| 客户端 | 个人（全局） | 随项目 |
|---|---|---|
| Claude Code | `%USERPROFILE%\.claude\skills\itools-plugin-dev\` | `.claude/skills/itools-plugin-dev/` |
| Codex CLI | `%USERPROFILE%\.codex\skills\itools-plugin-dev\` | `.codex/skills/itools-plugin-dev/` |

装好后，直接跟 AI 说「给 iTools 做个能做 XX 的插件」，它就会用这个 skill 生成。

## 可选：再接上开发者中心 MCP（推荐）

Skill 解决的是「AI 知不知道怎么写」，MCP 解决的是「AI 能不能自己跑起来看结果」。**两个是互补的**：
接上 MCP 之后，AI 能自己扫描插件、打开调试窗口、读运行日志、发布前自检、提交审核，
你不用再在中间当传声筒。

iTools 开着就自动在跑，默认 `http://127.0.0.1:7345/mcp`（只绑本机）。配置：

```bash
# Claude Code
claude mcp add --transport http itools http://127.0.0.1:7345/mcp
```

其它客户端的配置位置、以及真实端口，见「管理中心 → 开发者中心」最上面那张卡片。

> ### ⚠️ 开着代理 / VPN 会连不上，且报错很误导人
>
> 代理软件设的 `HTTP_PROXY` 环境变量会让 AI 客户端把「连 `127.0.0.1:7345`」的请求**送进代理**，
> 代理在自己那侧找不到 iTools，回一个 **502** —— 看着像 iTools 坏了，其实请求根本没到它跟前。
>
> **处方**：把 `127.0.0.1,localhost,::1` **追加**进用户级 `NO_PROXY`（大小写各设一份），重启 AI 客户端。
> 别覆盖原有的值，也别图省事设成 `*`（那等于全局关代理）。
> 完整诊断步骤见 `references/mcp-dev-center.md` 第二节。

## 用法示例

> 「用 iTools 插件 skill，帮我做一个 Base64 编解码插件」
> 「给 iTools 写个颜色格式转换的插件（HEX/RGB/HSL 互转）」
> 「做个 iTools 插件：输入时间戳转成日期」

AI 会生成一个插件目录（`plugin.json` + `index.html` + 可选 `logo.png`）。把它放进 iTools 的插件目录（`%LOCALAPPDATA%\iTools\plugins\` 或项目 `plugins/`），在 iTools **托盘 →「重新加载插件」**，就能在主搜索栏用了。

## 目录

```
itools-plugin-dev/
├── SKILL.md                      # 给 AI 读的主指南（规范摘要 + 铁律 + 步骤 + 最小例子）
├── README.md                     # 本文件（给人看的安装说明）
├── references/
│   ├── mcp-dev-center.md         # 开发者中心 MCP：接入 / 工具清单 / 调试闭环 / 代理排错
│   ├── plugin-spec.md            # plugin.json 完整字段 / cmd 类型 / 权限 / 加载
│   ├── plugin-settings-spec.md   # 插件设置 settings.json 规范
│   └── window-itools-api.md      # window.itools 每个方法的签名与用法
└── assets/
    ├── itools.d.ts               # TypeScript 类型定义（AI 生成时的契约参照）
    ├── templates/minimal/        # 最小骨架（plugin.json + index.html）
    └── examples/                 # 完整可加载示例：base64 / json-format / word-count(text触发)
```

## 分发给别人

直接把 `itools-plugin-dev/` 目录打包（zip）发给对方，对方放进自己的 `~/.claude/skills/` 即可；也可通过 Skill Hub 分享。
