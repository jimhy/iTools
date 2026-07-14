/** 全局右键菜单状态：一次显示一个菜单（位置 + 菜单项）。任意组件用 openContextMenu 打开。 */
import { create } from "zustand";

export interface MenuItem {
  label: string;
  danger?: boolean;
  onSelect: () => void;
}

interface ContextMenuState {
  menu: { x: number; y: number; items: MenuItem[] } | null;
  open: (x: number, y: number, items: MenuItem[]) => void;
  close: () => void;
}

export const useContextMenuStore = create<ContextMenuState>((set) => ({
  menu: null,
  open: (x, y, items) => set({ menu: { x, y, items } }),
  close: () => set({ menu: null }),
}));

export const openContextMenu = (x: number, y: number, items: MenuItem[]) =>
  useContextMenuStore.getState().open(x, y, items);
