//! 插件包**内容哈希**（市场信任链的地基）。
//!
//! # 为什么不直接用 zip 的 sha256
//!
//! GitHub 的归档**不是字节级确定的**：同一个 commit 在不同时间下载，zip 可能因为压缩实现变化
//! 而得到不同的哈希（2023 年 GitHub 更换压缩库时，全网依赖归档 checksum 的工具集体失效过一次）。
//! 拿它当校验依据，早晚会变成「市场里所有插件突然全部校验失败」。
//!
//! 所以校验的是**内容**而不是**容器**：只跟每个文件的路径与字节有关，与怎么压、什么时候压、
//! 文件时间戳、目录项顺序统统无关。
//!
//! # 算法（客户端 Rust 侧必须逐字实现同一套，任何一处不同都会导致全量校验失败）
//!
//! ```text
//! 对插件目录下的每个【文件】（目录本身不算，符号链接在读 zip 时就已拒绝）：
//!     rel  = 相对插件目录的路径，分隔符固定为 '/'，无前导 './'
//!     line = rel + "\t" + hex(sha256(文件字节))
//! 把所有 line 按 rel 的 **UTF-8 字节序**升序排序        ← 不是本地化排序，不是 UTF-16 序
//! body = lines.join("\n")                                ← 用 \n，行尾不留
//! contentHash = "sha256:" + hex(sha256(utf8(body)))
//! ```
//!
//! 排序用字节序是为了跨语言一致：Rust 的 `String` 排序天然是 UTF-8 字节序，
//! 而 JS 的 `Array.sort()` 默认按 UTF-16 code unit 比较 —— 对纯 ASCII 路径两者一致，
//! 但插件里只要有一个中文文件名就会分道扬镳。这里显式用 Buffer.compare 对齐 Rust。

import { createHash } from "node:crypto";

const sha256hex = (buf) => createHash("sha256").update(buf).digest("hex");

/**
 * 计算内容哈希。
 * @param {Map<string, Buffer>} entries 相对路径 → 文件字节
 * @returns {string} 形如 `sha256:9f86d0…`
 */
export function contentHash(entries) {
  const rows = [];
  for (const [rel, content] of entries) {
    rows.push({ key: Buffer.from(rel, "utf8"), line: `${rel}\t${sha256hex(content)}` });
  }
  rows.sort((a, b) => Buffer.compare(a.key, b.key));
  const body = rows.map((r) => r.line).join("\n");
  return `sha256:${sha256hex(Buffer.from(body, "utf8"))}`;
}

/** 供测试/排错：打印参与哈希的明细，用于两端结果不一致时逐行比对。 */
export function contentHashDetail(entries) {
  const rows = [];
  for (const [rel, content] of entries) {
    rows.push({ key: Buffer.from(rel, "utf8"), rel, sha: sha256hex(content), size: content.length });
  }
  rows.sort((a, b) => Buffer.compare(a.key, b.key));
  return rows.map(({ rel, sha, size }) => ({ rel, sha, size }));
}

export { sha256hex };
