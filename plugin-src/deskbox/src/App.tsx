import { useEffect } from "react";
import { itools } from "./services/itools";
import { useUiStore } from "./state/uiStore";
import { useCryptoStore } from "./state/cryptoStore";
import { useNotesStore } from "./state/notesStore";
import { useTodosStore } from "./state/todosStore";
import { useVaultStore } from "./state/vaultStore";
import { Topbar } from "./components/shared/Topbar";
import { StatusBar } from "./components/shared/StatusBar";
import { Toast } from "./components/shared/Toast";
import { Modal } from "./components/shared/Modal";
import { ContextMenu } from "./components/shared/ContextMenu";
import { ImageViewer } from "./components/notes/ImageViewer";
import { NotesView } from "./components/notes/NotesView";
import { TodoView } from "./components/todo/TodoView";
import { VaultView } from "./components/vault/VaultView";

/** DeskBox 外壳：顶栏 + 当前分区视图 + 状态栏 + 全局 Toast / Modal。 */
export default function App() {
  const tab = useUiStore((s) => s.tab);

  useEffect(() => {
    itools.setHeight(660).catch(() => {});
    void useCryptoStore.getState().load();
    void useNotesStore.getState().load();
    void useTodosStore.getState().load();
    void useVaultStore.getState().load();
    // 进入插件时按触发关键词切到对应分区（笔记 / 待办 / 密码）。
    itools.onEnter((info) => {
      const t = info.code === "todo" ? "todo" : info.code === "passwords" ? "vault" : "notes";
      useUiStore.getState().setTab(t);
    });
    // 屏蔽浏览器默认右键菜单（后退/重新加载/检查等）；文本输入区保留系统右键（复制/粘贴/剪切）。
    // 自定义右键菜单的元素会 stopPropagation，不冒泡到此，故不受影响。
    const onCtx = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (t.closest("input, textarea, [contenteditable=true]")) return;
      e.preventDefault();
    };
    window.addEventListener("contextmenu", onCtx);
    return () => window.removeEventListener("contextmenu", onCtx);
  }, []);

  return (
    <div className="app">
      <Topbar />
      <main className="views">
        {tab === "notes" && <NotesView />}
        {tab === "todo" && <TodoView />}
        {tab === "vault" && <VaultView />}
      </main>
      <StatusBar />
      <Toast />
      <Modal />
      <ContextMenu />
      <ImageViewer />
    </div>
  );
}
