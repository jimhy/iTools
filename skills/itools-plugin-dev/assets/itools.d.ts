/**
 * iTools 插件全局 API —— 注入到每个插件页的 `window.itools`。
 *
 * ⚠️ **本文件由 `scripts/gen-itools-dts.mjs` 从真实出口生成，请勿手改。**
 * 改了会在 `npm run check` 时被打回；要调整内容请改生成器或 API 参考文档，然后
 * `npm run gen:dts` 重新生成。
 *
 * 方法名与参数名来自 `src-tauri/src/plugin/bridge.js`（注入插件页的那个对象本身）；
 * 类型来自 `skills/itools-plugin-dev/references/window-itools-api.md` 的签名行。
 * 文档里没有正式签名的方法，参数类型退化成 `any` 并在该行标注 —— 宁可标成 any，
 * 也不编一个看起来很像的类型。**语义、限制与错误文案一律以 API 参考文档为准**，
 * 这份 .d.ts 只回答「有哪些方法、参数叫什么」。
 *
 * 当前覆盖：173 个方法，其中 149 个取到了正式签名。
 *
 * ⚠️ 首选裸引用 `itools.xxx`，或起别名 `const api = window.itools`。
 * 旧版 iTools 用 defineProperty(configurable:false) 注入，顶层 `const itools = window.itools;`
 * 会让整个 <script> 抛 SyntaxError、一行不执行（页面渲染正常但按钮全灭）；新版已改普通属性
 * 注入、该写法不再致命，但为兼容旧版仍建议避开。`itools` 已 Object.freeze，勿赋值。
 */

interface IToolsAccount {
  state(): Promise<{ loggedIn: boolean; cloudConfigured: boolean; syncEnabled: boolean }>;
  isLoggedIn(): Promise<boolean>;
}

interface IToolsAttach {
  put(id: string, data: any, mime?: string): Promise<void>;
  get(id: string): Promise<{ dataB64: any; mime: any; size: any } | null>;
  remove(id: string): Promise<void>;
  list(): Promise<{ id: any; mime: any; size: any; createdAt: any }[]>;
}

interface IToolsCamera {
  list(): Promise<Array<{ deviceId: string; name: string; formats: Array<{ width: number; height: number; fps: number | null }> }>>;
  grab(deviceId: string, opts?: { width?: number; height?: number; format?: "png" | "jpeg" | "jpg"; quality?: number }): Promise<string>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  streamStart(deviceId?: any, opts?: any, handlers?: any): any;
  streamStop(streamId: string): Promise<void>;
}

interface IToolsClipboard {
  watchStart(): Promise<void>;
  watchStop(): Promise<void>;
  onChange(cb: any): void;
}

interface IToolsContext {
  activeWindow(): Promise<{ app: string; title: string; class: string; hwnd: number; rect: { left: any; top: any; right: any; bottom: any } }>;
  browserUrl(): Promise<string | null>;
  folderPath(): Promise<string | null>;
}

interface IToolsCrypto {
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  set(key?: any, value?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  get(key?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  remove(key?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  keys(prefix?: any): any;
}

interface IToolsData {
  get(key: string): Promise<any | null>;
  set(key: string, value: any): Promise<void>;
  remove(key: string): Promise<void>;
  keys(prefix?: string): Promise<string[]>;
  sync(): Promise<{ synced: boolean; reason?: string; pushed: number; pulled: number; message?: string }>;
}

interface IToolsDb {
  get(key: string): Promise<any | null>;
  set(key: string, value: any): Promise<void>;
  remove(key: string): Promise<void>;
  keys(prefix?: string): Promise<string[]>;
}

interface IToolsFs {
  pickDir(opts?: { title?: string }): Promise<Scope | null>;
  pickFile(opts?: { title?: string; filters?: { name: string; extensions: string[] }[] }): Promise<Scope | null>;
  listScopes(): Promise<Scope[]>;
  revokeScope(scopeId: string): Promise<void>;
  list(scopeId: string, subPath?: string): Promise<Entry[]>;
  stat(scopeId: string, path?: string): Promise<Entry>;
  hash(scopeId: string, path?: string, algo?: string): Promise<string>;
  read(scopeId: string, path?: string): Promise<string>;
  readChunk(scopeId: string, path: string, offset: number, len: number): Promise<string>;
  write(scopeId: string, path: string | null, contentB64: string): Promise<void>;
  zipCreate(scopeId: string, entries: string[], outPath: string): Promise<{ entryCount: number; archiveBytes: number }>;
  unzip(scopeId: string, zipPath: string, outSub: string): Promise<{ extractedFiles: number; extractedBytes: number }>;
  watchStart(scopeId: string, subPath: string | null, cb: any): Promise<watchId>;
  watchStop(watchId: string): Promise<void>;
  getFileIcon(scopeId: string, path?: string): Promise<string>;
}

interface IToolsImage {
  resize(data: string, width: number, height: number, mode?: "contain"|"fill"|"cover"): Promise<string>;
  crop(data: string, x: number, y: number, w: number, h: number): Promise<string>;
  convert(data: string, format: "png"|"jpeg"|"webp"|"bmp"): Promise<string>;
  compress(data: string, quality: number): Promise<string>;
  info(data: string): Promise<{ width: number; height: number; format: string; sizeBytes: number }>;
}

interface IToolsInput {
  typeString(text: string): Promise<void>;
  pasteText(text: string): Promise<void>;
  pasteFile(paths: string[]): Promise<void>;
  pasteImage(data: any): Promise<void>;
  keyTap(key: string, modifiers?: string[]): Promise<void>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  mouseMove(x?: any, y?: any): any;
  mouseClick(x: number, y: number): Promise<void>;
  mouseDoubleClick(x: number, y: number): Promise<void>;
  mouseRightClick(x: number, y: number): Promise<void>;
}

interface IToolsLan {
  announce(opts: any): Promise<void>;
  discover(timeoutMs?: number): Promise<{ ip: any; name: any; port: any; info: any }[]>;
}

interface IToolsPaths {
  resolve(name: string): Promise<NamedPathInfo[]>;
  scan(name: string, opts?: { maxDepth?: number; maxItems?: number }): Promise<ScanResult>;
}

interface IToolsPower {
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  lock(): any;
  sleep(): Promise<void>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  shutdown(force?: any): any;
  restart(force?: boolean): Promise<void>;
}

interface IToolsProc {
  list(): Promise<ProcInfo[]>;
  kill(pid: number): Promise<void>;
}

interface IToolsRecord {
  loopbackStart(): Promise<null>;
  loopbackStop(): Promise<string>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  videoStart(opts?: any, onProgress?: any): any;
  videoStop(recordId: string): Promise<string>;
}

interface IToolsRuntime {
  list(): Promise<RuntimeInfo[]>;
  ensure(name: string, onProgress?: any): Promise<RuntimeInfo>;
  exec(name: any, args?: any, opts?: any): Promise<{ code: any; stdout: any; stderr: any; timedOut: any; truncated: any }>;
  execStream(name: any, args?: any, opts?: any, handlers?: any): Promise<streamId>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  kill(streamId?: any): any;
  quit(streamId: string): Promise<void>;
  remove(name: string): Promise<void>;
}

interface IToolsSchedule {
  add(opts: any): Promise<{ taskId: any }>;
  remove(taskId: string): Promise<void>;
  list(): Promise<ScheduleInfo[]>;
  onFire(cb: any): void;
}

interface IToolsScreen {
  cursorPoint(): Promise<{ x: number; y: number }>;
  pickColorAt(x: number, y: number): Promise<{ hex: string; r: number; g: number; b: number }>;
  toDip(x: number, y: number): Promise<{ x: number; y: number }>;
  toPhysical(x: number, y: number): Promise<{ x: number; y: number }>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  rectToDip(x?: any, y?: any, width?: any, height?: any): any;
  rectToPhysical(x: any, y: any, width: any, height: any): Promise<{ x: any; y: any; width: any; height: any }>;
}

interface IToolsServe {
  start(opts: any): Promise<{ serveId: any; port: any; urls: string[] }>;
  stop(serveId: string): Promise<void>;
  list(): Promise<ServeInfo[]>;
}

interface IToolsSettings {
  get(key: string): Promise<any | null>;
  all(): Promise<Record<string, any>>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  onChange(cb?: any): any;
}

interface IToolsSqlite {
  open(name: string): Promise<handle>;
  exec(handle: string, sql: string, params?: any[]): Promise<affectedRows>;
  query(handle: string, sql: string, params?: any[]): Promise<Record<string, any>[]>;
  batch(handle: string, statements: { sql: string; params?: any[] }[]): Promise<affectedRows>;
  close(handle: string): Promise<void>;
}

interface IToolsStartup {
  list(): Promise<StartupItem[]>;
  remove(id: string): Promise<void>;
  setEnabled(id: string, on: boolean): Promise<void>;
}

interface IToolsSys {
  info(): Promise<SysInfo>;
  usage(): Promise<SysUsage>;
  getPath(name: string): Promise<string>;
}

interface IToolsTray {
  set(opts: any): Promise<void>;
  remove(): Promise<void>;
  onClick(cb: any): void;
  onMenu(cb: any): void;
}

interface IToolsWin {
  list(): Promise<WindowItem[]>;
  getForeground(): Promise<WindowItem>;
  focus(hwnd: number): Promise<{ success: boolean; reason?: string }>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  move(hwnd?: any, x?: any, y?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  resize(hwnd?: any, w?: any, h?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  setRect(hwnd?: any, rect?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  minimize(hwnd?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  maximize(hwnd?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  restore(hwnd?: any): any;
  close(hwnd: number): Promise<void>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  setTopmost(hwnd?: any, on?: any): any;
}

interface IToolsApi {
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  onEnter(cb?: any): any;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  onExit(cb?: any): any;
  registerHotkey(accelerator: string, code?: string | null): Promise<void>;
  unregisterHotkey(accelerator: string): Promise<void>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  onHotkey(cb?: any): any;
  hide(): Promise<void>;
  exit(): Promise<void>;
  setHeight(px: number): Promise<void>;
  copyText(text: string): Promise<void>;
  readText(): Promise<string>;
  readImage(): Promise<ArrayBuffer>;
  writeImage(data: ArrayBuffer | Uint8Array | string): Promise<void>;
  saveImage(data: ArrayBuffer | Uint8Array | string, defaultName?: string | null): Promise<string | null>;
  createPin(data: ArrayBuffer | Uint8Array | string, opacity?: number | null): Promise<string>;
  ocr(data: ArrayBuffer | Uint8Array | string, lang?: string | null): Promise<string>;
  startAudioRecord(): Promise<null>;
  stopAudioRecord(): Promise<ArrayBuffer>;
  startGifRecord(): Promise<null>;
  stopGifRecord(): Promise<ArrayBuffer>;
  readFile(path: string): Promise<string>;
  writeFile(path: string, content: string): Promise<void>;
  removeFile(path: string): Promise<void>;
  readLocalImage(path: string): Promise<ArrayBuffer>;
  listDisplays(): Promise<Array<{ id: number; name: string; x: number; y: number; width: number; height: number; scale: number; is_primary: boolean }>>;
  captureFull(displayId?: number | null): Promise<ArrayBuffer>;
  captureRegion(opts?: { full?: boolean }): Promise<{ action: "copy" | "save" | "pin" | "ocr"; image: ArrayBuffer } | null>;
  openExternal(url: string): Promise<void>;
  openPath(path: string): Promise<void>;
  notify(body: string): Promise<void>;
  runCommand(program: string, args?: string[]): Promise<void>;
  fetch(url: string, init?: { method?: string; headers?: Record<string,string>; body?: string; responseType?: "text"|"binary" }): Promise<{ status: number; ok: boolean; body: string; base64: boolean }>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  download(url?: any, dest?: any, id?: any, onProgress?: any): any;
  downloadCancel(id: string): Promise<void>;
  exec(program: string, args?: string[], opts?: { cwd?: string; timeoutMs?: number; encoding?: "utf8"|"gbk"|"auto" }): Promise<{ code: number; stdout: string; stderr: string; timedOut: boolean; truncated: boolean }>;
  /** ⚠️ 文档里没有这条的正式签名，参数名取自 bridge.js，类型未确认 */
  execStream(program?: any, args?: any, opts?: any, handlers?: any): any;
  execKill(streamId: string): Promise<void>;
  execQuit(streamId: string): Promise<void>;
  trash(paths: string[]): Promise<{ path: string; ok: boolean; error?: string }[]>;
  showItemInFolder(path: string): Promise<void>;
  installedApps(): Promise<InstalledApp[]>;
  onMainPush(getList: any): Promise<void>;
  notifyShow(opts: any): Promise<{ notifyId: any }>;
  onNotifyClick(cb: any): void;
  onNotifyAction(cb: any): void;
  registerTool(name: string, handler: any): Promise<void>;
  redirect(label: string, payload?: string): Promise<void>;
  createWindow(page: string, opts?: any): Promise<label>;
  closeWindow(label: string): Promise<void>;
  setFeature(feature: any): Promise<void>;
  removeFeature(code: string): Promise<boolean>;
  getFeatures(codes?: string[]): Promise<Feature[]>;
  showToast(msg: string): void;

  /** 平台标志（同步属性，不是方法） */
  platform: { isWindows: boolean; isMacOS: boolean; isLinux: boolean; isDev: boolean };
  account: IToolsAccount;
  attach: IToolsAttach;
  camera: IToolsCamera;
  clipboard: IToolsClipboard;
  context: IToolsContext;
  crypto: IToolsCrypto;
  data: IToolsData;
  db: IToolsDb;
  fs: IToolsFs;
  image: IToolsImage;
  input: IToolsInput;
  lan: IToolsLan;
  paths: IToolsPaths;
  power: IToolsPower;
  proc: IToolsProc;
  record: IToolsRecord;
  runtime: IToolsRuntime;
  schedule: IToolsSchedule;
  screen: IToolsScreen;
  serve: IToolsServe;
  settings: IToolsSettings;
  sqlite: IToolsSqlite;
  startup: IToolsStartup;
  sys: IToolsSys;
  tray: IToolsTray;
  win: IToolsWin;
}

declare const itools: IToolsApi;
declare global {
  interface Window {
    itools: IToolsApi;
  }
}

export {};
