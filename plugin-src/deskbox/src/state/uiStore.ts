/** 全局 UI 状态：当前分区 tab 与搜索词。组件订阅它做切换与过滤。 */
import { create } from "zustand";

export type Tab = "notes" | "todo" | "vault";

interface UiState {
  tab: Tab;
  query: string;
  /** 笔记侧栏宽度（px，可拖动调整，持久化到本地）。 */
  sidebarWidth: number;
  setTab: (tab: Tab) => void;
  setQuery: (query: string) => void;
  setSidebarWidth: (w: number) => void;
}

export const useUiStore = create<UiState>((set) => ({
  tab: "notes",
  query: "",
  sidebarWidth: 240,
  setTab: (tab) => set({ tab, query: "" }),
  setQuery: (query) => set({ query }),
  setSidebarWidth: (w) => set({ sidebarWidth: w }),
}));
