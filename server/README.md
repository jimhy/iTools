# iTools 云同步服务端

iTools 客户端「本地优先 + 配置化云端」架构的**云端实现**：账号鉴权 + 本地优先数据的云端存储与合并。
**Rust（axum + sqlx + RustCrypto）**，数据落 **MariaDB**（服务端强制依赖）。

> 本服务原为 Node + TypeScript + Fastify，现已整体改写为 Rust。契约、环境变量、数据库表结构
> **完全不变**，口令哈希算法与参数也逐位对齐，因此**旧库可直接接管、老用户无需重设密码**
> （`src/auth.rs` 里钉了一条 Node 实测向量做回归，对不上就在测试里炸）。

## 运行

```bash
cd server
# 先起一个 MariaDB（示例：Docker）
docker run --name itools-mariadb -e MARIADB_ROOT_PASSWORD=yourpass -e MARIADB_DATABASE=itools -p 3306:3306 -d mariadb:11
# 指向该库后启动（凭据只走环境变量）
SYNC_DB_PASSWORD=yourpass cargo run --release   # 默认监听 http://127.0.0.1:8787
# 开发编译：   cargo run
# 静态检查：   cargo clippy --all-targets
# 测试：       cargo test            （分两组，见下）
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
| `SYNC_TOKEN_BYTES` | `32` | 会话令牌随机字节数（夹紧到 16~256） |
| `SYNC_LOG` | `true` | 访问日志开关（关掉只留 warn 以上） |
| `SYNC_RUST_LOG` | — | 可选，更细的日志级别（语法同 `RUST_LOG`） |
| `SYNC_TRUST_PROXY` | `false` | 是否信任 `X-Forwarded-For`（决定客户端 IP，进而决定限流按谁计数）。**置于反向代理之后必须打开**，见「限流」 |
| `SYNC_MIRRORS_FILE` | `<工作目录>/config/mirrors.json` | 镜像源列表文件（见「镜像源与健康探测」） |
| `SYNC_MIRROR_PROBE` | `true` | 定时健康探测开关 |
| `SYNC_MIRROR_PROBE_INTERVAL_SEC` | `900` | 探测周期（秒，下限 60） |
| `SYNC_MIRROR_PROBE_TIMEOUT_MS` | `10000` | 单次探测超时 |
| `SYNC_MIRROR_FAIL_THRESHOLD` | `3` | 连续失败多少次才判 unhealthy |
| `SYNC_MIRROR_CACHE_SEC` | `300` | `/api/mirrors` 的 `Cache-Control: max-age` |
| `SYNC_MIRROR_OKAT_GRANULARITY_SEC` | `3600` | `lastOkAt` 时间戳的量化粒度（0 = 不量化，代价是 304 几乎不再命中，见「ETag 与 304」） |
| `SYNC_MIRROR_RATE_MAX` | `120` | `/api/mirrors` 每 IP 每窗口最大请求数（0 = 关闭限流） |
| `SYNC_MIRROR_RATE_WINDOW_SEC` | `60` | 限流窗口长度（秒） |
| `SYNC_TLS_CERT_FILE` / `SYNC_TLS_KEY_FILE` | — | 两个都设置则本进程直接起 HTTPS、自行终结 TLS |

> 测试分两组：
> - `cargo test --lib --test mirrors`——单元测试 + 镜像源/探测测试，**不连数据库、不打外网**
>   （网络层注入 mock，存储层用懒连接的池，一旦误访问会立刻报错而不是静默通过），可直接跑。
> - `cargo test --test api`——账号/同步集成测试，连真实 MariaDB。
> - `cargo test` 依次跑上面两组。
>
> 起一个测试库（Docker，用非标准端口避免冲突）：
> ```bash
> docker run --name itools-mariadb-test -e MARIADB_ROOT_PASSWORD=roottest -e MARIADB_DATABASE=itools -p 3307:3306 -d mariadb:11
> SYNC_DB_PORT=3307 SYNC_DB_USER=root SYNC_DB_PASSWORD=roottest SYNC_DB_NAME=itools cargo test
> ```

### 容器部署

```bash
docker build -t itools-sync-server ./server
docker run -d --name itools-sync -p 8787:8787 \
  -e SYNC_PORT=8787 -e SYNC_DB_HOST=… -e SYNC_DB_PASSWORD=… \
  -v /your/mirrors.json:/app/config/mirrors.json \
  itools-sync-server
```

> ⚠ **构建期内存**：rustc 默认按 CPU 核数并行，编译 axum / sqlx 这类大 crate 时很容易在
> 2GB 内存的容器或小 VPS 上被 OOM killer 干掉（实测：Docker Desktop 默认内存下直接 SIGKILL）。
> 因此 Dockerfile 里默认 `CARGO_BUILD_JOBS=2`，内存宽裕时可以调大：
> `docker build --build-arg CARGO_BUILD_JOBS=8 …`。构建机内存实在紧张就用 `=1`。
> 运行期不受影响——服务本身很轻。

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
| `GET /data/_usage` | Bearer | — | `{counts:{ns:n},bytes}` |
| `GET /api/mirrors` | **无需认证** | — | GitHub 镜像源配置（见下节） |
| `GET /health` | — | — | `{ok:true,...}` |

插件市场与提审的端点见「插件市场与提审」一节。

- **鉴权**：会话令牌走 `Authorization: Bearer <token>`。
- **数据模型**：按 `(用户, 命名空间)` 隔离。命名空间：核心 App 用 `app`，第三方插件用 `plugin:<id>`。
- **合并策略**：`last-write-wins`，按客户端提供的 `updatedAt`（大者胜）。`/data/:ns` 上行 dirty 记录后，
  回拉返回该命名空间全部记录，但**排除刚推送的纯回声**（同 key 同 updatedAt），让客户端 `pulled` 计数只反映真正的新数据。
- **错误响应**统一为 `{"error":"…"}`；存储层异常一律记进日志、只回一句通用文案，不把库结构泄露给客户端。

### 首登自动注册

客户端只有「登录」入口。为便于自托管直接使用，`SYNC_ALLOW_REGISTER=true`（默认）时，
**用户名首次登录即以该口令注册账号**；此后同名登录会校验口令。设 `SYNC_ALLOW_REGISTER=false`
可关闭自动注册（未知用户名登录返回 404），改由你自己的开户流程建账号。

## 镜像源与健康探测（`GET /api/mirrors`）

iTools 的插件生态只用 **GitHub**，而 GitHub 在国内可达性差。解决办法不是自建反代，而是：
**服务端维护一份镜像源列表并定时探测其有效性，客户端定期拉取最新配置来挑下载源。**
不可能所有镜像源同时失效，因此这条链路比单一反代更抗打。

### 端点契约

```
GET /api/mirrors            无需认证（但按 IP 限流，见「限流」）
  If-None-Match: "<etag>"   （可选）内容未变则回 304
→ 200 application/json      配置体（格式见下）
  ETag: "<32 位 hex>"        按响应体内容算的强 ETag
  Cache-Control: public, max-age=300, must-revalidate
  X-Mirror-Probe-At: <ISO>  最近一轮探测结束时刻（诊断用；从未探测过时不发此头）
  X-RateLimit-Limit / X-RateLimit-Remaining
→ 304 Not Modified          内容未变（无响应体；ETag 与 X-Mirror-Probe-At 照常带）
→ 429 Too Many Requests     超出限流配额，带 Retry-After（秒）
```

- **必须无需认证**：没登录的用户也要能装插件，这里加鉴权会直接堵死插件安装这条路。
- 响应中的 `mirrors` **已排序**：健康的在前，同为健康的按 `latencyMs` 升序（无延迟数据的排后，再按 id 稳定排序）。
- **探测任务从未跑过（刚启动）也返回一份可用配置**：此时 `healthy` 用配置文件里的初值，
  绝不返回空列表、绝不 500。配置文件缺失/写坏时回落到代码内置兜底列表（同样有日志）。
- `updatedAt` 是**服务端这份数据的最后一次实质变更时间**（配置重载，或探测结果实质变化），
  不是配置文件里的字面值，**也不是「最后一次探测时刻」**——后者在响应头 `X-Mirror-Probe-At` 里。
- 客户端侧约定（Rust）：本地 TTL 30 分钟；拉取失败不阻塞安装（退回本地缓存 → 内置默认）；
  **官方源不在本列表里**，它是客户端内置的固定候选，始终参与竞速。

### ETag 与 304：怎么保证它真的会命中

强 ETag 的语义是「响应体变一个字节，ETag 就必须变」。所以 304 能不能命中，**完全取决于响应体本身稳不稳定**——
不能靠「把某些字段排除在 ETag 计算之外」来伪造稳定（那会让两份不同的响应体共用一个 ETag，
客户端收到 304 后手里拿着与服务端不一致的数据却毫不知情）。因此规则是
**「每轮必变、且客户端不消费的数据，一律不进响应体」**：

| 数据 | 去处 | 原因 |
|---|---|---|
| 本轮探测时刻 | 响应头 `X-Mirror-Probe-At`（200/304 都带） | 每轮必变；响应头是「这次响应」的元数据，不参与 ETag，`curl -I` 一眼可见新鲜度 |
| 连续失败次数 | 只留内存 + 失败日志（日志本就逐次打印第几次） | 每个失败轮次都 +1，进响应体会让故障期间 ETag 每轮失效 |
| `latencyMs` | 进响应体，但**量化 + 滞回** | 按 50ms 取整；与已发布值相差 <100ms 且 <25% 时沿用旧值。它只用于给候选源排序，这点精度足够 |
| `lastOkAt` | 进响应体，但**按 `SYNC_MIRROR_OKAT_GRANULARITY_SEC` 向下取整**（默认 1 小时） | 秒级时间戳每轮必变；取整后同一小时内的多轮探测不改写响应体 |
| `lastError` | 进响应体 | 同一故障持续时文案不变，不会抖动 |

于是稳定态（各镜像健康状况与延迟都没实质变化）下，**探测跑多少轮都不会刷新 `updatedAt` / ETag**，
客户端带 `If-None-Match` 来就是 304。只有真出现实质变化（健康翻转、延迟跨阈值、失败原因变化、配置重载）
才刷新一次。`cargo test --test mirrors` 里有对应的回归用例（连续多轮相同结果 → 304；持续失败 → 仍 304）。

> 想要精确到秒的 `lastOkAt`？设 `SYNC_MIRROR_OKAT_GRANULARITY_SEC=0`。**代价是每轮探测都会改写响应体，
> 304 基本不会再命中**——这是明确的取舍，不是 bug。

### 限流（唯一的免认证读端点）

`/api/mirrors` 免认证是刻意的取舍（未登录用户也要能装插件），但它因此是全服唯一一个不需要令牌的读端点。
虽然单请求成本极低（响应体有内存缓存、配置重载有 5s stat 节流、响应带 `Cache-Control: max-age=300`），
被刷时仍有**带宽放大与日志膨胀**风险，所以单独给它上了一层按 IP 限流：

- **算法**：滑动窗口计数器（自实现，见 `src/ratelimit.rs`）。**被拒的请求也计数**——
  否则被限的客户端靠不停重试照样能把日志和带宽打满。
- **默认配额：每 IP 每 60 秒 120 次**（`SYNC_MIRROR_RATE_MAX` / `SYNC_MIRROR_RATE_WINDOW_SEC`）。
  正常客户端本地 TTL 是 30 分钟（约 2 次/小时），默认配额留了三个数量级的余量，
  同一个 NAT 出口后的整栋楼也打不满；恶意刷取则被压到每 IP 每秒 2 次的量级。
- **超限**：`429` + `Retry-After: <秒>`（给的是「加权计数降到配额以下的最早时刻」，不是简单的窗口剩余时间），
  响应体 `{"error":"请求过于频繁，请稍后再试"}`。放行时也带 `X-RateLimit-Limit` / `X-RateLimit-Remaining`。
- **日志**：只在某个 IP「刚刚越限」时打一条 warn，之后不再重复——限流日志自己不能变成新的日志放大源。
- **内存**：最多跟踪 20000 个 IP，超出先清过期桶、再按最近最少使用淘汰。
- **调整**：调宽 `SYNC_MIRROR_RATE_MAX=600`；改窗口 `SYNC_MIRROR_RATE_WINDOW_SEC=300`；
  **完全关闭用 `SYNC_MIRROR_RATE_MAX=0`**（关闭时连 `X-RateLimit-*` 头都不发，不谎报限流存在）。

> ⚠ **置于 Nginx/Caddy 等反向代理之后时，必须设 `SYNC_TRUST_PROXY=true`**（或填代理的 IP/CIDR、跳数）。
> 否则客户端 IP 一律取到代理的地址，**所有客户端会共用同一个限流桶**而互相误伤。
> 服务端在越限日志里也会带上这条提示。默认不信任 `X-Forwarded-For`，因为该头可被任意伪造。

> `/health` **不限流**：健康检查被限流会让负载均衡/监控误判服务已死。它只返回一个常量对象，
> 不读库不读文件，成本可以忽略。这是明确的例外，不是遗漏（`cargo test --test mirrors` 里有对应用例）。

### 配置体 / `config/mirrors.json` 格式

客户端内置默认、客户端本地缓存、服务端返回**三处同一格式**：

```json
{
  "version": 1,
  "updatedAt": "2026-08-12T10:00:00Z",
  "probe": {
    "owner": "octocat", "repo": "Hello-World", "ref": "HEAD", "path": "README",
    "sha256": "03ba204e50d126e4674c005e04d82e84c21366780af1f43bd54a37816b6ab340"
  },
  "mirrors": [
    {
      "id": "gh-proxy",
      "label": "gh-proxy.com",
      "raw": "https://gh-proxy.com/https://raw.githubusercontent.com/{owner}/{repo}/{ref}/{path}",
      "archive": "https://gh-proxy.com/https://github.com/{owner}/{repo}/archive/{ref}.zip",
      "healthy": true,
      "latencyMs": 900,
      "lastOkAt": "2026-08-12T09:00:00.000Z"
    }
  ]
}
```

- 占位符只有四个：`{owner}` `{repo}` `{ref}` `{path}`（`archive` 模板不含 `{path}`）。
  替换时 `owner`/`repo` 整体 URL 编码，`ref`/`path` **按段编码、斜杠保留**（`refs/heads/main` 要原样保留）。
- **每个镜像必须各自声明 `raw` 与 `archive` 模板，不能假设各家路径形态统一。**
  实测（2026-08-12）：`{mirror}/https://codeload.github.com/...` 在 gh-proxy.com 可用、在 ghfast.top 返回 **403**；
  而 `{mirror}/https://github.com/{o}/{r}/archive/{ref}.zip` 各家都可用——**随附配置统一用 archive 形态**。
- 服务端响应里还会带一个**诊断字段**（客户端可忽略）：`lastError`（最近一次失败原因）。
  `latencyMs` / `lastOkAt` 是**量化后的近似值**，`lastCheckedAt`（本轮探测时刻）走响应头
  `X-Mirror-Probe-At`、`consecutiveFailures` 只进日志——原因见上面「ETag 与 304」。
- 写坏的配置（缺占位符、id 重复、sha256 不是 64 位 hex、mirrors 为空……）会被**整份拒绝**并打错误日志，
  服务端沿用上一份可用配置——不会「半吞半吐」地用一份残缺列表。

### 维护方法（镜像站寿命很短，这是常规运维动作）

1. 改 `server/config/mirrors.json`（容器部署建议 `-v /your/mirrors.json:/app/config/mirrors.json` 挂载）。
2. **存盘即生效，不用重启**：服务端按文件 mtime+size 轮询（请求路径上检查，最小间隔 5s），
   变化即重载并立刻补跑一轮探测。
   *为什么选 mtime 轮询而不是「手动触发端点」：`/api/mirrors` 是公开端点，再加一个能改状态的端点
   就得单独做鉴权与防滥用；stat 成本极低且已节流。*
3. 换探测样本时，`probe.sha256` 要在**能访问 GitHub 的机器**上算好再填：
   ```bash
   curl -sL https://raw.githubusercontent.com/octocat/Hello-World/HEAD/README | sha256sum
   ```

### 探测机制

- 频率 `SYNC_MIRROR_PROBE_INTERVAL_SEC`（默认 900s = 15 分钟，可配 15~30 分钟），**启动后先跑一次**；
  并发上限 4，单次超时 `SYNC_MIRROR_PROBE_TIMEOUT_MS`（默认 10s）。
- 探测方式：**经该镜像的 `raw` 模板**拉取 `probe` 指定的文件，校验内容 **SHA-256**，记录耗时。
  **不能只看 HTTP 200**——镜像站挂掉时经常返回 200 + 错误页/验证页，只看状态码会把死掉的镜像判成健康；
  比对内容哈希能同时检出「失效」与「被劫持」。
- 失败原因分类并写进响应：`DNS 解析失败 (…)` / `超时` / `HTTP 403` / `哈希不匹配 (…)`。
- **连续失败 `SYNC_MIRROR_FAIL_THRESHOLD` 次（默认 3）才标 unhealthy**（避免一次网络抖动误杀）；
  **恢复则立即标回 healthy** 并清空失败计数。
- 探测任务的异常（含单个任务 panic）全部收在任务内部（记日志，下轮继续），不会拖垮主服务；
  进程退出时定时任务被 abort，不阻止退出。

### ⚠ 服务器不需要 VPN、不需要能直连 GitHub

**探测走的就是镜像本身**——服务端只对镜像域名发请求，从不访问 `github.com` / `raw.githubusercontent.com`。
期望哈希 `probe.sha256` 由维护者预置在配置文件里，服务端**不会**去官方源核对。
所以：**部署在国内的服务器不需要任何代理/VPN 就能完成健康探测**（当然，探测结果也正是国内网络下的真实可达性，
这恰恰是我们想要的信号）。

### 探测状态为什么不落库

`healthy` / `latencyMs` / `lastOkAt` 只存内存，**不写 MariaDB**，理由：

1. 这些是「此刻的网络可达性」，重启后最多 15 分钟内（实际是启动即跑的那一轮）就重新测出来，落库的价值极低；
   反而会把重启前的陈旧判断带到重启后。
2. 端点无需认证、是插件安装的关键路径，让它依赖 DB 会凭空多一个故障面——DB 抖一下就装不了插件。
   现在即使 DB 完全挂掉，只要进程活着，`/api/mirrors` 照常返回。
3. 启动窗口期不留空白：探测跑出结果前用配置文件里的初值，客户端拿到的永远是一份可用列表。

## 安全

- 口令**从不明文存储**：scrypt 派生哈希（N=16384、r=8、p=1、64 字节）+ 每用户 16 字节随机盐；
  校验用常量时间比较（`subtle`）防时序侧信道。派生跑在阻塞线程池里，不占 async worker。
- 会话令牌为 CSPRNG 随机值（默认 32 字节），服务端会话表可单个 / 全设备吊销。
- 任何密钥 / 凭据都不写死在源码，全走环境变量。
- 默认监听 `127.0.0.1`。对公网暴露时请置于 **HTTPS 反向代理**（Nginx/Caddy）之后，
  或用 `SYNC_TLS_CERT_FILE` / `SYNC_TLS_KEY_FILE` 让本进程自行终结 TLS（适配 frps 纯 TCP 透传），
  并按需收紧 `SYNC_ALLOW_REGISTER`。
- 唯一的免认证读端点 `/api/mirrors` 有**按 IP 限流**（默认 120 次/60 秒，见「限流」）。
  置于反向代理之后时记得设 `SYNC_TRUST_PROXY`，否则限流按代理 IP 计数、形同虚设且会误伤。
- TLS 用 rustls + ring，内置 webpki 根证书；不依赖系统 OpenSSL，容器运行层也不需要 ca-certificates。
- **已知缺口（如实标注）**：`POST /auth/login` 与 `POST /account/delete` 目前**没有**失败次数限制或限流，
  对公网暴露时口令爆破只受口令强度保护（scrypt 本身把单次尝试压到了几十毫秒量级，但这不是限流）。
  生产暴露前请在反向代理层加登录限流（如 Nginx `limit_req`），或等本服务补上登录侧限流。
- **DB 连接不启用 TLS**（与旧 Node 版一致）：默认假设服务端与 MariaDB 同机或在可信内网。
  要跨公网连库，请自行走 SSH 隧道 / VPN，别把库直接暴露出去。

## 存储

数据落 **MariaDB**（sqlx 连接池），服务端强制依赖，连不上则启动失败并打印清晰错误（不会假装起来）。
启动时幂等建库建表（`users` / `sessions` / `data_records`，`utf8mb4`）。数据模型：

- `users(username PK, password_hash, salt, created_at)`——口令只存 scrypt 哈希 + 盐。
- `sessions(token PK, username, created_at)`——会话令牌，按用户可全设备吊销。
- `data_records(username, ns, k) PK, v LONGTEXT, updated_at`——按 `(用户, 命名空间, key)` 隔离，
  `v` 为 JSON 序列化后的值；上行合并用 `INSERT ... ON DUPLICATE KEY UPDATE` 一条语句实现 last-write-wins。

表结构与旧 Node 版**逐字段一致**，直接接管旧库即可，无需迁移脚本。

## 目录

```
server/
├─ src/
│  ├─ config.rs    环境变量配置（MariaDB 连接、镜像探测、审核模型、市场）
│  ├─ auth.rs      口令哈希（scrypt）/ 令牌
│  ├─ store.rs     MariaDB 存储（sqlx 连接池）
│  ├─ mirrors.rs   镜像源配置装载（热更新）+ 健康探测 + 快照/ETag
│  ├─ ratelimit.rs 滑动窗口按 IP 限流（供免认证的 /api/mirrors 与 /api/market/*）
│  ├─ proxy.rs     X-Forwarded-For 取真实客户端 IP
│  ├─ pkg.rs       提审插件包：解 zip / 安全校验 / 清单解析 / 内容哈希
│  ├─ llm.rs       插件代码的大模型审核（OpenAI 兼容协议）
│  ├─ market.rs    提审编排 + 市场索引（取代原先的 GitHub registry）
│  ├─ routes.rs    axum 路由（全部契约）
│  ├─ lib.rs       库入口（供集成测试构造 Router）
│  └─ main.rs      可执行入口（HTTP / HTTPS、优雅关停）
├─ config/mirrors.json  可编辑的镜像源列表（随部署提供，可挂载覆盖）
├─ data/packages/       已上线插件包（运行期生成，**必须挂持久卷**）
├─ tests/
│  ├─ api.rs      账号/同步集成测试（连真实 MariaDB）
│  └─ mirrors.rs  镜像源/探测测试（不连库、不打外网，网络层注入 mock）
├─ .env.example
├─ docker-compose.yml   群晖 / 单机一键编排（server + MariaDB）
├─ Dockerfile
└─ Cargo.toml
```

## 插件市场与提审

**插件市场的真相源就是这台服务器**，不再是 GitHub 上的 `registry/index.json`。作者在 iTools 的
「开发者中心 → 发布」里点「提交审核」，客户端把插件目录打成 zip 传上来，服务端审完直接发布。

### 端点

| 方法 & 路径 | 鉴权 | 说明 |
|---|---|---|
| `POST /api/plugins/submit` | Bearer | body = 插件包 zip 的**原始字节**。立即返回 `202` + 提审单（`status=reviewing`） |
| `GET /api/plugins/submissions` | Bearer | 我的提审记录（新→旧，最多 50 条） |
| `GET /api/plugins/submissions/{id}` | Bearer | 单条详情，含**模型裁决原文** |
| `GET /api/market/index` | **无需认证** | 市场索引（带 ETag、按 IP 限流） |
| `GET /api/market/package/{name}` | **无需认证** | 已上线插件包 zip |
| `POST /api/market/revoke` | Bearer（限 `SYNC_ADMIN_USERS`） | 下架 / 恢复，`{name, revoked?, reason}` |

### 审核是两段

1. **机械校验**（`pkg.rs`，同步、必过）：zip 路径安全（Zip Slip / 盘符 / `..` / 结尾点空格）、
   23 种可执行扩展名整包拒收、体积与条目数上限、`plugin.json` 结构、`name` 白名单、
   `features`/`code` 非空不重复、`permissions` 必须是已知能力名、必须有 `index.html`、
   版本号必须高于线上。**这些规则与客户端 `install.rs` 逐条对齐**——否则会出现
   「服务端收下并发布了一个客户端装不上的包」。
2. **大模型审核**（`llm.rs`，异步）：把包里的文本源码交给模型，判断有无恶意行为、
   申请的权限是否名副其实、描述是否与实际功能相符、有无 `eval` / 动态 `import()` 这类
   「审核时无害、运行时拉远程代码」的手法。

因为第 2 段要几十秒到几分钟，提交接口**立即返回**，作者在客户端轮询状态。

### ⚠ 没配模型 = 不放行，绝不自动通过

`ITOOLS_LLM_API_KEY` 没配、模型调用失败、裁决无法解析、进程重启中断审核——**这四种情况一律落
`manual`（待人工处理），插件不会上线**。启动时日志会明确警告，索引里的 `reviewMode` 会是
`manual`，客户端市场页据此把文案改成「本服务端没有接入自动代码审核」。

把「没审成」当成「审过了」，等于市场上所有「已审核」的字样都是假的——这是本模块的第一条红线。

### 提审单状态

| status | 含义 |
|---|---|
| `reviewing` | 已收下，模型正在读代码 |
| `approved` | 通过，已发布到市场 |
| `rejected` | 未通过，`message` 是原因原文 |
| `manual` | **审核没能完成**，需维护者人工处理。**不是通过**，插件未上线 |

### 归属：同名插件只能由首次上线它的账号更新

`market_entries.owner` 记着首发账号。别人提交同名包直接 `403`。这不是洁癖：客户端是按**插件名**
归属用户授权与数据的，顶包等于直接继承受害插件的用户授权。

### 存储

- 提审单与市场条目在 MariaDB（`plugin_submissions` / `market_entries`，启动时幂等建表）；
- 插件包在 `SYNC_PACKAGES_DIR`（默认 `/app/data/packages`），按 `<name>/<version>.zip` 存。
  **容器里必须挂持久卷**，否则重启后索引还在、包没了，所有插件下载失败。

### 内容哈希

发布时算一次，写进索引；客户端下载解压后重算比对，对不上拒绝安装。算法在
`server/src/pkg.rs`、`src-tauri/src/plugin/market.rs`、`scripts/registry/hash.mjs` 三处实现，
**必须逐字一致**（排序用 UTF-8 字节序，不是语言默认排序）。三处都有基准用例钉死。

## 群晖部署

群晖的 Container Manager 支持直接跑 docker compose。

```bash
# 1. 把 server/ 目录传到群晖（如 /volume1/docker/itools-server）
# 2. 复制 .env.example 为 .env，至少填这两项：
#      SYNC_DB_PASSWORD=<自己设一个强口令>
#      ITOOLS_LLM_API_KEY=<审核模型的 key>
#    再按需填 SYNC_ADMIN_USERS=<你的账号名>（不填就没人能下架插件）
# 3. 起服务
docker compose up -d --build
```

几点群晖特有的注意事项：

- **构建内存**：`Dockerfile` 默认 `CARGO_BUILD_JOBS=2`。低配 NAS（2GB 内存）编译 axum/sqlx
  容易被 OOM killer 干掉，可以 `docker compose build --build-arg CARGO_BUILD_JOBS=1`。
  实在编不动就在别的机器上 `docker build` 完导出镜像再 `docker load` 进群晖。
- **持久化**：compose 已把 `./data/mysql` 与 `./data/packages` 挂出来。**这两个目录不能删**——
  前者是账号与数据，后者是市场里所有插件的包。
- **反向代理**：用群晖的「反向代理」把 `api.jimhy.cn:7101` 指到容器端口时，
  **必须设 `SYNC_TRUST_PROXY=true`**，否则所有客户端在服务端看来都是同一个 IP，
  会共用一个限流桶、互相误伤。
- **上传体积**：提审包最大 32MB（`SYNC_MAX_UPLOAD_MB`）。群晖反代 / Nginx 侧
  也要把 `client_max_body_size` 放到同等或更大，否则大包会在代理层就被 413 掉。
- **凭据**：`.env` 已在 `.gitignore` 与 `.dockerignore` 里，不会进仓库也不会进镜像。
  LLM 的 key 只存在这份 `.env` 里。
