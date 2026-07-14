/** 图片预览灯箱：滚轮缩放、拖动平移、双击放大/复位、Esc/点遮罩/✕ 关闭。 */
import { useEffect, useState, type MouseEvent, type WheelEvent } from "react";
import { useImageViewerStore } from "../../state/imageViewerStore";
import styles from "./ImageViewer.module.css";

export function ImageViewer() {
  const url = useImageViewerStore((s) => s.url);
  const close = useImageViewerStore((s) => s.close);
  const [scale, setScale] = useState(1);
  const [pos, setPos] = useState({ x: 0, y: 0 });

  useEffect(() => {
    if (!url) return;
    setScale(1);
    setPos({ x: 0, y: 0 });
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [url, close]);

  if (!url) return null;

  const zoom = (factor: number) => setScale((s) => Math.min(8, Math.max(0.2, s * factor)));
  const onWheel = (e: WheelEvent) => zoom(e.deltaY < 0 ? 1.15 : 1 / 1.15);
  const reset = () => {
    setScale(1);
    setPos({ x: 0, y: 0 });
  };

  const onMouseDown = (e: MouseEvent) => {
    e.preventDefault();
    const start = { x: e.clientX, y: e.clientY, px: pos.x, py: pos.y };
    const move = (ev: globalThis.MouseEvent) => setPos({ x: start.px + (ev.clientX - start.x), y: start.py + (ev.clientY - start.y) });
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  return (
    <div
      className={styles.backdrop}
      onWheel={onWheel}
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <div className={styles.toolbar} onMouseDown={(e) => e.stopPropagation()}>
        <button title="放大" onClick={() => zoom(1.25)}>＋</button>
        <span>{Math.round(scale * 100)}%</span>
        <button title="缩小" onClick={() => zoom(1 / 1.25)}>－</button>
        <button title="复位" onClick={reset}>复位</button>
        <button title="关闭 (Esc)" onClick={close}>✕</button>
      </div>
      <img
        className={styles.img}
        src={url}
        draggable={false}
        style={{ transform: `translate(${pos.x}px, ${pos.y}px) scale(${scale})` }}
        onMouseDown={onMouseDown}
        onDoubleClick={() => (scale === 1 ? setScale(2) : reset())}
      />
    </div>
  );
}
