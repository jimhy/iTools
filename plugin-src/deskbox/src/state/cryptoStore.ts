/**
 * 加密会话状态：主密码配置（sec，落盘）+ 主密钥（masterKey，仅内存）。
 * 密码库与加密笔记共用它判断「是否已设主密码 / 是否已解锁」，并执行创建/解锁/锁定。
 */
import { create } from "zustand";
import type { SecConfig } from "../types";
import {
  PBKDF2_ITER,
  VERIFIER_TEXT,
  b64ToBuf,
  bufToB64,
  cryptoOk,
  decWith,
  deriveKey,
  encWith,
} from "../services/crypto";
import { KEY, store } from "../services/store";

interface CryptoState {
  masterKey: CryptoKey | null;
  sec: SecConfig | null;
  cryptoOk: boolean;
  hasMaster: () => boolean;
  isUnlocked: () => boolean;
  /** 启动时加载已保存的主密码配置。 */
  load: () => Promise<void>;
  /** 首次设置主密码：派生密钥 + 生成 verifier 并落盘。 */
  createMaster: (pw: string) => Promise<void>;
  /** 用口令尝试解锁：verifier 解密比对，成功则置入 masterKey。 */
  tryUnlock: (pw: string) => Promise<boolean>;
  /** 锁定：清除内存中的主密钥。 */
  lock: () => void;
}

export const useCryptoStore = create<CryptoState>((set, get) => ({
  masterKey: null,
  sec: null,
  cryptoOk,
  hasMaster: () => Boolean(get().sec),
  isUnlocked: () => Boolean(get().masterKey),

  load: async () => {
    const sec = await store.get<SecConfig>(KEY.sec);
    set({ sec });
  },

  createMaster: async (pw) => {
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const key = await deriveKey(pw, salt, PBKDF2_ITER);
    const verifier = await encWith(key, VERIFIER_TEXT);
    const sec: SecConfig = { v: 1, salt: bufToB64(salt), iter: PBKDF2_ITER, verifier };
    await store.set(KEY.sec, sec);
    set({ sec, masterKey: key });
  },

  tryUnlock: async (pw) => {
    const { sec } = get();
    if (!sec) return false;
    const key = await deriveKey(pw, b64ToBuf(sec.salt), sec.iter);
    try {
      const t = await decWith(key, sec.verifier);
      if (t !== VERIFIER_TEXT) return false;
    } catch {
      return false;
    }
    set({ masterKey: key });
    return true;
  },

  lock: () => set({ masterKey: null }),
}));
