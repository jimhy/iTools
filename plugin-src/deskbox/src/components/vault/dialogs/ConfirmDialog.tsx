/** 通用确认弹窗（用于删除条目）。确认后关闭并回调；取消仅关闭。 */
import { closeModal } from "../../../state/modalStore";
import styles from "./Dialog.module.css";

interface Props {
  title: string;
  desc: string;
  confirmText?: string;
  onConfirm: () => void;
}

export function ConfirmDialog({ title, desc, confirmText = "确定", onConfirm }: Props) {
  return (
    <>
      <h3 className={styles.title}>{title}</h3>
      <div className={styles.desc}>{desc}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={closeModal}>
          取消
        </button>
        <button
          className={`${styles.primary} ${styles.danger}`}
          onClick={() => {
            closeModal();
            onConfirm();
          }}
        >
          {confirmText}
        </button>
      </div>
    </>
  );
}
