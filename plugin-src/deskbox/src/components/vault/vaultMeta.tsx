/**
 * 密码库领域元数据：图标集（分类图标 / 新增分类的图标选择器共用）+ 内置默认分类 + 头像品牌色 + 时间格式化。
 * 纯展示辅助（无副作用）。分类本身已数据化（见 vaultStore.categories），此处只提供默认种子与图标。
 */
import type { ReactNode } from "react";
import type { VaultCategory } from "../../types";

/** 图标集：key → 16×16 viewBox 内的 path。分类图标与「新增分类」图标选择器共用。 */
const ICONS: Record<string, ReactNode> = {
  all: (
    <>
      <path d="M8 2l6 3-6 3-6-3z" />
      <path d="M2 8l6 3 6-3" />
      <path d="M2 11l6 3 6-3" />
    </>
  ),
  website: (
    <>
      <circle cx="8" cy="8" r="6" />
      <path d="M2 8h12M8 2c1.8 2 1.8 10 0 12M8 2c-1.8 2-1.8 10 0 12" />
    </>
  ),
  finance: (
    <>
      <rect x="2" y="4" width="12" height="8" rx="1.5" />
      <path d="M2 7h12" />
    </>
  ),
  email: (
    <>
      <rect x="2" y="3.5" width="12" height="9" rx="1.5" />
      <path d="M2.5 4.5l5.5 4 5.5-4" />
    </>
  ),
  wifi: (
    <>
      <path d="M2 6.5a9 9 0 0 1 12 0M4 9a5.5 5.5 0 0 1 8 0M6 11.3a2.4 2.4 0 0 1 4 0" />
      <circle cx="8" cy="12.6" r=".7" fill="currentColor" stroke="none" />
    </>
  ),
  work: (
    <>
      <rect x="2.5" y="5.5" width="11" height="8" rx="1.5" />
      <path d="M6 5.5V4a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5" />
    </>
  ),
  other: (
    <>
      <circle cx="8" cy="8" r="6" />
      <path d="M8 5v3M8 10.5v.5" />
    </>
  ),
  tag: (
    <>
      <path d="M2.5 2.5h5l6 6-5 5-6-6z" />
      <circle cx="5" cy="5" r="1" fill="currentColor" stroke="none" />
    </>
  ),
  star: <path d="M8 2l1.7 3.6 4 .5-2.9 2.7.7 3.9L8 10.9 4.5 12.7l.7-3.9L2.3 6.1l4-.5z" />,
  key: (
    <>
      <circle cx="5.5" cy="6" r="2.8" />
      <path d="M7.6 7.6L13 13M11 11l1.4-1.4M12.6 12.4L14 11" />
    </>
  ),
  cloud: <path d="M4.6 12a3 3 0 0 1 .2-6 3.6 3.6 0 0 1 6.8.8A2.6 2.6 0 0 1 11.4 12z" />,
  cart: (
    <>
      <circle cx="6" cy="13" r="1" fill="currentColor" stroke="none" />
      <circle cx="12" cy="13" r="1" fill="currentColor" stroke="none" />
      <path d="M1.5 2.5h2l1.5 8h7l1.5-5.5H4" />
    </>
  ),
  game: (
    <>
      <rect x="2" y="5" width="12" height="6.5" rx="3.25" />
      <path d="M5 8h2M6 7v2" />
      <circle cx="10.5" cy="7.3" r=".7" fill="currentColor" stroke="none" />
      <circle cx="11.6" cy="9" r=".7" fill="currentColor" stroke="none" />
    </>
  ),
  heart: <path d="M8 13.5C4 10.5 2.5 8.5 2.5 6.3A2.8 2.8 0 0 1 8 5a2.8 2.8 0 0 1 5.5 1.3c0 2.2-1.5 4.2-5.5 7.2z" />,
  phone: (
    <>
      <rect x="4.5" y="1.5" width="7" height="13" rx="1.5" />
      <path d="M7 12.5h2" />
    </>
  ),
};

/** 图标选择器可选的图标 key（新增 / 编辑分类用；排除「全部」的专用图标）。 */
export const ICON_KEYS = Object.keys(ICONS).filter((k) => k !== "all");

/** 首次使用时种入的内置分类（id 沿用旧枚举值，老数据无需迁移）。均可增删改。 */
export const DEFAULT_CATS: VaultCategory[] = [
  { id: "website", name: "网站", icon: "website" },
  { id: "finance", name: "金融", icon: "finance" },
  { id: "email", name: "邮箱", icon: "email" },
  { id: "wifi", name: "WiFi", icon: "wifi" },
  { id: "work", name: "工作", icon: "work" },
  { id: "other", name: "其他", icon: "other" },
];

/** 分类 / 选择器图标（按 icon key 渲染，未知回落 other）。 */
export function CatIcon({ icon, size = 15 }: { icon: string; size?: number }): ReactNode {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {ICONS[icon] ?? ICONS.other}
    </svg>
  );
}

const BRAND_COLORS: Record<string, string> = {
  github: "#4a4f5a",
  google: "#4285F4",
  figma: "#A259FF",
  notion: "#4a4a4a",
  apple: "#555a63",
  twitter: "#1DA1F2",
  x: "#1DA1F2",
  facebook: "#1877F2",
  amazon: "#FF9900",
  netflix: "#E50914",
  spotify: "#1DB954",
  slack: "#4A154B",
  微信: "#07C160",
  支付: "#1677FF",
  淘宝: "#FF4400",
  招商: "#C7000B",
  工商: "#C7000B",
  建设: "#004EA2",
  腾讯: "#0052D9",
  阿里: "#FF6A00",
  百度: "#2932E1",
  京东: "#E1251B",
  银行: "#C7000B",
  邮箱: "#5B8CFF",
};

function avatarColor(s: string): string {
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) % 360;
  return `hsl(${h} 62% 52%)`;
}

/** 头像底色：常见品牌用其品牌色，其余按名称 hash 取色（首字母作图标，不内嵌各家 logo）。 */
export function brandColor(title: string): string {
  const k = (title || "").toLowerCase();
  for (const name in BRAND_COLORS) if (k.includes(name)) return BRAND_COLORS[name];
  return avatarColor(title || "?");
}

/** 头像首字母。 */
export function avatarInitial(title: string): string {
  return (title || "?").trim().charAt(0).toUpperCase();
}

/** 时间戳 → "YYYY-MM-DD HH:mm"。 */
export function fmt(ts: number): string {
  if (!ts) return "";
  const d = new Date(ts);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
