/**
 * 待办状态：todos（落盘）+ 当前分类 cat + 增删改 / 优先级循环 / 拖拽排序。
 * 分类视图：今天/本周为「按 createdAt」的虚拟视图；工作/个人按 category；已完成按 done。
 * 数据落盘统一走 store service（itools.db）；对外暴露 `todos: Todo[]` 供 StatusBar 计数。
 */
import { create } from "zustand";
import type { Todo, TodoCat } from "../types";
import { KEY, store, uid } from "../services/store";

/** 优先级循环顺序：低 → 中 → 高 → 低。 */
const PRIO_SEQ = ["low", "mid", "high"] as const;

const isSameDay = (ts: number): boolean => {
  const a = new Date(ts);
  const b = new Date();
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
};
const withinWeek = (ts: number): boolean => {
  const diff = (Date.now() - ts) / 86400000;
  return diff >= -1 && diff < 7;
};

/** 判断一条待办是否属于某分类视图（分类栏计数与主列表过滤共用）。 */
export function todoInCat(t: Todo, cat: TodoCat): boolean {
  if (cat === "done") return t.done;
  if (cat === "today") return isSameDay(t.createdAt);
  if (cat === "week") return withinWeek(t.createdAt);
  if (cat === "work") return t.category === "work";
  if (cat === "personal") return t.category === "personal";
  return true;
}

interface TodosState {
  todos: Todo[];
  /** 当前选中的分类视图。 */
  cat: TodoCat;
  /** 启动时加载持久化的待办。 */
  load: () => Promise<void>;
  setCat: (cat: TodoCat) => void;
  /** 新建：空文本忽略；在「工作/个人」分类下新建则归入该分类；插入到列表顶部。 */
  add: (text: string) => void;
  /** 切换完成态并记录/清除完成时间。 */
  toggle: (id: string) => void;
  remove: (id: string) => void;
  /** 提交内联编辑（空文本忽略）。 */
  edit: (id: string, text: string) => void;
  /** 循环切换优先级。 */
  cyclePriority: (id: string) => void;
  /** HTML5 拖拽排序：把 dragId 移动到 targetId 之前/之后并重排 order。 */
  reorder: (dragId: string, targetId: string, after: boolean) => void;
}

export const useTodosStore = create<TodosState>((set, get) => {
  /** 更新内存态并落盘（唯一写出口）。 */
  const persist = (todos: Todo[]): void => {
    set({ todos });
    void store.set(KEY.todos, todos);
  };

  return {
    todos: [],
    cat: "today",

    load: async () => {
      const todos = (await store.get<Todo[]>(KEY.todos)) ?? [];
      set({ todos });
    },

    setCat: (cat) => set({ cat }),

    add: (text) => {
      const t = text.trim();
      if (!t) return;
      const { todos, cat } = get();
      const minOrder = todos.length ? Math.min(...todos.map((x) => x.order)) : 0;
      const category = cat === "work" || cat === "personal" ? cat : null;
      const todo: Todo = {
        id: uid(),
        text: t,
        done: false,
        priority: "mid",
        category,
        order: minOrder - 1,
        createdAt: Date.now(),
        doneAt: null,
      };
      persist([...todos, todo]);
    },

    toggle: (id) => {
      persist(
        get().todos.map((t) => (t.id === id ? { ...t, done: !t.done, doneAt: t.done ? null : Date.now() } : t)),
      );
    },

    remove: (id) => persist(get().todos.filter((t) => t.id !== id)),

    edit: (id, text) => {
      const v = text.trim();
      if (!v) return;
      persist(get().todos.map((t) => (t.id === id ? { ...t, text: v } : t)));
    },

    cyclePriority: (id) => {
      persist(
        get().todos.map((t) => {
          if (t.id !== id) return t;
          const next = PRIO_SEQ[(PRIO_SEQ.indexOf(t.priority) + 1) % PRIO_SEQ.length];
          return { ...t, priority: next };
        }),
      );
    },

    reorder: (dragId, targetId, after) => {
      if (dragId === targetId) return;
      const sorted = [...get().todos].sort((a, b) => a.order - b.order);
      const drag = sorted.find((t) => t.id === dragId);
      if (!drag) return;
      const rest = sorted.filter((t) => t.id !== dragId);
      const idx = rest.findIndex((t) => t.id === targetId);
      if (idx < 0) return;
      rest.splice(after ? idx + 1 : idx, 0, drag);
      persist(rest.map((t, i) => ({ ...t, order: i })));
    },
  };
});
