/**
 * Password Card 行：品牌色徽标 + 名称/用户名/掩码密码（眼睛可显隐）+ 复制 + 强度点。
 * 点行打开编辑弹框；右键菜单（编辑/复制密码/复制用户名/删除）。对标 Pencil「Password Card」。
 */
import { useState, type MouseEvent } from "react";
import type { VaultEntry } from "../../types";
import { useVaultStore } from "../../state/vaultStore";
import { openContextMenu, type MenuItem } from "../../state/contextMenuStore";
import { openModal } from "../../state/modalStore";
import { toast } from "../../state/toastStore";
import { itools } from "../../services/itools";
import { brandColor, avatarInitial } from "./vaultMeta";
import { CopyIcon, EyeIcon } from "./icons";
import { ConfirmDialog } from "./dialogs/ConfirmDialog";
import { openVaultForm } from "./VaultForm";
import styles from "./Vault.module.css";

export function VaultEntryRow({ entry }: { entry: VaultEntry }) {
  const copyPassword = useVaultStore((s) => s.copyPassword);
  const decryptField = useVaultStore((s) => s.decryptField);
  const remove = useVaultStore((s) => s.remove);

  const [revealed, setRevealed] = useState(false);
  const [plain, setPlain] = useState("");

  const lv = entry.strength || 0;
  const stClass = lv >= 2 ? styles.stStrong : lv === 1 ? styles.stMedium : styles.stWeak;
  const stLabel = lv >= 2 ? "强" : lv === 1 ? "中" : "弱";

  const toggleReveal = async (e: MouseEvent) => {
    e.stopPropagation();
    if (revealed) {
      setRevealed(false);
      return;
    }
    if (!entry.secret) {
      toast("此条目无密码");
      return;
    }
    try {
      const pw = plain || (await decryptField(entry.secret));
      setPlain(pw);
      setRevealed(true);
    } catch {
      toast("解密失败");
    }
  };

  const onMenu = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    const items: MenuItem[] = [
      { label: "编辑", onSelect: () => openVaultForm(entry.id) },
      { label: "复制密码", onSelect: () => void copyPassword(entry) },
    ];
    if (entry.username) {
      items.push({
        label: "复制用户名",
        onSelect: () => {
          void itools.copyText(entry.username);
          toast("用户名已复制");
        },
      });
    }
    items.push({
      label: "删除",
      danger: true,
      onSelect: () =>
        openModal(
          <ConfirmDialog
            title="删除密码"
            desc="删除后无法恢复，确定吗？"
            confirmText="删除"
            onConfirm={() => void remove(entry.id)}
          />,
        ),
    });
    openContextMenu(e.clientX, e.clientY, items);
  };

  return (
    <div className={styles.card} onClick={() => openVaultForm(entry.id)} onContextMenu={onMenu}>
      <div className={styles.badge} style={{ background: brandColor(entry.title || "?") }}>
        {avatarInitial(entry.title)}
      </div>
      <div className={styles.info}>
        <div className={styles.name}>{entry.title || "未命名"}</div>
        <div className={styles.user}>{entry.username || entry.url || "—"}</div>
        <div className={styles.mask}>{revealed ? plain || "（空密码）" : "••••••••••"}</div>
      </div>
      <div className={styles.cardActs}>
        <div className={styles.actIcons}>
          <button className={styles.act} title={revealed ? "隐藏密码" : "显示密码"} onClick={toggleReveal}>
            <EyeIcon size={16} />
          </button>
          <button
            className={styles.act}
            title="复制密码"
            onClick={(e) => {
              e.stopPropagation();
              void copyPassword(entry);
            }}
          >
            <CopyIcon size={16} />
          </button>
        </div>
        <span className={`${styles.strength} ${stClass}`} title={`强度：${stLabel}`} />
      </div>
    </div>
  );
}
