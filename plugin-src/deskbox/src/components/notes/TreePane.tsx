/** 左侧栏：新建按钮 + 「笔记库」分区标题 + 笔记列表（有搜索词时切换为搜索结果）。右缘可拖动调宽。 */
import { useEffect, type MouseEvent } from "react";
import { useNotesStore } from "../../state/notesStore";
import { useUiStore } from "../../state/uiStore";
import { store, KEY } from "../../services/store";
import { NotesList } from "./NotesList";
import { SearchResults } from "./SearchResults";
import styles from "./TreePane.module.css";

const MIN_W = 180;
const MAX_W = 480;

export function TreePane() {
  const query = useUiStore((s) => s.query).trim();
  const newNode = useNotesStore((s) => s.newNode);
  const noteCount = useNotesStore((s) => s.tree.filter((n) => n.type === "note").length);
  const width = useUiStore((s) => s.sidebarWidth);
  const setWidth = useUiStore((s) => s.setSidebarWidth);

  // 载入持久化宽度（越界值忽略，回落默认 240）。
  useEffect(() => {
    void store.get<number>(KEY.sidebarW).then((w) => {
      if (typeof w === "number" && w >= MIN_W && w <= MAX_W) setWidth(w);
    });
  }, [setWidth]);

  const onResizeDown = (e: MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = useUiStore.getState().sidebarWidth;
    const move = (ev: globalThis.MouseEvent) => {
      setWidth(Math.min(MAX_W, Math.max(MIN_W, startW + (ev.clientX - startX))));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void store.set(KEY.sidebarW, useUiStore.getState().sidebarWidth);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  return (
    <aside className={styles.pane} style={{ width }}>
      <div className={styles.actions}>
        <button className={styles.newNote} onClick={() => void newNode("note")}>
          ＋ 笔记
        </button>
        <button className={styles.newFolder} onClick={() => void newNode("folder")}>
          ＋ 文件夹
        </button>
      </div>
      <div className={styles.divider} />
      <div className={styles.sectionRow}>
        <span className={styles.sectionTitle}>{query ? "搜索结果" : "笔记库"}</span>
        {!query && <span className={styles.countBadge}>{noteCount}</span>}
      </div>
      {query ? <SearchResults query={query} /> : <NotesList />}
      <div className={styles.resizer} onMouseDown={onResizeDown} title="拖动调整宽度" />
    </aside>
  );
}
