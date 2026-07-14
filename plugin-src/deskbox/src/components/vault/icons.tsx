/** 密码库内联线性图标（复制 / 显隐 / 生成 / 打开 / 锁）。仅展示，无逻辑。 */

const base = {
  fill: "none",
  stroke: "currentColor",
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

export function CopyIcon({ size = 13 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1.5" {...base}>
      <rect x="5.5" y="5.5" width="8" height="8" rx="1" />
      <path d="M2.5 10.5v-8h8" />
    </svg>
  );
}

export function EyeIcon({ size = 13 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1.5" {...base}>
      <path d="M1.5 8c1.8-3 4-4.5 6.5-4.5S12.7 5 14.5 8c-1.8 3-4 4.5-6.5 4.5S3.3 11 1.5 8z" />
      <circle cx="8" cy="8" r="1.8" />
    </svg>
  );
}

export function GenIcon({ size = 13 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1.5" {...base}>
      <rect x="2.5" y="2.5" width="11" height="11" />
      <circle cx="5.5" cy="5.5" r="1.1" fill="currentColor" stroke="none" />
      <circle cx="8" cy="8" r="1.1" fill="currentColor" stroke="none" />
      <circle cx="10.5" cy="10.5" r="1.1" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function OpenIcon({ size = 13 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1.5" {...base}>
      <path d="M6.5 3.5H2.5v10h10V9.5" />
      <path d="M9 2.5h4.5V7" />
      <path d="M13.5 2.5L7.5 8.5" />
    </svg>
  );
}

export function LockIcon({ size = 36 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1.1" {...base}>
      <rect x="3.5" y="7" width="9" height="6.5" />
      <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
      <path d="M8 9.5v1.5" />
    </svg>
  );
}

export function VaultGlyph({ size = 16 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" strokeWidth="1" {...base}>
      <circle cx="5" cy="8" r="2.7" />
      <path d="M7.7 8h6.5" />
      <path d="M11.5 8v2.2" />
      <path d="M14.2 8v1.6" />
    </svg>
  );
}
