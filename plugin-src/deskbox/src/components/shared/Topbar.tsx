import type { JSX } from "react";
import { useUiStore, type Tab } from "../../state/uiStore";
import styles from "./Topbar.module.css";

const ICONS: Record<Tab, JSX.Element> = {
  notes: (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <path d="M3.5 1.5h6l3 3v10h-9z" />
      <path d="M9.5 1.5v3h3" />
    </svg>
  ),
  todo: (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="8" cy="8" r="6" />
      <path d="M5.5 8l1.7 1.7L11 6" />
    </svg>
  ),
  vault: (
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="5" cy="8" r="2.7" />
      <path d="M7.7 8h6.5M11.5 8v2.2M14.2 8v1.6" />
    </svg>
  ),
};

const TABS: [Tab, string][] = [
  ["notes", "笔记"],
  ["todo", "待办"],
  ["vault", "密码"],
];

export function Topbar() {
  const tab = useUiStore((s) => s.tab);
  const query = useUiStore((s) => s.query);
  const setTab = useUiStore((s) => s.setTab);
  const setQuery = useUiStore((s) => s.setQuery);

  return (
    <header className={styles.topbar}>
      <div className={styles.brand}>
        <div className={styles.logo}>DB</div>
        <span className={styles.name}>DeskBox</span>
      </div>
      <nav className={styles.tabs}>
        {TABS.map(([key, label]) => (
          <button key={key} className={`${styles.tab}${tab === key ? " " + styles.active : ""}`} onClick={() => setTab(key)}>
            {ICONS[key]}
            {label}
          </button>
        ))}
      </nav>
      <div className={styles.right}>
        <div className={styles.searchBox}>
          <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round">
            <circle cx="7" cy="7" r="4.5" />
            <path d="M10.5 10.5L14 14" />
          </svg>
          <input
            className={styles.search}
            placeholder="搜索笔记、待办、密码…"
            spellCheck={false}
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
      </div>
    </header>
  );
}
