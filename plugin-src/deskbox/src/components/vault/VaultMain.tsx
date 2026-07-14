/** 密码主区：Header（分类标题 + 副标题 + 锁按钮）+ 分隔线 + Password Card 列表。对标 Pencil「DeskBox 密码 · Main Area」。 */
import { useVaultStore } from "../../state/vaultStore";
import { useCryptoStore } from "../../state/cryptoStore";
import { useUiStore } from "../../state/uiStore";
import { toast } from "../../state/toastStore";
import { LockIcon } from "./icons";
import { VaultEntryRow } from "./VaultEntryRow";
import styles from "./Vault.module.css";

export function VaultMain() {
  const vault = useVaultStore((s) => s.vault);
  const categories = useVaultStore((s) => s.categories);
  const cat = useVaultStore((s) => s.cat);
  const query = useUiStore((s) => s.query);
  const lock = useCryptoStore((s) => s.lock);

  const title = cat === "all" ? "全部密码" : categories.find((c) => c.id === cat)?.name ?? "全部密码";

  const q = query.trim().toLowerCase();
  let list = vault.slice().sort((a, b) => (b.updatedAt || 0) - (a.updatedAt || 0));
  if (cat !== "all") list = list.filter((e) => (e.category || "other") === cat);
  if (q) list = list.filter((e) => [e.title, e.username, e.url].some((x) => (x || "").toLowerCase().includes(q)));

  const onLock = () => {
    lock();
    toast("密码库已锁定");
  };

  return (
    <div className={styles.mainCol}>
      <div className={styles.pageHead}>
        <div className={styles.pageHeadText}>
          <h1>{title}</h1>
          <div className={styles.pageSub}>
            <span className={styles.mono}>{list.length}</span> 条凭据 · AES-256 加密
          </div>
        </div>
        <button className={styles.lockBtn} title="锁定密码库" onClick={onLock}>
          <LockIcon size={14} />
          锁定
        </button>
      </div>
      <div className={styles.headDivider} />
      <div className={styles.list}>
        {list.length === 0 ? (
          <div className={styles.listEmpty}>{query ? "没有匹配的条目" : "还没有密码，点左上「＋ 新建密码」"}</div>
        ) : (
          list.map((en) => <VaultEntryRow key={en.id} entry={en} />)
        )}
      </div>
    </div>
  );
}
