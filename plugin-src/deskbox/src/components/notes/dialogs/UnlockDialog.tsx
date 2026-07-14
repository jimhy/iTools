/**
 * 解锁弹窗：输入主密码校验并置入内存主密钥。成功 resolve(true)；口令错误提示重试；取消 resolve(false)。
 * 若派生进行中被取消，回滚解锁，保持“已锁定”。
 */
import { useEffect, useRef, useState } from "react";
import { useCryptoStore } from "../../../state/cryptoStore";
import { closeModal } from "../../../state/modalStore";
import { ask } from "./ask";
import styles from "./dialog.module.css";

interface Props {
  resolve: (v: boolean) => void;
}

function UnlockDialog({ resolve }: Props) {
  const [pw, setPw] = useState("");
  const [err, setErr] = useState("");
  const cancelledRef = useRef(false);
  const resolveRef = useRef(resolve);
  resolveRef.current = resolve;

  useEffect(
    () => () => {
      cancelledRef.current = true;
      resolveRef.current(false);
    },
    [],
  );

  const submit = async () => {
    const ok = await useCryptoStore.getState().tryUnlock(pw);
    if (cancelledRef.current) {
      useCryptoStore.getState().lock();
      return;
    }
    if (!ok) {
      setErr("主密码错误");
      return;
    }
    resolve(true);
    closeModal();
  };

  const cancel = () => {
    cancelledRef.current = true;
    resolve(false);
    closeModal();
  };

  return (
    <div>
      <h3 className={styles.title}>解锁</h3>
      <div className={styles.desc}>输入主密码以解锁加密内容。</div>
      <input
        className={styles.input}
        type="password"
        placeholder="主密码"
        autoFocus
        value={pw}
        onChange={(e) => {
          setPw(e.target.value);
          if (err) setErr("");
        }}
        onKeyDown={(e) => e.key === "Enter" && submit()}
      />
      <div className={styles.err}>{err}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={cancel}>
          取消
        </button>
        <button className={styles.primary} onClick={submit}>
          解锁
        </button>
      </div>
    </div>
  );
}

export function unlockDialog(): Promise<boolean> {
  return ask((resolve) => <UnlockDialog resolve={resolve} />);
}
