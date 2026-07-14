/**
 * 创建主密码：两次输入校验（≥8 位且一致）→ 派生密钥 + 生成 verifier 落盘（cryptoStore.createMaster）。
 * 若在派生耗时期间弹窗被关闭（遮罩/Esc），挂载标志置否后回滚，避免留下半创建的主密码。
 */
import { useEffect, useRef, useState } from "react";
import { useCryptoStore } from "../../../state/cryptoStore";
import { closeModal } from "../../../state/modalStore";
import { store, KEY } from "../../../services/store";
import styles from "./Dialog.module.css";

export function CreateMasterDialog() {
  const [pw1, setPw1] = useState("");
  const [pw2, setPw2] = useState("");
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const live = useRef(true);

  useEffect(() => {
    // 挂载时置真（StrictMode dev 会 mount→cleanup→remount，若不在挂载体重置，
    // 首次 cleanup 会把标志永久置 false，导致派生完成后误走「已关闭回滚」分支、busy 卡死）。
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const submit = async () => {
    if (busy) return;
    if (pw1.length < 8) {
      setErr("主密码至少 8 位（越长越安全）");
      return;
    }
    if (pw1 !== pw2) {
      setErr("两次输入不一致");
      return;
    }
    setBusy(true);
    try {
      await useCryptoStore.getState().createMaster(pw1);
      if (!live.current) {
        // 派生期间弹窗已被关闭：回滚，保持「未设置主密码」
        useCryptoStore.getState().lock();
        useCryptoStore.setState({ sec: null });
        await store.remove(KEY.sec);
        return;
      }
      closeModal();
    } catch (e) {
      if (live.current) setErr("创建失败：" + (e instanceof Error ? e.message : String(e)));
    } finally {
      if (live.current) setBusy(false);
    }
  };

  return (
    <>
      <h3 className={styles.title}>创建主密码</h3>
      <div className={styles.desc}>
        用于加密所有密码与上锁的笔记。请牢记，它<b>无法找回</b>。建议用一句好记的长口令。
      </div>
      <input
        className={styles.input}
        type="password"
        autoFocus
        placeholder="主密码（至少 8 位）"
        value={pw1}
        onChange={(e) => setPw1(e.target.value)}
      />
      <input
        className={styles.input}
        type="password"
        placeholder="再次输入"
        value={pw2}
        onChange={(e) => setPw2(e.target.value)}
        onKeyDown={(e) => e.key === "Enter" && submit()}
      />
      <div className={styles.err}>{err}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={closeModal}>
          取消
        </button>
        <button className={styles.primary} onClick={submit} disabled={busy}>
          {busy ? "创建中…" : "创建"}
        </button>
      </div>
    </>
  );
}
