//! 集成测试：用 fastify.inject() 直接打端点，覆盖客户端契约的全部路径。
//! 运行：`npm test`（tsx test/api.test.ts）。node:test 顺序执行、共享同一实例与状态。

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { FastifyInstance } from "fastify";

import { loadConfig } from "../src/config";
import { JsonStore } from "../src/store";
import { buildServer } from "../src/server";

const dataFile = join(tmpdir(), `itools-sync-test-${process.pid}.json`);
let app: FastifyInstance;
let token = "";

before(async () => {
  await fs.rm(dataFile, { force: true });
  await fs.rm(`${dataFile}.tmp`, { force: true });
  const config = { ...loadConfig({}), dataFile, logger: false };
  const store = new JsonStore(dataFile);
  await store.load();
  app = buildServer(store, config);
  await app.ready();
});

after(async () => {
  await app.close();
  await fs.rm(dataFile, { force: true });
  await fs.rm(`${dataFile}.tmp`, { force: true });
});

function post(url: string, payload: unknown, tok?: string) {
  return app.inject({
    method: "POST",
    url,
    payload: payload as object,
    headers: tok ? { authorization: `Bearer ${tok}` } : {},
  });
}

test("health 可达", async () => {
  const res = await app.inject({ method: "GET", url: "/health" });
  assert.equal(res.statusCode, 200);
  assert.equal(res.json().ok, true);
});

test("首登自动注册并返回 token", async () => {
  const res = await post("/auth/login", { username: "haifeng", password: "s3cret" });
  assert.equal(res.statusCode, 200);
  const body = res.json();
  assert.equal(typeof body.token, "string");
  assert.ok(body.token.length > 0);
  assert.equal(body.username, "haifeng");
  token = body.token;
});

test("同凭据再登录成功（非重复注册）", async () => {
  const res = await post("/auth/login", { username: "haifeng", password: "s3cret" });
  assert.equal(res.statusCode, 200);
  assert.equal(typeof res.json().token, "string");
});

test("密码错误 → 401", async () => {
  const res = await post("/auth/login", { username: "haifeng", password: "wrong" });
  assert.equal(res.statusCode, 401);
});

test("缺字段 → 400", async () => {
  const res = await post("/auth/login", { username: "haifeng" });
  assert.equal(res.statusCode, 400);
});

test("无令牌访问 /data → 401", async () => {
  const res = await post("/data/app", { records: [] });
  assert.equal(res.statusCode, 401);
});

test("上行数据 + 回拉（排除纯回声）", async () => {
  // 首次推送两条：本次响应应排除刚推送的回声 → 空
  const push = await post(
    "/data/app",
    { records: [
      { key: "nickname", value: "海风哥", updatedAt: 1000 },
      { key: "opts", value: { theme: "dark" }, updatedAt: 1000 },
    ] },
    token,
  );
  assert.equal(push.statusCode, 200);
  assert.deepEqual(push.json().records, []);

  // 「另一台设备」（同用户、空 dirty）来同步 → 应拉到服务端已存的两条
  const pull = await post("/data/app", { records: [] }, token);
  assert.equal(pull.statusCode, 200);
  const recs = pull.json().records as Array<{ key: string; value: unknown; updatedAt: number }>;
  assert.equal(recs.length, 2);
  const byKey = Object.fromEntries(recs.map((r) => [r.key, r]));
  assert.equal(byKey.nickname.value, "海风哥");
  assert.deepEqual(byKey.opts.value, { theme: "dark" });
});

test("last-write-wins：旧 updatedAt 不覆盖新值", async () => {
  // 先写新值（updatedAt=2000）
  await post("/data/app", { records: [{ key: "nickname", value: "新名", updatedAt: 2000 }] }, token);
  // 再推旧值（updatedAt=1500）——应被拒绝覆盖
  await post("/data/app", { records: [{ key: "nickname", value: "旧名", updatedAt: 1500 }] }, token);
  const pull = await post("/data/app", { records: [] }, token);
  const recs = pull.json().records as Array<{ key: string; value: unknown; updatedAt: number }>;
  const nick = recs.find((r) => r.key === "nickname");
  assert.equal(nick?.value, "新名", "应保留较新的值");
  assert.equal(nick?.updatedAt, 2000);
});

test("命名空间隔离 + plugin:ns 路由可用", async () => {
  const res = await post(
    "/data/plugin:demo",
    { records: [{ key: "k", value: 42, updatedAt: 1000 }] },
    token,
  );
  assert.equal(res.statusCode, 200);
  // app 命名空间不应看到 plugin:demo 的数据
  const appPull = await post("/data/app", { records: [] }, token);
  const appKeys = (appPull.json().records as Array<{ key: string }>).map((r) => r.key);
  assert.ok(!appKeys.includes("k"), "命名空间应隔离");
  // plugin:demo 能拉回自己的数据
  const pluginPull = await post("/data/plugin:demo", { records: [] }, token);
  const pr = pluginPull.json().records as Array<{ key: string; value: unknown }>;
  assert.equal(pr.length, 1);
  assert.equal(pr[0].value, 42);
});

test("退出登录使令牌失效", async () => {
  const res = await post("/auth/logout", { allDevices: false }, token);
  assert.equal(res.statusCode, 200);
  // 用已失效令牌访问 /data → 401
  const after = await post("/data/app", { records: [] }, token);
  assert.equal(after.statusCode, 401);
});

test("注销账号：错误口令拒绝，正确口令删除并清数据", async () => {
  // 错误口令
  const bad = await post("/account/delete", { username: "haifeng", password: "wrong" });
  assert.equal(bad.statusCode, 401);
  // 正确口令
  const ok = await post("/account/delete", { username: "haifeng", password: "s3cret" });
  assert.equal(ok.statusCode, 200);
  // 注销后重新登录 → 因 allowRegister 默认开，会作为「新账号」自动注册，其旧数据应已清空
  const relogin = await post("/auth/login", { username: "haifeng", password: "s3cret" });
  assert.equal(relogin.statusCode, 200);
  const newToken = relogin.json().token;
  const pull = await post("/data/app", { records: [] }, newToken);
  assert.deepEqual(pull.json().records, [], "注销后数据应已清空");
});
