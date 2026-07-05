/**
 * iTools 插件全局 API —— 注入到每个插件页的 `window.itools`。
 * 生成插件时把本文件当契约：只用这里声明的方法，全部经统一门面调用。
 *
 * ⚠️ `window.itools` 用 defineProperty(configurable:false) 注入。**顶层写
 * `const itools = window.itools;`（或 let/var 同名声明）会让整个 <script> 抛
 * SyntaxError、一行不执行（症状：页面渲染正常但所有按钮/逻辑全灭，普通浏览器复现不出来）。**
 * 直接裸引用 `itools.xxx`，或起别名 `const api = window.itools`。
 */
interface IToolsDisplayInfo {
  id: number;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  scale: number;
  is_primary: boolean;
}

interface IToolsEnterInfo {
  /** 命中的 feature.code */
  code: string;
  /** 本次触发类型：'keyword' / 'regex' / 'text' */
  type: "keyword" | "regex" | "text";
  /** 触发时搜索框里的查询串 */
  query: string;
}

interface IToolsDB {
  /** 取值（自动 JSON 反序列化）；不存在返回 null */
  get(key: string): Promise<any | null>;
  /** 存值（自动 JSON 序列化） */
  set(key: string, value: any): Promise<void>;
  remove(key: string): Promise<void>;
  /** 列出所有键（可按前缀过滤） */
  keys(prefix?: string): Promise<string[]>;
}

interface ITools {
  // —— 生命周期 ——
  /** 进入插件时回调（读剪贴板预填、初始化都放这）。注册即触发（若进入信息已就绪）。 */
  onEnter(cb: (info: IToolsEnterInfo) => void): void;
  /** 页面卸载/隐藏时回调 */
  onExit(cb: () => void): void;

  // —— 窗口 ——
  /** 隐藏插件窗口（保留，下次秒开） */
  hide(): Promise<void>;
  /** 关闭插件窗口 */
  exit(): Promise<void>;
  /** 设置窗口高度（像素，宽度不变） */
  setHeight(px: number): Promise<void>;

  // —— 剪贴板 ——
  copyText(text: string): Promise<void>;
  readText(): Promise<string>;
  /** 读剪贴板里的图片 → ArrayBuffer(PNG)；无图片则 reject */
  readImage(): Promise<ArrayBuffer>;
  /** 把图片（Uint8Array/ArrayBuffer/base64/dataURL）写入剪贴板为真实图片 */
  writeImage(data: ArrayBuffer | Uint8Array | string): Promise<void>;

  // —— 截屏（需声明并授权 `screen-capture`）——
  /** 列出所有显示器 */
  listDisplays(): Promise<IToolsDisplayInfo[]>;
  /** 全屏截图（指定 displayId 或缺省主屏）→ ArrayBuffer(PNG) */
  captureFull(displayId?: number): Promise<ArrayBuffer>;
  /** 区域框选截图：隐藏面板→冻结屏→覆盖层拖选→裁剪。返回 ArrayBuffer(PNG)，用户取消返回 null */
  captureRegion(): Promise<ArrayBuffer | null>;

  // —— 图片输出 ——
  /** 保存图片到用户选择的位置（原生「另存为」对话框）。返回保存路径，取消返回 null */
  saveImage(data: ArrayBuffer | Uint8Array | string, defaultName?: string): Promise<string | null>;
  /** 贴图：把图片钉成置顶浮窗（拖动/滚轮缩放/双击或 Esc 关闭/按 1 原始大小）。opacity 0.1~1，返回 pinId */
  createPin(data: ArrayBuffer | Uint8Array | string, opacity?: number): Promise<string>;
  /** 离线 OCR（Windows.Media.Ocr，本地免费）：识别图片文字。lang 可选（"zh-Hans"/"en"） */
  ocr(data: ArrayBuffer | Uint8Array | string, lang?: string): Promise<string>;

  // —— 全局热键（需声明并授权 `hotkey`）——
  /** 注册全局热键（如 "ctrl+shift+a"，需至少一个修饰键）；按下即唤起本插件并触发 onHotkey */
  registerHotkey(accelerator: string, code?: string): Promise<void>;
  unregisterHotkey(accelerator: string): Promise<void>;
  /** 热键按下回调（收到 { accelerator, code }） */
  onHotkey(cb: (info: { accelerator: string; code: string | null }) => void): void;

  // —— 录音（需声明并授权 `audio-capture`）——
  startAudioRecord(): Promise<void>;
  /** 停止并返回 ArrayBuffer(WAV, 16-bit PCM) */
  stopAudioRecord(): Promise<ArrayBuffer>;

  // —— 录屏 GIF（需声明并授权 `screen-capture`；v1 无音频/无 mp4）——
  startGifRecord(): Promise<void>;
  /** 停止并返回 ArrayBuffer(GIF) */
  stopGifRecord(): Promise<ArrayBuffer>;

  // —— 文件（均限插件沙盒，path 为相对路径，禁绝对路径与 `..`）——
  /** 读插件沙盒内文件（相对路径）；与 writeFile 对称 */
  readFile(path: string): Promise<string>;
  /** 写插件沙盒内文件（相对路径） */
  writeFile(path: string, content: string): Promise<void>;
  /** 删除插件沙盒内文件（相对路径）；不存在视为成功 */
  removeFile(path: string): Promise<void>;

  // —— 系统 ——
  /** 用默认浏览器打开网址（仅 http/https/mailto） */
  openExternal(url: string): Promise<void>;
  /** 用默认程序打开本地路径（拒绝可执行/脚本类文件） */
  openPath(path: string): Promise<void>;
  /** 通知（真·系统 toast 通知） */
  notify(body: string): Promise<void>;
  /** 执行程序（program + 参数数组，不经 shell）；高危，需声明并授权 `runCommand` */
  runCommand(program: string, args?: string[]): Promise<void>;
  /** 联网请求（需声明并授权 `network`）；经原生代理，只支持 http/https，返回文本体 */
  fetch(
    url: string,
    init?: { method?: string; headers?: Record<string, string>; body?: string },
  ): Promise<{ status: number; ok: boolean; body: string }>;

  // —— 存储（按插件隔离的 KV）——
  db: IToolsDB;

  // —— UI ——
  /** 轻量提示（纯前端 Toast，无需 await） */
  showToast(msg: string): void;

  // —— 平台 ——
  readonly platform: {
    readonly isWindows: boolean;
    readonly isMacOS: boolean;
    readonly isLinux: boolean;
    readonly isDev: boolean;
  };
}

declare global {
  interface Window {
    itools: ITools;
  }
}

export {};
