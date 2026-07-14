/**
 * 存储服务：DeskBox 三个功能的持久化，统一走 itools.db（宿主内即落 SQLite；浏览器 dev 落 localStorage）。
 * 这是数据落盘的唯一出口；hooks 经此层读写，容错（读失败返回空，不抛给上层）。
 */
import { itools } from "./itools";

export const KEY = {
  sec: "sec",
  tree: "notes.tree",
  expanded: "notes.expanded",
  body: (id: string) => `notes.body.${id}`,
  todos: "todos",
  vault: "vault",
  vaultCats: "vault.cats",
  ui: "ui",
  sidebarW: "notes.sidebarW",
} as const;

export const store = {
  async get<T>(key: string): Promise<T | null> {
    try {
      return await itools.db.get<T>(key);
    } catch {
      return null;
    }
  },
  async set(key: string, value: unknown): Promise<void> {
    return itools.db.set(key, value);
  },
  async remove(key: string): Promise<void> {
    return itools.db.remove(key);
  },
  async keys(prefix = ""): Promise<string[]> {
    try {
      return await itools.db.keys(prefix);
    } catch {
      return [];
    }
  },
};

/** 生成本地唯一 id（时间戳 + 随机，避免依赖可能不可用的 crypto.randomUUID）。 */
export function uid(): string {
  return "k" + Date.now().toString(36) + crypto.getRandomValues(new Uint32Array(1))[0].toString(36);
}
