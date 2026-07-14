/** 图片预览灯箱状态：全局单例，任意处（含编辑器 NodeView）调 openImageViewer(url) 打开。 */
import { create } from "zustand";

interface ImageViewerState {
  url: string | null;
  open: (url: string) => void;
  close: () => void;
}

export const useImageViewerStore = create<ImageViewerState>((set) => ({
  url: null,
  open: (url) => set({ url }),
  close: () => set({ url: null }),
}));

export const openImageViewer = (url: string) => useImageViewerStore.getState().open(url);
