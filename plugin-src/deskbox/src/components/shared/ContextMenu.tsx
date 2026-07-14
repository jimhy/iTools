import { useEffect } from "react";
import { useContextMenuStore } from "../../state/contextMenuStore";
import styles from "./ContextMenu.module.css";

/** 全局右键菜单：点空白 / Esc / 失焦即关闭。菜单项由触发方经 openContextMenu 提供。 */
export function ContextMenu() {
  const menu = useContextMenuStore((s) => s.menu);
  const close = useContextMenuStore((s) => s.close);

  useEffect(() => {
    if (!menu) return;
    const onDown = () => close();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("blur", close);
    };
  }, [menu, close]);

  if (!menu) return null;
  return (
    <div
      className={styles.menu}
      style={{ left: menu.x, top: menu.y }}
      onMouseDown={(e) => e.stopPropagation()}
    >
      {menu.items.map((it, i) => (
        <button
          key={i}
          className={`${styles.item}${it.danger ? " " + styles.danger : ""}`}
          onClick={() => {
            close();
            it.onSelect();
          }}
        >
          {it.label}
        </button>
      ))}
    </div>
  );
}
