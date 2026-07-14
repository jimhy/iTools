/** 笔记界面用到的线性图标（与 Pencil 设计稿一致的 1.5 描边风格，16 栅格）。 */
import type { JSX } from "react";

type P = { size?: number };

const svg = (size: number, children: JSX.Element) => (
  <svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
    {children}
  </svg>
);

// ---- 树 / 卡片 ----
export const IconFile = ({ size = 13 }: P) =>
  svg(size, <><path d="M3.5 1.5h6l3 3v10h-9z" /><path d="M9.5 1.5v3h3" /></>);

export const IconFolder = ({ size = 14 }: P) => svg(size, <path d="M1.5 13V3.5h4.2l1.5 2h7.3V13z" />);

export const IconFolderOpen = ({ size = 14 }: P) =>
  svg(size, <><path d="M1.5 13V3.5h4.2l1.5 2h6.3" /><path d="M3.2 13l1.6-4.5h9.7L12.9 13z" /></>);

export const IconArchive = ({ size = 14 }: P) =>
  svg(size, <><rect x="2" y="3" width="12" height="3" rx="0.8" /><path d="M3 6v7.5h10V6" /><path d="M6.5 9h3" /></>);

export const IconChevron = ({ size = 13 }: P) => svg(size, <path d="M6 3.5L10.5 8L6 12.5" />);

export const IconLock = ({ size = 13 }: P) =>
  svg(size, <><rect x="3.5" y="7" width="9" height="6.5" /><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" /></>);

export const IconUnlock = ({ size = 13 }: P) =>
  svg(size, <><rect x="3.5" y="7" width="9" height="6.5" /><path d="M5.5 7V5a2.5 2.5 0 0 1 4.6-1.3" /></>);

export const IconStar = ({ size = 15, filled = false }: P & { filled?: boolean }) => (
  <svg width={size} height={size} viewBox="0 0 16 16" fill={filled ? "currentColor" : "none"} stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
    <path d="M8 1.9l1.85 3.75 4.15.6-3 2.93.71 4.13L8 11.88l-3.71 1.95.71-4.13-3-2.93 4.15-.6z" />
  </svg>
);

export const IconShare = ({ size = 15 }: P) =>
  svg(size, <><circle cx="12" cy="3.6" r="1.9" /><circle cx="4" cy="8" r="1.9" /><circle cx="12" cy="12.4" r="1.9" /><path d="M5.7 7.1l4.6-2.6M5.7 8.9l4.6 2.6" /></>);

export const IconEllipsis = ({ size = 15 }: P) => (
  <svg width={size} height={size} viewBox="0 0 16 16" fill="currentColor" stroke="none">
    <circle cx="3.5" cy="8" r="1.15" />
    <circle cx="8" cy="8" r="1.15" />
    <circle cx="12.5" cy="8" r="1.15" />
  </svg>
);

// ---- 元信息 ----
export const IconHistory = ({ size = 12 }: P) =>
  svg(size, <><path d="M2.6 8a5.4 5.4 0 1 0 1.7-3.9" /><path d="M2.2 2.6v3h3" /><path d="M8 5v3l2 1.3" /></>);

export const IconFileText = ({ size = 12 }: P) =>
  svg(size, <><path d="M4 1.5h5l3 3v10H4z" /><path d="M9 1.5v3h3" /><path d="M6 8h4M6 10.5h4" /></>);

export const IconHash = ({ size = 12 }: P) => svg(size, <path d="M6 2.3L4.6 13.7M11.4 2.3L10 13.7M3 5.6h10M2.6 10.4h10" />);

// ---- 格式栏 ----
export const IconBold = ({ size = 15 }: P) => svg(size, <path d="M5 2.5h4a2.75 2.75 0 0 1 0 5.5H5zM5 8h4.6a2.75 2.75 0 0 1 0 5.5H5z" />);

export const IconItalic = ({ size = 15 }: P) => svg(size, <path d="M6.5 2.5h5M4.5 13.5h5M9.6 2.5L6.4 13.5" />);

export const IconUnderline = ({ size = 15 }: P) => svg(size, <><path d="M4.5 2.5v5a3.5 3.5 0 0 0 7 0v-5" /><path d="M3.5 14h9" /></>);

export const IconStrike = ({ size = 15 }: P) =>
  svg(size, <><path d="M2.6 8h10.8" /><path d="M11 5c-.4-1.1-1.4-1.9-3-1.9-1.7 0-2.9 1-2.9 2.3 0 .7.3 1.2.9 1.6M5 10.5c.3 1.3 1.4 2.2 3 2.2 2 0 3-1 3-2.4" /></>);

export const IconHeading = ({ size = 15 }: P) => svg(size, <path d="M4 2.8v10.4M11 2.8v10.4M4 8h7" />);

export const IconList = ({ size = 15 }: P) => (
  <svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
    <path d="M6 4h8M6 8h8M6 12h8" />
    <circle cx="3" cy="4" r="0.9" fill="currentColor" stroke="none" />
    <circle cx="3" cy="8" r="0.9" fill="currentColor" stroke="none" />
    <circle cx="3" cy="12" r="0.9" fill="currentColor" stroke="none" />
  </svg>
);

export const IconListOrdered = ({ size = 15 }: P) =>
  svg(size, <><path d="M6 4h8M6 8h8M6 12h8" /><path d="M2.4 2.8v2.5M1.9 8h1.4L1.9 9.7h1.4M1.9 12.3h1.3v2.2H1.9" /></>);

export const IconListChecks = ({ size = 15 }: P) => svg(size, <path d="M2.3 4l1.2 1.2L5.9 3M2.3 10.2l1.2 1.2L5.9 9M8.5 4h5.4M8.5 10.4h5.4" />);

export const IconCode = ({ size = 15 }: P) => svg(size, <path d="M6 4.5L2.5 8 6 11.5M10 4.5L13.5 8 10 11.5" />);

export const IconQuote = ({ size = 15 }: P) =>
  svg(size, <path d="M3 10c0-2.3 1.1-3.7 3.2-4M3 10v3h3.2v-3zM8.8 10c0-2.3 1.1-3.7 3.2-4M8.8 10v3H12v-3z" />);

export const IconLink = ({ size = 15 }: P) =>
  svg(size, <><path d="M6.7 9.3a2.6 2.6 0 0 0 3.7 0l2-2a2.6 2.6 0 1 0-3.7-3.7l-1 1" /><path d="M9.3 6.7a2.6 2.6 0 0 0-3.7 0l-2 2a2.6 2.6 0 1 0 3.7 3.7l1-1" /></>);

export const IconImage = ({ size = 15 }: P) =>
  svg(size, <><rect x="2.3" y="3" width="11.4" height="10" rx="1.6" /><circle cx="6" cy="6.6" r="1.1" /><path d="M3 11.2l3-2.6 2.4 2 2.1-1.7 2.5 2.1" /></>);

export const IconUndo = ({ size = 15 }: P) => svg(size, <><path d="M6 5.5h4.6a3 3 0 0 1 0 6H5" /><path d="M6 3L3.4 5.5 6 8" /></>);

export const IconRedo = ({ size = 15 }: P) => svg(size, <><path d="M10 5.5H5.4a3 3 0 0 0 0 6H11" /><path d="M10 3l2.6 2.5L10 8" /></>);

// ---- 编辑区空态 ----
export const IconNoteBig = () => (
  <svg viewBox="0 0 16 16" width="34" height="34" fill="none" stroke="currentColor" strokeWidth="1" strokeLinecap="round" strokeLinejoin="round">
    <path d="M3.5 1.5h6l3 3v10h-9z" />
    <path d="M9.5 1.5v3h3" />
    <path d="M6 8h4M6 10.5h4" />
  </svg>
);
