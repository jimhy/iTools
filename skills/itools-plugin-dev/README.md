# itools-plugin-dev — iTools 插件开发 Skill

一个 Claude Code / Claude Agent **Skill**：装上后，你的 AI 就能按 iTools 的插件规范，帮你一次生成可加载的 iTools 插件。

## 这是什么

iTools 是 macOS 风格的效率启动器，插件是「关键词触发 → 弹出 HTML 面板」的轻量小工具。这个 skill 把插件规范、`window.itools` API、模板和完整示例都打包好，让 AI **不需要 iTools 源码**就能正确生成插件。

## 安装

把整个 `itools-plugin-dev/` 目录放到你的 skills 目录：

- **Claude Code（个人）**：`~/.claude/skills/itools-plugin-dev/`（Windows：`%USERPROFILE%\.claude\skills\itools-plugin-dev\`）
- **随项目**：项目内 `.claude/skills/itools-plugin-dev/`

装好后，直接跟 AI 说「给 iTools 做个能做 XX 的插件」，它就会用这个 skill 生成。

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
│   ├── plugin-spec.md            # plugin.json 完整字段 / cmd 类型 / 权限 / 加载
│   └── window-itools-api.md      # window.itools 每个方法的签名与用法
└── assets/
    ├── itools.d.ts               # TypeScript 类型定义（AI 生成时的契约参照）
    ├── templates/minimal/        # 最小骨架（plugin.json + index.html）
    └── examples/                 # 完整可加载示例：base64 / json-format / word-count(text触发)
```

## 分发给别人

直接把 `itools-plugin-dev/` 目录打包（zip）发给对方，对方放进自己的 `~/.claude/skills/` 即可；也可通过 Skill Hub 分享。
