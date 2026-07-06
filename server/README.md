# iTools 云同步服务端

iTools 客户端「本地优先 + 配置化云端」架构的**云端实现**：账号鉴权 + 本地优先数据的云端存储与合并。
纯 TypeScript + Fastify + Node 内置 crypto，**零原生依赖**，Windows / Linux `npm install` 后直接可跑。

## 运行

```bash
cd server
npm install
npm start            # 默认监听 http://127.0.0.1:8787
# 开发热重载： npm run dev
# 类型检查：   npm run typecheck
# 集成测试：   npm test
```

配置全走环境变量（见 `.env.example`）：`SYNC_HOST` / `SYNC_PORT` / `SYNC_DATA_FILE` / `SYNC_ALLOW_REGISTER` / `SYNC_TOKEN_BYTES` / `SYNC_LOG`。

## 连接 iTools 客户端

给 iTools 设置环境变量后启动，登录 / 云同步即真实生效：

```
ITOOLS_SYNC_ENDPOINT=http://127.0.0.1:8787
```

未设置该变量时，客户端诚实显示「云端未接入」，数据只留本地——这是设计内的诚实降级。

## REST 契约（与客户端 `account.rs` / `sync.rs` 精确对齐）

| 方法 & 路径 | 鉴权 | 请求体 | 响应 |
|---|---|---|---|
| `POST /auth/login` | — | `{username,password}` | `{token,username}`（首登可自动注册，见下） |
| `POST /auth/logout` | Bearer | `{allDevices?:boolean}` | `{ok:true}` |
| `POST /account/delete` | — | `{username,password}` | `{ok:true}`（鉴权后删除账号+全部数据+会话） |
| `POST /data/:ns` | Bearer | `{records:[{key,value,updatedAt}]}` | `{records:[{key,value,updatedAt}]}` |
| `GET /health` | — | — | `{ok:true,...}` |

- **鉴权**：会话令牌走 `Authorization: Bearer <token>`。
- **数据模型**：按 `(用户, 命名空间)` 隔离。命名空间：核心 App 用 `app`，第三方插件用 `plugin:<id>`。
- **合并策略**：`last-write-wins`，按客户端提供的 `updatedAt`（大者胜）。`/data/:ns` 上行 dirty 记录后，
  回拉返回该命名空间全部记录，但**排除刚推送的纯回声**（同 key 同 updatedAt），让客户端 `pulled` 计数只反映真正的新数据。

### 首登自动注册

客户端只有「登录」入口。为便于自托管直接使用，`SYNC_ALLOW_REGISTER=true`（默认）时，
**用户名首次登录即以该口令注册账号**；此后同名登录会校验口令。设 `SYNC_ALLOW_REGISTER=false`
可关闭自动注册（未知用户名登录返回 404），改由你自己的开户流程建账号。

## 安全

- 口令**从不明文存储**：scrypt 派生哈希 + 每用户随机盐；校验用 `timingSafeEqual` 防时序侧信道。
- 会话令牌为 `crypto.randomBytes` 随机值，服务端会话表可单个 / 全设备吊销。
- 任何密钥 / 凭据都不写死在源码，全走环境变量。
- 默认监听 `127.0.0.1`。对公网暴露时请置于 **HTTPS 反向代理**（Nginx/Caddy）之后，并按需收紧 `SYNC_ALLOW_REGISTER`。

## 存储与可替换性

默认用 `JsonStore`：内存态 + 原子落盘（写 `.tmp` 再 `rename`，写入串行化）到 `SYNC_DATA_FILE`。
零外部依赖、单文件、适合个人 / 小团队自托管。数据量增长后可替换为 Postgres / MySQL：
只需另实现 `src/store.ts` 里 `JsonStore` 的同名方法（`getUser/createUser/deleteUser/createSession/
getSessionUser/deleteSession/deleteUserSessions/getData/upsertData`），其余代码不动。

## 目录

```
server/
├─ src/
│  ├─ config.ts   环境变量配置
│  ├─ auth.ts     口令哈希 / 令牌
│  ├─ store.ts    持久化存储（JSON，可替换为 DB）
│  ├─ server.ts   Fastify 路由（全部契约）
│  └─ index.ts    入口
├─ test/api.test.ts  集成测试（fastify.inject 打全部端点）
├─ .env.example
└─ package.json
```
