#!/usr/bin/env node
/**
 * gen-itools-dts.mjs —— 从**真实出口**生成 `itools.d.ts`，并守住它不再漂移
 * ==========================================================================
 *
 * ## 为什么需要它
 *
 * `itools.d.ts` 的开头写着「生成插件时把本文件当契约：**只用这里声明的方法**」，
 * AI 写插件时确实会照做。可这份文件是手抄的，于是它漂移了——而且漂了两次：
 *
 *   - `plugins/itools.d.ts`                        22 个方法（2026-07-06）
 *   - `skills/itools-plugin-dev/assets/itools.d.ts` 46 个方法（2026-07-13）
 *
 * 而 `window.itools` 的真实出口是 **173** 个。两份互不一致，且都没有 camera / record /
 * runtime / fs / exec / serve —— 也就是说，一个老老实实遵守「只用这里声明的方法」的 AI，
 * 会认定 iTools 做不了录屏、做不了文件读写、调不了外部程序，然后如实告诉用户「不支持」。
 * 文档缺一段只是查不到，**契约缺一段是会让人得出错误结论的**。
 *
 * 手抄一份新的解决不了问题：下次加 API 时它照样会漂。所以改成生成 + 校验。
 *
 * ## 数据从哪来
 *
 * 1. **有哪些方法、参数叫什么** —— 解析 `src-tauri/src/plugin/bridge.js` 里的
 *    `var itools = { … }`。这是注入到插件页的那个对象本身，不可能比它更权威。
 * 2. **参数与返回的类型** —— 解析 `references/window-itools-api.md` 里形如
 *    `` `itools.fs.pickDir(opts?): Promise<FsScope|null>` `` 的签名行。
 *    文档没给签名的，退化成 bridge.js 的参数名 + `any`，并在该行标注 —— 宁可标成 any，
 *    也不编一个看起来很像的类型。
 *
 * ## 怎么用
 *
 *   npm run gen:dts            # 重新生成两份 .d.ts
 *   npm run check:dts          # 只校验：与真实出口不一致就非零退出（已接进 npm run check）
 *
 * 退出码：0 = 一致 / 生成成功；1 = 有漂移或解析失败。
 */

import { readFileSync, writeFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const BRIDGE = resolve(ROOT, "src-tauri/src/plugin/bridge.js");
const API_MD = resolve(ROOT, "skills/itools-plugin-dev/references/window-itools-api.md");
const TARGETS = [
  resolve(ROOT, "skills/itools-plugin-dev/assets/itools.d.ts"),
  resolve(ROOT, "plugins/itools.d.ts"),
];

const C = { r: "[31m", g: "[32m", y: "[33m", d: "[2m", x: "[0m" };

// ==================== 1. 真实出口：bridge.js ====================

/**
 * 解析 `var itools = { … }`，返回 `{ top: [{name, params}], groups: { ns: [{name, params}] } }`。
 *
 * 用「按缩进判层级 + 大括号配平」而不是跑 JS 解析器：这个仓库没有 AST 依赖，
 * 而 bridge.js 里这个对象的写法是稳定的两层结构，缩进判层足够且不引入新依赖。
 */
function parseBridge() {
  const lines = readFileSync(BRIDGE, "utf8").split(/\r?\n/);
  const start = lines.findIndex((l) => /^\s*var itools = \{\s*$/.test(l));
  if (start < 0) throw new Error("bridge.js 里找不到 `var itools = {`——它的写法变了，本脚本要跟着改");

  let depth = 0;
  let end = -1;
  for (let i = start; i < lines.length; i++) {
    depth += (lines[i].match(/\{/g) || []).length - (lines[i].match(/\}/g) || []).length;
    if (i > start && depth === 0) {
      end = i;
      break;
    }
  }
  if (end < 0) throw new Error("bridge.js 里 `var itools = {` 没有配平的右括号");

  const body = lines.slice(start + 1, end);
  const base = body[0].length - body[0].trimStart().length;
  const top = [];
  const groups = {};
  let cur = null;

  for (const l of body) {
    const ind = l.length - l.trimStart().length;
    const openNs = l.match(/^\s*([A-Za-z_$][\w$]*)\s*:\s*\{\s*$/);
    if (openNs && ind === base) {
      cur = openNs[1];
      groups[cur] = [];
      continue;
    }
    const fn = l.match(/^\s*([A-Za-z_$][\w$]*)\s*:\s*(?:async\s+)?function\s*\(([^)]*)\)/);
    if (fn) {
      const entry = { name: fn[1], params: fn[2].trim() };
      if (ind === base) {
        top.push(entry);
        cur = null;
      } else if (cur && ind === base + 2) {
        groups[cur].push(entry);
      }
      continue;
    }
    if (ind === base && /^\s*\},?\s*$/.test(l)) cur = null;
  }
  return { top, groups };
}

// ==================== 2. 类型来源：API 参考文档 ====================

/**
 * 从 API 参考里抓形如 `` `itools.a.b(x: T, y?: U): Promise<R>` `` 的签名行。
 * 返回 `Map<"a.b", { params, ret }>`。抓不到的方法不进这张表，由调用方退化处理。
 */
function parseDocSignatures() {
  const md = readFileSync(API_MD, "utf8");
  const map = new Map();
  const re = /`itools\.([A-Za-z_$][\w$.]*)\(([^`]*?)\)\s*:\s*([^`]+?)`/g;
  let m;
  while ((m = re.exec(md)) !== null) {
    const path = m[1];
    if (map.has(path)) continue; // 同一个方法可能被提到多次，取第一处（章节里的正式签名）
    const params = m[2].trim();
    const ret = m[3].trim();
    // 文档里有 `itools.power.lock() / sleep(): Promise<void>` 这类**合并写法**，
    // 正则会把 `) / sleep(` 当成 lock 的参数。括号不配平就是撞上了这种行，直接跳过，
    // 让它退化成 any —— 生成一份编不过的 .d.ts 比缺几个类型糟糕得多。
    if (!balanced(params) || !balanced(ret)) continue;
    map.set(path, { params: sanitizeParams(params), ret: sanitizeType(ret) });
  }
  return map;
}

/** 括号/花括号/尖括号是否配平。 */
function balanced(s) {
  let round = 0;
  let curly = 0;
  let angle = 0;
  for (const ch of s) {
    if (ch === "(") round++;
    else if (ch === ")") round--;
    else if (ch === "{") curly++;
    else if (ch === "}") curly--;
    else if (ch === "<") angle++;
    else if (ch === ">") angle--;
    if (round < 0 || curly < 0) return false;
  }
  return round === 0 && curly === 0 && angle === 0;
}

/** 按顶层分隔符切分（忽略括号/引号内部的分隔符）。 */
function splitTop(s, seps) {
  const out = [];
  let depth = 0;
  let quote = null;
  let buf = "";
  for (const ch of s) {
    if (quote) {
      buf += ch;
      if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      buf += ch;
      continue;
    }
    if ("([{<".includes(ch)) depth++;
    else if (")]}>".includes(ch)) depth--;
    if (depth === 0 && seps.includes(ch)) {
      out.push(buf);
      buf = "";
      continue;
    }
    buf += ch;
  }
  if (buf.trim()) out.push(buf);
  return out.map((x) => x.trim()).filter(Boolean);
}

/** 对象字面量类型里的裸成员（`{ dataB64, mime }`）补成 `: any`——否则不是合法 TS。 */
function sanitizeType(t) {
  return t.replace(/\{([^{}]*)\}/g, (whole, inner) => {
    if (!inner.trim()) return whole;
    const members = splitTop(inner, ";,").map((mem) => {
      if (splitTop(mem, ":").length > 1 || mem.includes(":")) return mem;
      if (!/^[A-Za-z_$][\w$]*\??$/.test(mem)) return mem; // 不是纯标识符就别动它
      return `${mem}: any`;
    });
    return `{ ${members.join("; ")} }`;
  });
}

/** 参数表里没写类型的参数（`put(id: string, data, mime?)`）补成 `: any`。 */
function sanitizeParams(p) {
  if (!p.trim()) return "";
  return splitTop(p, ",")
    .map((one) => {
      const t = sanitizeType(one);
      if (t.includes(":")) return t;
      if (!/^\.{0,3}[A-Za-z_$][\w$]*\??$/.test(t)) return t;
      return `${t}: any`;
    })
    .join(", ");
}

// ==================== 3. 生成 ====================

/** bridge.js 的裸参数名 → 退化后的 TS 参数列表（全部 any，可选性无从得知，一律必填）。 */
function fallbackParams(params) {
  if (!params) return "";
  return params
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean)
    .map((p) => `${p}?: any`)
    .join(", ");
}

function methodLine(fullPath, entry, docs, stats) {
  const sig = docs.get(fullPath);
  if (sig) {
    stats.typed++;
    return `  ${entry.name}(${sig.params}): ${sig.ret};`;
  }
  stats.untyped.push(fullPath);
  return `  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */\n  ${entry.name}(${fallbackParams(entry.params)}): any;`;
}

function build() {
  const { top, groups } = parseBridge();
  const docs = parseDocSignatures();
  const stats = { typed: 0, untyped: [] };
  const total = top.length + Object.values(groups).reduce((n, v) => n + v.length, 0);
  const nsNames = Object.keys(groups).filter((n) => groups[n].length > 0).sort();
  const iface = (ns) => "ITools" + ns[0].toUpperCase() + ns.slice(1);

  // 先生成主体：表头要写「多少个有正式签名」，那个数字得等主体跑完才知道。
  const body = [];
  for (const ns of nsNames) {
    body.push(`interface ${iface(ns)} {`);
    for (const m of groups[ns]) body.push(methodLine(`${ns}.${m.name}`, m, docs, stats));
    body.push(`}
`);
  }
  body.push(`interface IToolsApi {`);
  for (const m of top) body.push(methodLine(m.name, m, docs, stats));
  body.push(``);
  body.push(`  /** 平台标志（同步属性，不是方法） */`);
  body.push(`  platform: { isWindows: boolean; isMacOS: boolean; isLinux: boolean; isDev: boolean };`);
  for (const ns of nsNames) body.push(`  ${ns}: ${iface(ns)};`);
  body.push(`}
`);
  body.push(`declare const itools: IToolsApi;`);
  body.push(`declare global {`);
  body.push(`  interface Window {`);
  body.push(`    itools: IToolsApi;`);
  body.push(`  }`);
  body.push(`}
`);
  body.push(`export {};`);

  const header = `/**
 * iTools 插件全局 API —— 注入到每个插件页的 \`window.itools\`。
 *
 * ⚠️ **本文件由 \`scripts/gen-itools-dts.mjs\` 从真实出口生成，请勿手改。**
 * 改了会在 \`npm run check\` 时被打回；要调整内容请改生成器或 API 参考文档，然后
 * \`npm run gen:dts\` 重新生成。
 *
 * 方法名与参数名来自 \`src-tauri/src/plugin/bridge.js\`（注入插件页的那个对象本身）；
 * 类型来自 \`skills/itools-plugin-dev/references/window-itools-api.md\` 的签名行。
 * 文档里没有正式签名的方法，参数类型退化成 \`any\` 并在该行标注 —— 宁可标成 any，
 * 也不编一个看起来很像的类型。**语义、限制与错误文案一律以 API 参考文档为准**，
 * 这份 .d.ts 只回答「有哪些方法、参数叫什么」。
 *
 * 当前覆盖：${total} 个方法，其中 ${stats.typed} 个取到了正式签名。
 *
 * ⚠️ 首选裸引用 \`itools.xxx\`，或起别名 \`const api = window.itools\`。
 * 旧版 iTools 用 defineProperty(configurable:false) 注入，顶层 \`const itools = window.itools;\`
 * 会让整个 <script> 抛 SyntaxError、一行不执行（页面渲染正常但按钮全灭）；新版已改普通属性
 * 注入、该写法不再致命，但为兼容旧版仍建议避开。\`itools\` 已 Object.freeze，勿赋值。
 */

`;
  return { text: header + body.join("\n") + "\n", stats, total };
}

// ==================== 4. 入口 ====================

const checkOnly = process.argv.includes("--check");

let built;
try {
  built = build();
} catch (e) {
  console.error(`${C.r}✗ 生成失败：${e.message}${C.x}`);
  process.exit(1);
}

if (checkOnly) {
  const stale = [];
  for (const t of TARGETS) {
    let cur = "";
    try {
      cur = readFileSync(t, "utf8");
    } catch {
      stale.push([t, "文件不存在"]);
      continue;
    }
    if (cur.replace(/\r\n/g, "\n") !== built.text) stale.push([t, "内容与真实出口不一致"]);
  }
  if (stale.length) {
    console.error(`${C.r}✗ itools.d.ts 与 window.itools 的真实出口对不上${C.x}`);
    for (const [t, why] of stale) console.error(`  - ${t.replace(ROOT, ".")}：${why}`);
    console.error(`\n${C.y}这份 .d.ts 被 AI 当作「只能用这里声明的方法」的契约，漂了就会让它`);
    console.error(`认定平台不支持某些能力。跑 ${C.x}npm run gen:dts${C.y} 重新生成。${C.x}`);
    process.exit(1);
  }
  console.log(`${C.g}✓ itools.d.ts 与真实出口一致${C.x}（${built.total} 个方法，${built.stats.typed} 个有正式签名）`);
  process.exit(0);
}

for (const t of TARGETS) writeFileSync(t, built.text, "utf8");
console.log(`${C.g}✓ 已生成 ${TARGETS.length} 份 itools.d.ts${C.x}`);
console.log(`${C.d}  方法总数 ${built.total}，其中 ${built.stats.typed} 个取到了正式签名${C.x}`);
if (built.stats.untyped.length) {
  console.log(`${C.y}  ${built.stats.untyped.length} 个方法在 API 参考里没有正式签名，已标注为 any：${C.x}`);
  console.log(`${C.d}    ${built.stats.untyped.join(", ")}${C.x}`);
}
