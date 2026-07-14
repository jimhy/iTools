/**
 * 待办界面的展示元数据（纯 view 层）：分类定义（图标 + 名称）、优先级中文标签、时间/日期格式化。
 * 分类的归属判定（todoInCat）属领域逻辑，放在 todosStore；这里只管「怎么显示」。
 */
import type { JSX } from "react";
import type { Todo, TodoCat } from "../../types";

/** 优先级 → 中文短标签。 */
export const PRIO_LABEL: Record<Todo["priority"], string> = { high: "高", mid: "中", low: "低" };

const two = (x: number): string => String(x).padStart(2, "0");

/** 创建时间 → HH:mm（待办行右侧时间）。 */
export const hhmm = (ts: number): string => {
  const d = new Date(ts);
  return `${two(d.getHours())}:${two(d.getMinutes())}`;
};

/** 今天日期串，如「2026-07-08 周三」（页头副标题）。 */
export const todayStr = (): string => {
  const d = new Date();
  return `${d.getFullYear()}-${two(d.getMonth() + 1)}-${two(d.getDate())} 周${"日一二三四五六"[d.getDay()]}`;
};

/** 分类图标专属色（对齐设计稿：今天橙 / 本周蓝 / 工作紫 / 个人粉 / 已完成青）。 */
export const CAT_COLORS: Record<TodoCat, string> = {
  today: "#f0b24a",
  week: "#5b8cff",
  work: "#9d7bff",
  personal: "#ff6b9d",
  done: "#38e1c9",
};

export interface CatDef {
  key: TodoCat;
  name: string;
  icon: JSX.Element;
}

const svg = (children: JSX.Element): JSX.Element => (
  <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
    {children}
  </svg>
);

/** 五个分类：今天 / 本周 / 工作 / 个人 / 已完成（lucide 风格线性图标）。 */
export const CAT_DEFS: CatDef[] = [
  {
    key: "today",
    name: "今天",
    icon: svg(
      <>
        <circle cx="8" cy="8" r="3.2" />
        <path d="M8 1.5v1.5M8 13v1.5M1.5 8h1.5M13 8h1.5M3.6 3.6l1 1M11.4 11.4l1 1M12.4 3.6l-1 1M4.6 11.4l-1 1" />
      </>,
    ),
  },
  {
    key: "week",
    name: "本周",
    icon: svg(
      <>
        <rect x="2.5" y="3.5" width="11" height="10" rx="1.5" />
        <path d="M2.5 6.5h11M5.5 2v3M10.5 2v3" />
      </>,
    ),
  },
  {
    key: "work",
    name: "工作",
    icon: svg(
      <>
        <rect x="2.5" y="5.5" width="11" height="8" rx="1.5" />
        <path d="M6 5.5V4a1 1 0 0 1 1-1h2a1 1 0 0 1 1 1v1.5" />
      </>,
    ),
  },
  {
    key: "personal",
    name: "个人",
    icon: svg(
      <>
        <circle cx="8" cy="5.5" r="2.5" />
        <path d="M3.5 13.5c0-2.5 2-4 4.5-4s4.5 1.5 4.5 4" />
      </>,
    ),
  },
  {
    key: "done",
    name: "已完成",
    icon: svg(
      <>
        <circle cx="8" cy="8" r="6" />
        <path d="M5.5 8l1.7 1.7L11 6" />
      </>,
    ),
  },
];
