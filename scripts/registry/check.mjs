#!/usr/bin/env node
//! 插件收录条目的机械校验 + index.json 聚合。
//!
//! 用法：
//!   node scripts/registry/check.mjs            # 校验全部条目，并重新生成 registry/index.json
//!   node scripts/registry/check.mjs --check    # 只校验不写文件（PR 检查用，有问题以非零码退出）
//!   node scripts/registry/check.mjs --only foo # 只处理某一个条目（本地调试用）
//!
//! # 这个脚本负责什么、不负责什么
//!
//! **负责**（机械可判定的部分，全自动）：
//!   - 条目 JSON 符合 schema；文件名与 name 一致；name 全局唯一
//!   - 按 `revision` 那个**确切 commit** 拉归档，核对里面的 plugin.json 与条目声明一致
//!     （name / version / permissions 三项对不上直接拒——否则市场展示的信息就是假的）
//!   - 插件目录结构合法（有 index.html）、不含可执行文件
//!   - 计算**内容哈希**并写进 index.json（客户端安装时逐字节校验，见 hash.mjs 的说明）
//!
//! **不负责**（需要人看的部分）：代码是否有恶意行为、申请的权限是否名副其实、描述是否属实。
//! 这些原计划由 AI 审核承担，当前**尚未启用**（见 README 的「审核现状」）。
//! 脚本不会假装做过这些判断，index.json 里也不会出现任何暗示「已审核代码」的字段。

import { readFileSync, writeFileSync, readdirSync, existsSync, mkdirSync } from "node:fs";
import { resolve, dirname, basename, join } from "node:path";
import { fileURLToPath } from "node:url";
import { readZipEntries, stripSingleRoot, subtree } from "./zip.mjs";
import { contentHash } from "./hash.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "../..");
const ENTRIES_DIR = resolve(ROOT, "registry/entries");
const INDEX_PATH = resolve(ROOT, "registry/index.json");
const SCHEMA_PATH = resolve(ROOT, "registry/schema/entry.schema.json");

/** 与客户端 `install.rs::DENIED_EXTS` 逐项一致。改这里必须同步改那边，否则会出现
 *  「收录时放行、安装时被拒」的分叉——用户装不上，而市场显示一切正常。 */
const DENIED_EXTS = new Set([
  "exe", "dll", "bat", "cmd", "com", "scr", "ps1", "psm1", "msi", "vbs", "vbe", "wsf", "jar",
  "sys", "drv", "cpl", "lnk", "url", "hta", "reg", "scf", "pif", "msc",
]);

/** 归档下载上限，与客户端 `MAX_TOTAL_BYTES` 同量级。 */
const MAX_ARCHIVE_BYTES = 32 * 1024 * 1024;

const C = process.stdout.isTTY
  ? { r: "\x1b[31m", g: "\x1b[32m", y: "\x1b[33m", b: "\x1b[1m", x: "\x1b[0m" }
  : { r: "", g: "", y: "", b: "", x: "" };

const argv = process.argv.slice(2);
const CHECK_ONLY = argv.includes("--check");
const ONLY = (() => {
  const i = argv.indexOf("--only");
  return i >= 0 ? argv[i + 1] : null;
})();

// ---------------------------------------------------------------- 精简 schema 校验
// 不引第三方（本仓零依赖惯例）。只实现 entry.schema.json 实际用到的关键字；
// 遇到不认识的关键字直接报错，而不是默默放行——放行等于校验形同虚设。
const SUPPORTED = new Set([
  "$schema", "$id", "title", "description", "type", "properties", "required",
  "additionalProperties", "pattern", "enum", "minLength", "maxLength", "maxItems",
  "minItems", "items", "default", "format",
]);

function validate(node, value, path, errs) {
  for (const k of Object.keys(node)) {
    if (!SUPPORTED.has(k)) errs.push(`${path}: schema 用了本校验器不支持的关键字 \`${k}\``);
  }
  if (node.type === "object") {
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      return errs.push(`${path}: 应为对象`);
    }
    for (const req of node.required ?? []) {
      if (!(req in value)) errs.push(`${path}: 缺少必填字段 \`${req}\``);
    }
    if (node.additionalProperties === false && node.properties) {
      for (const k of Object.keys(value)) {
        if (!(k in node.properties)) errs.push(`${path}: 出现未定义的字段 \`${k}\``);
      }
    }
    for (const [k, sub] of Object.entries(node.properties ?? {})) {
      if (k in value) validate(sub, value[k], `${path}.${k}`, errs);
    }
    if (node.additionalProperties && typeof node.additionalProperties === "object") {
      for (const [k, v] of Object.entries(value)) {
        validate(node.additionalProperties, v, `${path}.${k}`, errs);
      }
    }
    return;
  }
  if (node.type === "array") {
    if (!Array.isArray(value)) return errs.push(`${path}: 应为数组`);
    if (node.maxItems != null && value.length > node.maxItems) {
      errs.push(`${path}: 最多 ${node.maxItems} 项，实际 ${value.length}`);
    }
    if (node.minItems != null && value.length < node.minItems) {
      errs.push(`${path}: 至少 ${node.minItems} 项`);
    }
    value.forEach((v, i) => node.items && validate(node.items, v, `${path}[${i}]`, errs));
    return;
  }
  if (node.type === "boolean") {
    if (typeof value !== "boolean") errs.push(`${path}: 应为布尔`);
    return;
  }
  if (node.type === "string") {
    if (typeof value !== "string") return errs.push(`${path}: 应为字符串`);
    if (node.minLength != null && value.length < node.minLength) {
      errs.push(`${path}: 至少 ${node.minLength} 个字符`);
    }
    if (node.maxLength != null && value.length > node.maxLength) {
      errs.push(`${path}: 最多 ${node.maxLength} 个字符，实际 ${value.length}`);
    }
    if (node.pattern && !new RegExp(node.pattern).test(value)) {
      errs.push(`${path}: 不匹配 \`${node.pattern}\`（当前值 ${JSON.stringify(value)}）`);
    }
    if (node.enum && !node.enum.includes(value)) {
      errs.push(`${path}: 只能是 ${node.enum.join(" / ")} 之一`);
    }
    if (node.format === "uri" && !/^https?:\/\/\S+$/.test(value)) {
      errs.push(`${path}: 应为 http(s) URL`);
    }
    return;
  }
}

// ---------------------------------------------------------------- 归档下载
async function fetchArchive(owner, repo, revision) {
  const url = `https://codeload.github.com/${owner}/${repo}/zip/${revision}`;
  let resp;
  try {
    resp = await fetch(url, { redirect: "follow" });
  } catch (e) {
    throw new Error(
      `下载归档失败：${e.message}\n  ${url}\n` +
        `  若你在本地运行且需要代理，请用支持 --use-env-proxy 的 Node 版本，或直接让 CI 跑。`,
    );
  }
  if (!resp.ok) {
    const hint =
      resp.status === 404
        ? "（仓库不存在 / 是私有仓库 / 这个 commit sha 不在该仓库里）"
        : resp.status === 403
          ? "（被限流，稍后重试）"
          : "";
    throw new Error(`下载归档失败：HTTP ${resp.status} ${hint}\n  ${url}`);
  }
  const buf = Buffer.from(await resp.arrayBuffer());
  if (buf.length > MAX_ARCHIVE_BYTES) {
    throw new Error(`归档过大：${(buf.length / 1048576).toFixed(1)} MB，上限 32 MB`);
  }
  return buf;
}

/** 与客户端 `denied_ext` 同口径：先剥掉结尾的点与空格（Windows 落盘会剥），再取最后一段扩展名。 */
function deniedExt(path) {
  const name = basename(path).replace(/[. ]+$/, "");
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  return DENIED_EXTS.has(ext) ? ext : null;
}

// ---------------------------------------------------------------- 单条校验
async function checkEntry(entry, file, schema, seenNames) {
  const errs = [];
  const warns = [];
  validate(schema, entry, "条目", errs);
  if (errs.length) return { errs, warns };

  const expectFile = `${entry.name}.json`;
  if (basename(file) !== expectFile) {
    errs.push(`文件名应为 ${expectFile}（与 name 一致），实际是 ${basename(file)}`);
  }
  if (seenNames.has(entry.name)) {
    errs.push(`name \`${entry.name}\` 与另一个条目重复——插件目录名与数据命名空间都按 name 走，必须全局唯一`);
  }
  seenNames.add(entry.name);
  if (entry.revoked && !entry.revokedReason) {
    errs.push("revoked=true 时必须写 revokedReason（会原样展示给已安装的用户）");
  }
  for (const p of entry.permissions ?? []) {
    if (!entry.permissionReasons?.[p]) {
      errs.push(`声明了权限 \`${p}\` 却没在 permissionReasons 里说明用途`);
    }
  }
  if (errs.length) return { errs, warns };

  const m = /^https:\/\/github\.com\/([A-Za-z0-9._-]+)\/([A-Za-z0-9._-]+)$/.exec(entry.repo);
  const [, owner, repo] = m;

  const zip = await fetchArchive(owner, repo, entry.revision);
  let entries;
  try {
    entries = subtree(stripSingleRoot(readZipEntries(zip)), entry.path ?? "");
  } catch (e) {
    return { errs: [`读取归档失败：${e.message}`], warns };
  }
  if (entries.size === 0) {
    return {
      errs: [`归档里 path=\`${entry.path ?? ""}\` 下没有任何文件（子目录写错了？）`],
      warns,
    };
  }

  // 结构与内容
  const manifestRaw = entries.get("plugin.json");
  if (!manifestRaw) return { errs: ["该目录下没有 plugin.json"], warns };
  if (!entries.has("index.html")) errs.push("该目录下没有 index.html（插件无法运行）");

  let manifest;
  try {
    manifest = JSON.parse(manifestRaw.toString("utf8"));
  } catch (e) {
    return { errs: [`plugin.json 不是合法 JSON：${e.message}`], warns };
  }

  if (manifest.name !== entry.name) {
    errs.push(`plugin.json 的 name 是 \`${manifest.name}\`，与条目声明的 \`${entry.name}\` 不一致`);
  }
  if (manifest.version !== entry.version) {
    errs.push(
      `plugin.json 的 version 是 \`${manifest.version}\`，与条目声明的 \`${entry.version}\` 不一致` +
        `（条目里的 version 必须是 revision 那个 commit 下的真实版本）`,
    );
  }
  const declared = [...(entry.permissions ?? [])].sort().join(",");
  const actual = [...(manifest.permissions ?? [])].sort().join(",");
  if (declared !== actual) {
    errs.push(
      `权限声明不一致：条目写 [${declared || "无"}]，plugin.json 写 [${actual || "无"}]` +
        `（市场展示的权限必须与插件真实申请的一致，否则用户看到的授权提示是假的）`,
    );
  }

  for (const path of entries.keys()) {
    const ext = deniedExt(path);
    if (ext) errs.push(`包含可执行文件 \`${path}\`（.${ext} 属于双击即执行类型，客户端会整包拒绝安装）`);
  }

  // 关键字冲突只提示、不阻断
  const cmds = [];
  for (const f of manifest.features ?? []) {
    for (const c of f.cmds ?? []) if (typeof c === "string") cmds.push(c);
  }
  const declaredKw = new Set(entry.keywords ?? []);
  for (const c of cmds) {
    if (!declaredKw.has(c)) warns.push(`plugin.json 里的关键字 \`${c}\` 没写进条目的 keywords`);
  }

  const hash = contentHash(entries);
  return { errs, warns, hash, fileCount: entries.size };
}

// ---------------------------------------------------------------- 主流程
const schema = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));
if (!existsSync(ENTRIES_DIR)) mkdirSync(ENTRIES_DIR, { recursive: true });

const files = readdirSync(ENTRIES_DIR)
  .filter((f) => f.endsWith(".json"))
  .filter((f) => !ONLY || f === `${ONLY}.json`)
  .sort();

if (files.length === 0) {
  console.log(`${C.y}registry/entries/ 下还没有任何条目${C.x}——市场当前为空，这是正常的初始状态。`);
}

const seenNames = new Set();
const plugins = [];
let failed = 0;

for (const f of files) {
  const full = join(ENTRIES_DIR, f);
  let entry;
  try {
    entry = JSON.parse(readFileSync(full, "utf8"));
  } catch (e) {
    console.error(`${C.r}✗ ${f}${C.x}\n   不是合法 JSON：${e.message}`);
    failed++;
    continue;
  }

  process.stdout.write(`· ${f} … `);
  let res;
  try {
    res = await checkEntry(entry, full, schema, seenNames);
  } catch (e) {
    res = { errs: [e.message], warns: [] };
  }

  if (res.errs.length) {
    console.log(`${C.r}不通过${C.x}`);
    for (const e of res.errs) console.error(`   ${C.r}✗${C.x} ${e}`);
    failed++;
    continue;
  }
  console.log(`${C.g}通过${C.x}  ${res.fileCount} 个文件  ${res.hash.slice(0, 22)}…`);
  for (const w of res.warns) console.log(`   ${C.y}!${C.x} ${w}`);

  plugins.push({ ...entry, contentHash: res.hash, fileCount: res.fileCount });
}

// 用 process.exitCode 而不是 process.exit()：本脚本跑过 fetch，退出时 undici 的连接可能还没关完，
// process.exit() 会在 Windows 上撞 libuv 断言（`UV_HANDLE_CLOSING`, src\win\async.c）并把退出码
// 改成 127 —— CI 虽然照样判失败，但输出里多一行 C 断言崩溃，排查的人会以为是脚本自己坏了。
// 设置 exitCode 后让事件循环自然收尾，退出码才是我们想表达的那个。
if (failed) {
  console.error(`\n${C.r}${C.b}✗ ${failed} 个条目未通过${C.x}`);
  process.exitCode = 1;
} else if (CHECK_ONLY) {
  console.log(`\n${C.g}✓ 全部条目通过校验${C.x}（--check：未写入 index.json）`);
} else {
  // index.json 的字段是**客户端契约**：改动必须同步 src/types.ts 的 MarketIndex / MarketEntry。
  // 刻意不写生成时间戳——那会让每次 CI 运行都产生 diff，掩盖真正的条目变化。
  const index = {
    version: 1,
    plugins: plugins.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0)),
  };
  writeFileSync(INDEX_PATH, JSON.stringify(index, null, 2) + "\n", "utf8");
  console.log(`\n${C.g}✓ 已写入 registry/index.json${C.x}（${plugins.length} 个插件）`);
}
