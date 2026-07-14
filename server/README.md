# iTools 云同步服务端

iTools 客户端「本地优先 + 配置化云端」架构的**云端实现**：账号鉴权 + 本地优先数据的云端存储与合并。
TypeScript + Fastify + Node 内置 crypto，数据落 **MariaDB**（服务端强制依赖）。

## 运行

```bash
cd server
npm install
# 先起一个 MariaDB（示例：Docker）
docker run --name itools-mariadb -e MARIADB_ROOT_PASSWORD=yourpass -e MARIADB_DATABASE=itools -p 3306:3306 -d mariadb:11
# 指向该库后启动（凭据只走环境变量）
SYNC_DB_PASSWORD=yourpass npm start   # 默认监听 http://127.0.0.1:8787
# 开发热重载： npm run dev
# 类型检查：   npm run typecheck
# 集成测试：   npm test   （需可连的 MariaDB，见下）
```

配置全走环境变量（见 `.env.example`）：

| 变量 | 默认 | 说明 |
|---|---|---|
| `SYNC_HOST` / `SYNC_PORT` | `127.0.0.1` / `8787` | HTTP 监听地址与端口 |
| `SYNC_DB_HOST` | `127.0.0.1` | MariaDB 主机 |
| `SYNC_DB_PORT` | `3306` | MariaDB 端口 |
| `SYNC_DB_USER` | `root` | 数据库用户 |
| `SYNC_DB_PASSWORD` | （空） | 数据库口令（只从环境变量读） |
| `SYNC_DB_NAME` | `itools` | 数据库名（不存在会自动 `CREATE DATABASE`） |
| `SYNC_DB_CONNLIMIT` | `10` | 连接池上限 |
| `SYNC_DB_URL` | — | 可选，`mysql://user:pass@host:port/db` 覆盖上面各分项 |
| `SYNC_ALLOW_REGISTER` | `true` | 首登自动注册 |
| `SYNC_TOKEN_BYTES` | `32` | 会话令牌随机字节数 |
| `SYNC_LOG` | `true` | 访问日志开关 |

> 集成测试（`npm test`）连真实 MariaDB。示例（Docker，用非标准端口避免冲突）：
> ```bash
> docker run --name itools-mariadb-test -e MARIADB_ROOT_PASSWORD=roottest -e MARIADB_DATABASE=itools -p 3307:3306 -d mariadb:11
> SYNC_DB_PORT=3307 SYNC_DB_USER=root SYNC_DB_PASSWORD=roottest SYNC_DB_NAME=itools npm test
> ```

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

## 存储

数据落 **MariaDB**（`mysql2/promise` 连接池），服务端强制依赖，连不上则启动失败并打印清晰错误（不会假装起来）。
启动时幂等建库建表（`users` / `sessions` / `data_records`，`utf8mb4`）。数据模型：

- `users(username PK, password_hash, salt, created_at)`——口令只存 scrypt 哈希 + 盐。
- `sessions(token PK, username, created_at)`——会话令牌，按用户可全设备吊销。
- `data_records(username, ns, k) PK, v LONGTEXT, updated_at`——按 `(用户, 命名空间, key)` 隔离，
  `v` 为 JSON 序列化后的值；上行合并用 `INSERT ... ON DUPLICATE KEY UPDATE` 一条语句实现 last-write-wins。

## 目录

```
server/
├─ src/
│  ├─ config.ts   环境变量配置（含 MariaDB 连接）
│  ├─ auth.ts     口令哈希 / 令牌
│  ├─ store.ts    MariaDB 存储（mysql2/promise 连接池）
│  ├─ server.ts   Fastify 路由（全部契约）
│  └─ index.ts    入口
├─ test/api.test.ts  集成测试（连真实 MariaDB，fastify.inject 打全部端点）
├─ .env.example
└─ package.json
```
