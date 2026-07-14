/**
 * 侧栏笔记卡片：左侧强调条（选中态）+ 标题 + 摘要预览 + 时间 + 彩色圆点 / 锁 / 星标。
 * 点击打开、右键菜单（收藏 / 删除）、可拖拽到其它笔记后重排或拖入文件夹。
 */
import type { DragEvent, MouseEvent } from "react";
import { useNotesStore } from "../../state/notesStore";
import { openContextMenu } from "../../state/contextMenuStore";
import type { NoteNode } from "../../types";
import { confirmDialog } from "./dialogs/ConfirmDialog";
import { IconLock, IconStar } from "./icons";
import { fmtShort, colorFromId } from "./util";
import type { DragRef, DropMark } from "./dnd";
import styles from "./TreePane.module.css";

interface Props {
  node: NoteNode;
  dragId: DragRef;
  drop: DropMark;
  setDrop: (d: DropMark) => void;
}

export function NoteCard({ node, dragId, drop, setDrop }: Props) {
  const active = useNotesStore((s) => s.curNote === node.id);
  const openNote = useNotesStore((s) => s.openNote);
  const moveAfter = useNotesStore((s) => s.moveAfter);
  const toggleStar = useNotesStore((s) => s.toggleStar);

  const dropCls = drop?.id === node.id && drop.kind === "after" ? styles.dropAfter : "";
  const preview = node.locked ? "🔒 已加密内容" : node.excerpt || "";

  const confirmAndDelete = async () => {
    const ok = await confirmDialog("删除笔记", "删除后无法恢复，确定吗？");
    if (ok) await useNotesStore.getState().deleteNode(node.id);
  };

  const onMenu = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    openContextMenu(e.clientX, e.clientY, [
      { label: node.starred ? "取消收藏" : "收藏", onSelect: () => toggleStar(node.id) },
      { label: "删除", danger: true, onSelect: () => void confirmAndDelete() },
    ]);
  };

  return (
    <div
      className={`${styles.card} ${active ? styles.cardActive : ""} ${dropCls}`}
      draggable
      onClick={() => void openNote(node.id)}
      onContextMenu={onMenu}
      onDragStart={(e: DragEvent) => {
        dragId.current = node.id;
        e.dataTransfer.setData("text/plain", node.id);
        e.dataTransfer.effectAllowed = "move";
      }}
      onDragEnd={() => {
        dragId.current = null;
        setDrop(null);
      }}
      onDragOver={(e: DragEvent) => {
        e.preventDefault();
        const id = dragId.current;
        if (!id || id === node.id) return;
        setDrop({ id: node.id, kind: "after" });
      }}
      onDragLeave={() => {
        if (drop?.id === node.id) setDrop(null);
      }}
      onDrop={(e: DragEvent) => {
        e.preventDefault();
        e.stopPropagation();
        const id = e.dataTransfer.getData("text/plain") || dragId.current;
        setDrop(null);
        dragId.current = null;
        if (!id || id === node.id) return;
        moveAfter(id, node.id);
      }}
    >
      <span className={styles.accentBar} style={active ? { background: colorFromId(node.id) } : undefined} />
      <div className={styles.cardBody}>
        <div className={styles.cardTitle}>{node.title || "未命名笔记"}</div>
        {preview && <div className={styles.cardPreview}>{preview}</div>}
        <div className={styles.cardMeta}>
          <span className={styles.cardTs}>{fmtShort(node.updatedAt)}</span>
          <span className={styles.cardMetaRight}>
            {node.starred && (
              <span className={styles.cardStar}>
                <IconStar size={11} filled />
              </span>
            )}
            <span className={styles.cardDot} style={{ background: colorFromId(node.id) }} />
            {node.locked && (
              <span className={styles.cardLock}>
                <IconLock size={11} />
              </span>
            )}
          </span>
        </div>
      </div>
    </div>
  );
}
