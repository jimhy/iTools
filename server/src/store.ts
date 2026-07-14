//! 持久化存储：MariaDB（mysql2/promise 连接池）。服务端强制依赖 MariaDB。
//!
//! 结构清晰、单一职责：方法签名与旧 JsonStore 语义等价，但**全异步**。
//! last-write-wins 由 `updatedAt`（大者胜；相等以推送为准）决定，与旧实现一致。

import { createPool, type Pool, type PoolConnection, type RowDataPacket } from "mysql2/promise";

import type { DbConfig } from "./config";

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

const CREATE_USERS = `
  CREATE TABLE IF NOT EXISTS users (
    username VARCHAR(190) PRIMARY KEY,
    password_hash TEXT NOT NULL,
    salt VARCHAR(64) NOT NULL,
    created_at BIGINT NOT NULL
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4`;

const CREATE_SESSIONS = `
  CREATE TABLE IF NOT EXISTS sessions (
    token VARCHAR(190) PRIMARY KEY,
    username VARCHAR(190) NOT NULL,
    created_at BIGINT NOT NULL,
    INDEX idx_sessions_username (username)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4`;

const CREATE_DATA = `
  CREATE TABLE IF NOT EXISTS data_records (
    username VARCHAR(190) NOT NULL,
    ns VARCHAR(190) NOT NULL,
    k VARCHAR(255) NOT NULL,
    v LONGTEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (username, ns, k)
  ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4`;

export class MariaDbStore {
  private pool: Pool | undefined;

  constructor(private readonly config: DbConfig) {}

  /** 连接 + 建表（幂等）。连不上或建表失败会抛错（由调用方处理并退出）。 */
  async init(): Promise<void> {
    const { host, port, user, password, database, connectionLimit } = this.config;

    // 先用一条不指定库的连接确保目标库存在（幂等），避免库未建时直接连库报错。
    const bootstrap = createPool({
      host,
      port,
      user,
      password,
      connectionLimit: 1,
      charset: "utf8mb4_general_ci",
    });
    try {
      const safe = database.replace(/`/g, "``");
      await bootstrap.query(`CREATE DATABASE IF NOT EXISTS \`${safe}\` CHARACTER SET utf8mb4`);
    } finally {
      await bootstrap.end();
    }

    this.pool = createPool({
      host,
      port,
      user,
      password,
      database,
      connectionLimit,
      waitForConnections: true,
      charset: "utf8mb4_general_ci",
    });

    await this.pool.query(CREATE_USERS);
    await this.pool.query(CREATE_SESSIONS);
    await this.pool.query(CREATE_DATA);
  }

  /** 关闭连接池（进程退出 / 测试收尾）。 */
  async close(): Promise<void> {
    await this.pool?.end();
    this.pool = undefined;
  }

  private db(): Pool {
    if (!this.pool) throw new Error("MariaDbStore 未初始化：请先 await init()");
    return this.pool;
  }

  // ---------- 用户 ----------
  async getUser(username: string): Promise<UserRecord | undefined> {
    const [rows] = await this.db().query<RowDataPacket[]>(
      "SELECT username, password_hash AS passwordHash, salt, created_at AS createdAt FROM users WHERE username = ?",
      [username],
    );
    const r = rows[0];
    if (!r) return undefined;
    return {
      username: r.username as string,
      passwordHash: r.passwordHash as string,
      salt: r.salt as string,
      createdAt: Number(r.createdAt),
    };
  }

  async createUser(user: UserRecord): Promise<void> {
    // set 语义（与旧实现一致）：同名覆盖，避免并发下的重复键报错。
    await this.db().query(
      `INSERT INTO users (username, password_hash, salt, created_at) VALUES (?, ?, ?, ?)
       ON DUPLICATE KEY UPDATE password_hash = VALUES(password_hash), salt = VALUES(salt), created_at = VALUES(created_at)`,
      [user.username, user.passwordHash, user.salt, user.createdAt],
    );
  }

  /** 删除用户及其全部数据与会话（账号注销），事务保证一致性。 */
  async deleteUser(username: string): Promise<void> {
    const conn: PoolConnection = await this.db().getConnection();
    try {
      await conn.beginTransaction();
      await conn.query("DELETE FROM data_records WHERE username = ?", [username]);
      await conn.query("DELETE FROM sessions WHERE username = ?", [username]);
      await conn.query("DELETE FROM users WHERE username = ?", [username]);
      await conn.commit();
    } catch (err) {
      await conn.rollback();
      throw err;
    } finally {
      conn.release();
    }
  }

  // ---------- 会话 ----------
  async createSession(token: string, username: string): Promise<void> {
    await this.db().query(
      `INSERT INTO sessions (token, username, created_at) VALUES (?, ?, ?)
       ON DUPLICATE KEY UPDATE username = VALUES(username), created_at = VALUES(created_at)`,
      [token, username, Date.now()],
    );
  }

  async getSessionUser(token: string): Promise<string | undefined> {
    const [rows] = await this.db().query<RowDataPacket[]>(
      "SELECT username FROM sessions WHERE token = ?",
      [token],
    );
    const r = rows[0];
    return r ? (r.username as string) : undefined;
  }

  async deleteSession(token: string): Promise<void> {
    await this.db().query("DELETE FROM sessions WHERE token = ?", [token]);
  }

  async deleteUserSessions(username: string): Promise<void> {
    await this.db().query("DELETE FROM sessions WHERE username = ?", [username]);
  }

  // ---------- 数据 ----------
  /** 取某用户某命名空间的全部记录（不存在 → 空对象）。 */
  async getData(username: string, ns: string): Promise<Record<string, DataRecord>> {
    const [rows] = await this.db().query<RowDataPacket[]>(
      "SELECT k, v, updated_at AS updatedAt FROM data_records WHERE username = ? AND ns = ?",
      [username, ns],
    );
    const out: Record<string, DataRecord> = {};
    for (const r of rows) {
      out[r.k as string] = {
        value: JSON.parse(r.v as string),
        updatedAt: Number(r.updatedAt),
      };
    }
    return out;
  }

  /** 上行合并：对每条推送记录做 last-write-wins（updatedAt 大者胜；相等以推送为准覆盖）。 */
  async upsertData(
    username: string,
    ns: string,
    records: Array<{ key: string; value: unknown; updatedAt: number }>,
  ): Promise<void> {
    if (records.length === 0) return;
    // JSON.stringify(undefined) 为 undefined，会违反 NOT NULL；退化为 "null"（真实契约里 value 恒存在）。
    const values = records.map((r) => {
      const v = JSON.stringify(r.value);
      return [username, ns, r.key, v === undefined ? "null" : v, r.updatedAt];
    });
    await this.db().query(
      `INSERT INTO data_records (username, ns, k, v, updated_at) VALUES ?
       ON DUPLICATE KEY UPDATE
         v = IF(VALUES(updated_at) >= updated_at, VALUES(v), v),
         updated_at = GREATEST(updated_at, VALUES(updated_at))`,
      [values],
    );
  }
}
