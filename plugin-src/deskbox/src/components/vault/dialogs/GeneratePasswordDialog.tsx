/** 生成强密码：长度 + 字符集选项 → genPassword，可换一个；「用于此条目」回填到表单密码框。 */
import { useCallback, useEffect, useState } from "react";
import { genPassword } from "../../../services/crypto";
import { closeModal } from "../../../state/modalStore";
import styles from "./Dialog.module.css";

interface Props {
  onUse: (pw: string) => void;
}

export function GeneratePasswordDialog({ onUse }: Props) {
  const [len, setLen] = useState(16);
  const [upper, setUpper] = useState(true);
  const [lower, setLower] = useState(true);
  const [digit, setDigit] = useState(true);
  const [sym, setSym] = useState(true);
  const [out, setOut] = useState("");

  const regen = useCallback(() => {
    const n = Math.min(64, Math.max(6, len || 16));
    setOut(genPassword(n, { upper, lower, digit, sym }));
  }, [len, upper, lower, digit, sym]);

  useEffect(() => {
    regen();
  }, [regen]);

  return (
    <>
      <h3 className={styles.title}>生成强密码</h3>
      <input className={`${styles.input} ${styles.genOut}`} readOnly value={out} />
      <div className={styles.genOpts}>
        <label>
          长度
          <input
            className={styles.lenInput}
            type="number"
            min={6}
            max={64}
            value={len}
            onChange={(e) => setLen(Number(e.target.value))}
          />
        </label>
        <label>
          <input type="checkbox" checked={upper} onChange={(e) => setUpper(e.target.checked)} /> 大写
        </label>
        <label>
          <input type="checkbox" checked={lower} onChange={(e) => setLower(e.target.checked)} /> 小写
        </label>
        <label>
          <input type="checkbox" checked={digit} onChange={(e) => setDigit(e.target.checked)} /> 数字
        </label>
        <label>
          <input type="checkbox" checked={sym} onChange={(e) => setSym(e.target.checked)} /> 符号
        </label>
      </div>
      <div className={styles.row}>
        <button className={styles.ghost} onClick={regen}>
          ↻ 换一个
        </button>
        <button className={styles.ghost} onClick={closeModal}>
          关闭
        </button>
        <button
          className={styles.primary}
          onClick={() => {
            onUse(out);
            closeModal();
          }}
        >
          用于此条目
        </button>
      </div>
    </>
  );
}
