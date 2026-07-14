/** 通用确认弹窗（危险操作用红色确定按钮）。confirmDialog() 返回 Promise<boolean>。 */
import { useEffect, useRef } from "react";
import { closeModal } from "../../../state/modalStore";
import { ask } from "./ask";
import styles from "./dialog.module.css";

interface Props {
  title: string;
  desc: string;
  resolve: (v: boolean) => void;
}

function ConfirmDialog({ title, desc, resolve }: Props) {
  const resolveRef = useRef(resolve);
  resolveRef.current = resolve;
  // 点遮罩关闭 → 组件卸载 → 兜底以“取消”resolve。
  useEffect(() => () => resolveRef.current(false), []);

  const done = (v: boolean) => {
    resolve(v);
    closeModal();
  };

  return (
    <div>
      <h3 className={styles.title}>{title}</h3>
      <div className={styles.desc}>{desc}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={() => done(false)}>
          取消
        </button>
        <button className={`${styles.primary} ${styles.danger}`} onClick={() => done(true)} autoFocus>
          确定
        </button>
      </div>
    </div>
  );
}

/** 打开确认弹窗，返回用户是否点了“确定”。 */
export function confirmDialog(title: string, desc: string): Promise<boolean> {
  return ask((resolve) => <ConfirmDialog title={title} desc={desc} resolve={resolve} />);
}
