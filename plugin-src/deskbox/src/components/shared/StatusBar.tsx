import { useNotesStore } from "../../state/notesStore";
import { useTodosStore } from "../../state/todosStore";
import { useVaultStore } from "../../state/vaultStore";
import styles from "./StatusBar.module.css";

/** 底部状态栏：实时反映笔记 / 待办 / 密码条数 + 加密标识。 */
export function StatusBar() {
  const notes = useNotesStore((s) => s.tree.filter((n) => n.type === "note").length);
  const todos = useTodosStore((s) => s.todos.length);
  const vault = useVaultStore((s) => s.vault.length);
  return (
    <footer className={styles.bar}>
      <div className={styles.left}>
        <span className={styles.dot} />
        <span>
          <b>{notes}</b> 笔记 · <b>{todos}</b> 待办 · <b>{vault}</b> 密码
        </span>
      </div>
      <div className={styles.right}>
        <svg width="12" height="12" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
          <path d="M8 1.5l5 2v4.2c0 3-2.1 5.3-5 6.3-2.9-1-5-3.3-5-6.3V3.5z" />
          <path d="M5.6 8l1.7 1.7L10.6 6.3" />
        </svg>
        AES-256 加密
      </div>
    </footer>
  );
}
