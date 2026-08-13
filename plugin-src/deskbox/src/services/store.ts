/**
 * 存储服务：DeskBox 三个功能的持久化，统一走 **itools.data**（宿主内落 SQLite 的 `plugin_data` 表；
 * 浏览器 dev 落 localStorage）。这是数据落盘的唯一出口；hooks 经此层读写，容错（读失败返回空，不抛给上层）。
 *
 * ⚠ 为什么是 `itools.data` 而不是 `itools.db`：宿主有两套插件存储，
 * `itools.db.*` 落 `plugin_kv` 表——**纯本地、设计上不参与云同步**；
 * `itools.data.*` 落 `plugin_data` 表，带 updated_at/dirty，会被「立即同步」推到云端。
 * 早先本文件用的是前者，于是笔记 / 待办 / 密码库在任何设备间都同步不了，
 * 而「我的数据」页又只统计后者，导致用户看到的条数与实际存储完全对不上。
 * 两套 API 的 get/set/remove/keys 签名逐字相同，切换不需要改调用方。
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
      return await itools.data.get<T>(key);
    } catch {
      return null;
    }
  },
  async set(key: string, value: unknown): Promise<void> {
    return itools.data.set(key, value);
  },
  async remove(key: string): Promise<void> {
    return itools.data.remove(key);
  },
  async keys(prefix = ""): Promise<string[]> {
    try {
      return await itools.data.keys(prefix);
    } catch {
      return [];
    }
  },
};

/** 生成本地唯一 id（时间戳 + 随机，避免依赖可能不可用的 crypto.randomUUID）。 */
export function uid(): string {
  return "k" + Date.now().toString(36) + crypto.getRandomValues(new Uint32Array(1))[0].toString(36);
}
