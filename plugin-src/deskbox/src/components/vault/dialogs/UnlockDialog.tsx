/** 解锁：输入主密码 → cryptoStore.tryUnlock（verifier 解密比对）。成功关闭并置入内存主密钥。 */
import { useState } from "react";
import { useCryptoStore } from "../../../state/cryptoStore";
import { closeModal } from "../../../state/modalStore";
import styles from "./Dialog.module.css";

export function UnlockDialog() {
  const [pw, setPw] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (busy) return;
    setBusy(true);
    try {
      const ok = await useCryptoStore.getState().tryUnlock(pw);
      if (!ok) {
        setErr("主密码错误");
        setBusy(false);
        return;
      }
      closeModal();
    } catch {
      setErr("解锁失败");
      setBusy(false);
    }
  };

  return (
    <>
      <h3 className={styles.title}>解锁</h3>
      <div className={styles.desc}>输入主密码。</div>
      <input
        className={styles.input}
        type="password"
        autoFocus
        placeholder="主密码"
        value={pw}
        onChange={(e) => {
          setPw(e.target.value);
          setErr("");
        }}
        onKeyDown={(e) => e.key === "Enter" && submit()}
      />
      <div className={styles.err}>{err}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={closeModal}>
          取消
        </button>
        <button className={styles.primary} onClick={submit} disabled={busy}>
          {busy ? "解锁中…" : "解锁"}
        </button>
      </div>
    </>
  );
}
