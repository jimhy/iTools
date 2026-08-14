# 开发者中心 MCP —— 接入、工具清单与排错

iTools 主进程内置一个 MCP 服务器，把「开发者中心」的能力开放给 AI 编程助手。
**接上之后，你（AI）不再只是生成文件，而是能自己跑起来看结果**：扫描插件、打开调试窗口、
读运行日志、发布前自检、提交审核、查审核结论。

> **这个 skill 本身不依赖 MCP。** 没接 MCP 也能照 `SKILL.md` 生成完整插件，
> 只是调试与提审要用户手动点。接上 MCP 只是把「人肉传声筒」那一段去掉。

---

## 一、接入

### 1.1 前提

**iTools 必须开着。** 这个服务器跑在 iTools 进程里，应用退出就连不上。
它随应用自动启动，默认监听 `http://127.0.0.1:7345/mcp`（**只绑回环，不对外**）。

真实地址以「管理中心 → 开发者中心」最上面那张卡片为准 —— 端口被占用时会换，
卡片上显示的是**本机实际监听结果**，不是配置回显。

### 1.2 配置（各家客户端位置不同，内容一致）

```json
{
  "mcpServers": {
    "itools": { "url": "http://127.0.0.1:7345/mcp" }
  }
}
```

| 客户端 | 放哪儿 |
|---|---|
| Claude Code | `claude mcp add --transport http itools http://127.0.0.1:7345/mcp`，或写进项目根 `.mcp.json` |
| Codex CLI | `~/.codex/config.toml` 里加一节 MCP server（HTTP 形态，url 同上） |
| Cursor | `~/.cursor/mcp.json`（或项目内 `.cursor/mcp.json`） |
| 其它 | 找该客户端的 MCP 配置文件，按 **Streamable HTTP** 形态填这个 url |

传输是 **Streamable HTTP**（`POST /mcp`，JSON-RPC 2.0），**不是 stdio、也没有 SSE 推送**。
配置时选错传输形态是连不上的常见原因之一。

填完**重启 AI 客户端**，工具列表里会出现 `read_docs` / `list_plugins` / `run_plugin` 等 8 个工具。

---

## 二、⚠️ 开了代理 / VPN 就连不上（最高频的坑）

**如果用户机器上开着代理或 VPN，这一节几乎必踩。** 先看症状对不对得上：

### 2.1 症状

- AI 客户端报 MCP 连接失败、**502 Bad Gateway**、或工具列表空空如也；
- 但开发者中心卡片明明显示「运行中」，端口也正常；
- 报错内容看起来像「服务器有问题」，于是开始怀疑 iTools —— **方向错了**。

### 2.2 根因

代理软件（Clash / v2rayN / 企业 VPN…）开启时通常会设上 `HTTP_PROXY` / `HTTPS_PROXY`
环境变量。AI 客户端的 HTTP 栈（Claude Code 走 Node/undici、Codex 走 Rust reqwest）**都读这些变量**。

如果 `NO_PROXY` 里没排除本机，客户端就会把「连 `127.0.0.1:7345`」这个请求**发给代理服务器**。
代理服务器在**它自己的网络位置**上解析 `127.0.0.1` —— 那是代理自己，它上面当然没有 iTools ——
于是回一个 **502**。

**这就是它误导人的地方**：拿到的是 502 而不是「连接被拒绝」，看着像服务端故障，
实际 MCP 服务器从头到尾好好的，请求压根没到它跟前。

> **别和插件里的 `itools.fetch` 搞混。** 插件调 `itools.fetch` 访问本机地址时，
> iTools 原生层**保证直连、永不走代理**（见 `plugin-spec.md` 的网络代理说明），那条路没有这个问题。
> 这里坏掉的是 **AI 客户端 → MCP 端点**这一段 —— 走的是 AI 客户端自己的网络栈，**iTools 管不着**。

### 2.3 一分钟确诊

```powershell
# ① 强制绕过代理直连。通了 → 板上钉钉是代理问题
curl.exe --noproxy "*" -s -X POST http://127.0.0.1:7345/mcp `
  -H "Content-Type: application/json" `
  -d '{"jsonrpc":"2.0","id":1,"method":"ping"}'
# 期望看到：{"jsonrpc":"2.0","id":1,"result":{}}

# ② 看看当前代理变量是什么
$env:HTTP_PROXY; $env:HTTPS_PROXY; $env:NO_PROXY
```

判读：

| ① 绕过代理 | ② 变量 | 结论 |
|---|---|---|
| 通 | 有 proxy、NO_PROXY 不含 127.0.0.1 | **就是这个坑**，按 2.4 修 |
| 通 | 没设 proxy | 客户端读的是**系统代理**（Windows Internet 选项），仍按 2.4 修，并检查代理软件的直连规则 |
| 不通 | — | 不是代理问题。查 iTools 是否在跑、端口是否被占（见第四节） |

### 2.4 长期处方：把本机加进 `NO_PROXY`

**只追加，绝不覆盖**用户已有的值（覆盖会毁掉他原本的代理绕过规则）：

```powershell
$old  = [Environment]::GetEnvironmentVariable('NO_PROXY','User')
$need = '127.0.0.1,localhost,::1'
$new  = if ([string]::IsNullOrWhiteSpace($old)) { $need } else { "$old,$need" }
[Environment]::SetEnvironmentVariable('NO_PROXY',  $new, 'User')
[Environment]::SetEnvironmentVariable('no_proxy',  $new, 'User')   # 小写也设一份
```

要点：

- **大小写各设一份**：Node/undici 认大写 `NO_PROXY`，curl 传统上认小写 `no_proxy`，reqwest 两个都认。设两份最省事。
- **改完必须重启 AI 客户端**（连同它所在的终端）。环境变量是进程启动时读的，已经开着的进程看不到新值。
- ❌ **绝不要把 `NO_PROXY` 设成 `*`** —— 那等于全局关掉代理，用户会连不上 GitHub 和其它需要代理的站点。只加本机这几个。
- 部分代理软件启动时会**重写系统代理设置**。除了环境变量，也确认它自己的「绕过 / 直连规则」里包含回环地址（Clash 默认规则通常有，但 TUN 模式或自定义配置可能漏掉）。

### 2.5 不想改全局变量？先临时验证

在**准备启动 AI 客户端的那个终端里**临时设一下，再从这个终端启动客户端：

```powershell
$env:NO_PROXY = "127.0.0.1,localhost,::1"
claude          # 或 codex
```

通了就说明诊断没错，再按 2.4 落成永久配置。

---

## 三、工具清单与工作流

| 工具 | 作用 | 什么时候调 |
|---|---|---|
| `read_docs` | 读三份规范原文（开发规范 / 开发者中心 / 分发规范） | 这个 skill 已含规范摘要，通常**不必调**；需要逐字原文或拿不准细节时再调 |
| `list_plugins` | 列全部调试插件：id、目录、版本、清单校验结论 `issues`、features、是否可运行；**返回里还有调试目录列表** | 开工第一件事 —— 新建插件要往哪个目录放，答案在这 |
| `rescan` | 重扫全部调试目录 | **每次增删改插件文件后必调**，否则 `list_plugins` 拿到的是旧清单 |
| `run_plugin(id, code, query?, kind?)` | 打开调试窗口实际跑一次（等同点「运行」） | 写完 / 改完，验证真能跑 |
| `read_logs(afterSeq?, limit?)` | 每次 `window.itools.*` 调用的入参/返回/耗时/成败、console 输出、未捕获异常 | **排查「点了没反应」的首选**。日志不落盘，重启 iTools 即清零 |
| `preflight(id)` | 发布前自检，会**现查线上版本**比对版本号 | 提审前 |
| `submit(id)` | 打包上传、提交审核 | 自检通过后 |
| `publish_status(id)` | 本地版本 / 线上版本 / 审核结论与历史 | 提审后查结果，被驳回时里面是模型给出的逐条理由 |

### 典型闭环

```
list_plugins            → 拿到调试目录路径 + 现有插件
（用文件工具写插件目录到 fixed=true 的那个调试目录下）
rescan → list_plugins   → 确认扫到了，且 issues 里没有 level=error
run_plugin              → 实际跑一次
read_logs               → 看调用与报错；有问题就改 → rescan → run_plugin → read_logs
（改 plugin.json 的 version —— iTools 不会替你升）
preflight               → 自检；版本号被拦时返回里的 suggestedVersion 可直接用
submit → publish_status → 提审与查结论
```

### 边界（照实说，别替它吹）

- **它开放的就是开发者中心的能力，不多也不少。** 面板做不到的这里也做不到；面板会拒的（`issues` 有 error、版本号没升、没登录云账号），这里同样会拒，并给出**一模一样的原文**。
- `preflight` 通过 **不等于** 会过审 —— 代码审核在服务端的大模型那一侧，本地看不出结论。
- `submit` **需要用户已登录 iTools 云账号**，且它走的是市场审核流程，**不是**往 `plugins/` 里拷文件。
- 调试环境**刻意放宽**了一部分校验（如 `name` ≠ 目录名，正式环境直接不加载，这里只报告警）。**「调试能跑」不等于「能发布」**，发布前把 `issues` 清到零。
- ⚠️ **MCP 服务器没有鉴权**，本机任意进程都能调它，包括 `submit`。这是产品取舍不是疏漏；要关掉设 `ITOOLS_MCP=off` 后重启 iTools。

---

## 四、其它连不上的原因（排除代理之后）

| 现象 | 原因 | 处理 |
|---|---|---|
| 绕过代理也连不通，卡片显示「未运行」并带错误 | 端口 `7345` 被别的程序占了 | 设 `ITOOLS_MCP_PORT=其它端口`（`0` = 系统分配）后重启 iTools，**客户端 url 同步改成卡片上的新地址** |
| 卡片显示「已被环境变量关闭」 | 设过 `ITOOLS_MCP=off` | 删掉这个环境变量后重启 iTools |
| 客户端连上了但工具列表是空的 | 传输形态填成了 stdio / SSE | 改成 **Streamable HTTP**，url 填到 `/mcp` 这一层 |
| 一切正常但工具调用报「插件不在列表里」 | 改了文件没重扫 | 先 `rescan` 再 `list_plugins` 确认 id |
| iTools 重启后 `read_logs` 空了 | 日志只在内存里，不落盘 | 正常行为。重新 `run_plugin` 再读 |
