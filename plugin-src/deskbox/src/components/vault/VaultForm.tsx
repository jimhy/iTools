/**
 * 密码编辑弹框（模态）：名称 / 分类（药丸，数据化）/ 用户名 / 网址 / 密码（显隐·生成·复制）/ 备注（加密）+ 保存 / 删除 / 取消。
 * 本地持有明文输入；密码/备注挂载时经 store 解密回填，保存时经 store 加密落盘。经 openVaultForm(id) 打开
 * （弹框宽 720，两列布局；每次 openModal 挂载全新实例，本地状态随「新建 / 切换条目」重置）。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import type { VaultCat } from "../../types";
import { useVaultStore } from "../../state/vaultStore";
import { itools } from "../../services/itools";
import { toast } from "../../state/toastStore";
import { openModal, closeModal } from "../../state/modalStore";
import { fmt } from "./vaultMeta";
import { CopyIcon, EyeIcon, GenIcon, OpenIcon } from "./icons";
import { ConfirmDialog } from "./dialogs/ConfirmDialog";
import { GeneratePasswordDialog } from "./dialogs/GeneratePasswordDialog";
import styles from "./Vault.module.css";

export function VaultForm() {
  const save = useVaultStore((s) => s.save);
  const remove = useVaultStore((s) => s.remove);
  const decryptField = useVaultStore((s) => s.decryptField);
  const categories = useVaultStore((s) => s.categories);
  const curEntry = useVaultStore((s) => s.curEntry);
  const entry = useVaultStore((s) => (s.curEntry ? s.vault.find((e) => e.id === s.curEntry) : null));

  // 仅挂载时取一次初始值（弹框每次 openModal 都是新实例）。
  const init = useMemo(() => {
    const st = useVaultStore.getState();
    const e = st.curEntry ? st.vault.find((x) => x.id === st.curEntry) : null;
    const defCat: VaultCat = st.cat !== "all" ? st.cat : st.categories[0]?.id ?? "other";
    return { e, defCat, isNew: !e };
  }, []);

  const [title, setTitle] = useState(init.e?.title ?? "");
  const [username, setUsername] = useState(init.e?.username ?? "");
  const [url, setUrl] = useState(init.e?.url ?? "");
  const [category, setCategory] = useState<VaultCat>(init.e?.category ?? init.defCat);
  const [password, setPassword] = useState("");
  const [note, setNote] = useState("");
  const [showPass, setShowPass] = useState(false);
  const titleRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    let live = true;
    if (init.e) {
      decryptField(init.e.secret)
        .then((v) => live && setPassword(v))
        .catch(() => toast("解密失败"));
      decryptField(init.e.note)
        .then((v) => live && setNote(v))
        .catch(() => {});
    } else {
      titleRef.current?.focus();
    }
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const close = () => {
    useVaultStore.getState().closeForm();
    closeModal();
  };

  const onSave = async () => {
    if (!title.trim()) {
      toast("请填写名称");
      titleRef.current?.focus();
      return;
    }
    const id = await save({ title, username, url, category, password, note });
    if (id) {
      toast("已保存");
      close();
    }
  };

  const onDelete = () => {
    const id = useVaultStore.getState().curEntry;
    if (!id) return;
    openModal(
      <ConfirmDialog
        title="删除密码"
        desc="删除后无法恢复，确定吗？"
        confirmText="删除"
        onConfirm={() => void remove(id)}
      />,
    );
  };

  const onGenerate = () =>
    openModal(
      <GeneratePasswordDialog
        onUse={(pw) => {
          setPassword(pw);
          setShowPass(true);
        }}
      />,
    );

  const copyUser = async () => {
    if (!username.trim()) return;
    await itools.copyText(username.trim());
    toast("用户名已复制");
  };
  const copyPass = async () => {
    if (!password) return;
    await itools.copyText(password);
    toast("密码已复制");
  };
  const openUrl = () => {
    const u = url.trim();
    if (/^https?:\/\//i.test(u)) itools.openExternal(u);
    else toast("请填写 http/https 网址");
  };

  return (
    <div className={styles.dialog}>
      <div className={styles.dlgHead}>
        <h3>{init.isNew ? "新建密码" : "编辑密码"}</h3>
        <button className={styles.dlgClose} title="关闭" onClick={close}>
          ✕
        </button>
      </div>

      <div className={styles.dlgBody}>
        <div className={`${styles.field} ${styles.spanAll}`}>
          <label>名称</label>
          <div className={styles.ctl}>
            <input ref={titleRef} value={title} onChange={(e) => setTitle(e.target.value)} placeholder="如：GitHub" />
          </div>
        </div>

        <div className={`${styles.field} ${styles.spanAll}`}>
          <label>分类</label>
          <div className={styles.catSel}>
            {categories.map((c) => (
              <span
                key={c.id}
                className={`${styles.catOpt} ${category === c.id ? styles.on : ""}`}
                onClick={() => setCategory(c.id)}
              >
                {c.name}
              </span>
            ))}
          </div>
        </div>

        <div className={styles.field}>
          <label>用户名 / 邮箱</label>
          <div className={styles.ctl}>
            <input value={username} onChange={(e) => setUsername(e.target.value)} placeholder="用户名" />
            <button className={styles.mini} title="复制" onClick={copyUser}>
              <CopyIcon />
            </button>
          </div>
        </div>

        <div className={styles.field}>
          <label>网址</label>
          <div className={styles.ctl}>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://" />
            <button className={styles.mini} title="打开" onClick={openUrl}>
              <OpenIcon />
            </button>
          </div>
        </div>

        <div className={`${styles.field} ${styles.spanAll}`}>
          <label>密码</label>
          <div className={styles.ctl}>
            <input
              type={showPass ? "text" : "password"}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="密码"
            />
            <button className={styles.mini} title="显示 / 隐藏" onClick={() => setShowPass((v) => !v)}>
              <EyeIcon />
            </button>
            <button className={styles.mini} title="生成强密码" onClick={onGenerate}>
              <GenIcon />
            </button>
            <button className={styles.mini} title="复制" onClick={copyPass}>
              <CopyIcon />
            </button>
          </div>
        </div>

        <div className={`${styles.field} ${styles.spanAll}`}>
          <label>备注（加密）</label>
          <textarea value={note} onChange={(e) => setNote(e.target.value)} placeholder="安全备注…" />
        </div>
      </div>

      <div className={styles.dlgFoot}>
        <button className={styles.primary} onClick={onSave}>
          保存
        </button>
        {curEntry && (
          <button className={`${styles.ghost} ${styles.danger}`} onClick={onDelete}>
            删除
          </button>
        )}
        <span className={styles.footSpacer} />
        {entry && <span className={styles.meta}>修改 {fmt(entry.updatedAt)}</span>}
        <button className={styles.ghost} onClick={close}>
          取消
        </button>
      </div>
    </div>
  );
}

/** 打开密码编辑弹框（宽 720）：id 为条目 id（编辑）或 null（新建）。 */
export function openVaultForm(id: string | null) {
  const st = useVaultStore.getState();
  if (id) st.openEntry(id);
  else st.openNew();
  openModal(<VaultForm />, 720);
}
