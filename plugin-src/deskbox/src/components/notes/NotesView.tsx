/** 笔记界面入口：左侧树 + 右侧编辑区。卸载/退出前 flush 草稿落盘。 */
import { useEffect } from "react";
import { useNotesStore } from "../../state/notesStore";
import { TreePane } from "./TreePane";
import { EditorPane } from "./EditorPane";
import styles from "./NotesView.module.css";

export function NotesView() {
  useEffect(() => {
    const onUnload = () => useNotesStore.getState().flush();
    window.addEventListener("beforeunload", onUnload);
    return () => {
      window.removeEventListener("beforeunload", onUnload);
      useNotesStore.getState().flush();
    };
  }, []);

  return (
    <div className={styles.view}>
      <TreePane />
      <EditorPane />
    </div>
  );
}
