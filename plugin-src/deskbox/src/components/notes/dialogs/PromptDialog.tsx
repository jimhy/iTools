/** 通用单行输入弹窗（用于链接 URL 等）。promptDialog() 返回 Promise<string | null>（取消为 null）。 */
import { useEffect, useRef, useState } from "react";
import { openModal, closeModal } from "../../../state/modalStore";
import styles from "./dialog.module.css";

interface Props {
  title: string;
  placeholder?: string;
  initial?: string;
  confirmText?: string;
  resolve: (v: string | null) => void;
}

function PromptDialog({ title, placeholder, initial, confirmText, resolve }: Props) {
  const [val, setVal] = useState(initial ?? "");
  const inputRef = useRef<HTMLInputElement>(null);
  const resolveRef = useRef(resolve);
  resolveRef.current = resolve;

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);
  // 点遮罩关闭 → 卸载 → 兜底以“取消”resolve。
  useEffect(() => () => resolveRef.current(null), []);

  const done = (v: string | null) => {
    resolve(v);
    closeModal();
  };

  return (
    <div>
      <h3 className={styles.title}>{title}</h3>
      <input
        ref={inputRef}
        className={styles.input}
        placeholder={placeholder}
        value={val}
        onChange={(e) => setVal(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") {
            e.preventDefault();
            done(val.trim() || null);
          } else if (e.key === "Escape") {
            done(null);
          }
        }}
      />
      <div className={styles.row}>
        <button className={styles.ghost} onClick={() => done(null)}>
          取消
        </button>
        <button className={styles.primary} onClick={() => done(val.trim() || null)}>
          {confirmText ?? "确定"}
        </button>
      </div>
    </div>
  );
}

export function promptDialog(
  title: string,
  opts?: { placeholder?: string; initial?: string; confirmText?: string },
): Promise<string | null> {
  return new Promise((res) => {
    let done = false;
    const settle = (v: string | null) => {
      if (done) return;
      done = true;
      res(v);
    };
    openModal(
      <PromptDialog
        title={title}
        placeholder={opts?.placeholder}
        initial={opts?.initial}
        confirmText={opts?.confirmText}
        resolve={settle}
      />,
    );
  });
}
