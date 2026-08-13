//! 最小 ZIP 读取器（零依赖，只用 node:zlib）。
//!
//! # 为什么要自己写，而不是调 unzip / tar / 某个 npm 包
//!
//! 这个脚本算出来的内容哈希，必须与**客户端解压后算出来的**逐字节一致，否则市场里每个插件
//! 都会在安装时报「内容校验失败」。于是有两条硬约束：
//!
//! 1. **不能用 `git clone` 取源码**：插件仓库若带 `.gitattributes` 的 eol 设置，检出时 git 会
//!    改写换行符，得到的字节与客户端下载的 zip 里的内容不同 —— 哈希必然对不上。
//!    客户端走的是「下载 zip → 解压」，CI 就必须走同一条路。
//! 2. **不能依赖外部命令**：`unzip` 在 Windows 上没有、`tar` 的 zip 支持随版本变化，
//!    维护者本地跑和 CI 上跑必须得到同一个结果。
//!
//! 支持 store(0) 与 deflate(8) 两种压缩方法 —— GitHub 的归档只用这两种。
//! 遇到其它方法直接报错，绝不静默跳过（跳过会让哈希漏算文件，等于校验形同虚设）。

import { inflateRawSync } from "node:zlib";

const EOCD_SIG = 0x06054b50;
const EOCD64_LOCATOR_SIG = 0x07064b50;
const EOCD64_SIG = 0x06064b50;
const CENTRAL_SIG = 0x02014b50;
const LOCAL_SIG = 0x04034b50;

/** 从尾部回扫定位 EOCD（注释最长 65535，加上 EOCD 自身 22 字节）。 */
function findEocd(buf) {
  const maxBack = Math.min(buf.length, 22 + 0xffff);
  for (let i = buf.length - 22; i >= buf.length - maxBack; i--) {
    if (i >= 0 && buf.readUInt32LE(i) === EOCD_SIG) return i;
  }
  throw new Error("不是合法的 ZIP：找不到 End of Central Directory");
}

/**
 * 读出 zip 里的全部**文件**条目。
 *
 * 返回 `Map<相对路径, Buffer>`；目录条目与符号链接条目会被剔除：
 * 目录本身不参与内容哈希；符号链接在客户端侧是被明确拒收的，CI 也不能把它算进去。
 */
export function readZipEntries(buf) {
  const eocd = findEocd(buf);
  let entryCount = buf.readUInt16LE(eocd + 10);
  let cdOffset = buf.readUInt32LE(eocd + 16);

  // ZIP64：EOCD 里的字段被写成全 1 时，真实值在 ZIP64 EOCD 里
  if (entryCount === 0xffff || cdOffset === 0xffffffff) {
    const locator = eocd - 20;
    if (locator < 0 || buf.readUInt32LE(locator) !== EOCD64_LOCATOR_SIG) {
      throw new Error("ZIP64 标记存在但找不到 ZIP64 EOCD Locator");
    }
    const z64 = Number(buf.readBigUInt64LE(locator + 8));
    if (buf.readUInt32LE(z64) !== EOCD64_SIG) throw new Error("ZIP64 EOCD 签名不对");
    entryCount = Number(buf.readBigUInt64LE(z64 + 32));
    cdOffset = Number(buf.readBigUInt64LE(z64 + 48));
  }

  const out = new Map();
  let p = cdOffset;
  for (let i = 0; i < entryCount; i++) {
    if (buf.readUInt32LE(p) !== CENTRAL_SIG) throw new Error(`第 ${i} 个中央目录条目签名不对`);
    const method = buf.readUInt16LE(p + 10);
    const compSize = buf.readUInt32LE(p + 20);
    const rawSize = buf.readUInt32LE(p + 24);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const commentLen = buf.readUInt16LE(p + 32);
    const externalAttrs = buf.readUInt32LE(p + 38);
    const localOffset = buf.readUInt32LE(p + 42);
    const name = buf.toString("utf8", p + 46, p + 46 + nameLen);
    p += 46 + nameLen + extraLen + commentLen;

    if (name.endsWith("/")) continue; // 目录条目

    // 高 16 位是 Unix mode；0xA000 = S_IFLNK。客户端明确拒收符号链接，CI 同样不计入。
    const unixMode = (externalAttrs >>> 16) & 0xffff;
    if ((unixMode & 0xf000) === 0xa000) {
      throw new Error(`归档内含符号链接条目：${name}（客户端会拒绝安装，收录前请移除）`);
    }

    // 从 local header 取数据：它的变长字段长度与中央目录里的可能不同，必须重读
    if (buf.readUInt32LE(localOffset) !== LOCAL_SIG) {
      throw new Error(`${name} 的 local header 签名不对`);
    }
    const lNameLen = buf.readUInt16LE(localOffset + 26);
    const lExtraLen = buf.readUInt16LE(localOffset + 28);
    const dataStart = localOffset + 30 + lNameLen + lExtraLen;
    const raw = buf.subarray(dataStart, dataStart + compSize);

    let content;
    if (method === 0) content = Buffer.from(raw);
    else if (method === 8) content = inflateRawSync(raw);
    else throw new Error(`${name} 使用了不支持的压缩方法 ${method}`);

    if (content.length !== rawSize) {
      throw new Error(`${name} 解压后长度 ${content.length} 与记录的 ${rawSize} 不符`);
    }
    out.set(name, content);
  }
  return out;
}

/**
 * 剥掉归档的单一顶层目录（GitHub 归档形如 `repo-<sha>/…`）。
 *
 * 与客户端 `install.rs` 的口径一致：**不硬编码目录名**，只在「恰好只有一个顶层目录」时剥离，
 * 否则原样返回。
 */
export function stripSingleRoot(entries) {
  const roots = new Set();
  for (const name of entries.keys()) roots.add(name.split("/")[0]);
  if (roots.size !== 1) return entries;
  const root = [...roots][0] + "/";
  const out = new Map();
  for (const [name, content] of entries) {
    if (!name.startsWith(root)) return entries; // 保守起见：有不在该根下的条目就不剥
    out.set(name.slice(root.length), content);
  }
  return out;
}

/** 取子目录下的条目，并把路径改成相对该子目录。`sub` 为空串时原样返回。 */
export function subtree(entries, sub) {
  if (!sub) return entries;
  const prefix = sub.replace(/^\/+|\/+$/g, "") + "/";
  const out = new Map();
  for (const [name, content] of entries) {
    if (name.startsWith(prefix)) out.set(name.slice(prefix.length), content);
  }
  return out;
}
