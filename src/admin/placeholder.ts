//! 「敬请期待」占位面板：我的数据 / AI Agent 连接 / 所有功能 / 插件应用市场共用。
import { h } from "./ui";

export function renderPlaceholder(
  root: HTMLElement,
  title: string,
  desc: string,
): void {
  root.appendChild(
    h(
      "div",
      { class: "placeholder" },
      h("div", { class: "ph-badge", text: "TODO" }),
      h("div", { class: "ph-title", text: title }),
      h("div", { class: "ph-desc", text: desc }),
    ),
  );
}
