//! 持久化存储：内存态 + 原子落盘的 JSON 存储（零原生依赖，Windows/Linux 直接可跑）。
//!
//! 结构清晰、单一职责，便于日后替换为 Postgres/MySQL：只需实现同样的方法签名。
//! 写入经 `writeChain` 串行化 + 写临时文件再 rename，保证并发写不互相截断、崩溃不留半文件。

import { promises as fs } from "node:fs";
import { dirname } from "node:path";

/** 一个用户账号（口令只存哈希 + 盐，绝不明文）。 */
export interface UserRecord {
  username: string;
  passwordHash: string;
  salt: string;
  createdAt: number;
}

/** 一条同步数据记录（value 为任意 JSON；updatedAt 由客户端提供，用于 last-write-wins）。 */
export interface DataRecord {
  value: unknown;
  updatedAt: number;
}

/** 一个会话。 */
export interface SessionRecord {
  username: string;
  createdAt: number;
}

interface Db {
  users: Record<string, UserRecord>;
  sessions: Record<string, SessionRecord>;
  /** username -> namespace -> key -> record */
  data: Record<string, Record<string, Record<string, DataRecord>>>;
}

function emptyDb(): Db {
  return { users: {}, sessions: {}, data: {} };
}

export class JsonStore {
  private db: Db = emptyDb();
  private writeChain: Promise<void> = Promise.resolve();

  constructor(private readonly file: string) {}

  /** 启动时从磁盘加载（文件不存在 → 空库；损坏 → 抛错，避免静默丢数据）。 */
  async load(): Promise<void> {
    try {
      const text = await fs.readFile(this.file, "utf8");
      const parsed = JSON.parse(text) as Partial<Db>;
      this.db = {
        users: parsed.users ?? {},
        sessions: parsed.sessions ?? {},
        data: parsed.data ?? {},
      };
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "ENOENT") {
        this.db = emptyDb();
      } else {
        throw err;
      }
    }
  }

  /** 串行化 + 原子落盘（写 .tmp 再 rename）。返回本次写完成的 Promise。 */
  private persist(): Promise<void> {
    const snapshot = JSON.stringify(this.db, null, 2);
    this.writeChain = this.writeChain
      .then(async () => {
        await fs.mkdir(dirname(this.file), { recursive: true });
        const tmp = `${this.file}.tmp`;
        await fs.writeFile(tmp, snapshot, "utf8");
        await fs.rename(tmp, this.file);
      })
      .catch((err) => {
        console.error("[store] 持久化失败", err);
      });
    return this.writeChain;
  }

  // ---------- 用户 ----------
  getUser(username: string): UserRecord | undefined {
    return this.db.users[username];
  }

  async createUser(user: UserRecord): Promise<void> {
    this.db.users[user.username] = user;
    await this.persist();
  }

  /** 删除用户及其全部数据与会话（账号注销）。 */
  async deleteUser(username: string): Promise<void> {
    delete this.db.users[username];
    delete this.db.data[username];
    for (const [token, s] of Object.entries(this.db.sessions)) {
      if (s.username === username) delete this.db.sessions[token];
    }
    await this.persist();
  }

  // ---------- 会话 ----------
  async createSession(token: string, username: string): Promise<void> {
    this.db.sessions[token] = { username, createdAt: Date.now() };
    await this.persist();
  }

  getSessionUser(token: string): string | undefined {
    return this.db.sessions[token]?.username;
  }

  async deleteSession(token: string): Promise<void> {
    delete this.db.sessions[token];
    await this.persist();
  }

  async deleteUserSessions(username: string): Promise<void> {
    for (const [token, s] of Object.entries(this.db.sessions)) {
      if (s.username === username) delete this.db.sessions[token];
    }
    await this.persist();
  }

  // ---------- 数据 ----------
  /** 取某用户某命名空间的全部记录（不存在 → 空对象）。 */
  getData(username: string, ns: string): Record<string, DataRecord> {
    return this.db.data[username]?.[ns] ?? {};
  }

  /** 上行合并：对每条推送记录做 last-write-wins（updatedAt 大者胜；相等则以推送为准覆盖）。 */
  async upsertData(
    username: string,
    ns: string,
    records: Array<{ key: string; value: unknown; updatedAt: number }>,
  ): Promise<void> {
    const userData = (this.db.data[username] ??= {});
    const nsData = (userData[ns] ??= {});
    for (const r of records) {
      const existing = nsData[r.key];
      if (!existing || r.updatedAt >= existing.updatedAt) {
        nsData[r.key] = { value: r.value, updatedAt: r.updatedAt };
      }
    }
    await this.persist();
  }
}
