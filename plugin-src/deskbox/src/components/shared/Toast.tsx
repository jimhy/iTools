import { useToastStore } from "../../state/toastStore";
import styles from "./Toast.module.css";

/** 屏幕底部居中的 toast 层。 */
export function Toast() {
  const toasts = useToastStore((s) => s.toasts);
  return (
    <div className={styles.layer}>
      {toasts.map((t) => (
        <div key={t.id} className={styles.toast}>
          {t.msg}
        </div>
      ))}
    </div>
  );
}
