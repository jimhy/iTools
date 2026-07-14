/**
 * 通用弹窗容器状态：一次显示一个 React 节点作为弹窗内容。
 * 各功能自建对话框组件（确认 / 创建主密码 / 解锁 / 生成密码 / 设置…），用 openModal 打开。
 * 可选 width 覆盖默认卡片宽度（如密码编辑弹框需要更宽）。
 */
import type { ReactNode } from "react";
import { create } from "zustand";

interface ModalState {
  content: ReactNode | null;
  width: number | null;
  open: (content: ReactNode, width?: number) => void;
  close: () => void;
}

export const useModalStore = create<ModalState>((set) => ({
  content: null,
  width: null,
  open: (content, width) => set({ content, width: width ?? null }),
  close: () => set({ content: null, width: null }),
}));

export const openModal = (content: ReactNode, width?: number) => useModalStore.getState().open(content, width);
export const closeModal = () => useModalStore.getState().close();
