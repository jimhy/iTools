# iTools 插件市场索引（已迁移，本目录仅作历史留存）

> ⚠️ **插件市场的真相源已经不是这个目录了。**
>
> 市场索引、插件包与审核**全部迁到自建服务端**：客户端连的是「设置 → 网络 → 服务器地址」
> 里那一台（与云同步共用），拉的是 `GET {服务器}/api/market/index`，包从
> `GET {服务器}/api/market/package/{name}` 下载。
>
> 作者上架的方式也变了：**在 iTools 的「开发者中心 → 发布」里点「提交审核」**，
> 客户端把插件目录打成 zip 传上去，服务端跑机械校验 + 大模型审核，通过即自动发布。
> 不再需要提 Issue 或 PR。

## 现在的链路在哪儿

| 你想找的 | 去哪儿看 |
|---|---|
| 作者怎么上架、状态怎么看 | [`doc/插件系统/开发者中心.md`](../doc/插件系统/开发者中心.md) 第 4.6 节 |
| 市场的信任模型与审核边界 | [`doc/插件系统/插件分发规范.md`](../doc/插件系统/插件分发规范.md) 第九节 |
| 服务端怎么部署、端点契约 | [`server/README.md`](../server/README.md)「插件市场与提审」 |
| 服务端的审核实现 | `server/src/pkg.rs`（机械校验）、`server/src/llm.rs`（模型审核）、`server/src/market.rs`（编排与索引） |

## 这个目录里还有什么用

- **`schema/entry.schema.json`**：市场条目的字段定义。服务端 `market.rs::publish` 生成的条目
  与它大体同形，留着便于对照；**但它不再是校验入口**（服务端不读它）。
- **`entries/` 与 `index.json`**：迁移前收录过的条目，**客户端已经不再拉取它们**。留作历史记录。
- **`scripts/registry/hash.mjs`**：内容哈希算法的 **Node 侧实现**。它仍然有用——
  内容哈希在三处各有一份实现（这里、`server/src/pkg.rs`、`src-tauri/src/plugin/market.rs`），
  必须逐字一致，客户端侧的跨语言基准用例（`market.rs::content_hash_matches_node_golden`）
  就是拿它的输出钉死的。**改任一侧算法都要三处同时改并重跑那组用例。**
- **`scripts/registry/check.mjs`**：针对旧 GitHub 收录流程的校验脚本，**当前链路不再使用**。

## 内容哈希算法（三处实现必须逐字一致）

```
对插件目录下每个【文件】（目录不算，符号链接在解 zip 时已拒收）：
    rel  = 相对插件目录的路径，分隔符固定 '/'
    line = rel + "\t" + hex(sha256(文件字节))
所有 line 按 rel 的 UTF-8 字节序升序排序
contentHash = "sha256:" + hex(sha256(utf8(lines.join("\n"))))
```

排序特意用**字节序**而非语言默认排序：Rust 的 `String` 排序天然是 UTF-8 字节序，而 JS 默认按
UTF-16 比较——纯 ASCII 路径下两者一致，但插件里只要有一个中文文件名就会分道扬镳，
导致该插件在客户端**永远校验失败**，而两边的测试各自都是绿的。
