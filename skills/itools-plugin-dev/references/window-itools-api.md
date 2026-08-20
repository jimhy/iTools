# window.itools API 参考

iTools 在插件页任何脚本前注入全局 `window.itools`。**所有系统能力都经它调用**；面板拿不到 Node、`window.__TAURI__`、npm 包。除 `showToast` / `platform` 外都返回 `Promise`。类型定义见同目录 `../assets/itools.d.ts`。

## 生命周期

### `itools.onEnter(cb)`
进入插件时回调（初始化都放这）。**保证进入插件时恰好触发一次，无需 `setTimeout` / `DOMContentLoaded` 兜底**（晚注册也会补触发）。**后台常驻插件的这个回调会被多次调用**，见下方「后台常驻」一节。
```js
itools.onEnter((info) => {
  // info = { code, type, query, files }
  // code : 命中的 feature.code（区分同插件多功能）
  // type : "keyword" | "regex" | "text" | "files" | "background" | "window"
  // query: 触发时搜索框里的文本。⚠️ text/regex 触发时是「用户输入的内容」（可预填）；
  //        keyword 触发时是关键词本身（别当内容填进输入框——做预填要判 info.type）
  // files: 仅 type === "files" 时有值，是拖入文件的**真实绝对路径数组**
  //        （网页 File 对象拿不到路径，拖放是在原生层接住的，这是它能给真实路径的原因）
});
```
`info.type` 六种取值：
- `"keyword"` / `"regex"` / `"text"`：搜索框触发，同上。
- `"files"`：用户把文件拖进主面板命中了 `plugin.json` 里声明的 `{"type":"files","ext":[...]}` / `{"type":"img"}` 触发规则，`info.files` 是绝对路径数组。
- `"background"`：插件被 iTools 后台拉起（开机自启动 / 定时任务 / 外部 AI 调用等），见「后台常驻」一节——此时不要弹界面。
- `"detached"`：这个页面是插件自己用 `itools.createWindow()` 开出来的独立窗口，不是主面板。**它不叫 `"window"`**——`plugin.json` cmds 里已经有一个 `{"type":"window", app/title/class}`，那是「仅当前台是某个窗口时才让某条 keyword/regex/text 出现」的门控条件（命中后 `info.type` 仍是 `keyword`/`regex`/`text`）。两者含义完全不同，所以独立窗口这一路改用 `detached`，避免同名混淆。

### `itools.onExit(cb)`
页面卸载/隐藏时回调。

## 窗口

- `itools.hide(): Promise<void>` — 隐藏插件窗口（保留，下次秒开）。约定：面板监听 Esc → `hide()`。
- `itools.exit(): Promise<void>` — 关闭插件窗口。
- `itools.setHeight(px: number): Promise<void>` — 设窗口高度（宽度不变），按内容自适应高度时用。

## 剪贴板

- `itools.copyText(text: string): Promise<void>` — 写剪贴板。
- `itools.readText(): Promise<string>` — 读剪贴板文本。常在 `onEnter` 里读来预填输入。

## 文件（限插件沙盒）

`readFile`/`writeFile` 只在插件自己的沙盒目录内，**path 为相对路径**（禁绝对路径与 `..`）。适合插件持久化自己的小文件；**不能读写用户任意路径**。

- `itools.readFile(path: string): Promise<string>`
- `itools.writeFile(path: string, content: string): Promise<void>`

> 需要持久化配置/数据，优先用 `db`（更简单，KV + 自动 JSON）。

## ⚠️ 浏览器级存储：每次进插件都会被清空

所有插件页**同源**（`itplugin://localhost/<id>/...`，id 在路径段不在 host），`localStorage` / `sessionStorage` / `IndexedDB` / Cache Storage 这些浏览器级存储天然是**插件间共享**的——插件 B 一行 `localStorage.getItem(...)` 就能读到插件 A 存的东西。为堵住这个隔离漏洞，iTools **每次进入插件都会清空它们**（在插件自己的脚本跑之前）。

- 结果：这几类存储对插件而言只是**会话级**的——本次进入插件时永远是空的，退出/切换插件后写的东西不会保留。
- 这是有意为之的隔离机制，不是 bug，别指望绕过。
- **要持久化一律用 `itools.db` / `itools.data` / `itools.sqlite`**（后端按插件 id 隔离，真正跨会话保留）。历史记录、上次输入、用户配置这类东西存进 `localStorage` 是白存——这是新手最容易踩的坑。

```js
// ❌ 存了也留不住，下次进来就是空的
localStorage.setItem("history", JSON.stringify(list));
// ✅ 持久化用 db（纯本地）或 data（本地优先 + 可同步）
await itools.db.set("history", list);
```

## 存储（KV，按插件隔离）

值自动 JSON 序列化/反序列化，可直接存对象/数组。

- `itools.db.get(key: string): Promise<any | null>`
- `itools.db.set(key: string, value: any): Promise<void>`
- `itools.db.remove(key: string): Promise<void>`
- `itools.db.keys(prefix?: string): Promise<string[]>`

```js
await itools.db.set("opts", { len: 16, symbol: true });
const opts = await itools.db.get("opts"); // → { len: 16, symbol: true } 或 null
```

## 账号态（只读）

查询用户的云账号登录态，用于决定「是否走云同步 / 是否引导登录」。**只暴露状态，不含用户名/token**（隐私）。

- `itools.account.state(): Promise<{ loggedIn: boolean; cloudConfigured: boolean; syncEnabled: boolean }>`
  - `loggedIn` — 是否已登录云账号。
  - `cloudConfigured` — 云端服务是否已接入（`false` = 未接入，只能本地）。
  - `syncEnabled` — 用户是否开启了「登录后自动同步」。
- `itools.account.isLoggedIn(): Promise<boolean>` — 便捷判断是否已登录。

## 同步型数据（本地优先 + 云同步）

与 `db` 一样是按插件隔离的 KV（值自动 JSON 序列化），**区别是 `data` 参与云同步**：写入**先落本地**（离线始终可用），用户已登录且云端已接入时可经 `sync()` 上行云端并回拉合并。

- `itools.data.get(key: string): Promise<any | null>`
- `itools.data.set(key: string, value: any): Promise<void>` — 先写本地、标记待同步。
- `itools.data.remove(key: string): Promise<void>`
- `itools.data.keys(prefix?: string): Promise<string[]>`
- `itools.data.sync(): Promise<{ synced: boolean; reason?: string; pushed: number; pulled: number; message?: string }>`
  - **诚实降级**：`synced=false` 时 `reason ∈ "cloud_not_configured" | "not_logged_in" | "offline" | "session_expired" | "error"`，数据仍安全保留在本地。

```js
// 本地优先：随时读写，离线也可用
await itools.data.set("note", { text: "hello" });
// 登录后可同步；未登录/未接云端时诚实返回 reason，不谎报
const r = await itools.data.sync();
if (!r.synced) itools.showToast(r.reason === "not_logged_in" ? "登录后可云端同步" : "已保存在本地");
```

> `db` = 纯本地永久 KV；`data` = 本地优先 + 可云同步。只在本机用选 `db`，想跨设备同步选 `data`。

## 设置（settings，只读）

读取用户在 iTools「插件管理 → 点开插件 → 设置」里为本插件配置的值。设置项由插件目录根的可选文件 `settings.json` 声明（schema），iTools 据此自动渲染设置界面并保存用户改动。运行时读到的**值 = schema 默认 + 用户覆盖**（iTools 已合并好）。**只读**：值只能由用户在管理中心改，插件没有 `set`。

- `itools.settings.get(key: string): Promise<any | null>` — 读单项；`key` 是 `settings.json` 里某项的 `key`，不存在返回 `null`。
- `itools.settings.all(): Promise<Record<string, any>>` — 读全部，返回 `{ key: value, ... }`。
- `itools.settings.onChange(cb: (values: Record<string, any>) => void): void` — 用户在管理中心改了本插件设置时回调，`cb` 收到最新的**全量**设置对象；用于实时重新应用（如启停全局热键）。

```js
const cfg = await itools.settings.all();        // { instantShot: true, ... }
const prefix = await itools.settings.get("filenamePrefix"); // 不存在 → null
itools.settings.onChange((v) => { /* 重新应用最新配置 */ });
```

> ⚠️ 插件自己的**业务状态**（历史记录、上次选中项等）用 `db` / `data`，**不要**塞进 settings。
> `settings.json` 的完整写法、控件类型（text/textarea/number/boolean/select/path/color/hotkey）见 `plugin-settings-spec.md`。

## 系统

- `itools.openExternal(url: string): Promise<void>` — 用默认浏览器打开网址（**仅 http/https/mailto**）。
- `itools.openPath(path: string): Promise<void>` — 用默认程序打开本地路径（拒绝可执行/脚本类文件）。
- `itools.notify(body: string): Promise<void>` — 通知（当前落日志）。

## 高危（需声明 permissions + 用户授权，见 plugin-spec.md）

- `itools.runCommand(program: string, args?: string[]): Promise<void>` — 执行程序（不经 shell）。需 `"permissions":["runCommand"]` + 授权，否则报错。
- `itools.fetch(url: string, init?: { method?: string; headers?: Record<string,string>; body?: string; responseType?: "text"|"binary" }): Promise<{ status: number; ok: boolean; body: string; base64: boolean }>` — 联网（原生代理，**非浏览器 fetch**，支持 **http/https**，返回文本体）。需 `"permissions":["network"]` + 用户授权。
  - **成功** resolve `{ status, ok, body, base64 }`（`base64` 缺省 `false`，`body` 是响应文本；要 JSON 自己 `JSON.parse(r.body)`）。
  - **失败**（未授权 / 断网 / DNS / 超时）会 **reject（throw）**——必须 `try/catch`。HTTP 4xx/5xx **不 throw**，resolve 成 `{ ok:false, status }`。
  - `responseType: "binary"`：响应体按二进制处理，`base64` 变 `true`，此时 `body` 是 base64 而不是文本。⚠️ 二进制响应体**上限 32MB**（整段留在内存里，base64 后还要再膨胀 1/3），超限直接报错而不是静默截断；更大的文件请用下面的 `itools.download`。
```js
// 标准写法：loading / 未授权 / 错误 三态
async function load() {
  setLoading(true);
  try {
    const r = await itools.fetch("https://api.example.com/x");
    if (r.ok) render(JSON.parse(r.body));
    else showError("请求失败 HTTP " + r.status);
  } catch (e) {
    // 最常见是未授权：提示用户去 iTools「插件管理」给本插件打开 network 授权
    showError("联网失败（若首次使用，请在「插件管理」授权本插件联网）：" + e);
  } finally { setLoading(false); }
}
// 取二进制（如图片）
const bin = await itools.fetch(imgUrl, { responseType: "binary" });
if (bin.ok) img.src = "data:image/png;base64," + bin.body; // bin.base64 === true
```

### 执行外部程序（exec 系列，需 `runCommand` 授权）

跟 `runCommand`（发射后不管，拿不到输出）不同，这组能拿到 stdout/stderr/退出码，适合 adb 一类要读命令输出的场景。跟 `runCommand` 一样**不经 shell**（`&`/`|`/`>` 等元字符不会被解释，没有 shell 注入面）。

- `itools.exec(program: string, args?: string[], opts?: { cwd?: string; timeoutMs?: number; encoding?: "utf8"|"gbk"|"auto" }): Promise<{ code: number; stdout: string; stderr: string; timedOut: boolean; truncated: boolean }>` — 一次性执行，等它跑完整体返回。
  - `timeoutMs`：到点是**真杀**进程（Windows 上 `TerminateProcess`），不是假装退出；此时 `timedOut=true`，`code` 只是「被杀那一刻」的系统退出码，不代表程序自身的真实结束状态。
  - ⚠️ `truncated`：单路输出超过 **16MB** 会被截断，为 `true` 时**不能**据 `stdout`/`stderr` 下「没有结果 / 没匹配到」这类结论——要完整输出请改用 `execStream`。
  - `encoding`：缺省 `"auto"`——对整段字节做严格 UTF-8 校验，能过就当 UTF-8，不能过就当 GBK 解。Windows 上不少中文 CLI 工具（如 adb、部分老工具链）用 GBK 而非 UTF-8 输出控制台内容，**明知目标程序编码时应显式传 `"gbk"`/`"utf8"`**，不要依赖自动探测。
- `itools.execStream(program: string, args?: string[], opts?: 同上, handlers?: { onData?(chunk: string): void; onErr?(chunk: string): void; onExit?(code: number, timedOut: boolean): void }): Promise<streamId>` — 流式执行，边跑边收输出，适合长跑命令（如 `adb logcat`）。
- `itools.execKill(streamId: string): Promise<void>` — 强杀。
- `itools.execQuit(streamId: string): Promise<void>` — 礼貌请求退出（是否优雅退出取决于目标程序自己）。

```js
// 一次性执行，注意 truncated
const r = await itools.exec("adb", ["shell", "pm", "list", "packages"], { encoding: "gbk" });
if (r.truncated) itools.showToast("输出过长已截断，结果可能不全，请改用 execStream");

// 长跑命令：流式收
const id = await itools.execStream("adb", ["logcat"], null, {
  onData: (chunk) => appendLog(chunk),
  onExit: (code, timedOut) => itools.showToast(timedOut ? "已超时终止" : `已退出 code=${code}`),
});
// 用完记得 execKill(id) 或 execQuit(id)，别让子进程孤儿挂着
```

### 下载文件（download 系列，需 `network` 授权）

大文件别用 `fetch`（整个响应体进内存，二进制还要 base64 膨胀，且全程没有进度）。`download` 边下边写落盘，带进度、可取消。

- `itools.download(url: string, dest: string, id: string, onProgress?: (p: { id: string; received: number; total: number|null; done: boolean; error?: string }) => void): Promise<{ path: string; size: number }>`
  - `dest` 是插件**沙盒内的相对路径**——与 `fs.write` 不同，这里**会自动新建父目录**。
  - `id` 由调用方**自己生成**并传入（不是下载开始后再返回）：命令是 async 的，等它返回时下载可能已经跑完了，届时再拿 id 去取消就晚了。
  - `total` 为 `null` 表示服务端没给 `Content-Length`，此时只能显示已下载量，不能算百分比。
  - 只支持 http/https，单文件上限 4GB。
- `itools.downloadCancel(id: string): Promise<void>`。

```js
const id = String(Date.now());
itools.download("https://example.com/big.mp4", "video.mp4", id, (p) => {
  progressBar.value = p.total ? p.received / p.total : 0;
  if (p.done) itools.showToast(p.error ? "下载失败：" + p.error : "下载完成");
}).catch((e) => itools.showToast("下载失败：" + e));
// 中途取消：itools.downloadCancel(id);
```

## 文件系统访问（用户选择即授权，需 `fs-user-scope`）

跟前面「文件（限插件沙盒）」不同：这组能碰**用户指定的任意目录/文件**，代价是插件不能自己指定路径去读——必须先弹原生对话框，由用户亲手选一次，插件此后只持有一个不透明的 `scopeId`，看不到、也改不了真实路径之外的任何东西。需在 `plugin.json` 声明 `"permissions":["fs-user-scope"]` + 用户在「插件管理」授权。

- `itools.fs.pickDir(opts?: { title?: string }): Promise<Scope | null>` — 弹「选择文件夹」对话框，用户选中即授权，取消返回 `null`。`Scope = { id, kind: "dir"|"file", path, label, created }`（`path` 只作展示，实际操作都传 `id`）。
- `itools.fs.pickFile(opts?: { title?: string; filters?: { name: string; extensions: string[] }[] }): Promise<Scope | null>` — 同上，选单个文件。
- `itools.fs.listScopes(): Promise<Scope[]>` — 列出本插件已获得的所有授权范围（跨会话持久保存，用户选过一次不用每次重选）。
- `itools.fs.revokeScope(scopeId: string): Promise<void>` — 主动放弃一个授权范围。
- `itools.fs.list(scopeId: string, subPath?: string): Promise<Entry[]>` — 枚举目录 scope 下一层内容。`Entry = { name, isDir, size, modified }`。
- `itools.fs.stat(scopeId: string, path?: string): Promise<Entry>` — 查询某项元信息；`path` 缺省指 scope 根本身（目录 scope）或该文件本身（文件 scope）。
- `itools.fs.hash(scopeId: string, path?: string, algo?: string): Promise<string>` — 目前只支持 `"sha256"`（缺省），流式读取，不会整份文件载入内存。
- `itools.fs.read(scopeId: string, path?: string): Promise<string>` — 读整个文件，返回 **base64**（用户文件可能是任意二进制，不能假设是文本）；大文件建议用 `readChunk`。
- `itools.fs.readChunk(scopeId: string, path: string, offset: number, len: number): Promise<string>` — 分块读，base64；单块上限 16MB。
- `itools.fs.write(scopeId: string, path: string | null, contentB64: string): Promise<void>` — 覆盖式写。⚠️ **不会自动建目录**，目标所在目录必须已经存在。
- `itools.fs.zipCreate(scopeId: string, entries: string[], outPath: string): Promise<{ entryCount: number; archiveBytes: number }>` — 把 scope 内若干文件/目录打成一个 zip（仅 deflate 压缩）。`entries` 是 scope 内相对路径，目录会被递归打包并保留自身这层目录名。`outPath` 所在目录同样**不会自动创建**（与 `write` 一致），必须已存在。
- `itools.fs.unzip(scopeId: string, zipPath: string, outSub: string): Promise<{ extractedFiles: number; extractedBytes: number }>` — 解压到 scope 内 `outSub`。⚠️ 与 `zipCreate`/`write` 相反，`outSub` **不存在会自动创建**（解压到某目录是用户的直觉预期）。内置 Zip Slip 防护、条目数/体积上限（防 zip 炸弹），拒绝符号链接条目。
- `itools.fs.watchStart(scopeId: string, subPath: string | null, cb): Promise<watchId>` — 监听目录变化，事件驱动（`ReadDirectoryChangesW`），不是轮询。`cb` 收到 `{ watchId, kind: "created"|"modified"|"removed"|"renamed"|"error", path, oldPath?, message? }`；同一路径 300ms 内的多次写入会合并成一条。`kind === "error"` 时该 `watchId` 此后不会再有事件（监听已终止）。同一插件最多同时开 20 个监听。
- `itools.fs.watchStop(watchId: string): Promise<void>`。
- `itools.fs.getFileIcon(scopeId: string, path?: string): Promise<string>` — 取文件/目录的系统图标（32×32），返回裸 base64 PNG。

```js
const scope = await itools.fs.pickDir({ title: "选择要清理的目录" });
if (!scope) return; // 用户取消
const entries = await itools.fs.list(scope.id);
const big = entries.filter((e) => !e.isDir && e.size > 100 * 1024 * 1024);

// 打包后清理
await itools.fs.zipCreate(scope.id, ["logs"], "logs-backup.zip");

// 目录变化监听
const watchId = await itools.fs.watchStart(scope.id, null, (ev) => {
  if (ev.kind === "error") return console.warn("监听已终止：", ev.message);
  console.log(ev.kind, ev.path);
});
// 用完记得 itools.fs.watchStop(watchId)
```

⚠️ **iTools 自身的数据目录一律拒绝访问**（那里面是所有插件的沙盒文件/KV/权限表），即便用户在对话框里亲自选中了它也会被拒绝——没有例外，不接受任何形式的「用户已同意」旁路。

## 命名系统位置与回收站（`paths` 需 `fs-named-path`，`trash` 需 `fs-trash`）

面向磁盘清理类插件：只开放一组**写死在后端的命名位置**，插件不能自己拼一个路径充当命名位置。

- `itools.paths.resolve(name: string): Promise<NamedPathInfo[]>` — 需 `"permissions":["fs-named-path"]`。`name` 取值：`temp` / `windowsTemp` / `downloads` / `desktop` / `documents` / `appData` / `localAppData` / `recycleBin` / `browserCache`。`NamedPathInfo = { label, path, exists, writable }`。⚠️ **返回值恒为数组**——`browserCache` 可能命中多个浏览器/多 profile 各一条，其余命名位置也统一包成单元素数组保持接口形状一致；不要只取 `[0]` 当作唯一结果，未安装的浏览器不会出现在结果里。
- `itools.paths.scan(name: string, opts?: { maxDepth?: number; maxItems?: number }): Promise<ScanResult>` — 只读枚举（不删除/不移动），供插件自己拼清理 UI。`maxDepth` 缺省 3、上限 8；`maxItems` 缺省 2000、上限 20000。`ScanResult = { fileCount, totalSize, items: ScanItem[], truncated }`，`ScanItem = { path, size, isDir }`。`truncated` 只反映条数截断，不反映深度截断。
- `itools.trash(paths: string[]): Promise<{ path: string; ok: boolean; error?: string }[]>` — 需 `"permissions":["fs-trash"]`（与 `fs-named-path` 分开申请，读位置 ≠ 允许删）。送入**回收站**（可还原）；iTools 不给插件「真删」的接口，也不会加。批量逐条报告，某一条失败不影响其它条目。

⚠️ 与 `fs.*` 一样，任何落在 iTools 自身数据目录内的路径一律拒绝，硬黑名单，无例外。

## 图像处理（`image`，无需授权）

纯内存计算，不读屏幕、不碰文件系统之外的东西，因此不设权限门禁。输入输出都是**裸 base64**（不带 `data:image/...;base64,` 前缀）。

- `itools.image.resize(data: string, width: number, height: number, mode?: "contain"|"fill"|"cover"): Promise<string>` — `mode` 缺省 `"contain"`（等比不裁剪）；`"fill"` 拉伸不保比例；`"cover"` 等比裁剪填满。输出沿用原图格式。
- `itools.image.crop(data: string, x: number, y: number, w: number, h: number): Promise<string>` — 越界直接报错，不做「自动裁小」的静默处理。
- `itools.image.convert(data: string, format: "png"|"jpeg"|"webp"|"bmp"): Promise<string>`。
- `itools.image.compress(data: string, quality: number): Promise<string>` — `quality` 1-100，越界会被夹到区间内，不报错。⚠️ **只支持 JPEG 源图**：WebP 在本项目里只有无损编码器，做不出「按质量调体积」的有损压缩；PNG/BMP 本身也没有「质量」这个维度。要压别的格式，先用 `convert` 转成 jpeg 再压。
- `itools.image.info(data: string): Promise<{ width: number; height: number; format: string; sizeBytes: number }>` — `sizeBytes` 是原始文件体积（base64 解码后），不是解码后的像素占用。

⚠️ **WebP 编码只有无损（VP8L）**：`convert` 转出的 webp 体积不一定比原图小，不要假设「转 webp 就变小」。

## 屏幕辅助（`screen`）

鼠标坐标、屏幕取色、DIP（设备无关像素，即插件页 CSS/DOM 坐标）↔ 物理像素（Win32 原生坐标）换算。多屏高 DPI 下不换算会出现「点在这、贴图对到那」。

- `itools.screen.cursorPoint(): Promise<{ x: number; y: number }>` — 鼠标当前**物理像素**坐标，无需授权。
- `itools.screen.pickColorAt(x: number, y: number): Promise<{ hex: string; r: number; g: number; b: number }>` — **需 `screen-capture` 授权**（读屏幕内容，与截图共用同一个权限标识）。
- `itools.screen.toDip(x: number, y: number): Promise<{ x: number; y: number }>` / `itools.screen.toPhysical(x: number, y: number): Promise<{ x: number; y: number }>` — 单点换算。
- `itools.screen.rectToDip(x, y, width, height)` / `itools.screen.rectToPhysical(x, y, width, height): Promise<{ x; y; width; height }>` — 矩形换算。

```js
const p = await itools.screen.cursorPoint(); // 物理像素
const dip = await itools.screen.toDip(p.x, p.y); // 换算成页面可用的 CSS 坐标
```

⚠️ 多屏呈对角线摆放等非常规布局下，换算可能有像素级偏差（常规左右/上下拼接布局是精确的）。

## 系统信息与标准路径（`sys`，无需授权）

只读查询，公开信息，不设权限门禁。

- `itools.sys.info(): Promise<SysInfo>` — 静态信息（开机期间基本不变）：`{ osVersion, hostname, cpuName, cpuLogicalCores, gpuNames, memoryTotalBytes, hasBattery, disks }`，`disks: { drive, totalBytes, freeBytes, fileSystem, driveType }[]`（`driveType` 为 `"fixed"|"removable"|"remote"|"cdrom"|"ramdisk"`）。⚠️ 每一项都可能是 `null`/空数组——取不到就如实留空，**不会**拿 0 或猜测值顶替，调用方必须判空。
- `itools.sys.usage(): Promise<SysUsage>` — 动态信息（需采样）：`{ cpuPercent, memoryUsedBytes, memoryAvailableBytes, memoryUsedPercent, batteryPercent, isCharging, downBytesPerSec, upBytesPerSec, disks }`。⚠️ 会真实 sleep ~200ms 采样两次算差值，耗时比普通查询略高，是预期行为；网速用的是旧版 32 位计数器（`GetIfTable`），极高流量下理论上有回绕（wraparound）风险。
- `itools.sys.getPath(name: string): Promise<string>` — 白名单：`home`/`appData`/`localAppData`/`temp`/`desktop`/`documents`/`downloads`/`pictures`/`videos`/`music`/`fonts`/`startup`/`programFiles`。⚠️ **只是把名字翻译成路径字符串，不代表获得访问权限**——要真正读写用户文件，仍须走 `itools.fs`（`fs-user-scope` 授权 + 用户亲自选择）。
- `itools.showItemInFolder(path: string): Promise<void>` — 在资源管理器中定位并高亮该文件（`explorer /select,`），路径必须已存在，否则报错。

## SQLite（`sqlite`，无需授权，按插件隔离）

`db`/`data` 是 KV，数据量大、要做关联查询时换这个。库文件按插件 id 物理隔离，别的插件碰不到。

- `itools.sqlite.open(name: string): Promise<handle>` — `name` 只允许 `[A-Za-z0-9_-]`，1-64 字符，且不能是 Windows 保留设备名（`CON`/`PRN`/…）；库不存在则自动创建。
- `itools.sqlite.exec(handle: string, sql: string, params?: any[]): Promise<affectedRows>` — 写语句（INSERT/UPDATE/DELETE/CREATE...），返回受影响行数。
- `itools.sqlite.query(handle: string, sql: string, params?: any[]): Promise<Record<string, any>[]>` — 查询，每行是 `{ 列名: 值 }`。
- `itools.sqlite.batch(handle: string, statements: { sql: string; params?: any[] }[]): Promise<affectedRows>` — 整体在一个事务里执行，任何一条失败**全部回滚**。
- `itools.sqlite.close(handle: string): Promise<void>`。

参数用 `?` 占位符 + 数组绑定，**不要自己拼 SQL 字符串**；BLOB 用 `{ "$blob": "<base64>" }` 标记（查询结果里的 BLOB 列也是这个形状）。

```js
const h = await itools.sqlite.open("notes");
await itools.sqlite.exec(h, "CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY, body TEXT)");
await itools.sqlite.exec(h, "INSERT INTO t(body) VALUES(?)", ["hello"]);
const rows = await itools.sqlite.query(h, "SELECT * FROM t WHERE id > ?", [0]);
await itools.sqlite.close(h);
```

⚠️ **红线（插件间数据隔离）**：`ATTACH`/`DETACH`、`VACUUM`/`VACUUM INTO`，以及大多数会改变库行为的 `PRAGMA`（如 `journal_mode`）一律被拒绝——这些能绕过隔离碰到别的插件的库，没有豁免。只读自省类 PRAGMA（`table_info`/`index_list`/`foreign_key_list` 等）放行。**每次调用只能喂一条语句**（`batch` 除外，它是多条语句的专用通道）。

## 上下文感知（`context`，需 `context-read` 授权）

告诉插件「用户唤起 iTools 之前，前台是什么」——「在 Chrome 里唤起就给下载视频、在资源管理器里唤起就给清理此目录」这类场景的地基。需声明 `"permissions":["context-read"]` + 用户授权。

- `itools.context.activeWindow(): Promise<{ app: string; title: string; class: string; hwnd: number; rect: { left; top; right; bottom } }>` — 唤起前的前台窗口快照。iTools 刚启动、用户还没从别的窗口切换过来时会 **reject**（不会返回假数据）。
- `itools.context.browserUrl(): Promise<string | null>` — 读浏览器地址栏。**只支持 Chrome / Edge**（按控件类名精确定位）；Firefox 是尽力而为（按 AutomationId 找，不同版本可能失效）。⚠️ 拿不到就是 `null`（非浏览器窗口、地址栏控件找不到、控件没有值），**必须判空**，不会拼一个看着像 URL 的假值。
- `itools.context.folderPath(): Promise<string | null>` — 读资源管理器当前目录。虚拟命名空间目录（此电脑/回收站/控制面板…没有文件系统路径）同样返回 `null`。

```js
const url = await itools.context.browserUrl();
if (url) prefillFrom(url);
else itools.showToast("当前不是浏览器窗口，或读不到地址栏");
```

## 后台常驻（随 iTools 启动）

面向「需要一直监听全局快捷键」的插件（典型如截图工具）：不唤起也能在后台跑，注册好热键等用户随时按。

- **声明**：`plugin.json` 顶层加 `"background": true`。没声明的插件，「插件管理」里根本不会出现「随 iTools 启动」这个开关（后端也会直接拒绝开启请求）——不是「开了没用」的假开关，是压根不给开。
- **开关**：用户在「插件管理 → 点开插件 → 设置」里打开「随 iTools 启动」后，iTools 每次启动都会在一个**隐藏窗口**里把这个插件加载起来（开关本身打开的瞬间也会立即拉起一次，不用等下次重启）。
- **后台态 `onEnter`**：被开机拉起时，`onEnter(info)` 收到的是 **`info.type === "background"`**。此时应当**只做注册**（`registerHotkey`、事件监听等），**不要弹界面、不要读剪贴板**——窗口是隐藏的，弹了用户也看不见，纯占资源。
- **⚠️ 最大的行为差异：`onEnter` 会被多次调用**。用户之后主动唤起该插件时，iTools **不会新开一份实例**去装同一个插件（避免两份实例抢同一个全局热键、并发写同一份 db/sqlite），而是把这个后台窗口显示出来；页面**不重载**（内存状态、监听器都保留），但会经事件总线**再触发一次 `onEnter`**，这次 `info.type` 是真实触发类型（`keyword`/`text`/`regex`）。所以 `onEnter` 里的注册逻辑要**可重复调用而不出错**（比如重复 `registerHotkey` 前先判断是否已注册过，或用一个标志位只跑一次初始化），别假设它这辈子只会被调一次。
- **关闭**：用户关掉「随 iTools 启动」开关、禁用插件、或卸载插件，后台实例都会被立刻关闭；它注册的全局热键随之失效（热键绑定按窗口会话存，窗口没了自然收不到）。

> 这个开关不是让插件常驻的唯一途径：声明了 `mainPush: true` 的 feature（见下文「搜索结果注入」）**自动**后台常驻，不需要这个开关；被外部 AI 通过 `registerTool` 调用时（见「暴露为 MCP 工具」）宿主也会按需临时拉起，同样不需要它。这个开关只解决「插件自己想常驻」这一种场景（典型是需要注册全局热键的截图类插件）；`itools.schedule.*`（定时任务）额外需要 `background` **权限**（不是这里的 `background` 清单字段，见「定时任务」一节，两者是不同的门，容易搞混）。

```js
itools.onEnter((info) => {
  if (info.type === "background") {
    // 后台态：只注册，不碰界面
    if (!window.__hotkeyRegistered) {
      itools.registerHotkey("CmdOrCtrl+Shift+A"); // 需 hotkey 授权
      window.__hotkeyRegistered = true;
    }
    return;
  }
  // 真实触发（keyword/text/regex）：这时才渲染界面
  // 注意：后台常驻插件的 onEnter 可能被多次调用，初始化 DOM 监听时也要防重复绑定
  renderPanel(info);
});
```

## 输入注入（`input`，需 `input-inject` 授权）

模拟键鼠输入，供翻译/代码片段/表情包/自动化类插件使用。**标准动作是「隐藏 iTools → 写/发到目标应用」**。

- `itools.input.typeString(text: string): Promise<void>` — 按“输入法”原理输入任意字符串（支持 Emoji、中文等任意 Unicode），不是逐键模拟，不要求目标控件支持 IME。
- `itools.input.pasteText(text: string): Promise<void>` — 写入剪贴板并发送 Ctrl+V。
- `itools.input.pasteFile(paths: string[]): Promise<void>` — 把一批**本机绝对路径**以文件列表形式写入剪贴板并 Ctrl+V，等价于资源管理器里 Ctrl+C 这些文件再粘贴。
- `itools.input.pasteImage(data): Promise<void>` — 图片写入剪贴板并 Ctrl+V；`data` 接受裸 base64、带 `data:image/...;base64,` 前缀的字符串，或 `ArrayBuffer`/`Uint8Array`。
- `itools.input.keyTap(key: string, modifiers?: string[]): Promise<void>` — 组合键点按。`modifiers` 支持 `"ctrl"`/`"shift"`/`"alt"`/`"win"` 任意组合；`key` 支持单字符（按当前键盘布局识别）与具名键（方向键、功能键 `F1`~`F12`、`Enter`/`Esc`/`Tab`/`Space`/`Backspace`/`Delete` 等）。
- `itools.input.mouseMove(x, y)` / `mouseClick(x, y)` / `mouseDoubleClick(x, y)` / `mouseRightClick(x, y): Promise<void>` — 屏幕**物理像素**坐标（虚拟桌面坐标系，跨多屏）。

⚠️ **两条会导致「时灵时不灵」的限制，务必了解**：
1. **注入前必须先 `itools.hide()`**（或切走焦点）。本组 API 只管把输入事件灌进 Windows 全局输入队列，它会打到当前**前台窗口**——如果 iTools 自己的窗口还在前台，输入会打在 iTools 自己身上，不是目标应用。
2. **受 Windows UIPI 限制**：非管理员运行的 iTools 无法向运行在更高完整性级别的窗口注入（如以管理员身份运行的程序、UAC 对话框）。此时会静默失败或部分失败，这是 Windows 的强制安全边界，不是 bug，也无法在用户态绕过。

```js
async function pasteTranslation(text) {
  await itools.hide();               // 先隐藏，别打到自己身上
  await itools.input.pasteText(text);
}
```

## 剪贴板变化监听（`clipboard`，需 `clipboard-watch` 授权）

- `itools.clipboard.watchStart(): Promise<void>` — 开始监听（轮询序列号，300ms 一次）。同一会话重复调用是幂等的。
- `itools.clipboard.watchStop(): Promise<void>`。
- `itools.clipboard.onChange(cb): void` — `cb` 收到 `{ sequence: number }`。⚠️ **事件只带序列号，不带内容**——变了之后自己再调 `itools.readText()`/`readImage()` 去读，免得每次变化都把剪贴板全文推一遍。

## 桌面窗口管理（`win`，需 `window-manage` 授权）

管理**别的应用**的窗口（置顶/移动/缩放/最小化/关闭/切前台），窗口置顶器、批量排列这类小工具的地基。

- `itools.win.list(): Promise<WindowItem[]>` — 枚举当前「用户看得见、有意义」的顶层窗口（已过滤不可见、无标题、工具窗口、被 DWM cloak 的窗口）。`WindowItem = { hwnd, app, title, class, rect: {left,top,right,bottom}, minimized, maximized, topmost }`。
- `itools.win.getForeground(): Promise<WindowItem>` — 此刻真正的前台窗口（与 `context.activeWindow` 不同：这个是**当下**，那个是「唤起 iTools 之前」；如果此刻前台确实是 iTools，如实返回 iTools 自己）。
- `itools.win.focus(hwnd: number): Promise<{ success: boolean; reason?: string }>` — 激活并前置目标窗口。⚠️ **不保证成功**：Windows 有前台窗口抢占锁定，`success` 如实反映这次是否真的切成功了，调用方不能因为没抛错就当成功了。
- `itools.win.move(hwnd, x, y)` / `resize(hwnd, w, h)` / `setRect(hwnd, { x, y, w, h }): Promise<void>` — 移动/缩放/一次性设置位置尺寸（物理像素）。
- `itools.win.minimize(hwnd)` / `maximize(hwnd)` / `restore(hwnd)` / `setTopmost(hwnd, on): Promise<void>`。
- `itools.win.close(hwnd: number): Promise<void>` — ⚠️ 只投递 `WM_CLOSE` 消息，**不保证目标真的关闭**——目标可能弹「是否保存」对话框，也可能自己忽略这条消息。

⚠️ **绝不能操作 iTools 自己的窗口**：所有会改变窗口状态的命令，目标若是 iTools 自身进程的窗口一律拒绝并报错——这是防止插件把宿主搞成「热键唤不出来、看不见窗口」的不可恢复状态。

## 托管本地服务与局域网发现（`serve` / `lan`，需 `local-server` 授权）

供「手机扫码连电脑传文件」这类插件使用。**宿主托管**，插件拿不到裸 socket。

- `itools.serve.start(opts): Promise<{ serveId, port, urls: string[] }>` — 起一个本地 HTTP 文件服务，绑 `0.0.0.0`（局域网可访问）。
  - `opts: { scopeId, subPath?, port?, readOnly?, allowUpload?, token?, ttlSecs? }`。`scopeId` 必须是已通过 `fs.pickDir`/`pickFile` 拿到的 scope；越界校验复用 `fs.*` 同一套黑名单。`readOnly` 与 `allowUpload` 不能同时为 `true`。`ttlSecs` 范围 60~86400，缺省 1800 秒，到点宿主自动停止（插件应周期性调用保活/续期，不是起一次挂一整天）。
  - `urls` 是已内嵌 `?token=` 的可直接访问地址；**默认必带访问令牌**，不传 `token` 由宿主随机生成——局域网不是可信网络，没有「裸奔」这个选项。
  - ⚠️ **不生成二维码**：宿主只给 `urls`，二维码请插件自己用内联 JS（canvas / 前端 qrcode 库）画。
  - ⚠️ 上传非流式，单次上限 256MB；不支持 Range 请求（视频类文件拖进度条不生效）。
- `itools.serve.stop(serveId: string): Promise<void>` / `itools.serve.list(): Promise<ServeInfo[]>`。
- `itools.lan.announce(opts): Promise<void>` — 广播「我在这」。`opts: { name, port, info?, ttlSecs? }`（缺省 300 秒，到期需插件自己重新调用续期）。
- `itools.lan.discover(timeoutMs?: number): Promise<{ ip, name, port, info }[]>` — 广播查询，收集 `timeoutMs`（会被夹到 200~15000）毫秒内的应答。⚠️ 走 UDP **广播**不是组播，发现范围仅限同一子网/同一路由器下，跨路由/跨 VLAN 发现不了。

```js
const scope = await itools.fs.pickDir({ title: "选择要分享的文件夹" });
const { urls } = await itools.serve.start({ scopeId: scope.id, readOnly: true });
// urls[0] 已带 token，插件自己拿去画二维码给手机扫
```

## 进程 / 已装软件 / 启动项 / 电源

四类系统管理能力，**分三档权限**，不合并成一个大权限：

- `itools.proc.list(): Promise<ProcInfo[]>`（需 `process-manage`）— `ProcInfo = { pid, name, exePath?, memoryBytes?, parentPid? }`。`exePath`/`memoryBytes` 常因权限不足（系统进程/其它用户的进程）查不到，此时为 `null`，不会伪造成 0。
- `itools.proc.kill(pid: number): Promise<void>`（需 `process-manage`）— **立即、不可逆**结束进程，未保存数据会丢失。会拒绝结束 iTools 自身进程、其父进程链上的进程、以及固定名单里的系统关键进程（`csrss.exe`/`winlogon.exe`/`services.exe` 等）与保留 pid，其余只要 Windows 权限允许就真的会杀掉，调用方需自行确认目标 pid。
- `itools.installedApps(): Promise<InstalledApp[]>`（需 `system-read`）— 读注册表卸载项，`{ name, version?, publisher?, installLocation?, uninstallCommand?, estimatedSizeBytes? }`。只**读出**卸载命令，不负责执行。
- `itools.startup.list(): Promise<StartupItem[]>`（需 `system-manage`）— `{ id, name, command?, source, enabled }`。
- `itools.startup.remove(id: string): Promise<void>`（需 `system-manage`）— **立即、不可逆**删除该启动项，无回收站。
- `itools.startup.setEnabled(id: string, on: boolean): Promise<void>`（需 `system-manage`）— 走 Windows 官方的禁用标记（与任务管理器「启动」标签页效果一致），可逆。
- `itools.power.lock() / sleep(): Promise<void>`（需 `system-manage`）。
- `itools.power.shutdown(force?) / restart(force?: boolean): Promise<void>`（需 `system-manage`）— **默认非强制**（会给前台应用弹「是否保存」的机会）；`force=true` 会强制关闭所有应用，**未保存数据会丢失**，调用前务必让用户明确知情。

⚠️ **不提供「添加启动项」**：只能列出/移除/禁用已存在的启动项，刻意不给「新增」接口——这是恶意软件驻留系统最经典的手法，不开这个口子。

## 宿主托管运行时（`runtime`，需 `runtime` 授权）

ffmpeg / adb / yt-dlp 这类插件用得上但塞不进插件包（几十上百 MB，且插件包禁止携带可执行文件）的外部程序，由宿主统一下载、SHA-256 校验、管理版本；插件按「名字 + 参数」调用，**拿不到裸路径**。

- `itools.runtime.list(): Promise<RuntimeInfo[]>` — `{ name, displayName, version, installed, sizeBytes?, description, installable }`。`installable: false` 表示清单没有可信哈希，`ensure` 一定会失败——UI 应据此禁用「安装」按钮，别让用户点了才发现装不上。
- `itools.runtime.ensure(name: string, onProgress?): Promise<RuntimeInfo>` — 未安装则下载+校验+安装，已安装直接返回现状。`onProgress` 收 `{ name, received, total, done, error }`。
  - ⚠️ **当前 `ffmpeg` / `adb` / `yt-dlp` 三项的官方清单都还没填入可信 SHA-256**，`ensure` 会**直接拒绝、不发起任何下载**，报错说明原因——这是真实现状，不是「即将支持」也不是「可用但要下载」，UI/文案不能写成「可用」。宁可暂时装不上，也不放行未经校验的可执行文件。
- `itools.runtime.exec(name, args?, opts?): Promise<{ code, stdout, stderr, timedOut, truncated }>` — 一次性执行并等待结果，需已 `ensure` 过。`opts: { cwd?, timeoutMs? }`。输出按 UTF-8 宽松解码（不像 `itools.exec` 那样做 GBK 探测）。⚠️ 单路输出同样有 16MB 截断上限；与 `itools.exec` 一样，超限时 `truncated` 为 `true`，**此时不能据 `stdout`/`stderr` 下「没有结果」这类结论**，请改用 `execStream` 取完整输出。
- `itools.runtime.execStream(name, args?, opts?, handlers?): Promise<streamId>` — 流式执行。`handlers: { onStdout, onStderr, onExit(code, timedOut), onFfmpegProgress }`；`onFfmpegProgress` 仅 `name === "ffmpeg"` 时有，宿主已把进度行解析成 `{ frame, fps, bitrate, time, timeSecs, speed }`，不用自己写解析器。
- `itools.runtime.kill(streamId)` / `quit(streamId): Promise<void>`。
- `itools.runtime.remove(name: string): Promise<void>` — 删除已下载的运行时，释放磁盘。

⚠️ **权限边界**：`runtime` 只能跑清单里这几个宿主下载校验过的固定程序，风险远低于能跑任意程序的 `runCommand`，两者是完全独立的权限，不能互相替代。但 `runtime` 也不是完全沙盒化的执行环境——ffmpeg/adb 本身功能强大，`-i`/`push`/`pull` 这类参数天然允许读写宿主进程有权限触达的文件，授权 `runtime` 等于允许插件以宿主进程权限调用这些程序。

## 加密存储 / 附件存储 / 定时任务

### 加密存储（`crypto`，无需权限声明，会话内即用）

与 `db` 一样按插件隔离的 KV，区别是值用 **Windows DPAPI**（`CryptProtectData`）加密落盘，密钥完全由系统托管、绑定当前登录账户。

- `itools.crypto.set(key, value)` / `get(key): Promise<any | null>` / `remove(key)` / `keys(prefix?): Promise<string[]>` — 用法与 `db.*` 完全一致，值自动 JSON 序列化。

⚠️ **必须诚实理解防护边界**：
- **防得住**：别的 Windows 用户账户登录后读这份数据；有人把整个数据文件拷到别的机器/账户下想离线解密。
- **防不住**：**当前登录账户下运行的其它程序**。任何以同一用户身份跑的进程都能调用一模一样的 API 把密文解开——这是 DPAPI 保护「账户边界」而非「进程边界」的本质决定的，不是 iTools 的实现缺陷。所以这套存储只能定位为「防拷库 / 防换人」，**不能**当成「防同机其它软件窥探」的强隔离，UI/文案不得夸大成「绝对安全」。

### 附件存储（`attach`，无需权限声明）

存二进制大对象（图片/音频/导出文件），按插件隔离，**单个附件上限 32MB**，超限直接报错（不静默截断）。

- `itools.attach.put(id: string, data, mime?: string): Promise<void>` — `data` 同 `pasteImage`，接受 base64（带/不带 `data:` 前缀）或 `ArrayBuffer`/`Uint8Array`。
- `itools.attach.get(id: string): Promise<{ dataB64, mime, size } | null>` — 不存在返回 `null`。
- `itools.attach.remove(id: string): Promise<void>` — 幂等，不存在也算成功。
- `itools.attach.list(): Promise<{ id, mime, size, createdAt }[]>`。

### 定时任务（`schedule`，需 `background` 权限）

- `itools.schedule.add(opts): Promise<{ taskId }>` — `opts: { everySecs, code, payload? }`。**只支持固定间隔**，到点推 `plugin-schedule-fire` 事件（`{ taskId, code, payload, firedAt }`），不会自作主张弹窗口。
  - ⚠️ **不支持 cron**：`opts.atCron` 传了会直接报错拒绝，不会被静默忽略成「看似接受、实则从不触发」的假成功。
  - 任务持久化在插件数据目录，不含下次触发时间——重启/重装载后从「此刻起再走一个整间隔」计时，**不做错过重排的追赶式补发**。
- `itools.schedule.remove(taskId: string): Promise<void>` / `itools.schedule.list(): Promise<ScheduleInfo[]>`。
- `itools.schedule.onFire(cb): void`。

```js
const { taskId } = await itools.schedule.add({ everySecs: 3600, code: "check-update" });
itools.schedule.onFire((ev) => {
  if (ev.code === "check-update") checkForUpdate();
});
```

⚠️ **`schedule.*` 需要的 `background` 是一个独立的高危权限**（要在 `permissions` 里声明 `"background"`，并由用户在「插件管理」里像 `runCommand`/`network` 那样单独授权），**跟顶层清单字段 `"background": true`（后台常驻资格声明）不是一回事**——两者恰好同名，很容易混淆：
- `"background": true`（清单顶层字段）：让插件**有资格**被设为「随 iTools 启动」，是「能不能常驻」的门。
- `"permissions": ["background"]` + 用户授权：是 `schedule.*` 这组调用本身的门禁，属于「能不能用定时任务」。
一个用到定时任务的插件通常两者都需要：声明 `background: true` 并引导用户打开「随 iTools 启动」（否则插件没在跑，任务无从触发），同时在 `permissions` 里声明并让用户授权 `background`（否则 `schedule.add` 等调用直接报错）。

## 通知增强与插件托盘

### 通知（`notifyShow` 等，无需权限声明）

是旧版 `itools.notify(body)` 的加强版——旧版只有一行正文，没有标题/点击回调/动作按钮；这些**现在都已真正生效**，不是收下不生效的占位字段。

- `itools.notifyShow(opts): Promise<{ notifyId }>` — `opts: { title?, body, featureCode?, silent?, actions?: {id, label}[] }`。
  - `featureCode`：点击通知**本体**时唤起本插件的该 feature；必须是清单里真实声明的 code，传错直接报错拒绝。不传则点击本体改为推 `plugin-notify-click` 事件（带 `notifyId`）。
  - `actions`：通知上的动作按钮，点击推 `plugin-notify-action` 事件（带 `notifyId`、`actionId`），与点击本体互不影响——按钮永远走 action 事件。
  - `silent`：静音展示，已生效。
- `itools.onNotifyClick(cb): void` — `cb({ notifyId })`。
- `itools.onNotifyAction(cb): void` — `cb({ notifyId, actionId })`。

### 插件托盘（`tray`，需 `tray` 授权）

一个插件最多一个托盘图标，与 iTools 宿主自己的托盘完全隔离（改不到宿主那个）。后台常驻类插件通常靠它作为唯一可见入口。

- `itools.tray.set(opts): Promise<void>` — `opts: { icon?, tooltip?, menu?: {id,label,enabled?,separator?}[] }`。**重复 `set` 是更新**（复用同一个图标对象），不是每次新建。`icon` 缺省用插件自己的 logo，两者都没有就报错——没图标的托盘图标在系统里是个空白方块，不能悄悄这么弹给用户。
- `itools.tray.remove(): Promise<void>`。
- `itools.tray.onClick(cb): void` — 点击图标本体。
- `itools.tray.onMenu(cb): void` — `cb({ id })`，点击某个菜单项。

## 插件间跳转与独立窗口

- `itools.redirect(label: string, payload?: string): Promise<void>` — 跳到另一个插件的某个功能（调用方自己先隐去）。`label` 可用 `"插件id#code"` 精确匹配，或对方的关键字/标题模糊匹配。`payload` 经对方的 `onEnter` 的 `info.query` 送达（不新开一条通道）。
- `itools.createWindow(page: string, opts?): Promise<label>` — 开一个**独立窗口**（可留在桌面上，与主面板并存）。`page` 是插件目录内的相对页面路径，走同一个受限协议、同样注入 `window.itools`、同样受 CSP 约束——不是绕开沙盒的口子。该页面的 `onEnter` 收到 `info.type === "detached"`。`opts: { title?, width?, height?, resizable?, alwaysOnTop?, query? }`。同一插件对同一 `page` 多次调用视为复用（不会攒一堆重复窗口）。调试会话暂不支持。
- `itools.closeWindow(label: string): Promise<void>` — 只能关本插件自己开的窗口。

## 动态指令（无需权限声明）

运行时增删本插件的触发条目，不必改 `plugin.json`，加完立刻能在主搜索里搜到（与清单里的静态 feature 走同一套匹配规则）。用途：让用户在插件里自定义条目（如「网址快开」让用户自己加网址）。

- `itools.setFeature(feature): Promise<void>` — 新增/更新一条动态指令，同 `code` 覆盖，结构与 `plugin.json` 的 feature 一致。
- `itools.removeFeature(code: string): Promise<boolean>` — 返回是否真的存在并被删除。
- `itools.getFeatures(codes?: string[]): Promise<Feature[]>` — 列出本插件的动态指令，可按 code 过滤。

调试会话暂不支持动态指令（避免污染用户已装的同名正式插件的数据目录）。

## 搜索结果注入（`onMainPush`，无需权限声明，自动后台常驻）

让本插件的结果直接出现在**主搜索框**里，用户边打字就能看到——这是「一堆小面板」和「平台」的分界线。

- `itools.onMainPush(getList): Promise<void>` — `getList(query)` 返回数组（或 Promise），每项 `{ title, subtitle?, payload?, code?, icon? }`；用户选中后会打开本插件，`payload` 经 `onEnter` 的 `info.query` 送达。

```js
// 必须在页面初始化时调用，与 onEnter 同级——不能写在 onEnter 回调里
itools.onMainPush((query) => {
  return history.filter((h) => h.includes(query)).slice(0, 5).map((h) => ({ title: h }));
});
itools.onEnter((info) => { /* ... */ });
```

**前提**：`plugin.json` 里至少一个 feature 声明 `"mainPush": true`。声明了 `mainPush` 的插件会被**自动后台常驻**，不需要用户去开「随 iTools 启动」开关——它必须活着才能回应查询。

⚠️ **两条硬约束**：
1. **必须在页面初始化时调用，不能写在 `onEnter` 回调里**（原因同 `registerTool`，见下）。
2. **宿主只等 250 毫秒**：主搜索是逐键触发的，任何插件卡一下整个搜索框都会顿。`getList` 回调里**不要做慢活**（不要发网络请求、不要扫磁盘），超时的结果会被**直接丢弃**——这不是失败，是设计上的取舍，务必让回调足够快。

## 暴露为 MCP 工具（`registerTool`，无需权限声明）

把插件的能力注册成 MCP 工具，让 **Claude Code / Cursor 等外部 AI** 直接调用——这是 iTools 独有的一条路：同类产品是「插件把工具注册给自家 AI 助手」，iTools 反过来，插件写好的能力自动成为任何外部 AI agent 可调用的工具。

- `itools.registerTool(name: string, handler): Promise<void>` — `name` 必须与 `plugin.json` 顶层 `tools` 里声明的键一致，否则报错拒绝。`handler(params, ctx)` 收到 AI 传入的参数，可以是同步函数或返回 Promise；返回值会回传给 AI（非字符串会被 `JSON.stringify`），抛错则把错误信息回传给 AI。

```json
// plugin.json
{ "tools": { "gifify": { "description": "把一段视频转成 gif", "inputSchema": { "type": "object", "properties": { "path": {"type":"string"} } } } } }
```
```js
// 必须在页面初始化时调用，与 onEnter 同级
itools.registerTool("gifify", async (params, ctx) => {
  // params.path、ctx.requestId
  return { ok: true, outPath: "..." };
});
itools.onEnter((info) => { /* ... */ });
```

⚠️ **必须在页面初始化时注册，绝不能写在 `onEnter` 回调的某个触发分支里**：外部 AI 调用时插件是被**后台拉起**的，走的是 `info.type === "background"` 那条路径；如果注册代码只在别的分支（如 keyword 分支）里才执行，它就永远不会跑，AI 会永远收到「工具未注册」。这一点与 `onMainPush` 一样，是 uTools 生态踩过的坑，务必在插件初始化脚本的顶层（不在任何 `if` 分支里）调用。

> 值得注意：`registerTool` 触发的后台拉起**不要求** `plugin.json` 声明 `"background": true`——宿主在外部 AI 调用时会按需临时把插件拉起来，这一点与「随 iTools 启动」开关无关。

## 摄像头（`camera`，需 `camera` 授权）

设备枚举、单帧抓拍、低帧率预览流。基于 Media Foundation 实现，**仅 Windows**——非 Windows 平台 `list` / `grab` / `streamStart` 一律报错 `"摄像头能力仅支持 Windows"`，不是返回空数组。**不提供录像接口**（`camera.rs` 明确取舍：项目里没有视频编码器依赖，宁可不做也不给一个产出坏文件的假接口）；要录像得自己拉预览帧喂给 `itools.runtime` 托管的 ffmpeg。

授权是两道门：`plugin.json` 的 `permissions` 里必须声明 `"camera"`，且用户在「插件管理」里授权；任一不满足都返回 `"插件未获授权使用摄像头（请在「插件管理」里授权 camera）"`。调用窗口没绑定插件会话时报 `"没有正在运行的插件"`。

- `itools.camera.list(): Promise<Array<{ deviceId: string; name: string; formats: Array<{ width: number; height: number; fps: number | null }> }>>` — 列出系统当前可见的摄像头；`deviceId` 是 Media Foundation 的符号链接（不是友好名字），`grab` / `streamStart` 都靠它定位设备，`name` 读不到时是空字符串 `""`，没有摄像头就返回 `[]` 而不报错。
  - ⚠️ **`formats` 是尽力而为**：枚举原生格式必须真正把设备打开一次，设备正被别的程序占用就打不开，此时该设备的 `formats` 诚实返回 `[]`（`deviceId` / `name` 仍然正常）。
  - ⚠️ 每台设备最多枚举 64 条格式（`MAX_FORMATS`），按 `(宽, 高, 帧率)` 去重；`fps` 由设备上报的分数四舍五入得到，取不到或算出 0 时是 `null`，不编造。
  - ⚠️ 这一次调用会对**每台**设备做一遍「打开 → 读格式 → 关闭」，设备多时明显变慢，期间会短暂占用设备。拿不到符号链接的设备直接跳过，不会出现在结果里。

- `itools.camera.grab(deviceId: string, opts?: { width?: number; height?: number; format?: "png" | "jpeg" | "jpg"; quality?: number }): Promise<string>` — 抓一帧静态图，返回图片字节的标准 base64（**不带** `data:image/...;base64,` 前缀，前缀自己拼）。
  - `format` 默认 `"png"`（无损）；`"jpeg"` / `"jpg"` 走 JPEG，`quality` 默认 90、夹到 1~100。**其它任意值（如 `"webp"`）会静默按 PNG 处理**，不报错。
  - 返回值里**没有宽高字段**：实际分辨率是设备协商结果，要知道只能自己解码图片。
  - ⚠️ **分辨率是「请求」不是保证，且 `width` 与 `height` 必须同时给才生效**（只给一个会被完全忽略）。设备不支持所请求的分辨率时，实现会退一步只要 RGB32、由设备给默认分辨率——**静默退化，不报错**。（`camera.rs` 里 `GrabOptions::width` 的注释写的是「不做静默降级」，与实现不符，以实现为准。）
  - ⚠️ 每次调用固定读 3 帧、只保留最后一次成功的那帧（丢弃刚开机的自动曝光/白平衡帧）。单次读帧碰到空样本最多重试 60 次、每次 sleep 20ms（≈1.2 秒），所以最坏情况一次 `grab` 会阻塞数秒。
  - ⚠️ 抓完立刻 `Shutdown` 释放设备，**因此托盘的「正在使用摄像头」指示器（每秒轮询一次）通常来不及显示 `grab`**——`camera.rs` 模块头写明这是刻意取舍。
  - ⚠️ 代码里对 `width`/`height` 和返回 base64 的大小**都没有上限**，高分辨率 PNG 会是极大的字符串，按需自己控制。
  - 典型错误：`"deviceId 不能为空"`、`"找不到摄像头设备（deviceId=…，可能已拔出或被系统重新枚举，试试重新调用摄像头列表）"`、`"打开摄像头失败（可能被其它程序占用）: …"`、`"设置摄像头输出格式失败（该设备可能不支持直采 RGB，或不支持所选分辨率）: …"`、`"摄像头连续 60 次未返回帧数据（设备可能被其它程序独占）"`。

- `itools.camera.streamStart(deviceId: string, opts?: { width?: number; height?: number; fps?: number; quality?: number }, handlers?: { onFrame?: (p: { streamId: string; b64: string; width: number; height: number }) => void; onStopped?: (reason: string) => void }): Promise<string>` — 启动预览流，持续把 JPEG 帧推给 `onFrame`，返回 `streamId`（纳秒时间戳 hex + `-` + 自增计数 hex）供 `streamStop` 使用。
  - `onFrame` 的 `b64` **始终是 JPEG**（预览流没有 PNG 选项），无 data URI 前缀；`width`/`height` 是设备实际协商到的分辨率，不一定等于你请求的。回调按 `streamId` 过滤，只收本流的帧。
  - `onStopped` 只拿到一个字符串 `reason`（就是读帧失败的错误文案，如 `"摄像头数据流已结束"`）。
  - ⚠️ **`fps` 硬上限 15**（`MAX_STREAM_FPS`），默认 10，传入值夹到 1~15；`quality` 默认 70，夹到 1~100。
  - ⚠️ **建议分辨率不超过 640×480**：每帧要走「MF 出图 → JPEG → base64 → 拼 JS 字符串 → `webview.eval` 注入」，1280×720 起单帧字符串十几万字符，10fps 会明显拖慢插件窗口。高分辨率场景请改用 `grab` 按需抓单帧。这是如实的能力边界，不是待优化的 bug。
  - ⚠️ 分辨率规则与 `grab` 完全相同：`width`/`height` 同时给才生效，设备不支持就静默退化到设备默认分辨率（以 `onFrame` 里的 `width`/`height` 为准）。
  - ⚠️ 节流是**丢帧式**的：没到下一个出帧时间点的帧直接丢弃（连 JPEG 编码都不做），不会攒帧。JPEG 编码失败只跳过该帧并写宿主日志，**不通知插件**。
  - ⚠️ 帧只推给**发起调用的那个窗口**（`plugin` / `plugin-dev` / `plugin-bg-<id>`），别的窗口收不到。
  - ⚠️ **`onStopped` 只代表「意外结束」**：主动 `streamStop`、托盘一键掐断、宿主切换插件窗口或关闭后台常驻实例，走的都是静默退出分支，**不会**触发 `onStopped`。
  - ⚠️ 桥接层的事件回调注册后**永不注销**：每次 `streamStart` 都会往同一个通道再压一份回调，反复启停回调数量只增不减（靠 `streamId` 过滤所以不会串流，但会白白累积）。
  - ⚠️ 代码里没有「每个插件最多几路流」的限制，同时开多路要自己节制。
  - 启动阶段的错误除与 `grab` 相同的那套打开/协商失败文案外，还有 `"打开摄像头超时"`（等待设备就绪超过 8 秒）与 `"预览流线程提前退出"`；**运行阶段的失败不 reject，走 `onStopped`**。

- `itools.camera.streamStop(streamId: string): Promise<void>` — 停掉一路自己开的预览流，resolve 值为空。
  - ⚠️ **没有帧数、没有时长、没有停止原因**——这些信息代码里根本不收集，要统计帧数得自己在 `onFrame` 里累加。
  - ⚠️ 只是把停止标志置位并从注册表摘除，worker 线程在**下一轮循环**才真正退出并释放设备（一次读帧最长约 1.2 秒），调用返回时设备可能还没释放完。
  - ⚠️ 控制类命令也复核权限：`camera` 授权若在流运行期间被用户撤销，`streamStop` 会直接以未授权报错，**插件自己停不掉这路流**，只能靠托盘一键掐断、切换插件窗口或关闭后台实例来收摊。
  - 错误：`"该预览流不存在或已结束"`（没存在过 / 已停过 / 已因读帧出错自行结束）、`"该预览流不属于本插件"`（归属校验用完整会话身份 = 插件 id + dev 标志，调试会话与同名正式插件互相停不掉）。

⚠️ **隐私指示器无法规避**：只要有预览流在跑，托盘 tooltip 就会显示 `iTools — <插件id> 正在使用摄像头`，菜单里出现「⚠ …（点击停止）」，插件没有任何接口能隐藏或阻止它；用户点一下会掐断**当前所有**敏感能力使用（麦克风 / 录屏 / 摄像头 / 录屏录音），不只是你这一路。此外宿主在「插件窗口被换成另一个插件」和「关闭后台常驻实例」两处会强制停掉该会话名下所有预览流。

⚠️ **每次权限判定都会写审计**：无论放行还是拒绝都记一条（同插件同能力 5 秒内的连续调用合并计数），用户在插件管理里能看到你调了多少次摄像头。

```js
// 1) 列设备
const devs = await itools.camera.list();
if (!devs.length) throw new Error("没有可用摄像头");
const cam = devs[0];
console.log(cam.name, cam.formats.map(f => `${f.width}x${f.height}@${f.fps ?? "?"}`).join(", "));

// 2) 单帧抓拍：返回裸 base64，前缀自己拼
const b64 = await itools.camera.grab(cam.deviceId, { width: 640, height: 480, format: "jpeg", quality: 85 });
document.querySelector("#shot").src = "data:image/jpeg;base64," + b64;

// 3) 预览流：fps 上限 15，分辨率别超过 640x480
const img = document.querySelector("#preview");
let frames = 0; // 想统计帧数只能自己数
const sid = await itools.camera.streamStart(
  cam.deviceId,
  { width: 640, height: 480, fps: 10, quality: 70 },
  {
    onFrame: (p) => { frames++; img.src = "data:image/jpeg;base64," + p.b64; }, // p:{streamId,b64,width,height}
    onStopped: (reason) => console.warn("预览意外结束：", reason),               // 主动停/被掐断不触发
  }
);

setTimeout(async () => {
  try {
    await itools.camera.streamStop(sid); // resolve 值为空
    console.log("已停止，共收到", frames, "帧");
  } catch (e) {
    console.error(e); // 例如 "该预览流不存在或已结束"
  }
}, 5000);
```

## mp4 录屏（`record.video*`，需 `screen-capture` 授权）

正式版录像：真 h264/mp4、可选混入系统声音与麦克风。视频编码外包给宿主托管的 ffmpeg，所以实际要用的插件通常要同时声明 `screen-capture` + `runtime`（带音频再加 `audio-capture`）；权限必须先写进 `plugin.json` 的 `permissions`，再由用户在「插件管理」里授权，缺任一都会被拒（未声明还会额外记一条审计）。

- `itools.record.videoStart(opts?: { area?: { x: number; y: number; w: number; h: number }; displayId?: number; hwnd?: number; fps?: number; includeSystemAudio?: boolean; includeMic?: boolean }, onProgress?: (p: { recordId: string; elapsedMs: number; frames: number; sizeBytes: number }) => void): Promise<string>` — 开始录制并返回 `recordId`（形如 `rec-<纳秒时间戳16进制>-<自增计数16进制>`），录制期间约每 500ms 回调一次进度。
- `itools.record.videoStop(recordId: string): Promise<string>` — 停止录制，等 ffmpeg 收尾（有音频时再混流一遍），返回最终 mp4 的**绝对路径字符串**（不是文件数据）。

⚠️ 必须先 `itools.runtime.ensure("ffmpeg")`（那条 API 需 `runtime` 授权），否则 videoStart 直接报「未找到宿主托管的 ffmpeg，请先调用 itools.runtime.ensure('ffmpeg') 安装后再开始录制」，不会先录一半才发现编码不了。

⚠️ `includeSystemAudio` / `includeMic` 任一为 `true` 时**还需** `audio-capture`，只有 `screen-capture` 会报「录屏+录音需要同时授权 screen-capture 与 audio-capture（请在「插件管理」里补授权 audio-capture）」。

⚠️ `fps` 缺省 15，被夹在 1~30：传 60 会被**静默**压到 30，传 0 抬到 1，都不报错。

⚠️ 输出宽高强制向下取偶（H.264 的 yuv420p 要求偶数），1921 宽的区域实际录成 1920；取偶后不足 2 像素报「录制区域过小（不足 2x2 像素）」。

⚠️ `area` 是相对**所选显示器左上角**的局部像素坐标，不是虚拟桌面全局坐标——写成全局坐标会报「录制区域超出显示器边界（显示器 {W}x{H}）」。`displayId` 取自 `itools.listDisplays()`，`hwnd` 取自 `itools.win.list()`；给了 `hwnd` 就忽略 `area`/`displayId`。

⚠️ 窗口（`hwnd`）录制是退化路径：xcap 只给显示器提供事件驱动的 `video_recorder()`，窗口只能定时轮询 `capture_image()`，CPU 更高；目标窗口被遮挡/最小化导致抓取失败时实现选择**体面结束录制**（不重试、不报错），你会得到一段提前结束的视频。

⚠️ 没有 `duration`/`maxSeconds` 参数，视频侧也没有时长上限——必须插件自己调 `videoStop`。但音频采集线程上限 3600 秒，超过 1 小时后音频线程自行退出、视频继续；混流带 `-shortest`，最终 mp4 会被截到较短那条流的长度。

⚠️ 音视频弱同步（实现方写明的已知边界，不是罕见 bug）：`-framerate` 是名义帧率，取不到新帧就复用上一帧；音频是独立线程实时采样、事后与视频 `-shortest` 对齐混流。系统卡顿丢帧或采样率漂移时，几十分钟以上的长录制可能音画轻微不同步。音频还全程缓存在内存（`Vec<i16>`），不分段写盘。

⚠️ `onProgress` 的桥接是**只加不减的全局监听，且不按 recordId 过滤**：每次 `videoStart` 都往回调数组里 push 一个，从不移除；同时录多路时每个回调都会收到**所有**录制的进度。必须自己比对 `p.recordId`。

⚠️ `sizeBytes` 是对静默视频文件（有音频时是 `_tmp_video_*.mp4` 临时文件）做实时 `stat` 的读数，取不到为 0；ffmpeg 自己有编码/写盘缓冲，这是近似值，不代表「已安全落盘」。`frames` 是已写入 ffmpeg 标准输入的帧数，含「取不到新帧时复用上一帧」的那些。

⚠️ 落盘在 `%LOCALAPPDATA%\itools\plugin-recordings\record_<YYYYmmdd_HHMMSS>_<recordId>.mp4`（debug 构建是 `itools-dev`），时间戳取本地时区。**插件读不回来**：`itools.fs.*` 对 iTools 数据根是硬黑名单（「禁止访问 iTools 自身的数据目录（会破坏插件间数据隔离），请重新选择其它文件夹或文件」），即使用户在对话框里亲自选中也会被拒；`itools.readFile` 只认插件自己的沙盒。可用的路子是 `itools.openPath(filePath)`——无需授权，扩展名白名单含 `mp4`（也含 `wav`/`gif`）。把文件搬到用户目录的通路**未确认**。

⚠️ `videoStop` 无权限门禁，只做归属校验（插件 id + `dev` 标志都要对得上，同名的调试会话与正式会话是两个归属者）。等待预算：等 ffmpeg 收尾最多 120 秒（超时报「等待 ffmpeg 收尾超时」并 kill），混流最多 600 秒；join/编码/混流全在 blocking 线程池，不冻结 UI，但 Promise 会真的挂那么久。

⚠️ 混流失败时**故意不删中间产物**，错误信息里带上保留下来的无声视频与 wav 路径（「ffmpeg 混流音频失败（退出码 …）……已保留无声视频与音频原始文件：…」），不让已录内容凭空丢失。

⚠️ 录制期间托盘显示「屏幕录制（mp4）」并可一键掐断（算在 `screen-capture` 名下）。被掐断时只置停止标志并 `kill` ffmpeg，会话仍留在状态表里——之后调 `videoStop` 拿到的是「ffmpeg 视频编码失败（退出码 …）：…」而不是文件路径。插件被禁用、被卸载、被切换走时同样强制掐断；但**撤销授权本身不会中断进行中的录制**，只影响下一次调用。

⚠️ 锁屏时 `videoStart` 报「创建屏幕捕获会话失败: 拒绝访问」（真机验收实测）。这是 Windows 会话隔离的正常行为——后台常驻插件不能假设屏幕能力随时可用。

```js
// plugin.json: "permissions": ["screen-capture", "runtime"]（要声音再加 "audio-capture"）
async function recordScreen() {
  // 1) 确保宿主已装 ffmpeg（首次会真下载约 167 MB；已装则立即返回）
  await itools.runtime.ensure("ffmpeg", p => {
    console.log("ffmpeg", p.received, "/", p.total, p.done ? "done" : "");
  });

  // 2) 开录：主显示器全屏、15fps、混入系统声音
  let myId = null;
  myId = await itools.record.videoStart(
    { fps: 15, includeSystemAudio: true },
    p => {
      if (myId && p.recordId !== myId) return;   // 回调不按 id 过滤，自己比对
      console.log(p.elapsedMs + "ms", p.frames + "帧", p.sizeBytes + "B");
    }
  );

  // 3) 5 秒后停止，拿到最终 mp4 的绝对路径
  await new Promise(r => setTimeout(r, 5000));
  const filePath = await itools.record.videoStop(myId);
  console.log("录像已保存:", filePath);

  // 4) 插件读不到这个目录，但可以交给系统播放器打开
  await itools.openPath(filePath);
}
recordScreen().catch(e => console.error("录屏失败:", e));
```

## 系统内录（`record.loopback*`，需 `audio-capture` 授权）

录「电脑正在播放的声音」，走 WASAPI 回环：把**默认输出设备**当输入设备打开。与麦克风录音（`startAudioRecord`）是两条独立的通路和两张独立的状态表。

- `itools.record.loopbackStart(): Promise<null>` — 开始系统内录；resolve 即表示声卡流已建好并开始采样（内部最长等 5 秒设备就绪）。
- `itools.record.loopbackStop(): Promise<string>` — 停止并返回 **base64 编码的 WAV 字符串**（RIFF/PCM/16-bit）。

⚠️ **返回的是裸 base64 字符串，不是 ArrayBuffer**——本组 API 里最容易踩的坑：桥接层这一路没有接解码，而同文件的 `stopAudioRecord` / `stopGifRecord` 都接了。要二进制得自己 `atob`，或直接拼 `data:audio/wav;base64,` 用。

⚠️ 必须存在活动的**渲染**端点（扬声器/耳机），否则报「找不到系统扬声器/输出设备」；「有没有声音在播」不影响能否开始录。真机验收里这一项是当轮唯一的失败项——机器上 4 个活动音频端点全是麦克风、没有任何活动渲染端点，判为环境限制而非缺陷。

⚠️ 时长上限 600 秒：到点采集线程自行退出，但会话仍在表里——之后 `loopbackStop` 仍能拿到前 600 秒的 WAV，不报错；**托盘的「系统内录（回环声卡）」指示器要等你调 stop 才消失**。

⚠️ 全局单会话（跨插件）：整个宿主同一时刻只允许一路独立内录，第二个插件（或同一插件重复调用）报「已经在录系统声音了」。与 `record.videoStart({ includeSystemAudio: true })` 互不冲突——那一路是录制内部独立的音频线程，不占这张表。

⚠️ 只录**默认输出设备**，采样率与声道数沿用 `default_output_config()`（不固定，常见 48000Hz / 2ch），无法选设备、无法调参数。没采到任何采样时 WAV 只有 44 字节的空头。

⚠️ 音频全程缓存在内存，停止时才编码：10 分钟 48kHz 立体声 16bit 约 110 MB，再 base64 膨胀约 1/3 跨 IPC 传给前端——长内录的内存与传输开销是真实的。

⚠️ 用户从托盘一键掐断后，采集立即停止、托盘指示器消失，但会话仍留在状态表里——之后 `loopbackStop` **仍会 resolve**，返回掐断前那段音频，不是报错。只有从没 start 过或已经 stop 过才报「当前没有系统内录」。

⚠️ `loopbackStop` 无权限门禁，只做归属校验（插件 id + `dev` 标志）。锁屏时的表现**未确认**：真机验收的锁屏一组只覆盖了屏幕/输入/前台窗口类能力，没覆盖音频采集。

```js
// plugin.json: "permissions": ["audio-capture"]
async function recordSystemSound() {
  await itools.record.loopbackStart();                 // 返回 null
  await new Promise(r => setTimeout(r, 5000));

  const b64 = await itools.record.loopbackStop();      // string，不是 ArrayBuffer
  const bin = atob(b64);
  const u8 = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);

  if (u8.byteLength <= 44) {
    console.warn("只有 WAV 头，没采到声音");
    return;
  }
  const a = document.createElement("audio");
  a.controls = true;
  a.src = URL.createObjectURL(new Blob([u8], { type: "audio/wav" }));
  document.body.appendChild(a);
}
// 无渲染端点时：「找不到系统扬声器/输出设备」
recordSystemSound().catch(e => console.error(e));
```

## GIF 录屏（`startGifRecord` / `stopGifRecord`，需 `screen-capture` 授权）

宿主内自行编码的轻量版，用来「快速录一小段发群里」；要真帧率/真分辨率/带声音的正式录像用 `itools.record.videoStart`。

- `itools.startGifRecord(): Promise<null>` — 开始录主屏 GIF（固定约 5fps、宽上限 640px、最多 150 帧）。
- `itools.stopGifRecord(): Promise<ArrayBuffer>` — 停止并返回 GIF 二进制（无宽高/帧数等元数据，只有裸字节）。

⚠️ **固定录主显示器**，没有 `displayId` / `area` / 窗口参数。

⚠️ 150 帧硬上限 + 每帧 200ms 延迟 ⇒ 实际时长天花板约 30 秒。满帧后线程自行结束，之后 stop 仍能拿到 GIF，但**托盘的「屏幕录制」指示器要等你调 stop 才消失**。

⚠️ 单帧按 640px 宽度上限等比缩小（宽度 ≤640 的屏保持原尺寸），所以 GIF 分辨率最高 640px 宽；无限循环、量化用的是最快/质量最低那一档。无声音、无 mp4。

⚠️ `startGifRecord` 只负责起线程、立刻返回 Ok，**抓屏失败不会在这里报出来**：锁屏或截屏被拒时线程首帧就退出，错误在 `stopGifRecord` 处以「没有抓到任何帧」的形式浮现。

⚠️ `stopGifRecord` 可能真的要等几秒：150 帧的 GIF 量化编码是 CPU 密集操作（跑在 blocking 线程池，不冻结 UI，但 Promise 会挂那么久）。数据以 base64 跨 IPC 传输，膨胀约 1/3；帧也全程缓存在内存。

⚠️ 全局单会话（跨插件），已在录时报「已经在录屏了」。用户从托盘一键掐断后**已抓到的帧被直接丢弃**（不 join、不要编码结果），再调 `stopGifRecord` 得到「当前没有录屏」。

⚠️ 只给内存里的 ArrayBuffer，没有落盘通路（与 `record.videoStop` 返回文件路径相反）；`stopGifRecord` 无权限门禁，只做归属校验。

```js
// plugin.json: "permissions": ["screen-capture"]
async function gifDemo() {
  await itools.startGifRecord();                 // 返回 null；固定主屏，约 5fps / 640px 宽
  await new Promise(r => setTimeout(r, 5000));   // 最多约 30 秒（150 帧）

  const buf = await itools.stopGifRecord();      // ArrayBuffer(GIF)
  console.log("GIF 字节数:", buf.byteLength);
  const img = document.createElement("img");
  img.src = URL.createObjectURL(new Blob([buf], { type: "image/gif" }));
  document.body.appendChild(img);
}
// 锁屏 / 截屏被拒时：「没有抓到任何帧」
gifDemo().catch(e => console.error("GIF 录屏失败:", e));
```

## 麦克风录音（`startAudioRecord` / `stopAudioRecord`，需 `audio-capture` 授权）

- `itools.startAudioRecord(): Promise<null>` — 开始录默认麦克风；内部等 cpal 建流并 play 成功才 resolve（最长 5 秒），所以 resolve 就意味着麦克风真的开起来了。
- `itools.stopAudioRecord(): Promise<ArrayBuffer>` — 停止并返回 WAV 二进制（RIFF/PCM/16-bit）。

⚠️ **设备坏掉不会报错，只会录出 44 字节的空 WAV 头**：真机验收里 C930c 麦克风故障时 `startAudioRecord` 正常成功、`stopAudioRecord` 只出 44 字节；A/B 用与 iTools 无关的 ffmpeg 直开同一设备同样录不出内容。插件必须自己判 `buf.byteLength <= 44`。

⚠️ 时长上限 600 秒：到点线程退出但会话仍在表里，之后 stop 仍能拿到前 600 秒、不报错；**托盘的「麦克风」指示器要等你调 stop 才消失**。

⚠️ 全局单会话（跨插件），已在录时报「已经在录音了」。与 `record.videoStart({ includeMic: true })` 互不冲突（那是录制内部独立的音频线程）。

⚠️ 只录**默认输入设备**，采样率与声道沿用 `default_input_config()`（不固定），不能选设备、不能指定参数；返回值里没有单独的采样率/声道字段，要用就自己从 WAV 头读（偏移 22 = 声道数 u16LE，偏移 24 = 采样率 u32LE）。

⚠️ 采样全程缓存在内存，停止时才编码，再以 base64 跨 IPC 传给前端（膨胀约 1/3）：10 分钟 44.1kHz 立体声在百 MB 量级。

⚠️ 用户从托盘一键掐断后已采样的数据随线程退出被丢弃，再调 `stopAudioRecord` 得到「当前没有录音」——这是如实的，会话确实已被用户注销。

⚠️ `stopAudioRecord` 无权限门禁，只做归属校验（插件 id + `dev` 标志）。锁屏时的表现**未确认**：真机验收的锁屏一组没覆盖麦克风采集。

```js
// plugin.json: "permissions": ["audio-capture"]
async function micDemo() {
  await itools.startAudioRecord();               // resolve 即表示麦克风真的开起来了
  await new Promise(r => setTimeout(r, 5000));   // 上限 600 秒

  const buf = await itools.stopAudioRecord();    // ArrayBuffer(WAV 16-bit PCM)
  if (buf.byteLength <= 44) {
    console.warn("只有 WAV 头，设备可能故障或被占用");
    return;
  }
  const dv = new DataView(buf);
  console.log("声道数:", dv.getUint16(22, true), "采样率:", dv.getUint32(24, true));

  const a = document.createElement("audio");
  a.controls = true;
  a.src = URL.createObjectURL(new Blob([buf], { type: "audio/wav" }));
  document.body.appendChild(a);
}
// 无设备时：「找不到麦克风输入设备」
micDemo().catch(e => console.error("录音失败:", e));
```

## 屏幕截图（顶层 `itools.*`，需 `screen-capture` 授权）

三个方法共用 `capture.rs::require_capture`：`plugin.json` 里声明了 `"permissions": ["screen-capture"]` **且**用户在「插件管理」里给本插件开过，缺一不可，否则报「插件未获授权截屏（请在「插件管理」里授权 screen-capture）」。窗口上没有正在运行的插件会话时报「没有正在运行的插件」。这组返回的所有坐标 / 尺寸都是**物理像素**（Win32 口径），不是插件页里的 CSS/DIP 坐标——换算用 `itools.screen.toDip` / `rectToDip`。

- `itools.listDisplays(): Promise<Array<{ id: number; name: string; x: number; y: number; width: number; height: number; scale: number; is_primary: boolean }>>` — 列出所有显示器：`x`/`y` 是该屏左上角在虚拟桌面里的物理坐标（副屏摆在主屏左侧 / 上方时是负数），`scale` 是缩放因子（`1.0` = 100%，`1.5` = 150%），`id` 就是 `captureFull` 要的那个值。
  ⚠️ 字段是下划线的 **`is_primary`**，不是 `isPrimary`（Rust 结构体只 `derive(Serialize)`，没有 `rename_all`），写成驼峰拿到的是 `undefined`。
  ⚠️ 每个字段都是 `unwrap_or(默认值)` 静默降级：取不到就给 `0` / `""` / `1.0` / `false`，**不会报错**，所以拿到 `id === 0` 或 `width === 0` 得自己防。
  ⚠️ 数组顺序就是系统枚举顺序，代码不做排序，**主屏不保证排第一**。
- `itools.captureFull(displayId?: number | null): Promise<ArrayBuffer>` — 截某块屏的整屏，返回该屏的 PNG 字节（裸 `ArrayBuffer`，没有包装对象）。`displayId` 省略 / 传 `null` = 主屏（系统没给 `is_primary` 标记时退化为枚举到的第一块屏）。
  ⚠️ 只能整屏截，**截不了局部**——要区域请用 `captureRegion`，或截完再 `itools.image.crop`。
  ⚠️ 传了不存在的 id 报「找不到显示器 id=<n>」；一块屏都枚举不到报「未找到任何显示器」；抓屏本身失败报「截屏失败: <系统错误>」。
  ⚠️ 锁屏 / 安全桌面下必失败，真机实测为「截屏失败: 句柄无效 (0x80070006)」——这是 Windows 会话隔离，不是 iTools 的限制。**后台常驻插件不能假设截屏随时可用**，必须处理这类失败。
  ⚠️ 代码里**没有**任何体积 / 分辨率上限常量，但图片是经 base64 过 IPC 的，4K 整屏一张 PNG 的体积和耗时都不小。
  ⚠️ 是否合成鼠标指针：**未确认**（直接调 xcap 的 `capture_image`，代码未做任何指针处理）。
- `itools.captureRegion(opts?: { full?: boolean }): Promise<{ action: "copy" | "save" | "pin" | "ocr"; image: ArrayBuffer } | null>` — PixPin 风格的交互式区域截图：弹一个横跨**整个虚拟桌面**的原生 GDI 覆盖层，用户自己拖框选区、（可选）就地标注，再点悬浮工具栏上的 复制 / 保存 / 贴图 / OCR，Promise 才 resolve；`image` 是**已裁剪并合成好标注**的最终 PNG 字节。`opts.full = true` 则开局即选中整个虚拟桌面矩形并直接进编辑态。
  ⚠️ **必须用户交互，无法静默截区域**。用户按 Esc 或右键（非文字输入态）取消 → resolve 成 **`null`**，不抛错。
  ⚠️ **后端只把 `action` 回传，不替你执行动作**：拿到 `"copy"` 要自己调 `writeImage`，`"save"` 调 `saveImage`，`"pin"` 调 `createPin`，`"ocr"` 调 `ocr()`。（宿主自带的截图热键走的是另一条在 Rust 里落地动作的路径，插件路径不是。）
  ⚠️ 同一时刻只允许一次：已有一次在跑时再调报「截图正在进行中」。
  ⚠️ 覆盖层里可用的编辑能力：9 种标注工具（矩形 / 椭圆 / 箭头 / 直线 / 画笔 / 荧光笔 / 文字 / 序号 / 马赛克）、8 种颜色、线宽 1~12（默认 3）、撤销（每点一次弹掉最后一笔，**无重做**）；编辑态在选区内双击 = `copy`，Shift+双击 = `pin`。
  ⚠️ 调用会先隐藏**发起调用的那个插件窗**（仅当它当时可见），返回后再 show + 聚焦；热键唤起时面板本就隐藏则全程不显示它。
  ⚠️ 锁屏 / 安全桌面下不可用。非 Windows 平台退回 WebView 覆盖层，且只冻结「光标所在那块屏」（Windows 是整个虚拟桌面）。

```js
// plugin.json 需声明 "permissions": ["screen-capture"]
const list = await itools.listDisplays();
const main = list.find(d => d.is_primary) || list[0];   // 注意是 is_primary
const png = await itools.captureFull(main.id);          // ArrayBuffer(PNG)
document.querySelector("img").src =
  URL.createObjectURL(new Blob([png], { type: "image/png" }));

const r = await itools.captureRegion();                 // 传 { full: true } 则开局全选
if (r) {                                                // null = 用户 Esc / 右键取消
  if (r.action === "copy")      await itools.writeImage(r.image);
  else if (r.action === "save") await itools.saveImage(r.image, "shot.png");
  else if (r.action === "pin")  await itools.createPin(r.image, 0.9);
  else if (r.action === "ocr")  console.log(await itools.ocr(r.image, "zh-Hans"));
}
```

## 图片读写（顶层 `itools.*`，无需授权）

读剪贴板、写剪贴板、另存为、读本地图片文件——都不设权限门禁（另存为的理由是「用户在对话框里显式选路径即授权」）。图片经 IPC 时统一走 base64 字符串而非 `Vec<u8>`：插件页 CSP 收紧后 Tauri IPC 退化为 postMessage，字节数组会被序列化成「数字数组」（体积 4×、极慢）。

- `itools.readImage(): Promise<ArrayBuffer>` — 读剪贴板里的图片，返回 **PNG** 字节。
  ⚠️ **拿不到原始格式**：不管用户复制的是 JPEG 还是别的，中间过了一遍 RGBA，出来一律是 PNG。
  ⚠️ 剪贴板里没有图片会 reject「剪贴板没有图片: <底层原因>」——必须 `try/catch`。剪贴板里是文本请用 `itools.readText()`。
- `itools.writeImage(data: ArrayBuffer | Uint8Array | string): Promise<void>` — 把图片写进剪贴板为**真实位图**（`arboard::ImageData` / RGBA），不是 base64 文本。输入接受 `ArrayBuffer` / `Uint8Array` / 裸 base64 / `data:` URL（前缀会被自动剥掉）。
  ⚠️ 任何 image crate 能解的格式都收（png / jpeg / webp / bmp / gif / tiff / ico…），但**都会被解成 RGBA 再写，原格式与元数据全部丢失**。解不了的（如 SVG）报「图片解码失败: …」。
- `itools.saveImage(data: ArrayBuffer | Uint8Array | string, defaultName?: string | null): Promise<string | null>` — 弹原生「另存为」对话框，返回用户选定的**绝对路径**；用户取消返回 `null`（不是错误）。默认目录是系统「图片」目录，默认文件名 `"iTools截图.png"`，过滤器写死「PNG 图片 / *.png」。
  ⚠️ **完全不转码**：拿到什么字节就 `fs::write` 原样落盘。传一个 JPEG 进去会得到一个后缀 `.png`、内容却是 JPEG 的文件——要真 PNG 请先 `itools.image.convert`。
  ⚠️ **必须用户交互**：锁屏 / 无交互桌面下对话框根本弹不出来（真机验收里 `fs.pickDir` 就是这个现象），调用会一直挂着。
  ⚠️ 保存位置由用户在对话框里选，**不受插件沙盒限制**。
- `itools.readLocalImage(path: string): Promise<ArrayBuffer>` — 读磁盘上的图片文件，返回**原始字节，原样读盘不转码**（读 `.jpg` 出来就是 JPEG 字节，这点与 `readImage` / `captureFull` 不同）。
  ⚠️ 扩展名白名单（大小写不敏感）：**png / jpg / jpeg / gif / bmp / webp / svg / ico / tif / tiff**，且**只看扩展名不看真实内容**；不在白名单报「只支持读取图片文件（png/jpg/jpeg/gif/bmp/webp/svg/ico/tiff）」。
  ⚠️ 单文件上限 **30 MB**，超限报「图片过大（>30MB）」。这个判断在 `fs::read` **之后**，超大文件仍会被完整读进内存一次才被拒。
  ⚠️ 拒 UNC / 远程路径（以 `\\` 或 `//` 开头）；空路径报「路径为空」；路径不存在或是目录报「文件不存在」。
  ⚠️ 传的是绝对路径且**不受插件沙盒限制**（磁盘任意位置的图片都能读），但纯只读，不落写入 / 执行面。
  ⚠️ 白名单里的 **svg 能读出字节，但 `writeImage` / `createPin` / `ocr` 都解不了 SVG**（image crate 不支持），会报「图片解码失败」。

```js
// 无需权限声明
const buf = await itools.readLocalImage("D:\\pics\\a.jpg");  // JPEG 原始字节
await itools.writeImage(buf);                                // 写进剪贴板（转成 RGBA 位图）

try {
  const shot = await itools.readImage();                     // 回读，得到的是 PNG
  const path = await itools.saveImage(shot, "屏幕截图.png");
  console.log(path === null ? "用户取消了保存" : "已保存到 " + path);
} catch (e) {
  console.log(String(e));                                    // 「剪贴板没有图片: ...」
}
```

## 离线 OCR 与贴图（顶层 `itools.*`，无需授权）

`plugin_ocr` / `plugin_create_pin` 的签名里都没有 registry / settings 参数，实现体里没有任何授权检查：OCR 只对传进来的图片字节做本地识别、不读屏幕；贴图只是把你已经拿到的图片钉成浮窗。**任何插件都能不经授权造一个置顶贴图窗**。

- `itools.ocr(data: ArrayBuffer | Uint8Array | string, lang?: string | null): Promise<string>` — 走 `Windows.Media.Ocr`（WinRT）识别图片里的文字，**完全离线、免费、不联网、不外发**，仅 Windows 可用（其他平台报「OCR 仅在 Windows 上可用」）。输入接受 `ArrayBuffer` / `Uint8Array` / 裸 base64 / `data:` URL。
  ⚠️ **识别不到任何文字时返回空字符串 `""`，既不是 `null` 也不报错**——判空要用 `text.trim() === ""`。
  ⚠️ 只返回整段文本，**不返回**文字框坐标、置信度、分行 / 分词结构（代码只取了 `OcrResult.Text()`）。
  ⚠️ `lang` **没有内置白名单**，字符串原样丢给 `Language::CreateLanguage`：凡是系统装了对应 OCR 语言包的 BCP-47 标签都行（常见 `"zh-Hans"` / `"en"`）。标签本身非法报「不支持的 OCR 语言：<lang>」，标签合法但系统没装包报「系统未安装 <lang> 的 OCR 语言包」。**代码里没有枚举可用语言的 API**，装没装只能 try/catch。
  ⚠️ 省略 `lang` / 传 `null` → 跟随系统用户配置语言；系统一个 OCR 包都没有时报「系统无可用 OCR 语言（请在 Windows 设置里安装语言的手写/OCR 组件）」。
  ⚠️ 输入图片任一边超过 **4000 px** 会先等比降采样到 4000 再识别（WinRT OCR 单边上限约 10000px，超限会抛晦涩错误）——识别是在缩小后的图上做的。
  ⚠️ 代码里没有输入体积上限常量。识别跑在 blocking 线程里，不阻塞 UI。
- `itools.createPin(data: ArrayBuffer | Uint8Array | string, opacity?: number | null): Promise<string>` — 把图片钉成无边框、透明、置顶、不进任务栏、不可拉伸的浮窗，返回 `pinId`（进程内自增计数器的十进制字符串，从 `"0"` 起；对应窗口 label 是 `pin-<pinId>`）。
  ⚠️ **拿到 `pinId` 后没有任何 API 能关掉 / 移动 / 缩放这张贴图**——`pin_close` / `pin_resize` / `pin_move` 只有贴图窗自己能调，`bridge.js` 根本没暴露。`pinId` 目前只能当标识符用，关窗只能靠用户操作。
  ⚠️ `opacity` 取值 **0.1 ~ 1.0**，省略 / `null` = 1.0。**超范围不报错**，代码是 `clamp(0.1, 1.0)`：传 `0` 得到 0.1，传 `5` 得到 1.0。
  ⚠️ 初始尺寸 = 图片原始像素当逻辑像素用；超过**主屏逻辑尺寸（`mon.size()` ÷ 缩放，是整屏不是工作区）的 80%** 则等比缩小；最小 24×24。
  ⚠️ 用户可做的交互：按住拖动；滚轮缩放（倍率夹在 **0.15× ~ 6×**）；双击关闭；Ctrl+双击变缩略图（宽 92px），缩略图态双击还原；Esc 关闭；按 `1` 回原始大小；右键菜单被禁用。
  ⚠️ 解不了的格式（如 SVG）报「图片解码失败: …」。图片字节常驻内存，窗口被销毁时才清掉；代码里**没有**「同时最多几张贴图」的上限。

```js
// 无需权限声明
const buf = await itools.readImage();
const text = await itools.ocr(buf, "zh-Hans");   // 省略第二参 = 跟随系统语言
if (text.trim() === "") console.log("没识别到文字");
else await itools.copyText(text);

const pinId = await itools.createPin(buf, 0.9);  // opacity 0.1~1，越界自动 clamp
console.log("pinId =", pinId);                   // 只能当标识符，插件关不掉（用户双击/Esc 自己关）
```

## 删除沙盒文件（`itools.removeFile`，无需授权）

没有权限门禁（实现体不调 `plugin_granted`），但**强制沙盒**：只认当前插件会话自己的沙盒相对路径。与 `readFile` / `writeFile` 共用同一个沙盒根。

- `itools.removeFile(path: string): Promise<void>` — 删除插件沙盒内的一个文件。`path` 必须是**相对路径**。
  ⚠️ **文件不存在不是错误**：`NotFound` 被显式吞掉返回成功，接口是幂等的。
  ⚠️ **只删文件，删不了目录**（用的是 `remove_file`），对目录会得到「删除文件失败: …」。
  ⚠️ 路径校验只放行 `Normal` / `CurDir` 组件，下列一律拒绝并报「只能访问插件沙盒内的相对路径（禁绝对路径/盘符/根/..）」：空串、`C:foo` 这类盘符相对、`/foo` 或 `\foo` 这类根相对、任何含 `..` 的路径。
  ⚠️ 沙盒根按会话分：正式会话是 `<数据根>/plugin-data/<插件id>/files/`（数据根 = `%LOCALAPPDATA%\itools`，debug 构建是 `itools-dev`），**调试会话是另一个根** `<数据根>/dev/plugin-data/<插件id>/files/`，两者互不可见。
  ⚠️ `saveImage` / `readLocalImage` 能碰到的磁盘任意路径，`removeFile` **一概删不了**。要删沙盒外的东西请用 `itools.trash`（走回收站，需 `fs-trash` 授权）。

```js
// 无需权限声明
await itools.writeFile("cache/last.json", JSON.stringify({ a: 1 }));
await itools.removeFile("cache/last.json");   // 相对路径，限插件沙盒
await itools.removeFile("cache/last.json");   // 再删一次也不报错（不存在视为成功）
// await itools.removeFile("C:\\x.txt");      // ✗ 「只能访问插件沙盒内的相对路径…」
```

## 全局热键（顶层 `itools.*`，需 `hotkey` 授权）

按下即唤起插件的系统级热键。注册与注销都要求 `plugin.json` 的 `permissions` 里声明 `hotkey`**且**用户在「插件管理」里授权，两者缺一即被拒。

- `itools.registerHotkey(accelerator: string, code?: string | null): Promise<void>` — 把一个全局热键绑定到当前插件；`code` 填 `plugin.json` 里 `features[].code`，用于「按下时窗口里不是本插件」的情况下决定拉起哪个 feature，不传即为 `null`。
  ⚠️ 写法：`+` 分隔，token 前后空格会被 trim，整体大小写不敏感（`"Alt + Space"` 合法）。修饰键 `ALT`/`OPTION`、`CTRL`/`CONTROL`、`SHIFT`、`CMD`/`COMMAND`/`SUPER`；iTools 额外把 `WIN`/`META` 归一成 `SUPER`（上游别名表里没有 `win`）；`CommandOrControl`/`CommandOrCtrl`/`CmdOrCtrl`/`CmdOrControl` 在 Windows 上等于 `CONTROL`。
  ⚠️ 主键写 W3C code 名或短别名：`KeyA`–`KeyZ` 或 `A`–`Z`；`Digit0`–`Digit9` 或 `0`–`9`；`F1`–`F24`；`Escape`/`Esc`、`Space`、`Tab`、`Enter`、`Backspace`、`Delete`、`Insert`、`Home`、`End`、`PageUp`、`PageDown`、`CapsLock`、`NumLock`、`ScrollLock`、`PrintScreen`、`Pause`/`PauseBreak`；`ArrowUp`/`Up` 等四个方向键；`Numpad0`–`Numpad9`（`Num0`–`Num9`）与 `NumpadAdd`/`NumAdd`、`NumpadEnter`/`NumEnter` 等小键盘键；符号键 `` Backquote/` ``、`Minus`/`-`、`Equal`/`=`、`Comma`/`,`、`Period`/`.`、`Slash`/`/`、`Semicolon`/`;`、`Quote`/`'`、`BracketLeft`/`[`、`BracketRight`/`]`、`Backslash`/`\`；以及 `AudioVolumeUp`/`VolumeUp` 等音量键与 `MediaPlayPause`/`MediaTrackNext` 等媒体键。
  ⚠️ 修饰键必须写在主键**之前**，且只能有一个主键：`"shift+KeyQ+alt"`、`"Ctrl+C+Shift"`、`"Ctrl+Shift+C+A"`、`"alt+"`（空 token）全部会得到「无效快捷键（需至少一个修饰键）：…」。
  ⚠️ 无修饰键时**只放行 F1–F12**：`"f3"` 合法，`"space"`、`"a"`、`"F13"` 一律被拒（F13–F24 是合法主键，但必须配至少一个修饰键）。
  ⚠️ 调试会话与正式插件互不抢占：跨边界注册同一个键直接报错，错误里会写清被哪一侧的哪个插件占着。同侧（都正式 / 都调试）是「后注册者胜出」——原插件不会收到任何通知，只在 iTools 日志里留一行换绑记录。
  ⚠️ 绑定只活在 iTools 进程内存里。关掉调试窗口、在「插件管理」里停用插件，**都不会**注销已注册的热键；真正能释放的只有插件自己调 `unregisterHotkey`，或退出 iTools。
  ⚠️ 与宿主热键的优先级：宿主截图热键 → 宿主贴图热键 → 插件热键 → 切换主搜索窗。此外，若该组合键已被宿主（主唤起 / 截图 / 贴图）注册，按 `tauri-plugin-global-shortcut` 2.3.2 + `global-hotkey` 0.8.0 的实现，`RegisterHotKey` 会以 `ERROR_HOTKEY_ALREADY_REGISTERED` 失败，插件这边拿到「注册热键失败（可能被系统或其它程序占用）：…」——此路径未在真机验证过。
- `itools.unregisterHotkey(accelerator: string): Promise<void>` — 注销本插件注册的那个键；该键本就不在插件热键表里时也返回成功（幂等），且不会碰宿主或别的插件的注册。
  ⚠️ 归属口径与注册完全一致（插件 id + 调试/正式标志都要相等）：同名的调试插件不能注销正式插件的键，反之亦然，会得到「该快捷键不属于本插件，拒绝注销」。
  ⚠️ accelerator 的合法写法与 `registerHotkey` 同一套解析器，「无修饰键只允许 F1–F12」照样生效——所以 `unregisterHotkey("space")` 得到的是「无效快捷键」，不是幂等成功。
- `itools.onHotkey(cb: (payload: { accelerator: string; code: string | null }) => void): void` — 同步注册回调，不是 Promise；`accelerator` 是注册时传的原始字符串（未规范化），`code` 是注册时传的那个值。
  ⚠️ **不是每次按键都会触发**。只有「共享插件窗（`plugin`）或调试窗（`plugin-dev`）存在，且窗口里当前加载的正是本插件」时才推 `hotkey` 事件。否则：注册时带了 `code` 的，宿主改为把插件拉起来（正式插件走 `open_plugin`，`hidden=true`，面板不会弹出来；调试插件走调试窗口），插件收到的是 **`onEnter` 且 `info.query === "__hotkey__"`**；注册时 `code` 为 `null` 的，宿主回退到默认行为——只开关主搜索窗，插件什么也收不到。
  ⚠️ 后台常驻实例（`plugin-bg-<id>`）和插件自开的独立窗口（`createWindow` 开的 `plugin-win-<id>-*`）**永远收不到 `hotkey` 事件**：分发只查那两个共享窗口的会话。后台常驻插件按下自己注册的热键，走的是「复用后台窗口 → 推一发 `enter` 事件」，因此要在 `onEnter` 里判断 `query`。
  ⚠️ 推事件前宿主**有意不**显示/聚焦面板（隐藏窗口上照常执行），要不要显示由插件自己决定。
  ⚠️ 事件无缓冲，注册 `cb` 之前推来的会直接丢弃；可多次调用叠加回调，没有取消订阅的接口；回调里抛出的异常会被事件总线吞掉并 `console.error("[iTools] 事件回调异常", e)`。

```js
itools.onEnter(async (info) => {
  // 后台常驻/热键唤起路径：窗口里不是本插件时，热键走的是 onEnter 而不是 onHotkey
  if (info.query === "__hotkey__") {
    console.log("由热键唤起（面板未显示），code =", info.code);
    return;
  }
  if (window.__hkDone) return; // onEnter 可能被多次调用，注册只做一次
  window.__hkDone = true;
  try {
    // 主键写 KeyS 或 S 都行；无修饰键只有 F1–F12 才合法
    await itools.registerHotkey("Alt+Shift+KeyS", "shot");
  } catch (e) {
    document.body.textContent = "热键注册失败：" + e; // e 就是后端返回的中文原因
  }
});

itools.onHotkey((p) => {
  console.log("热键触发", p.accelerator, p.code); // { accelerator: "Alt+Shift+KeyS", code: "shot" }
});

// 不主动注销的话，热键会一直活到 iTools 退出
itools.onExit(() => { itools.unregisterHotkey("Alt+Shift+KeyS").catch(() => {}); });
```

## 鼠标注入（`input`，需 `input-inject` 授权）

- `itools.input.mouseClick(x: number, y: number): Promise<void>` — 移动到 `(x, y)` 并单击左键。
- `itools.input.mouseDoubleClick(x: number, y: number): Promise<void>` — 移动到 `(x, y)` 并双击左键。
- `itools.input.mouseRightClick(x: number, y: number): Promise<void>` — 移动到 `(x, y)` 并单击右键。

⚠️ 坐标是**屏幕物理像素、虚拟桌面坐标系**（Win32 那一套，可跨多显示器，原点可能为负），不是页面里的 CSS/DIP 坐标。有缩放的屏幕上两者不相等，必须先用 `itools.screen.toPhysical(x, y)`（返回四舍五入后的整数 `{ x, y }`，无需授权）换算；`itools.screen.cursorPoint()` 返回的已经是物理像素，可直接喂进来。
⚠️ 越界坐标**被静默夹到虚拟桌面边缘，不会报错**（内部归一化到 `0..=65535` 后 clamp）。
⚠️ Rust 侧参数是 `i32`，请自己先取整——带小数的坐标会在参数反序列化阶段失败，报错不是本模块的中文文案。
⚠️ **调用前必须先 `await itools.hide()`**。本组 API 只把事件灌进 Windows 全局输入队列，它会打到 `GetForegroundWindow` 决定的那个窗口；iTools 自己还在前台时，点击就点在 iTools 身上。
⚠️ 受 Windows UIPI 限制：iTools 未以管理员运行时，无法向更高完整性级别的窗口注入（以管理员身份运行的程序、UAC 提权对话框等），表现为静默失败或部分失败，报错是「输入注入被系统拒绝（已发送 n/m 个事件）：…」。这是系统强制安全边界，用户态绕不过去。
⚠️ 事件在**同一次 `SendInput`** 里发出，`time` 全为 0，中间**没有任何 sleep**：单击/右键是 3 个事件（move + down + up），双击是 5 个（move + 两组 down/up）。按下时长与双击间隔都不可配置，目标程序认不认这个双击取决于系统的双击时间阈值（代码里没有读取或适配该阈值）。
⚠️ 本组只有 `mouseMove` / `mouseClick` / `mouseDoubleClick` / `mouseRightClick` 四个：**没有** `mouseDown`/`mouseUp`（做不了拖拽）、没有滚轮、没有中键。右键菜单弹出来之后也只能继续用 `mouseClick`/`keyTap` 盲点。
⚠️ 非 Windows 平台固定返回「输入注入仅在 Windows 上可用」。

```js
// 页面里的 DIP 坐标 → 物理像素 → 让出前台 → 点击
const p = await itools.screen.toPhysical(1200, 640);
await itools.hide();
try {
  await itools.input.mouseClick(p.x, p.y);
} catch (e) {
  console.error(e); // 目标是管理员窗口时，这里是 UIPI 的中文说明
}
```

## 电源（`power`，需 `system-manage` 授权）

- `itools.power.sleep(): Promise<void>` — 让整机进入睡眠（Win32 `SetSuspendState`，睡眠而非休眠）。
  ⚠️ 三个参数写死、插件改不了：不休眠、不强制挂起有未保存数据的应用、不禁止唤醒事件。睡眠期间保电维持内存内容，不丢未保存数据，但外接设备/网络连接会短暂中断。
  ⚠️ **没有任何确认对话框**，调了就睡。要确认得插件自己弹。
  ⚠️ 失败时是「使系统进入睡眠失败（部分设备/驱动不支持挂起，或被系统电源策略阻止）」；非 Windows 固定返回「系统睡眠目前仅支持 Windows」。
  ⚠️ 未确认：`SetSuspendState` 按 Win32 语义在系统恢复后才返回，实现跑在 `spawn_blocking` 上，因此这个 Promise 很可能要到设备被唤醒后才 resolve——仓库里没有验证过该时序，别依赖 resolve 时机。
- `itools.power.restart(force?: boolean): Promise<void>` — 重启系统（`ExitWindowsEx(EWX_REBOOT)`），`force` 缺省 `false`。
  ⚠️ `force` 只改一个标志位：`true` 时额外带 `EWX_FORCE`（**不是** `EWX_FORCEIFHUNG`），对没卡死的应用也照样强杀，**未保存的数据会丢失**。`false` 走正常关机流程，前台应用有「是否保存」的机会，但**某个应用阻塞也可能让重启被推迟或取消**。
  ⚠️ resolve 只代表 `ExitWindowsEx` 调用成功（重启请求已发出），**不代表机器一定会重启**。
  ⚠️ **没有确认对话框，也没有倒计时或取消接口**，iTools 不提供 abort。
  ⚠️ 每次调用先启用 `SeShutdownPrivilege`；组策略剥夺该特权时会得到「启用系统特权「SeShutdownPrivilege」失败（当前账户策略可能不允许，关机/重启需要该特权）」。非 Windows 固定返回「关机/重启目前仅支持 Windows」。
  ⚠️ `itools.power.shutdown(force?)` 是同一实现体（只把 `EWX_REBOOT` 换成 `EWX_SHUTDOWN`），`force` 语义完全一致。
  ⚠️ 该命令对**后台常驻实例与插件独立窗口同样放行**（capability 覆盖 `plugin` / `plugin-bg-*` / `plugin-win-*`），后台插件也能把机器重启掉。

```js
// 两个动作都没有系统确认框，要确认只能自己弹
if (confirm("确定让电脑睡眠？")) {
  try {
    await itools.power.sleep();
  } catch (e) {
    alert(e); // 例如「使系统进入睡眠失败（部分设备/驱动不支持挂起，或被系统电源策略阻止）」
  }
}

// 默认非强制：给别的程序保存的机会（也因此可能被某个程序拦下来）
// await itools.power.restart();
// 强制重启：所有应用被强杀，未保存数据会丢
// await itools.power.restart(true);
```

## 托管运行时收尾（`runtime`，需 `runtime` 授权）

- `itools.runtime.quit(streamId: string): Promise<void>` — 请 `execStream` 拉起的那个子进程自己退出（Windows 上给它的进程组发 `CTRL_BREAK`）。除 `runtime` 授权外，还要求该 `streamId` 的归属会话与调用者一致（插件 id + 调试/正式标志都相等），否则「该执行会话不属于本插件」。
  ⚠️ resolve 只表示**信号已发出**，不代表进程已经退出。真正的结束由等待线程（50ms 轮询）感知后推 `plugin-runtime-exit`，即 `execStream` 的 `handlers.onExit(code, timedOut)`；拿不到退出码时 `code` 为 `-1`，`timeoutMs` 到点被杀时 `timedOut` 为 `true`。
  ⚠️ 「优雅」是有水分的：源码注释明说不保证程序真的会优雅处理——系统默认动作通常就是终止，只是给了它一个走清理路径的机会。要立即强杀用 `itools.runtime.kill(streamId)`（`TerminateProcess`，不给任何清理机会）。
  ⚠️ `quit` / `kill` 都**不**从会话表里摘掉条目（移除与 exit 事件统一由等待线程完成），所以紧接着再 `quit` 一次通常也会成功；进程已经退出后再调，会得到「执行会话不存在或已结束」，或「附加目标进程控制台失败（pid=…，可能已退出）」。
  ⚠️ 子进程的 stdin 是 `Stdio::null()`，插件**没有**任何写 stdin 的通道——ffmpeg 常用的「往 stdin 写 q」收尾在这里做不到，想让它正常写完文件尾只能靠 `quit`。
  ⚠️ **流不会自动收尾**：插件被切走 / 关闭时，宿主会清理本地服务、摄像头、录屏录音、输入相关状态，但**清理名单里没有 runtime**。`execStream` 拉起的 ffmpeg/adb/yt-dlp 会一直跑到自己退出，插件必须自己在退出前 `quit`/`kill`，否则留下孤儿进程。
  ⚠️ 只对 `execStream` 返回的 `streamId` 有效；一次性的 `itools.runtime.exec` 没有对应的 `quit`/`kill`。
  ⚠️ `runtime` 权限只覆盖宿主清单里下载并校验过 SHA-256 的固定几个程序（ffmpeg / adb / yt-dlp），与 `runCommand` 是完全独立的两套授权，不共享。

```js
const id = await itools.runtime.execStream(
  "ffmpeg",
  ["-i", "in.mp4", "-c:v", "libx264", "out.mp4"],
  { timeoutMs: 600000 },
  {
    onStderr: (s) => console.log(s),
    onExit: (code, timedOut) => console.log("结束", code, timedOut),
  }
);

// 优雅收尾：发 CTRL_BREAK，给 ffmpeg 一次写文件尾的机会（不保证它会理）
await itools.runtime.quit(id);

// 插件被关掉时流不会自动清理，必须自己收尾
itools.onExit(() => { itools.runtime.kill(id).catch(() => {}); });
```

## UI / 平台

- `itools.showToast(msg: string): void` — 轻量提示（**同步，无需 await**）。
- `itools.platform` — 只读属性：`{ isWindows, isMacOS, isLinux, isDev }`。

## 惯用模式

```js
// 进入即初始化：text/regex 触发用 info.query，否则读剪贴板；生成器类直接产出不必读剪贴板
itools.onEnter(async (info) => {
  if ((info.type === "text" || info.type === "regex") && info.query) input.value = info.query;
  else { try { const t = await itools.readText(); if (t && t.trim()) input.value = t; } catch (_) {} }
  input.focus(); input.select();
});
// 复制结果 + 提示
copyBtn.onclick = async () => { await itools.copyText(out.value); itools.showToast("已复制"); };
// Esc 收起
window.addEventListener("keydown", (e) => { if (e.key === "Escape") itools.hide(); });
```
