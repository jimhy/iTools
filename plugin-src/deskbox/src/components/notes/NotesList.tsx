/**
 * 侧栏笔记列表：根级节点按 order 渲染（文件夹 → 分组，散落笔记 → 卡片）。
 * 列表空白区作为「移到根级」的放置区（把文件夹里的笔记拖出来）。
 */
import { useRef, useState } from "react";
import type { DragEvent } from "react";
import { useNotesStore } from "../../state/notesStore";
import { FolderGroup } from "./FolderGroup";
import { NoteCard } from "./NoteCard";
import type { DropMark } from "./dnd";
import styles from "./TreePane.module.css";

export function NotesList() {
  const tree = useNotesStore((s) => s.tree);
  const moveToRoot = useNotesStore((s) => s.moveToRoot);

  const dragId = useRef<string | null>(null);
  const [drop, setDrop] = useState<DropMark>(null);

  const byOrder = (a: { order: number }, b: { order: number }) => a.order - b.order;
  const roots = tree.filter((n) => n.parentId === null).sort(byOrder);
  const notesOf = (fid: string) => tree.filter((n) => n.parentId === fid && n.type === "note").sort(byOrder);

  if (roots.length === 0) {
    return <div className={styles.hint}>还没有笔记，点上方按钮新建</div>;
  }

  return (
    <div
      className={styles.list}
      onDragOver={(e: DragEvent) => e.preventDefault()}
      onDrop={(e: DragEvent) => {
        // 具体节点上的放置由子组件处理（会 stopPropagation）；落到空白 → 移到根级。
        e.preventDefault();
        const id = e.dataTransfer.getData("text/plain") || dragId.current;
        setDrop(null);
        dragId.current = null;
        if (id) moveToRoot(id);
      }}
    >
      {roots.map((n) =>
        n.type === "folder" ? (
          <FolderGroup key={n.id} folder={n} notes={notesOf(n.id)} dragId={dragId} drop={drop} setDrop={setDrop} />
        ) : (
          <NoteCard key={n.id} node={n} dragId={dragId} drop={drop} setDrop={setDrop} />
        ),
      )}
    </div>
  );
}
