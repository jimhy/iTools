# 插件设置规范（settings.json）

让插件的可配置项**统一在 iTools「插件管理 → 点开插件 → 设置」里配置**，而不是每个插件自绘一套设置 UI、各写各的存储。开发者只需在插件目录放一个 `settings.json` 声明有哪些设置项，iTools 就会**自动渲染设置界面、保存用户改动**；插件运行时用 `itools.settings` 只读读取生效。

## 一、放哪、怎么被读

- **文件位置**：插件目录根 `<plugin>/settings.json`（与 `plugin.json`、`index.html` 同级）。
- 没有 `settings.json` 的插件，详情页「设置」tab 会诚实显示「该插件没有可配置项」——不会伪造空表单。
- **schema 只读**、随插件目录走；用户改的**值**由 iTools 存到数据区
  （`%LOCALAPPDATA%\itools\plugin-data\<插件id>\settings.json`），与插件业务 `db`（`kv.json`）分开互不干扰。

## 二、文件结构

顶层：

```json
{
  "version": 1,
  "groups": [
    { "title": "分组标题", "description": "可选说明", "items": [ /* 设置项 */ ] }
  ]
}
```

- `version`：schema 版本，当前恒为 `1`。
- `groups`：分组数组，每组一个标题 + 若干设置项，UI 按组分节渲染。
- **简写**：项目少、不需要分组时，可省略 `groups`，直接在顶层写 `items: [...]`（iTools 会归一成单个匿名分组）。

每个 `item`（设置项）字段：

| 字段 | 必填 | 说明 |
|---|---|---|
| `key` | ✅ | 存储键（插件内唯一）；运行时 `itools.settings.get(key)` 用它取值 |
| `type` | | 控件类型，缺省 `text`（见第三节） |
| `label` | | 展示标题（缺省用 key） |
| `description` | | 控件下方的灰字说明 |
| `default` | | 默认值（用户没设时生效，也是运行时读到的兜底） |
| 其余 | | 各控件类型专属字段（`options`/`min`/`max`/`step`/`mode`/`placeholder`） |

## 三、控件类型

| `type` | 渲染成 | 值类型 | 专属字段 |
|---|---|---|---|
| `text` | 单行输入框 | string | `placeholder` |
| `textarea` | 多行输入框 | string | `placeholder` |
| `number` | 数字输入框 | number | `min` / `max` / `step` |
| `boolean` | 开关 | boolean | — |
| `select` | 下拉框 | 取决于选项 value | `options`: `[{ "value": ..., "label": "..." }]` |
| `path` | 路径框 + 「选择…」按钮 | string | `mode`: `"folder"`（默认）或 `"file"` |
| `color` | 颜色选择器 | string（`#rrggbb`） | — |
| `hotkey` | 快捷键录制框 | string（如 `"ctrl+shift+a"`） | — |

> `default` 要与值类型一致：`boolean` 项写 `true`/`false`，`number` 写数字，`select` 写某个 `option.value`。

## 四、运行时读取（`itools.settings`，只读）

插件页里读用户配置的值（值 = schema 默认 + 用户覆盖，iTools 已合并好）：

```js
// 读全部：{ key: value, ... }
const cfg = await itools.settings.all();
if (cfg.instantShot) startCapture();

// 读单项（不存在返回 null）
const prefix = await itools.settings.get("filenamePrefix");

// 用户在管理中心改了本插件设置 → 实时回调（cb 收到最新全量设置对象）
itools.settings.onChange((values) => {
  /* 重新应用，如启停全局快捷键 */
});
```

> ⚠️ `itools.settings` **只读**：值只能由用户在 iTools 管理中心修改，插件没有 `set`。
> 插件想存自己的**业务状态**（如「上次选中的标签」「历史记录」），用 `itools.db`（纯本地）
> 或 `itools.data`（可云同步），**不要**塞进 settings。

## 五、完整示例（PixShot 截图插件）

`plugins/pixshot/settings.json`：

```json
{
  "version": 1,
  "groups": [
    {
      "title": "截图",
      "items": [
        { "key": "instantShot", "type": "boolean", "label": "唤起后立即截图", "default": true },
        { "key": "hotkeyEnabled", "type": "boolean", "label": "启用全局快捷键 Ctrl+Shift+A", "default": false }
      ]
    },
    {
      "title": "保存",
      "items": [
        { "key": "filenamePrefix", "type": "text", "label": "文件名前缀", "default": "PixShot_" },
        { "key": "historyLimit", "type": "number", "label": "最近截图保留数量", "default": 3, "min": 0, "max": 10, "step": 1 }
      ]
    }
  ]
}
```

`index.html` 里读取并**真生效**：

```js
let cfg = { instantShot: true, hotkeyEnabled: false, filenamePrefix: "PixShot_", historyLimit: 3 };

itools.onEnter(async () => {
  cfg = Object.assign(cfg, await itools.settings.all()); // 读 iTools 里的配置
  if (cfg.hotkeyEnabled) itools.registerHotkey("ctrl+shift+a", "shot");
  if (cfg.instantShot) startCapture();
});

// 用户在管理中心改设置 → 实时应用
itools.settings.onChange((v) => {
  const prev = cfg.hotkeyEnabled;
  cfg = Object.assign(cfg, v);
  if (cfg.hotkeyEnabled !== prev) { /* 启停快捷键 */ }
});
```

## 六、诚信要求（务必遵守）

1. **控件必须真生效**：`settings.json` 里声明的每一项，插件都要在运行时用 `itools.settings` 真读取并影响行为。声明了却不读 = 用户改了没反应 = 假控件，**违反 iTools 诚信红线**。
2. **默认值要对**：`default` 要与插件代码里的默认行为一致，避免「设置显示 A、实际按 B 跑」。
3. **只读语义**：不要试图让插件写 settings（没有 `set`）；用户配置项归用户改，业务状态归 `db`/`data`。

## 七、Checklist（生成插件时自查）

- [ ] `settings.json` 放在插件目录根，是合法 JSON
- [ ] 每个 item 有唯一 `key` + 合适的 `type` + 类型正确的 `default`
- [ ] `select` 项给了 `options`；`number` 项按需给 `min`/`max`/`step`
- [ ] index.html 在 `onEnter` 里 `itools.settings.all()` 读取并应用了**每一项**
- [ ] （可选但推荐）`itools.settings.onChange` 响应实时改动
- [ ] 业务状态用 `db`/`data`，没有混进 settings
