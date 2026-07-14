/**
 * 密码库状态：条目列表 + 分类筛选 + 当前选中 / 表单开关，以及增删改与 AES 加解密。
 *
 * 分层意图：所有涉及持久化与加解密的逻辑集中在此（数据层），组件只做展示与交互。
 * 主密钥不在本 store 保存——统一向 cryptoStore 取（masterKey 仅存内存）。
 * 对外暴露 `vault: VaultEntry[]`，供底部 StatusBar 计数。
 */
import { create } from "zustand";
import type { EncBlob, VaultCat, VaultCategory, VaultEntry } from "../types";
import { KEY, store, uid } from "../services/store";
import { calcStrength, decWith, encWith } from "../services/crypto";
import { itools } from "../services/itools";
import { DEFAULT_CATS } from "../components/vault/vaultMeta";
import { useCryptoStore } from "./cryptoStore";
import { toast } from "./toastStore";

/** 分类栏 / 筛选用的键：全部 + 各具体分类。 */
export type VaultCatKey = VaultCat | "all";

/** 编辑表单提交的明文草稿（密码 / 备注在 save 时才加密）。 */
export interface VaultDraft {
  title: string;
  username: string;
  url: string;
  category: VaultCat;
  password: string;
  note: string;
}

interface VaultState {
  vault: VaultEntry[];
  /** 密码分类（可增删改；首次使用种入内置默认）。 */
  categories: VaultCategory[];
  /** 当前分类筛选。 */
  cat: VaultCatKey;
  /** 编辑弹框正在编辑的现有条目 id（新建时为 null）；供 save 判定新建/更新。 */
  curEntry: string | null;

  load: () => Promise<void>;
  setCat: (cat: VaultCatKey) => void;
  /** 新增分类，返回新分类 id。 */
  addCategory: (name: string, icon: string) => Promise<string>;
  /** 修改分类名称 / 图标。 */
  updateCategory: (id: string, name: string, icon: string) => Promise<void>;
  /** 删除分类：其下条目归入「其他」（若不存在则置空，仅「全部」可见）；当前筛选若为它则切回「全部」。 */
  removeCategory: (id: string) => Promise<void>;
  /** 新建：清空编辑目标（弹框由调用方经 openVaultForm 打开）。 */
  openNew: () => void;
  /** 编辑：设为某条目（弹框由调用方经 openVaultForm 打开）。 */
  openEntry: (id: string) => void;
  /** 清空编辑目标（关闭弹框时调）。 */
  closeForm: () => void;
  /** 保存（新建或更新）：加密密码/备注、算强度、落盘。返回条目 id，失败返回 null。 */
  save: (draft: VaultDraft) => Promise<string | null>;
  /** 删除条目并落盘。 */
  remove: (id: string) => Promise<void>;
  /** 解密一个密文块为明文（空块返回空串）；供表单回填用。 */
  decryptField: (blob: EncBlob | null) => Promise<string>;
  /** 复制条目密码到剪贴板（解密后）。 */
  copyPassword: (entry: VaultEntry) => Promise<void>;
}

const persist = (vault: VaultEntry[]) => store.set(KEY.vault, vault);

export const useVaultStore = create<VaultState>((set, get) => ({
  vault: [],
  categories: [],
  cat: "all",
  curEntry: null,

  load: async () => {
    const vault = (await store.get<VaultEntry[]>(KEY.vault)) ?? [];
    let categories = await store.get<VaultCategory[]>(KEY.vaultCats);
    if (!categories || categories.length === 0) {
      // 首次使用：种入内置默认分类（id 沿用旧枚举值，老数据无需迁移）。
      categories = DEFAULT_CATS.slice();
      await store.set(KEY.vaultCats, categories);
    }
    set({ vault, categories });
  },

  setCat: (cat) => set({ cat }),

  addCategory: async (name, icon) => {
    const id = uid();
    const cat: VaultCategory = { id, name: name.trim() || "未命名", icon: icon || "tag" };
    const categories = [...get().categories, cat];
    set({ categories });
    await store.set(KEY.vaultCats, categories);
    return id;
  },

  updateCategory: async (id, name, icon) => {
    const categories = get().categories.map((c) => (c.id === id ? { ...c, name: name.trim() || c.name, icon } : c));
    set({ categories });
    await store.set(KEY.vaultCats, categories);
  },

  removeCategory: async (id) => {
    const categories = get().categories.filter((c) => c.id !== id);
    const fallback = categories.find((c) => c.id === "other")?.id ?? "";
    const vault = get().vault.map((e) => (e.category === id ? { ...e, category: fallback } : e));
    const cat = get().cat === id ? "all" : get().cat;
    set({ categories, vault, cat });
    await store.set(KEY.vaultCats, categories);
    await persist(vault);
  },

  openNew: () => set({ curEntry: null }),

  openEntry: (id) => set({ curEntry: id }),

  closeForm: () => set({ curEntry: null }),

  save: async (draft) => {
    const key = useCryptoStore.getState().masterKey;
    if (!key) {
      toast("密码库已锁定");
      return null;
    }
    const secret = draft.password ? await encWith(key, draft.password) : null;
    const noteBlob = draft.note ? await encWith(key, draft.note) : null;
    const now = Date.now();
    const strength = calcStrength(draft.password);
    const base = {
      title: draft.title.trim(),
      username: draft.username.trim(),
      url: draft.url.trim(),
      category: draft.category,
      strength,
      secret,
      note: noteBlob,
    };

    const curId = get().curEntry;
    const existing = curId ? get().vault.find((e) => e.id === curId) : null;
    let id: string;
    let vault: VaultEntry[];
    if (existing) {
      id = existing.id;
      const updated: VaultEntry = { ...existing, ...base, updatedAt: now };
      vault = get().vault.map((e) => (e.id === id ? updated : e));
    } else {
      id = uid();
      const created: VaultEntry = { id, ...base, createdAt: now, updatedAt: now };
      vault = [...get().vault, created];
    }
    set({ vault, curEntry: id });
    await persist(vault);
    return id;
  },

  remove: async (id) => {
    const vault = get().vault.filter((e) => e.id !== id);
    set({ vault, curEntry: null });
    await persist(vault);
  },

  decryptField: async (blob) => {
    if (!blob) return "";
    const key = useCryptoStore.getState().masterKey;
    if (!key) return "";
    return decWith(key, blob);
  },

  copyPassword: async (entry) => {
    const key = useCryptoStore.getState().masterKey;
    if (!key) {
      toast("密码库已锁定");
      return;
    }
    try {
      const pw = entry.secret ? await decWith(key, entry.secret) : "";
      if (pw) {
        await itools.copyText(pw);
        toast("密码已复制");
      } else {
        toast("此条目无密码");
      }
    } catch {
      toast("解密失败");
    }
  },
}));
