/**
 * iTools 插件全局 API —— 注入到每个插件页的 `window.itools`。
 * 生成插件时把本文件当契约：只用这里声明的方法，全部经统一门面调用。
 */
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

  // —— 文件（均限插件沙盒，path 为相对路径，禁绝对路径与 `..`）——
  /** 读插件沙盒内文件（相对路径）；与 writeFile 对称 */
  readFile(path: string): Promise<string>;
  /** 写插件沙盒内文件（相对路径） */
  writeFile(path: string, content: string): Promise<void>;

  // —— 系统 ——
  /** 用默认浏览器打开网址（仅 http/https/mailto） */
  openExternal(url: string): Promise<void>;
  /** 用默认程序打开本地路径（拒绝可执行/脚本类文件） */
  openPath(path: string): Promise<void>;
  /** 通知（首期落日志，OS 原生通知后续接） */
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
