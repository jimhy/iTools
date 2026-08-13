#!/usr/bin/env node
/**
 * check-contract.mjs —— 前后端**双向**契约校验
 * ============================================
 *
 * ## 为什么需要它
 *
 * 前端拿后端数据走的是 `invoke<T>(...)`，而 `invoke<T>` 的 `T` **只是一个类型断言**：
 * 它不校验运行期真实返回了什么。于是 Rust 结构体和 `src/types.ts` 的 interface 一旦对不上，
 * `cargo test` 全绿、`tsc --noEmit` 也全绿，两边各自「编译通过」，功能却在运行期静默断掉
 * （前端读到 `undefined`，UI 显示空白 / 恒假，没有任何报错）。
 *
 * 本仓库已经连续两轮栽在同一个成因上：
 *   - 上上轮：Rust 侧 `MirrorConfigView` 是扁平结构，TS 却声明成嵌套的 `{ config: … }`，
 *     结果「插件下载源」面板对着 3 个正在生效的内置镜像显示「当前配置里没有任何镜像站」。
 *   - 上一轮：为此在 Rust 侧加了契约快照测试（`contract_field_names_frozen`）。但它**只守单侧** ——
 *     本轮 Rust 给 `MirrorEntry` 加了 `host` / `unlisted` 两个字段、快照测试也同步钉成 9 个字段，
 *     `src/types.ts` 仍是 7 个。测试全绿、tsc 全绿，功能照样断在前端。
 *
 * 结论：只钉 Rust 一侧的快照是不够的，必须有人把**两侧的字段名集合真正比一遍**。这就是本脚本。
 *
 * ## 怎么用
 *
 *   npm run check:contract          # 校验，有差异则非零退出并打印中文差异报告
 *   npm run check                   # tsc --noEmit + 本校验（提交前建议跑这个）
 *   node scripts/check-contract.mjs --snapshot=<路径>   # 指定字段清单文件
 *   node scripts/check-contract.mjs --allow-missing     # 清单缺失时只告警不失败（应急用，见下）
 *   node scripts/check-contract.mjs --list              # 只打印两侧解析结果，不做判定
 *
 * 退出码：0 = 一致；1 = 存在差异 / 清单缺失 / 解析失败。
 *
 * ## 字段清单（Rust 侧）如何重新生成
 *
 * Rust 侧的 `contract_field_names_frozen` 测试会把「发给前端的结构体的顶层字段名集合」
 * 序列化后写成一份机器可读的 JSON（默认 `src-tauri/contract-snapshot.json`）。重新生成：
 *
 *   cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen
 *
 * ⚠ 本仓库共享 target 目录，并行 rustc 会触发 LLVM OOM / 0xc0000005，**必须带 `-j1`**。
 *
 * 该 JSON 是**生成物**，不要手改。要改契约请改 Rust 结构体 → 重新生成清单 → 同步改
 * `src/types.ts` 里同名 interface 及所有字段访问点。
 *
 * 支持的清单格式（脚本会自动归一化，容忍生成侧的几种常见写法）：
 *   { "MirrorEntry": ["id", "label", …] }                       ← 约定格式
 *   { "types": { "MirrorEntry": [...] }, "generatedAt": "…" }    ← 带外层包装 + 元数据
 *   { "MirrorEntry": { "fields": ["id", …] } }
 *   { "MirrorEntry": { "id": "String", "latencyMs": "Option<u64>" } }   ← 带类型，会额外做可空性检查
 *   { "MirrorEntry": [{ "name": "id", "type": "String" }, …] }
 *
 * ## 依赖
 *
 * 只用 Node 内置模块 + 仓库**已有的** `typescript` devDependency（`npm ci` 已装，不新增任何包）。
 * 选择 TS 编译器 AST 而非正则解析的理由见 `parseTypesTs` 上方注释。
 */

import { readFileSync, existsSync, statSync, readdirSync } from "node:fs";
import { resolve, dirname, relative, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const TYPES_TS = resolve(ROOT, "src/types.ts");

/** 字段清单的候选位置，按优先级。约定值在最前，其余是对生成侧的容错。 */
const SNAPSHOT_CANDIDATES = [
  "src-tauri/contract-snapshot.json",
  "src-tauri/contract_snapshot.json",
  "src-tauri/target/contract-snapshot.json",
  "contract-snapshot.json",
];

// ---------------------------------------------------------------- 小工具

const C = process.stdout.isTTY
  ? { r: "\x1b[31m", g: "\x1b[32m", y: "\x1b[33m", b: "\x1b[1m", d: "\x1b[2m", x: "\x1b[0m" }
  : { r: "", g: "", y: "", b: "", d: "", x: "" };

const rel = (p) => relative(ROOT, p).replace(/\\/g, "/");
const list = (arr) => arr.map((s) => `\`${s}\``).join("、");

function die(msg) {
  console.error(`${C.r}${C.b}✗ 契约校验无法执行${C.x}\n\n${msg}\n`);
  process.exit(1);
}

// ---------------------------------------------------------------- 参数

const argv = process.argv.slice(2);
const flag = (name) => argv.includes(`--${name}`);
const opt = (name) => {
  const hit = argv.find((a) => a.startsWith(`--${name}=`));
  return hit ? hit.slice(name.length + 3) : null;
};

const ALLOW_MISSING = flag("allow-missing");
const LIST_ONLY = flag("list");
const SNAPSHOT_OPT = opt("snapshot") ?? process.env.ITOOLS_CONTRACT_SNAPSHOT ?? null;

// ---------------------------------------------------------------- 1. 读 Rust 侧字段清单

function findSnapshot() {
  if (SNAPSHOT_OPT) {
    const p = resolve(ROOT, SNAPSHOT_OPT);
    return existsSync(p) ? p : null;
  }
  for (const c of SNAPSHOT_CANDIDATES) {
    const p = resolve(ROOT, c);
    if (existsSync(p)) return p;
  }
  return null;
}

const MISSING_SNAPSHOT_HELP = `没有找到 Rust 侧导出的契约字段清单。

已尝试的位置：
${(SNAPSHOT_OPT ? [SNAPSHOT_OPT] : SNAPSHOT_CANDIDATES).map((c) => `  - ${c}`).join("\n")}

它是 Rust 契约测试的**生成物**，请先生成（共享 target 必须带 -j1，否则会 LLVM OOM）：

  ${C.b}cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen${C.x}

或用 --snapshot=<路径> / 环境变量 ITOOLS_CONTRACT_SNAPSHOT 指定其它位置。

清单缺失**不等于契约没问题**，所以这里按失败处理。
若确知本次改动与前后端契约无关、需要临时放行，显式加 --allow-missing。`;

/**
 * 把生成侧可能写出的几种 JSON 形态归一化成 Map<接口名, Map<字段名, 类型字符串|null>>。
 * 宁可对格式宽容，也不能因为格式不认识就静默跳过 —— 认不出的形态一律报错。
 */
function normalizeSnapshot(raw, path) {
  let root = raw;
  if (root === null || typeof root !== "object" || Array.isArray(root)) {
    die(`契约字段清单 ${rel(path)} 的顶层不是 JSON 对象，无法解析。`);
  }

  // 允许外层包装：{ "types": { … }, "generatedAt": … }
  for (const key of ["types", "interfaces", "contracts", "structs", "snapshot"]) {
    const inner = root[key];
    if (inner && typeof inner === "object" && !Array.isArray(inner)) {
      root = inner;
      break;
    }
  }

  const out = new Map();
  for (const [name, value] of Object.entries(root)) {
    // 元数据标量（generatedAt / version / $schema …）直接跳过
    if (value === null || typeof value !== "object") continue;

    const fields = new Map();
    const push = (fname, ftype) => {
      if (typeof fname === "string" && fname.length > 0) {
        fields.set(fname, typeof ftype === "string" ? ftype : null);
      }
    };

    if (Array.isArray(value)) {
      for (const item of value) {
        if (typeof item === "string") push(item, null);
        else if (item && typeof item === "object") push(item.name ?? item.field, item.type ?? item.rust ?? null);
        else die(`契约字段清单 ${rel(path)} 里 \`${name}\` 的元素形态无法识别：${JSON.stringify(item)}`);
      }
    } else if (Array.isArray(value.fields)) {
      for (const item of value.fields) {
        if (typeof item === "string") push(item, null);
        else if (item && typeof item === "object") push(item.name ?? item.field, item.type ?? item.rust ?? null);
        else die(`契约字段清单 ${rel(path)} 里 \`${name}.fields\` 的元素形态无法识别：${JSON.stringify(item)}`);
      }
    } else {
      // { "字段名": "类型" } 形态
      for (const [fname, ftype] of Object.entries(value)) push(fname, ftype);
    }

    if (fields.size > 0) out.set(name, fields);
  }

  if (out.size === 0) {
    die(`契约字段清单 ${rel(path)} 解析后是空的 —— 生成侧可能改了格式。\n脚本支持的格式见 ${rel(resolve(HERE, "check-contract.mjs"))} 头部注释。`);
  }
  return out;
}

// ---------------------------------------------------------------- 2. 解析 src/types.ts
//
// 解析方式的选择：**用仓库已有的 typescript 依赖走 AST**，不用正则 / 状态机。
//
// 理由：正则解析必须自己处理块注释与行注释、字符串字面量里的花括号与分号、
// 联合类型跨行、嵌套对象字面量类型、泛型里的逗号（`Record<string, string[]>`）、
// 索引签名、以及 `interface A extends B` 的继承合并（本文件里 `ProfileView extends Profile` 就是）。
// 任何一条处理漏了，脚本都会**少数出或多数出**字段 —— 而这个脚本的全部价值就在于它的判定可信：
// 一个偶尔漏报的校验器比没有校验器更糟，因为它会让人以为已经把过关了。
// typescript 已经是本仓库的 devDependency（npm ci 就装了），走 AST **不新增任何依赖**，
// 且只用 `ts.createSourceFile`（纯语法解析，不建 Program、不读 tsconfig、不做类型检查），毫秒级。

function loadTypeScript() {
  const require = createRequire(import.meta.url);
  try {
    return require("typescript");
  } catch {
    die(`无法加载 typescript（本脚本用它做 AST 解析）。
typescript 是本仓库已声明的 devDependency，请先安装依赖：

  ${C.b}npm ci${C.x}`);
  }
}

/** 解析 types.ts，返回 Map<接口名, { fields: Map<字段名, {optional, type}>, extends: string[] }> */
function parseTypesTs(ts, path) {
  const src = ts.createSourceFile(
    path,
    readFileSync(path, "utf8"),
    ts.ScriptTarget.Latest,
    /* setParentNodes */ true,
    ts.ScriptKind.TS,
  );

  const decls = new Map();

  const collectMembers = (members) => {
    const fields = new Map();
    const notes = [];
    for (const m of members) {
      if (ts.isPropertySignature(m)) {
        // 属性名可能是标识符、字符串字面量或计算属性
        let name = null;
        if (ts.isIdentifier(m.name) || ts.isStringLiteral(m.name) || ts.isNumericLiteral(m.name)) {
          name = m.name.text;
        }
        if (name === null) {
          notes.push("含计算属性名，已跳过");
          continue;
        }
        fields.set(name, {
          optional: m.questionToken !== undefined,
          type: m.type ? m.type.getText(src).replace(/\s+/g, " ").trim() : null,
        });
      } else if (ts.isIndexSignatureDeclaration(m)) {
        notes.push("含索引签名，无法逐字段比对");
      } else if (ts.isMethodSignature(m)) {
        notes.push("含方法签名，已跳过");
      }
    }
    return { fields, notes };
  };

  const visit = (node) => {
    if (ts.isInterfaceDeclaration(node)) {
      const { fields, notes } = collectMembers(node.members);
      const bases = [];
      for (const h of node.heritageClauses ?? []) {
        for (const t of h.types) {
          if (ts.isIdentifier(t.expression)) bases.push(t.expression.text);
        }
      }
      decls.set(node.name.text, { fields, bases, notes, line: lineOf(ts, src, node) });
    } else if (ts.isTypeAliasDeclaration(node) && ts.isTypeLiteralNode(node.type)) {
      // `type X = { … }` 也当作契约结构对待
      const { fields, notes } = collectMembers(node.type.members);
      decls.set(node.name.text, { fields, bases: [], notes, line: lineOf(ts, src, node) });
    }
    ts.forEachChild(node, visit);
  };
  visit(src);

  // 展开 extends（本文件内可解析的部分），父类字段并入子类
  const resolved = new Map();
  const expand = (name, seen = new Set()) => {
    if (resolved.has(name)) return resolved.get(name);
    const d = decls.get(name);
    if (!d) return null;
    if (seen.has(name)) return d; // 循环继承兜底
    seen.add(name);

    const merged = new Map();
    const notes = [...d.notes];
    for (const base of d.bases) {
      const parent = expand(base, seen);
      if (parent) for (const [k, v] of parent.fields) merged.set(k, v);
      else notes.push(`继承自 \`${base}\`，但它不在 ${rel(path)} 里（可能是 import 进来的），其字段未纳入比对`);
    }
    for (const [k, v] of d.fields) merged.set(k, v);

    const out = { fields: merged, notes, line: d.line };
    resolved.set(name, out);
    return out;
  };
  for (const name of decls.keys()) expand(name);
  return resolved;
}

function lineOf(ts, src, node) {
  return src.getLineAndCharacterOfPosition(node.getStart(src)).line + 1;
}

// ---------------------------------------------------------------- 3. 可空性检查（仅当清单带类型时）
//
// 只做**明确**的判定，绝不猜：Rust `Option<T>` ⇒ TS 必须能表示「没有值」（`?` 或联合里含 null/undefined），
// 反之非 Option 的字段 TS 不该声明成可空（否则前端会写出永远走不到的兜底分支）。
// 拿不准的一律不报 —— 噪声会让人把整个校验关掉。

function nullabilityIssues(rustType, tsField) {
  if (!rustType || !tsField.type) return null;
  const rustOptional = /^\s*Option\s*</.test(rustType);
  const tsNullable =
    tsField.optional || /(^|\|)\s*(null|undefined)\s*(\||$)/.test(tsField.type);

  if (rustOptional && !tsNullable) {
    return `Rust 是 \`${rustType}\`（可能没有值），TS 声明成 \`${tsField.type}\` 不可空 → 运行期可能读到 null`;
  }
  if (!rustOptional && tsNullable) {
    return `Rust 是 \`${rustType}\`（恒有值），TS 却声明成可空 \`${tsField.optional ? "?: " : ""}${tsField.type}\` → 前端会写出永远走不到的兜底分支`;
  }
  return null;
}

// ---------------------------------------------------------------- 主流程

const snapshotPath = findSnapshot();
if (!snapshotPath) {
  if (ALLOW_MISSING) {
    console.error(
      `${C.y}${C.b}⚠ 契约校验被跳过${C.x}：没找到 Rust 侧字段清单，且指定了 --allow-missing。\n` +
        `${C.y}  这次**没有**校验前后端契约。生成清单：cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen${C.x}\n`,
    );
    process.exit(0);
  }
  die(MISSING_SNAPSHOT_HELP);
}

if (!existsSync(TYPES_TS)) die(`找不到 ${rel(TYPES_TS)}。`);

// 清单新鲜度：清单是 cargo test 的生成物，改了 Rust 却没重跑测试时它会停在旧版本，
// 于是本脚本比对的是**过期快照**——两边都「绿」，字段其实已经错位（纯前端流水线的假绿路径）。
// 这里只**告警不阻断**：改任意一个 .rs 都会触发，做成硬失败会逼人无脑重跑、反而变成噪音。
function newestRustMtime(dir) {
  let newest = 0;
  let newestFile = null;
  for (const ent of readdirSync(dir, { withFileTypes: true })) {
    if (ent.name === "target" || ent.name.startsWith(".")) continue;
    const p = join(dir, ent.name);
    if (ent.isDirectory()) {
      const sub = newestRustMtime(p);
      if (sub.mtime > newest) ({ mtime: newest, file: newestFile } = sub);
    } else if (ent.name.endsWith(".rs")) {
      const m = statSync(p).mtimeMs;
      if (m > newest) (newest = m), (newestFile = p);
    }
  }
  return { mtime: newest, file: newestFile };
}

try {
  const srcDir = resolve(ROOT, "src-tauri/src");
  if (existsSync(srcDir)) {
    const snapMtime = statSync(snapshotPath).mtimeMs;
    const { mtime: rsMtime, file: rsFile } = newestRustMtime(srcDir);
    if (rsMtime > snapMtime && rsFile) {
      console.error(
        `${C.y}${C.b}⚠ 契约清单可能已过期${C.x}\n` +
          `${C.y}  ${rel(rsFile)} 比 ${rel(snapshotPath)} 新——如果这次改动动过跨 invoke 边界的结构体，\n` +
          `  下面的比对结果基于**旧快照**，不作数。重新生成后再看：\n` +
          `    cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen${C.x}\n`,
      );
    }
  }
} catch {
  /* 新鲜度检查是尽力而为的提醒，取不到 mtime 不该让校验本身失败 */
}

let rawSnapshot;
try {
  rawSnapshot = JSON.parse(readFileSync(snapshotPath, "utf8"));
} catch (e) {
  die(`契约字段清单 ${rel(snapshotPath)} 不是合法 JSON：${e.message}\n它是生成物，请重新生成：cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen`);
}

const rustSide = normalizeSnapshot(rawSnapshot, snapshotPath);
const ts = loadTypeScript();
const tsSide = parseTypesTs(ts, TYPES_TS);

if (LIST_ONLY) {
  console.log(`${C.b}Rust 侧（${rel(snapshotPath)}）${C.x}`);
  for (const [n, f] of rustSide) console.log(`  ${n}: ${[...f.keys()].join(", ")}`);
  console.log(`\n${C.b}TS 侧（${rel(TYPES_TS)}）${C.x}`);
  for (const [n, d] of tsSide) console.log(`  ${n}: ${[...d.fields.keys()].join(", ")}`);
  process.exit(0);
}

/** @type {{name:string, missing:string[], ghost:string[], typeIssues:string[], absent:boolean, notes:string[], line:number|null}[]} */
const problems = [];
let checkedTypes = 0;

for (const [name, rustFields] of rustSide) {
  const tsDecl = tsSide.get(name);
  if (!tsDecl) {
    problems.push({
      name,
      missing: [...rustFields.keys()],
      ghost: [],
      typeIssues: [],
      absent: true,
      notes: [],
      line: null,
    });
    continue;
  }

  const missing = [...rustFields.keys()].filter((f) => !tsDecl.fields.has(f));
  const ghost = [...tsDecl.fields.keys()].filter((f) => !rustFields.has(f));

  const typeIssues = [];
  for (const [f, rustType] of rustFields) {
    const tsField = tsDecl.fields.get(f);
    if (!tsField || !rustType) continue;
    checkedTypes++;
    const issue = nullabilityIssues(rustType, tsField);
    if (issue) typeIssues.push(`${f}: ${issue}`);
  }

  if (missing.length || ghost.length || typeIssues.length) {
    problems.push({ name, missing, ghost, typeIssues, absent: false, notes: tsDecl.notes, line: tsDecl.line });
  }
}

// ---------------------------------------------------------------- 报告

const header =
  `${C.d}Rust 侧字段清单：${rel(snapshotPath)}（${rustSide.size} 个契约结构）\n` +
  `TS  侧声明文件：  ${rel(TYPES_TS)}（解析出 ${tsSide.size} 个 interface / type）${C.x}`;

if (problems.length === 0) {
  console.log(`${C.g}${C.b}✓ 前后端契约一致${C.x}`);
  console.log(header);
  const covered = [...rustSide.keys()].join("、");
  console.log(`${C.d}已比对：${covered}${C.x}`);
  if (checkedTypes > 0) console.log(`${C.d}其中 ${checkedTypes} 个字段带类型信息，已做可空性检查。${C.x}`);
  else console.log(`${C.d}清单未提供字段类型，仅比对了字段名集合（未做类型/可空性检查）。${C.x}`);
  process.exit(0);
}

console.error(`${C.r}${C.b}✗ 前后端契约不一致：${problems.length} 个结构对不上${C.x}\n`);
console.error(header + "\n");

for (const p of problems) {
  console.error(`${C.b}── ${p.name}${C.x}`);
  if (p.absent) {
    console.error(
      `   ${C.r}TS 侧完全没有这个 interface${C.x}\n` +
        `   Rust 有 ${p.missing.length} 个字段：${list(p.missing)}\n` +
        `   ${C.y}→ 在 ${rel(TYPES_TS)} 里新增 \`export interface ${p.name}\`${C.x}`,
    );
    console.error("");
    continue;
  }
  const at = p.line ? `${rel(TYPES_TS)}:${p.line}` : rel(TYPES_TS);
  if (p.missing.length) {
    console.error(
      `   ${C.r}TS 少声明了 ${p.missing.length} 个字段${C.x}（Rust 有、TS 无 → 前端读不到，或以为它不存在）：\n` +
        `     ${list(p.missing)}\n` +
        `   ${C.y}→ 到 ${at} 的 \`${p.name}\` 里补上${C.x}`,
    );
  }
  if (p.ghost.length) {
    console.error(
      `   ${C.r}TS 多声明了 ${p.ghost.length} 个幽灵字段${C.x}（TS 有、Rust 无 → 运行期恒为 undefined，UI 会静默失效）：\n` +
        `     ${list(p.ghost)}\n` +
        `   ${C.y}→ 到 ${at} 的 \`${p.name}\` 里删掉，并检查所有读它的地方${C.x}`,
    );
  }
  for (const t of p.typeIssues) console.error(`   ${C.r}类型不符${C.x} ${t}`);
  for (const n of p.notes) console.error(`   ${C.d}注：${n}${C.x}`);
  console.error("");
}

console.error(
  `${C.y}${C.b}怎么修${C.x}\n` +
    `  以 ${C.b}Rust 为准${C.x}：Rust 结构体是数据的真实来源，${rel(TYPES_TS)} 是它的镜像。\n` +
    `  1. 若 Rust 侧改得对 → 改 ${rel(TYPES_TS)} 对齐，并检查所有字段访问点（tsc 不会替你发现漏改）。\n` +
    `  2. 若 Rust 侧改错了 → 改 Rust 结构体，然后重新生成清单：\n` +
    `     ${C.b}cargo test --manifest-path src-tauri/Cargo.toml -j1 contract_field_names_frozen${C.x}   ${C.d}(共享 target 必须 -j1)${C.x}\n\n` +
    `${C.d}提醒：invoke<T>() 只是类型断言，tsc --noEmit 与 cargo test 都发现不了这类错位 —— 这正是本脚本存在的理由。${C.x}`,
);

process.exit(1);
