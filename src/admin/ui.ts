//! 管理中心通用 UI 原语：元素工厂 h、轻提示 toast、开关、快捷键格式化与录制。

/** h() 的属性表：class/text/html/dataset 特殊处理，on* 作事件监听，其余作 HTML 属性。 */
export interface Attrs {
  class?: string;
  /** 文本内容（textContent，安全） */
  text?: string;
  /** 富 HTML（innerHTML，仅用于内联可信 SVG） */
  html?: string;
  dataset?: Record<string, string>;
  title?: string;
  type?: string;
  id?: string;
  src?: string;
  alt?: string;
  value?: string;
  placeholder?: string;
  onClick?: (ev: MouseEvent) => void;
  onChange?: (ev: Event) => void;
  onInput?: (ev: Event) => void;
  onKeydown?: (ev: KeyboardEvent) => void;
  onKeyup?: (ev: KeyboardEvent) => void;
  onMousedown?: (ev: MouseEvent) => void;
  onDblclick?: (ev: MouseEvent) => void;
  onFocus?: (ev: FocusEvent) => void;
  onBlur?: (ev: FocusEvent) => void;
  /** 其余任意 HTML 属性（min/max/step/href/aria-* 等） */
  [key: string]: unknown;
}

/** 创建元素：设置属性/事件/子节点。子节点可为元素、字符串（转文本节点）或空（跳过）。 */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs?: Attrs,
  ...children: Array<Node | string | null | undefined>
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (attrs) {
    for (const [key, value] of Object.entries(attrs)) {
      if (value == null) continue;
      if (key === "class") el.className = String(value);
      else if (key === "text") el.textContent = String(value);
      else if (key === "html") el.innerHTML = String(value);
      else if (key === "dataset") Object.assign(el.dataset, value);
      else if (key.startsWith("on") && typeof value === "function") {
        el.addEventListener(key.slice(2).toLowerCase(), value as EventListener);
      } else {
        el.setAttribute(key, String(value));
      }
    }
  }
  for (const child of children) {
    if (child == null) continue;
    el.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return el;
}

// ---------- toast ----------

let toastTimer: number | undefined;

/** 顶部轻提示（复用 #toast 容器，1.8s 后淡出）。 */
export function toast(msg: string): void {
  const el = document.querySelector<HTMLElement>("#toast");
  if (!el) return;
  el.textContent = msg;
  el.classList.add("show");
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => el.classList.remove("show"), 1800);
}

// ---------- 开关 ----------

/** iOS 风格开关：返回 label.switch 元素，勾选变化时回调新状态。 */
export function makeSwitch(
  checked: boolean,
  onChange: (checked: boolean) => void,
): HTMLLabelElement {
  const input = h("input", { type: "checkbox" });
  input.checked = checked;
  input.addEventListener("change", () => onChange(input.checked));
  return h("label", { class: "switch" }, input, h("span", { class: "slider" }));
}

// ---------- 快捷键 ----------

/** 修饰键/别名 → 展示文案 */
const HOTKEY_LABELS: Record<string, string> = {
  ctrl: "Ctrl",
  control: "Ctrl",
  alt: "Alt",
  option: "Alt",
  shift: "Shift",
  win: "Win",
  meta: "Win",
  super: "Win",
  cmd: "Win",
  command: "Win",
  space: "Space",
  enter: "Enter",
  esc: "Esc",
  escape: "Esc",
};

/** 把存储用的快捷键串（"ctrl+d" / "alt+Space" / "ctrl+KeyA"）格式化成 "Ctrl + D" 展示。 */
export function formatHotkey(s: string): string {
  if (!s) return "";
  return s
    .split("+")
    .map((raw) => {
      const p = raw.trim().toLowerCase();
      if (HOTKEY_LABELS[p]) return HOTKEY_LABELS[p];
      let m = p.match(/^key([a-z])$/);
      if (m) return m[1].toUpperCase();
      m = p.match(/^digit([0-9])$/);
      if (m) return m[1];
      m = p.match(/^arrow(up|down|left|right)$/);
      if (m) return m[1].charAt(0).toUpperCase() + m[1].slice(1);
      if (/^f([1-9]|1[0-2])$/.test(p)) return p.toUpperCase();
      if (p.length === 1) return p.toUpperCase();
      return p.charAt(0).toUpperCase() + p.slice(1);
    })
    .join(" + ");
}

/** 从键盘事件提取主键 token（与 Rust 侧 parse_hotkey 接受的形式对齐）：
 *  字母→小写 a..z，数字→0..9，空格→space，功能/方向键→W3C code；纯修饰键返回 null。 */
function normalizeKey(e: KeyboardEvent): string | null {
  const code = e.code;
  let m = code.match(/^Key([A-Z])$/);
  if (m) return m[1].toLowerCase();
  m = code.match(/^Digit([0-9])$/);
  if (m) return m[1];
  if (code === "Space") return "space";
  if (/^F([1-9]|1[0-2])$/.test(code)) return code; // F1..F12
  if (/^Arrow(Up|Down|Left|Right)$/.test(code)) return code;
  if (code === "Enter") return "enter";
  // 纯修饰键或不支持的键：不作为主键
  return null;
}

/** 让一个只读输入框充当快捷键录制器。
 *  聚焦/点击进入录制态；按下「修饰键+主键」即回调 setValue 并退出；Esc 取消；失焦还原当前值。 */
export function bindHotkeyRecorder(
  input: HTMLInputElement,
  getCurrent: () => string,
  setValue: (hotkey: string) => void,
): void {
  input.readOnly = true;
  const render = (): void => {
    input.value = formatHotkey(getCurrent());
  };
  render();

  let recording = false;
  const start = (): void => {
    if (recording) return;
    recording = true;
    input.classList.add("recording");
    input.value = "按下快捷键…";
  };
  const stop = (): void => {
    recording = false;
    input.classList.remove("recording");
    render();
  };

  input.addEventListener("focus", start);
  input.addEventListener("click", start);
  input.addEventListener("blur", stop);
  input.addEventListener("keydown", (e) => {
    if (!recording) return;
    e.preventDefault();
    // 拦住冒泡：录制中的 Esc/组合键不应触发 main.ts 的 document 级 Esc 关窗监听
    e.stopPropagation();
    if (e.key === "Escape") {
      input.blur();
      return;
    }
    const key = normalizeKey(e);
    if (!key) return; // 只按下修饰键，继续等主键
    const mods: string[] = [];
    if (e.ctrlKey) mods.push("ctrl");
    if (e.altKey) mods.push("alt");
    if (e.shiftKey) mods.push("shift");
    if (e.metaKey) mods.push("win");
    if (mods.length === 0) return; // 全局热键至少要一个修饰键
    setValue([...mods, key].join("+"));
    input.blur();
  });
}
