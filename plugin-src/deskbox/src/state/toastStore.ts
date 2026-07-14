/** 轻量 toast 提示队列。任意层调 `toast(msg)` 弹一条，1.8s 自动消失。 */
import { create } from "zustand";

interface ToastItem {
  id: number;
  msg: string;
}

interface ToastState {
  toasts: ToastItem[];
  show: (msg: string) => void;
}

let seq = 0;

export const useToastStore = create<ToastState>((set) => ({
  toasts: [],
  show: (msg) => {
    const id = ++seq;
    set((s) => ({ toasts: [...s.toasts, { id, msg }] }));
    setTimeout(() => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })), 1800);
  },
}));

/** 命令式入口，供非组件代码（hooks/services 回调）直接弹提示。 */
export const toast = (msg: string) => useToastStore.getState().show(msg);
