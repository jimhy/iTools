/**
 * 创建主密码弹窗：设置用于加密上锁笔记 / 密码库的主密码。
 * 成功 resolve(true)；取消 resolve(false)。若在密钥派生（PBKDF2）进行中被取消，回滚已创建的主密码，保持“未设置”一致。
 */
import { useEffect, useRef, useState } from "react";
import { useCryptoStore } from "../../../state/cryptoStore";
import { KEY, store } from "../../../services/store";
import { closeModal } from "../../../state/modalStore";
import { errMsg } from "../util";
import { ask } from "./ask";
import styles from "./dialog.module.css";

interface Props {
  resolve: (v: boolean) => void;
}

function CreateMasterDialog({ resolve }: Props) {
  const [p1, setP1] = useState("");
  const [p2, setP2] = useState("");
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
    if (p1.length < 8) return setErr("主密码至少 8 位（越长越安全）");
    if (p1 !== p2) return setErr("两次输入不一致");
    try {
      await useCryptoStore.getState().createMaster(p1);
      if (cancelledRef.current) {
        // 派生期间被取消：撤销已创建的主密码，保持一致。
        useCryptoStore.setState({ masterKey: null, sec: null });
        await store.remove(KEY.sec);
        return;
      }
      resolve(true);
      closeModal();
    } catch (e) {
      setErr("创建失败：" + errMsg(e));
    }
  };

  const cancel = () => {
    cancelledRef.current = true;
    resolve(false);
    closeModal();
  };

  return (
    <div>
      <h3 className={styles.title}>创建主密码</h3>
      <div className={styles.desc}>
        用于加密所有密码与上锁的笔记。请牢记，它<b>无法找回</b>。建议用一句好记的长口令。
      </div>
      <input
        className={styles.input}
        type="password"
        placeholder="主密码（至少 8 位）"
        autoFocus
        value={p1}
        onChange={(e) => setP1(e.target.value)}
      />
      <input
        className={styles.input}
        type="password"
        placeholder="再次输入"
        value={p2}
        onChange={(e) => setP2(e.target.value)}
        onKeyDown={(e) => e.key === "Enter" && submit()}
      />
      <div className={styles.err}>{err}</div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={cancel}>
          取消
        </button>
        <button className={styles.primary} onClick={submit}>
          创建
        </button>
      </div>
    </div>
  );
}

export function createMasterDialog(): Promise<boolean> {
  return ask((resolve) => <CreateMasterDialog resolve={resolve} />);
}
