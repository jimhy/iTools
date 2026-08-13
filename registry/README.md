# iTools 插件市场索引

这个目录是 iTools 插件市场的**真相源**。客户端「插件市场」页拉取的就是这里生成的 `index.json`。

```
registry/
├── entries/            一个插件一个 JSON，收录的 PR / 提交单位
│   └── <name>.json
├── index.json          CI 从 entries/ 聚合生成，客户端拉的就是它（不要手改）
├── schema/
│   └── entry.schema.json
└── README.md           本文件
```

---

## 一、当前的审核现状（请先读这一段）

**目前只有机械校验，没有代码审核。**

CI（`scripts/registry/check.mjs`）会自动做这些**机器能判定**的事：

| 检查项 | 说明 |
|---|---|
| 条目格式 | 按 `schema/entry.schema.json` 校验；文件名必须等于 `name` |
| `name` 全局唯一 | 它同时是安装后的目录名和数据命名空间 `plugin:<name>`，重名会串数据 |
| 清单一致性 | 拉 `revision` 那个**确切 commit** 的归档，核对里面 `plugin.json` 的 `name` / `version` / `permissions` 与条目声明**逐项一致** |
| 结构完整 | 目标目录下必须有 `plugin.json` 与 `index.html` |
| 无可执行文件 | exe / dll / bat / ps1 / lnk / reg 等 23 种「双击即执行」类型一律拒收 |
| 无符号链接 | 归档里出现符号链接条目直接拒收 |
| **内容哈希** | 计算并写进 `index.json`，客户端安装后逐字节校验（见第四节） |

CI **不做**、需要人看的部分：

- 代码里有没有恶意行为（窃取数据、隐蔽外联、执行任意命令）
- 申请的权限是否名副其实（申请了 `network` 但代码里根本没用，或用了却没申请）
- 描述与实际功能是否相符
- 有没有 `eval` / `new Function` / 动态 `import()` 这类「审核时无害、运行时拉远程代码」的手法

原计划由 **AI 自动审核**承担这部分，**当前尚未启用**——等真正有插件提交、跑出实际需求了再接。
在那之前，这些判断由维护者人工做，**市场里的条目不代表代码被审计过**。客户端的信任标识也据此如实呈现。

---

## 二、怎么提交一个插件

### 作者视角

1. 把插件推到一个**公开的 GitHub 仓库**（仓库根就是插件目录，或用子目录）
2. 确认 `plugin.json` 的 `name` 合法且不与已收录的重名（看 `entries/` 下有没有同名文件）
3. 记下要收录的那个**完整 40 位 commit sha**
4. 走下面任一条路：
   - **提 Issue**（推荐）：用[插件收录申请](../../issues/new?template=plugin-submit.yml)模板，维护者代为建条目
   - **提 PR**：自己在 `entries/` 下加一个 `<name>.json`，格式见 `schema/entry.schema.json`

### 为什么必须写完整 commit sha，不能写分支名

因为客户端**只安装这个确切的 commit**。

如果收录的是分支名，作者在收录通过之后往分支上推任何代码，都会自动到达所有用户——那样审核就只覆盖了「收录那一刻」，形同虚设。钉死 commit 之后，作者要发新版本必须**再次提审**，每个到达用户的版本都是被显式放行过的。

代价是发版多一步。这个代价是故意付的。

### 本地先自查

```bash
node scripts/registry/check.mjs --only <你的插件名>
```

它会真的去下载归档、跑完整套校验，和 CI 跑的是同一份代码。

---

## 三、更新与吊销

**更新**：改 `entries/<name>.json` 里的 `version` 与 `revision`，走同样的流程。CI 会重新核对并重算内容哈希。

**吊销**：事后发现插件有问题时，把条目改成：

```json
{ "revoked": true, "revokedReason": "具体原因，会原样展示给已安装该插件的用户" }
```

客户端检查更新时发现被吊销，会**警告并禁用**该插件。`revokedReason` 必填——用户有权知道为什么他装的东西被下架了。

---

## 四、内容哈希：为什么不是 zip 的 sha256

`index.json` 里每个插件都带一个 `contentHash`，客户端下载解压后会重算一遍比对，对不上就拒绝安装。这是**镜像可篡改**这条风险的收口手段：不管插件包经由官方源还是第三方镜像下载，内容对不上一律不装。

但校验的是**内容**，不是**容器**：

> GitHub 的归档不是字节级确定的。同一个 commit 在不同时间下载，zip 可能因为压缩实现变化而哈希不同（2023 年 GitHub 换压缩库时，全网依赖归档 checksum 的工具集体失效过一次）。拿 zip 的 sha256 当依据，早晚会变成「市场里所有插件突然全部校验失败」。

算法（`scripts/registry/hash.mjs` 与客户端 Rust 侧实现同一套）：

```
对插件目录下每个【文件】（目录不算，符号链接已在读 zip 时拒收）：
    rel  = 相对插件目录的路径，分隔符固定 '/'
    line = rel + "\t" + hex(sha256(文件字节))
所有 line 按 rel 的 UTF-8 字节序升序排序
contentHash = "sha256:" + hex(sha256(utf8(lines.join("\n"))))
```

排序特意用**字节序**而非语言默认排序：Rust 的 `String` 排序天然是 UTF-8 字节序，而 JS 默认按 UTF-16 比较——纯 ASCII 路径下两者一致，但插件里只要有一个中文文件名就会分道扬镳，导致该插件永远校验失败。

---

## 五、维护者操作

```bash
node scripts/registry/check.mjs           # 校验全部条目并重新生成 index.json
node scripts/registry/check.mjs --check   # 只校验不写文件（CI 在 PR 上跑的就是这个）
```

`index.json` 由 CI 在合并进 `main` 后自动重新生成并提交，**不要手改**。手改会在下次 CI 运行时被覆盖。
