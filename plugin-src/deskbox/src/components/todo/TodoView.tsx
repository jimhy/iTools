/**
 * 待办界面入口：左分类栏（新建按钮 + 分类列表）+ 主区（大标题/日期 + 进度条 + 输入框 + 待办列表）。
 * 展示 + 交互；数据/持久化在 todosStore，分类归属判定用 todoInCat，格式化/图标在 todoMeta。
 */
import { useMemo, useRef, useState } from "react";
import { todoInCat, useTodosStore } from "../../state/todosStore";
import { useUiStore } from "../../state/uiStore";
import { CAT_DEFS, todayStr } from "./todoMeta";
import { TodoCats } from "./TodoCats";
import { TodoRow } from "./TodoRow";
import styles from "./TodoView.module.css";

export function TodoView() {
  const todos = useTodosStore((s) => s.todos);
  const cat = useTodosStore((s) => s.cat);
  const add = useTodosStore((s) => s.add);
  const query = useUiStore((s) => s.query);

  const [input, setInput] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const def = CAT_DEFS.find((c) => c.key === cat);
  const q = query.toLowerCase();

  // 当前分类下的待办：先按分类视图过滤，再按搜索词过滤，最后未完成在前、同组按 order。
  const list = useMemo(() => {
    let l = todos.filter((t) => todoInCat(t, cat));
    if (q) l = l.filter((t) => t.text.toLowerCase().includes(q));
    return [...l].sort((a, b) => Number(a.done) - Number(b.done) || a.order - b.order);
  }, [todos, cat, q]);

  const total = list.length;
  const done = list.filter((t) => t.done).length;
  const pct = total ? Math.round((done / total) * 100) : 0;

  const submit = () => {
    add(input);
    setInput("");
  };

  return (
    <div className={styles.view}>
      <aside className={styles.sidebar}>
        <button className={styles.newBtn} onClick={() => inputRef.current?.focus()}>
          ＋ 新建待办
        </button>
        <div className={styles.divider} />
        <TodoCats />
      </aside>

      <div className={styles.main}>
        <div className={styles.head}>
          <h1 className={styles.title}>{def ? def.name : "待办"}</h1>
          <span className={styles.date}>{todayStr()}</span>
        </div>

        <div className={styles.progress}>
          <span className={styles.progLabel}>
            {done} / {total} 已完成
          </span>
          <div className={styles.bar}>
            <div className={styles.fill} style={{ width: pct + "%" }} />
          </div>
        </div>

        <div className={styles.add}>
          <input
            ref={inputRef}
            className={styles.addInput}
            value={input}
            placeholder="输入待办，回车添加到当前分类…"
            spellCheck={false}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
          />
          <button className={styles.addBtn} onClick={submit}>
            添加
          </button>
        </div>

        <div className={styles.list}>
          {list.length === 0 ? (
            <div className={styles.empty}>
              <div className={styles.emptyBig}>
                <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M8 1.5A6.5 6.5 0 1 0 14.5 8" />
                  <path d="M5.5 7.5L8 10l6-6.5" />
                </svg>
              </div>
              <div>{query ? "没有匹配的待办" : "这个分类还没有待办"}</div>
            </div>
          ) : (
            list.map((t) => <TodoRow key={t.id} todo={t} />)
          )}
        </div>
      </div>
    </div>
  );
}
