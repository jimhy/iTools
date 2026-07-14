/** 待办分类栏：五个分类（图标 + 名称 + 该分类实时计数），点击切换当前分类。 */
import { todoInCat, useTodosStore } from "../../state/todosStore";
import { CAT_COLORS, CAT_DEFS } from "./todoMeta";
import styles from "./TodoCats.module.css";

export function TodoCats() {
  const todos = useTodosStore((s) => s.todos);
  const cat = useTodosStore((s) => s.cat);
  const setCat = useTodosStore((s) => s.setCat);

  return (
    <div className={styles.list}>
      {CAT_DEFS.map((c) => {
        const count = todos.filter((t) => todoInCat(t, c.key)).length;
        return (
          <div
            key={c.key}
            className={`${styles.item}${cat === c.key ? " " + styles.active : ""}`}
            onClick={() => setCat(c.key)}
          >
            <span className={styles.ico} style={{ color: CAT_COLORS[c.key] }}>{c.icon}</span>
            <span className={styles.name}>{c.name}</span>
            <span className={styles.count}>{count}</span>
          </div>
        );
      })}
    </div>
  );
}
