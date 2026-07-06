//! 插件设置表单：把 schema 渲染成真实控件，改动即时经 plugin_settings_set 落盘。
//!
//! 诚信要点：每个控件的 onChange 都真调后端保存；保存的值与插件运行时 `itools.settings.get`
//! 读到的是同一份（后端 merged_values 保证），不存在「能改但插件读不到」的假控件。

import type { SettingsSchema, SettingsItem } from "../types";
import { h, makeSwitch, bindHotkeyRecorder } from "./ui";
import * as api from "./api";

/**
 * 渲染某插件的设置表单。
 * @param pluginName 插件 id
 * @param schema     设置 schema（规范化后 groups）
 * @param values     当前生效值（schema 默认 + 用户覆盖，用于回填）
 * @param toast      顶部轻提示
 */
export function renderSettingsForm(
  pluginName: string,
  schema: SettingsSchema,
  values: Record<string, unknown>,
  toast: (msg: string) => void,
): HTMLElement {
  const form = h("div", { class: "plugin-settings-form" });

  /** 即时保存一项；失败提示并不更新本地缓存。 */
  const save = async (key: string, value: unknown): Promise<void> => {
    try {
      await api.pluginSettingsSet(pluginName, key, value);
      values[key] = value;
    } catch (err) {
      console.error("plugin_settings_set failed", err);
      toast("保存失败");
    }
  };

  // 表单主体容器：恢复默认后就地重建，保证管理中心 UI 与后端存储一致
  const body = h("div", { class: "settings-body" });
  function rebuild(): void {
    body.innerHTML = "";
    for (const group of schema.groups) {
      const section = h("div", { class: "settings-group" });
      if (group.title) section.appendChild(h("div", { class: "settings-group-title", text: group.title }));
      if (group.description)
        section.appendChild(h("div", { class: "settings-group-desc", text: group.description }));
      for (const item of group.items) {
        section.appendChild(renderItem(item, values[item.key], save));
      }
      body.appendChild(section);
    }
  }
  rebuild();

  // 恢复默认：清空后端用户覆盖 → 本地把 values 回落 schema 默认并重建表单（管理中心即时反映）
  const resetBtn = h("button", { class: "settings-reset-btn", text: "恢复默认" });
  resetBtn.addEventListener("click", async () => {
    try {
      await api.pluginSettingsReset(pluginName);
      for (const k of Object.keys(values)) delete values[k];
      for (const group of schema.groups) {
        for (const item of group.items) {
          if (item.default !== undefined && item.default !== null) values[item.key] = item.default;
        }
      }
      rebuild();
      toast("已恢复默认");
    } catch (err) {
      console.error("plugin_settings_reset failed", err);
      toast("重置失败");
    }
  });

  form.append(body, h("div", { class: "settings-actions" }, resetBtn));
  return form;
}

/** 渲染一个设置项：label + 控件 + 说明。boolean 的开关贴右，其余控件另起一行。 */
function renderItem(
  item: SettingsItem,
  value: unknown,
  save: (key: string, value: unknown) => void,
): HTMLElement {
  const row = h("div", { class: "settings-item" });
  const head = h("div", { class: "settings-item-head" });
  head.appendChild(h("label", { class: "settings-item-label", text: item.label || item.key }));

  let body: HTMLElement | null = null;

  switch (item.type) {
    case "boolean": {
      const sw = makeSwitch(value === true, (checked) => save(item.key, checked));
      sw.classList.add("switch-sm");
      head.appendChild(sw);
      break;
    }
    case "select": {
      const sel = h("select", { class: "settings-select" });
      const opts = item.options ?? [];
      for (const opt of opts) {
        sel.appendChild(h("option", { value: String(opt.value) }, opt.label || String(opt.value)));
      }
      sel.value = String(value ?? item.default ?? (opts[0]?.value ?? ""));
      sel.addEventListener("change", () => {
        const chosen = opts.find((o) => String(o.value) === sel.value);
        save(item.key, chosen ? chosen.value : sel.value);
      });
      body = wrapControl(sel);
      break;
    }
    case "number": {
      const input = h("input", { type: "number", class: "settings-input" });
      if (item.min != null) input.min = String(item.min);
      if (item.max != null) input.max = String(item.max);
      if (item.step != null) input.step = String(item.step);
      if (item.placeholder) input.placeholder = item.placeholder;
      input.value = value == null ? "" : String(value);
      input.addEventListener("change", () =>
        save(item.key, input.value === "" ? null : Number(input.value)),
      );
      body = wrapControl(input);
      break;
    }
    case "color": {
      const input = h("input", { type: "color", class: "settings-color" });
      input.value = String(value ?? item.default ?? "#000000");
      input.addEventListener("change", () => save(item.key, input.value));
      body = wrapControl(input);
      break;
    }
    case "hotkey": {
      const input = h("input", { type: "text", class: "settings-input settings-hotkey" });
      let cur = String(value ?? "");
      bindHotkeyRecorder(
        input,
        () => cur,
        (hk) => {
          cur = hk;
          save(item.key, hk);
        },
      );
      const clearBtn = h("button", { class: "settings-pick-btn", text: "清除" });
      clearBtn.addEventListener("click", () => {
        cur = "";
        input.value = "";
        save(item.key, "");
      });
      body = wrapControl(input, clearBtn);
      break;
    }
    case "path": {
      const input = h("input", { type: "text", class: "settings-input" });
      input.value = String(value ?? "");
      if (item.placeholder) input.placeholder = item.placeholder;
      input.addEventListener("change", () => save(item.key, input.value));
      const pickBtn = h("button", { class: "settings-pick-btn", text: "选择…" });
      pickBtn.addEventListener("click", async () => {
        const picked =
          item.mode === "file"
            ? (await api.pickLaunchFiles())[0]
            : await api.pickLaunchFolder();
        if (picked) {
          input.value = picked;
          save(item.key, picked);
        }
      });
      body = wrapControl(input, pickBtn);
      break;
    }
    case "textarea": {
      const ta = h("textarea", { class: "settings-textarea" });
      if (item.placeholder) ta.placeholder = item.placeholder;
      ta.value = String(value ?? "");
      ta.addEventListener("change", () => save(item.key, ta.value));
      body = wrapControl(ta);
      break;
    }
    default: {
      // text（及未知类型兜底）
      const input = h("input", { type: "text", class: "settings-input" });
      if (item.placeholder) input.placeholder = item.placeholder;
      input.value = String(value ?? "");
      input.addEventListener("change", () => save(item.key, input.value));
      body = wrapControl(input);
      break;
    }
  }

  row.appendChild(head);
  if (body) row.appendChild(body);
  if (item.description) row.appendChild(h("div", { class: "settings-item-desc", text: item.description }));
  return row;
}

/** 把控件（可带附加按钮）包进一行容器。 */
function wrapControl(...els: HTMLElement[]): HTMLElement {
  return h("div", { class: "settings-item-control" }, ...els);
}
