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
