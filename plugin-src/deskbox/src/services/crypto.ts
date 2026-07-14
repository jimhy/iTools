/**
 * 加密服务：PBKDF2 → AES-GCM-256。真实加密（非混淆），逻辑迁移自 vanilla 版。
 * 主密钥仅存内存（不落盘）；密文块用 base64 存储。
 */
import type { EncBlob } from "../types";

const te = new TextEncoder();
const td = new TextDecoder();

/** PBKDF2 迭代次数（提高只影响新建/改密；旧库按记录里的 iter 解密仍可用）。 */
export const PBKDF2_ITER = 600_000;
/** 校验文本：用主密钥加密它得到 verifier，解锁时解密比对以判断口令是否正确。 */
export const VERIFIER_TEXT = "deskbox::verify::v1";
/** 运行环境是否支持 Web Crypto（无则密码库不可用，需在 UI 诚实告知）。 */
export const cryptoOk = Boolean(window.crypto?.subtle?.deriveKey);

export function bufToB64(buf: ArrayBuffer | Uint8Array): string {
  const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
}

export function b64ToBuf(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64);
  const u = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
  return u;
}

export async function deriveKey(pw: string, salt: BufferSource, iter: number): Promise<CryptoKey> {
  const base = await crypto.subtle.importKey("raw", te.encode(pw), { name: "PBKDF2" }, false, ["deriveKey"]);
  return crypto.subtle.deriveKey(
    { name: "PBKDF2", salt, iterations: iter, hash: "SHA-256" },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

export async function encWith(key: CryptoKey, text: string): Promise<EncBlob> {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, te.encode(text));
  return { iv: bufToB64(iv), ct: bufToB64(ct) };
}

export async function decWith(key: CryptoKey, blob: EncBlob): Promise<string> {
  const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv: b64ToBuf(blob.iv) }, key, b64ToBuf(blob.ct));
  return td.decode(pt);
}

/** 密码强度：长度 + 字符种类 → 0 弱 / 1 中 / 2 强。仅用于展示强度点，不涉及明文存储。 */
export function calcStrength(pw: string): 0 | 1 | 2 {
  if (!pw) return 0;
  const classes = [/[a-z]/, /[A-Z]/, /[0-9]/, /[^a-zA-Z0-9]/].filter((r) => r.test(pw)).length;
  let s = 0;
  if (pw.length >= 8) s++;
  if (pw.length >= 14) s++;
  if (classes >= 3) s++;
  if (classes >= 4) s++;
  return s >= 4 ? 2 : s >= 2 ? 1 : 0;
}

/** 生成强密码（避开易混字符）。 */
export function genPassword(len: number, opts: { lower: boolean; upper: boolean; digit: boolean; sym: boolean }): string {
  let pool = "";
  if (opts.lower) pool += "abcdefghijkmnpqrstuvwxyz";
  if (opts.upper) pool += "ABCDEFGHJKLMNPQRSTUVWXYZ";
  if (opts.digit) pool += "23456789";
  if (opts.sym) pool += "!@#$%^&*()-_=+[]{}";
  if (!pool) pool = "abcdefghijkmnpqrstuvwxyz23456789";
  const arr = crypto.getRandomValues(new Uint32Array(len));
  let out = "";
  for (let i = 0; i < len; i++) out += pool[arr[i] % pool.length];
  return out;
}
