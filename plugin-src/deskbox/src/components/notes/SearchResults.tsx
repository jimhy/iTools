/** 笔记搜索结果：按标题 + 正文匹配（加密笔记仅在已解锁时可搜正文）。命中以卡片列出，点击打开。 */
import { useEffect, useState } from "react";
import { useNotesStore, type NoteHit } from "../../state/notesStore";
import { useCryptoStore } from "../../state/cryptoStore";
import { highlight } from "./util";
import styles from "./TreePane.module.css";

export function SearchResults({ query }: { query: string }) {
  const tree = useNotesStore((s) => s.tree);
  const searchNotes = useNotesStore((s) => s.searchNotes);
  const openNote = useNotesStore((s) => s.openNote);
  const curNote = useNotesStore((s) => s.curNote);
  // 解锁状态变化时重搜（加密正文变为可搜）。
  const masterKey = useCryptoStore((s) => s.masterKey);
  const [hits, setHits] = useState<NoteHit[] | null>(null);

  useEffect(() => {
    let alive = true;
    void searchNotes(query).then((h) => {
      if (alive) setHits(h);
    });
    return () => {
      alive = false;
    };
  }, [query, tree, masterKey, searchNotes]);

  if (hits === null) return <div className={styles.list} />;
  if (hits.length === 0) return <div className={styles.hint}>未找到匹配的笔记</div>;

  return (
    <div className={styles.list}>
      {hits.map(({ node, snippet }) => (
        <div
          key={node.id}
          className={`${styles.card} ${curNote === node.id ? styles.cardActive : ""}`}
          onClick={() => void openNote(node.id)}
        >
          <span className={styles.accentBar} />
          <div className={styles.cardBody}>
            <div
              className={styles.cardTitle}
              dangerouslySetInnerHTML={{ __html: (node.locked ? "🔒 " : "") + highlight(node.title || "未命名笔记", query) }}
            />
            {snippet && <div className={styles.cardPreview} dangerouslySetInnerHTML={{ __html: highlight(snippet, query) }} />}
          </div>
        </div>
      ))}
    </div>
  );
}
