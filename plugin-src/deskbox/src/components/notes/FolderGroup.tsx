/**
 * 侧栏文件夹分组：可折叠头部（折叠箭头 + 彩色文件夹图标 + 名称 + 计数）+ 其下笔记卡片。
 * 头部作为拖拽目标：拖笔记进来 = 放入本文件夹；拖文件夹进来 = 排到本文件夹之后。双击 / 右键可重命名、删除。
 */
import { useEffect, useRef, useState } from "react";
import type { DragEvent, MouseEvent } from "react";
import { useNotesStore } from "../../state/notesStore";
import { openContextMenu } from "../../state/contextMenuStore";
import type { NoteNode } from "../../types";
import { NoteCard } from "./NoteCard";
import { confirmDialog } from "./dialogs/ConfirmDialog";
import { IconChevron, IconFolder } from "./icons";
import { colorFromId } from "./util";
import type { DragRef, DropMark } from "./dnd";
import styles from "./TreePane.module.css";

interface Props {
  folder: NoteNode;
  notes: NoteNode[];
  dragId: DragRef;
  drop: DropMark;
  setDrop: (d: DropMark) => void;
}

export function FolderGroup({ folder, notes, dragId, drop, setDrop }: Props) {
  const expanded = useNotesStore((s) => s.expanded.has(folder.id));
  const toggleFolder = useNotesStore((s) => s.toggleFolder);
  const rename = useNotesStore((s) => s.rename);
  const moveInto = useNotesStore((s) => s.moveInto);
  const moveAfter = useNotesStore((s) => s.moveAfter);

  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(folder.title);
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (editing) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [editing]);

  const color = colorFromId(folder.id);
  const isInto = drop?.id === folder.id && drop.kind === "into";
  const isAfter = drop?.id === folder.id && drop.kind === "after";

  const commit = () => {
    if (!editing) return;
    setEditing(false);
    rename(folder.id, draft);
  };

  const confirmAndDelete = async () => {
    const kids = useNotesStore.getState().subtreeCount(folder.id);
    const ok = await confirmDialog(
      "删除文件夹",
      kids > 0 ? `将删除该文件夹及其下 ${kids} 项，无法恢复。确定吗？` : "删除后无法恢复，确定吗？",
    );
    if (ok) await useNotesStore.getState().deleteNode(folder.id);
  };

  return (
    <div className={styles.folder}>
      <div
        className={`${styles.folderHeader} ${isInto ? styles.dropInto : ""} ${isAfter ? styles.dropAfter : ""}`}
        draggable={!editing}
        onClick={() => !editing && toggleFolder(folder.id)}
        onDoubleClick={(e) => {
          e.preventDefault();
          setDraft(folder.title);
          setEditing(true);
        }}
        onContextMenu={(e: MouseEvent) => {
          e.preventDefault();
          e.stopPropagation();
          openContextMenu(e.clientX, e.clientY, [
            {
              label: "重命名",
              onSelect: () => {
                setDraft(folder.title);
                setEditing(true);
              },
            },
            { label: "删除", danger: true, onSelect: () => void confirmAndDelete() },
          ]);
        }}
        onDragStart={(e: DragEvent) => {
          dragId.current = folder.id;
          e.dataTransfer.setData("text/plain", folder.id);
          e.dataTransfer.effectAllowed = "move";
        }}
        onDragEnd={() => {
          dragId.current = null;
          setDrop(null);
        }}
        onDragOver={(e: DragEvent) => {
          e.preventDefault();
          const id = dragId.current;
          if (!id || id === folder.id) return;
          const dragged = useNotesStore.getState().tree.find((n) => n.id === id);
          setDrop({ id: folder.id, kind: dragged?.type === "folder" ? "after" : "into" });
        }}
        onDragLeave={() => {
          if (drop?.id === folder.id) setDrop(null);
        }}
        onDrop={(e: DragEvent) => {
          e.preventDefault();
          e.stopPropagation();
          const id = e.dataTransfer.getData("text/plain") || dragId.current;
          setDrop(null);
          dragId.current = null;
          if (!id || id === folder.id) return;
          const dragged = useNotesStore.getState().tree.find((n) => n.id === id);
          if (dragged?.type === "folder") moveAfter(id, folder.id);
          else moveInto(id, folder.id);
        }}
      >
        <div className={styles.folderHl}>
          <span className={`${styles.chev} ${expanded ? styles.chevOpen : ""}`}>
            <IconChevron size={12} />
          </span>
          <span className={styles.folderIco} style={{ color }}>
            <IconFolder size={14} />
          </span>
          {editing ? (
            <input
              ref={inputRef}
              className={styles.folderInput}
              value={draft}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => setDraft(e.target.value)}
              onBlur={commit}
              onKeyDown={(e) => {
                e.stopPropagation();
                if (e.key === "Enter") {
                  e.preventDefault();
                  commit();
                } else if (e.key === "Escape") {
                  setEditing(false);
                }
              }}
            />
          ) : (
            <span className={styles.folderName}>{folder.title || "未命名文件夹"}</span>
          )}
        </div>
        <span className={styles.folderCount}>{notes.length}</span>
      </div>
      {expanded && notes.length > 0 && (
        <div className={styles.folderNotes}>
          {notes.map((n) => (
            <NoteCard key={n.id} node={n} dragId={dragId} drop={drop} setDrop={setDrop} />
          ))}
        </div>
      )}
    </div>
  );
}
