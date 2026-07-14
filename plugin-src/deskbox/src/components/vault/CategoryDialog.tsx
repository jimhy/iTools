/** 分类新增 / 编辑弹框：名称 + 图标选择 + 保存 / 删除 / 取消。经 openCategoryDialog(cat|null) 打开。 */
import { useState } from "react";
import type { VaultCategory } from "../../types";
import { useVaultStore } from "../../state/vaultStore";
import { openModal, closeModal } from "../../state/modalStore";
import { toast } from "../../state/toastStore";
import { ICON_KEYS, CatIcon } from "./vaultMeta";
import { ConfirmDialog } from "./dialogs/ConfirmDialog";
import styles from "./Vault.module.css";

function CategoryDialog({ cat }: { cat: VaultCategory | null }) {
  const addCategory = useVaultStore((s) => s.addCategory);
  const updateCategory = useVaultStore((s) => s.updateCategory);
  const removeCategory = useVaultStore((s) => s.removeCategory);
  const [name, setName] = useState(cat?.name ?? "");
  const [icon, setIcon] = useState(cat?.icon ?? "tag");

  const onSave = async () => {
    if (!name.trim()) {
      toast("请填写分类名称");
      return;
    }
    if (cat) await updateCategory(cat.id, name, icon);
    else await addCategory(name, icon);
    closeModal();
  };

  const onDelete = () => {
    if (!cat) return;
    openModal(
      <ConfirmDialog
        title="删除分类"
        desc="该分类下的密码将归入「其他」，不会丢失。确定删除吗？"
        confirmText="删除"
        onConfirm={() => void removeCategory(cat.id)}
      />,
    );
  };

  return (
    <div className={styles.dialog}>
      <div className={styles.dlgHead}>
        <h3>{cat ? "编辑分类" : "新增分类"}</h3>
        <button className={styles.dlgClose} title="关闭" onClick={closeModal}>
          ✕
        </button>
      </div>

      <div className={styles.field}>
        <label>名称</label>
        <div className={styles.ctl}>
          <input
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="如：社交"
            onKeyDown={(e) => e.key === "Enter" && onSave()}
          />
        </div>
      </div>

      <div className={styles.field}>
        <label>图标</label>
        <div className={styles.iconGrid}>
          {ICON_KEYS.map((k) => (
            <button
              key={k}
              className={`${styles.iconOpt} ${icon === k ? styles.iconOn : ""}`}
              onClick={() => setIcon(k)}
              title={k}
            >
              <CatIcon icon={k} size={16} />
            </button>
          ))}
        </div>
      </div>

      <div className={styles.dlgFoot}>
        <button className={styles.primary} onClick={onSave}>
          保存
        </button>
        {cat && (
          <button className={`${styles.ghost} ${styles.danger}`} onClick={onDelete}>
            删除
          </button>
        )}
        <span className={styles.footSpacer} />
        <button className={styles.ghost} onClick={closeModal}>
          取消
        </button>
      </div>
    </div>
  );
}

/** 打开分类弹框：cat 为编辑，null 为新增。 */
export function openCategoryDialog(cat: VaultCategory | null) {
  openModal(<CategoryDialog cat={cat} />, 420);
}
