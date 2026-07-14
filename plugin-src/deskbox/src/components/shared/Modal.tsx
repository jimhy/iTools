import { useModalStore } from "../../state/modalStore";
import styles from "./Modal.module.css";

/** 通用弹窗容器：点遮罩关闭；内容由各功能通过 openModal 注入。可选 width 覆盖默认卡片宽度。 */
export function Modal() {
  const { content, width, close } = useModalStore();
  if (!content) return null;
  return (
    <div className={styles.overlay} onMouseDown={(e) => e.target === e.currentTarget && close()}>
      <div className={styles.card} style={width ? { width } : undefined}>
        {content}
      </div>
    </div>
  );
}
