/** 未解锁遮罩：居中锁形卡片 + 网格纹理，按是否已设主密码切换文案与动作。 */
import { useCryptoStore } from "../../state/cryptoStore";
import { openModal } from "../../state/modalStore";
import { LockIcon } from "./icons";
import { CreateMasterDialog } from "./dialogs/CreateMasterDialog";
import { UnlockDialog } from "./dialogs/UnlockDialog";
import styles from "./Vault.module.css";

export function VaultGate() {
  const sec = useCryptoStore((s) => s.sec);
  const cryptoOk = useCryptoStore((s) => s.cryptoOk);
  const hasMaster = Boolean(sec);

  const title = hasMaster ? "密码库已锁定" : "创建主密码";
  const desc = hasMaster
    ? "输入主密码以解锁你的密码库。主密码只保存在你本机、不会明文存储。"
    : "首次使用请设置一个主密码，用于加密你的所有密码。请务必牢记——它无法找回。";

  const onClick = () => openModal(hasMaster ? <UnlockDialog /> : <CreateMasterDialog />);

  return (
    <div className={styles.gate}>
      <div className={styles.gateCard}>
        <div className={styles.gateIco}>
          <LockIcon size={36} />
        </div>
        <h3>{title}</h3>
        <p>{cryptoOk ? desc : "当前运行环境不支持 Web Crypto，无法启用加密密码库。"}</p>
        {cryptoOk && (
          <button className={styles.primary} onClick={onClick}>
            {hasMaster ? "解锁" : "创建主密码"}
          </button>
        )}
      </div>
    </div>
  );
}
