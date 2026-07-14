/** 分类侧栏：「＋ 新建密码」渐变按钮 + 「全部」+ 数据化分类列表（右键重命名 / 删除）+ 「＋ 新增分类」。 */
import { useVaultStore } from "../../state/vaultStore";
import { openContextMenu } from "../../state/contextMenuStore";
import { openModal } from "../../state/modalStore";
import { CatIcon } from "./vaultMeta";
import { openVaultForm } from "./VaultForm";
import { openCategoryDialog } from "./CategoryDialog";
import { ConfirmDialog } from "./dialogs/ConfirmDialog";
import styles from "./Vault.module.css";

export function VaultSidebar() {
  const vault = useVaultStore((s) => s.vault);
  const categories = useVaultStore((s) => s.categories);
  const cat = useVaultStore((s) => s.cat);
  const setCat = useVaultStore((s) => s.setCat);
  const removeCategory = useVaultStore((s) => s.removeCategory);

  const countOf = (key: string) =>
    key === "all" ? vault.length : vault.filter((e) => (e.category || "other") === key).length;

  return (
    <aside className={styles.sidebar}>
      <button className={styles.newBtn} onClick={() => openVaultForm(null)}>
        ＋ 新建密码
      </button>
      <div className={styles.divider} />
      <div className={styles.catList}>
        <div className={`${styles.catItem} ${cat === "all" ? styles.active : ""}`} onClick={() => setCat("all")}>
          <span className={styles.catIco}>
            <CatIcon icon="all" />
          </span>
          <span className={styles.catName}>全部</span>
          <span className={styles.catCount}>{countOf("all")}</span>
        </div>

        {categories.map((c) => (
          <div
            key={c.id}
            className={`${styles.catItem} ${cat === c.id ? styles.active : ""}`}
            onClick={() => setCat(c.id)}
            onContextMenu={(e) => {
              e.preventDefault();
              e.stopPropagation();
              openContextMenu(e.clientX, e.clientY, [
                { label: "重命名 / 改图标", onSelect: () => openCategoryDialog(c) },
                {
                  label: "删除",
                  danger: true,
                  onSelect: () =>
                    openModal(
                      <ConfirmDialog
                        title="删除分类"
                        desc={`删除「${c.name}」，其下密码归入「其他」不会丢失。确定吗？`}
                        confirmText="删除"
                        onConfirm={() => void removeCategory(c.id)}
                      />,
                    ),
                },
              ]);
            }}
          >
            <span className={styles.catIco}>
              <CatIcon icon={c.icon} />
            </span>
            <span className={styles.catName}>{c.name}</span>
            <span className={styles.catCount}>{countOf(c.id)}</span>
          </div>
        ))}

        <button className={styles.addCat} onClick={() => openCategoryDialog(null)}>
          ＋ 新增分类
        </button>
      </div>
    </aside>
  );
}
