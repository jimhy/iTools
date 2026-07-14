/** 密码界面入口：未解锁显示 gate，解锁后两栏（分类栏 + 主区列表）。点条目 / 新建走弹框（VaultForm）。 */
import { useCryptoStore } from "../../state/cryptoStore";
import { VaultGate } from "./VaultGate";
import { VaultSidebar } from "./VaultSidebar";
import { VaultMain } from "./VaultMain";
import styles from "./Vault.module.css";

export function VaultView() {
  const unlocked = useCryptoStore((s) => Boolean(s.masterKey));
  if (!unlocked) return <VaultGate />;
  return (
    <div className={styles.body}>
      <VaultSidebar />
      <VaultMain />
    </div>
  );
}
