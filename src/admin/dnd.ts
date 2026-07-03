//! 全局窗口级文件拖放。
//!
//! Tauri 会拦截浏览器原生 HTML5 拖放（dragover/drop 收不到 DataTransfer.files 的真实路径），
//! 必须走 webview 的 `onDragDropEvent`。事件坐标是物理像素，命中判断前要 ÷ devicePixelRatio。
//! 同一时刻只有一个活动拖放区（切面板时 clearDropZone）。

import { getCurrentWebview } from "@tauri-apps/api/webview";

interface Zone {
  el: HTMLElement;
  onPaths: (paths: string[]) => void;
  hoverClass: string;
}

let currentZone: Zone | null = null;
let unlisten: (() => void) | null = null;

/** 物理坐标是否落在元素可视矩形内（先 ÷dpr 转 CSS 逻辑像素再比较）。 */
function isInside(el: HTMLElement, pos: { x: number; y: number }): boolean {
  const dpr = window.devicePixelRatio || 1;
  const x = pos.x / dpr;
  const y = pos.y / dpr;
  const r = el.getBoundingClientRect();
  return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
}

/** 注册当前活动拖放区。paths = 拖入的文件/文件夹绝对路径。hoverClass 默认 "drop-hover"。 */
export function setDropZone(
  el: HTMLElement,
  onPaths: (paths: string[]) => void,
  hoverClass = "drop-hover",
): void {
  currentZone = { el, onPaths, hoverClass };
}

/** 注销当前拖放区并清除悬停高亮。 */
export function clearDropZone(): void {
  if (currentZone) {
    currentZone.el.classList.remove(currentZone.hoverClass);
    currentZone = null;
  }
}

/** 初始化窗口级拖放监听（幂等；main.ts 启动时调一次）。 */
export async function initDnd(): Promise<void> {
  if (unlisten) return;
  const webview = getCurrentWebview();
  unlisten = await webview.onDragDropEvent((event) => {
    const zone = currentZone;
    if (!zone) return;
    const p = event.payload;
    switch (p.type) {
      case "enter":
      case "over":
        zone.el.classList.toggle(zone.hoverClass, isInside(zone.el, p.position));
        break;
      case "drop":
        zone.el.classList.remove(zone.hoverClass);
        if (p.paths.length && isInside(zone.el, p.position)) {
          zone.onPaths(p.paths);
        }
        break;
      case "leave":
        zone.el.classList.remove(zone.hoverClass);
        break;
    }
  });
}
