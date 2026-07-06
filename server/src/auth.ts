//! 鉴权原语：口令哈希（scrypt，加盐）+ 会话令牌生成。全用 Node 内置 crypto，无第三方依赖。
//!
//! 安全：口令**从不明文存储**——存 scrypt 派生哈希 + 每用户随机盐；校验用 timingSafeEqual 防时序侧信道。

import { randomBytes, scryptSync, timingSafeEqual } from "node:crypto";

/** 派生密钥长度（字节）。 */
const KEYLEN = 64;

/** 用 scrypt 派生口令哈希；不传 salt 则随机生成。返回 hex 编码的 hash 与 salt。 */
export function hashPassword(password: string, salt?: string): { hash: string; salt: string } {
  const s = salt ?? randomBytes(16).toString("hex");
  const h = scryptSync(password, s, KEYLEN).toString("hex");
  return { hash: h, salt: s };
}

/** 校验口令是否与存储的哈希匹配（timing-safe）。 */
export function verifyPassword(password: string, salt: string, expectedHash: string): boolean {
  const actual = scryptSync(password, salt, KEYLEN);
  const expected = Buffer.from(expectedHash, "hex");
  // 长度不等直接失败（timingSafeEqual 要求等长）
  return actual.length === expected.length && timingSafeEqual(actual, expected);
}

/** 生成随机会话令牌（hex）。 */
export function generateToken(bytes = 32): string {
  return randomBytes(bytes).toString("hex");
}
