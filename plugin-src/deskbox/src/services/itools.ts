/**
 * iTools 宿主 API 的类型契约与访问封装。
 *
 * 设计意图：这是**唯一**直接接触 `window.itools` 全局的模块。上层（hooks / components）
 * 一律经此层调用，便于替换、mock 与测试；非 iTools 环境（浏览器 dev）自动降级到 no-op mock。
 */

export interface EnterInfo {
  code: string;
  type: "keyword" | "regex" | "text";
  query: string;
}

export interface ItoolsKV {
  get<T = unknown>(key: string): Promise<T | null>;
  set(key: string, value: unknown): Promise<void>;
  remove(key: string): Promise<void>;
  keys(prefix?: string): Promise<string[]>;
}

export interface SyncResult {
  synced: boolean;
  reason?: string;
  pushed: number;
  pulled: number;
  message?: string;
}

export interface ItoolsAPI {
  onEnter(cb: (info: EnterInfo) => void): void;
  onExit(cb: () => void): void;
  hide(): Promise<void>;
  setHeight(px: number): Promise<void>;
  copyText(text: string): Promise<void>;
  readText(): Promise<string>;
  showToast(msg: string): void;
  openExternal(url: string): Promise<void>;
  /** 读剪贴板图片 → ArrayBuffer(PNG)；无图片则 reject。 */
  readImage(): Promise<ArrayBuffer>;
  /** 读取本地图片文件（沙盒外，扩展名白名单）→ ArrayBuffer；供把外部图片本地化。 */
  readLocalImage(path: string): Promise<ArrayBuffer>;
  /** 另存图片到用户选择位置（原生对话框）。返回路径，取消返回 null。 */
  saveImage(data: ArrayBuffer | Uint8Array | string, defaultName?: string): Promise<string | null>;
  /** 插件沙盒内文件读写（相对路径）：笔记图片以 data URL 文本形式落盘于此。 */
  readFile(path: string): Promise<string>;
  writeFile(path: string, content: string): Promise<void>;
  removeFile(path: string): Promise<void>;
  /** 纯本地 KV（不参与云同步）。 */
  db: ItoolsKV;
  /** 本地优先 + 可云同步的数据。 */
  data: ItoolsKV & { sync(): Promise<SyncResult> };
  platform: { isWindows: boolean; isMacOS: boolean; isLinux: boolean; isDev: boolean };
}

declare global {
  interface Window {
    itools?: ItoolsAPI;
  }
}

/** 浏览器 dev 环境（无宿主）时的降级实现：数据落 localStorage，其余为 no-op，保证可本地预览。 */
function createMock(): ItoolsAPI {
  const kv = (ns: string): ItoolsKV => ({
    async get(key) {
      const raw = localStorage.getItem(`${ns}:${key}`);
      return raw == null ? null : (JSON.parse(raw) as never);
    },
    async set(key, value) {
      localStorage.setItem(`${ns}:${key}`, JSON.stringify(value));
    },
    async remove(key) {
      localStorage.removeItem(`${ns}:${key}`);
    },
    async keys(prefix = "") {
      return Object.keys(localStorage)
        .filter((k) => k.startsWith(`${ns}:`))
        .map((k) => k.slice(ns.length + 1))
        .filter((k) => k.startsWith(prefix));
    },
  });
  return {
    onEnter: () => {},
    onExit: () => {},
    hide: async () => {},
    setHeight: async () => {},
    copyText: async (t) => {
      await navigator.clipboard?.writeText(t).catch(() => {});
    },
    readText: async () => navigator.clipboard?.readText().catch(() => "") ?? "",
    showToast: (m) => console.info("[toast]", m),
    openExternal: async (u) => {
      window.open(u, "_blank");
    },
    readImage: async () => {
      const nav = navigator as unknown as { clipboard?: { read?: () => Promise<ClipboardItem[]> } };
      if (!nav.clipboard?.read) throw new Error("剪贴板图片不可用");
      const items = await nav.clipboard.read();
      for (const it of items) {
        const type = it.types.find((t) => t.startsWith("image/"));
        if (type) return (await it.getType(type)).arrayBuffer();
      }
      throw new Error("剪贴板没有图片");
    },
    readLocalImage: async () => {
      throw new Error("浏览器 dev 环境无法读取本地文件路径");
    },
    saveImage: async (data, defaultName) => {
      let url: string;
      let revoke = false;
      if (typeof data === "string") {
        url = data.startsWith("data:") ? data : `data:image/png;base64,${data}`;
      } else {
        url = URL.createObjectURL(new Blob([data as BlobPart]));
        revoke = true;
      }
      const a = document.createElement("a");
      a.href = url;
      a.download = defaultName || "image.png";
      a.click();
      if (revoke) setTimeout(() => URL.revokeObjectURL(url), 1000);
      return defaultName || "image.png";
    },
    readFile: async (path) => {
      const v = localStorage.getItem("itfiles:" + path);
      if (v == null) throw new Error("文件不存在: " + path);
      return v;
    },
    writeFile: async (path, content) => {
      localStorage.setItem("itfiles:" + path, content);
    },
    removeFile: async (path) => {
      localStorage.removeItem("itfiles:" + path);
    },
    db: kv("db"),
    data: { ...kv("data"), sync: async () => ({ synced: false, reason: "mock", pushed: 0, pulled: 0 }) },
    platform: { isWindows: false, isMacOS: false, isLinux: false, isDev: true },
  };
}

/** 全局唯一的宿主 API 句柄。上层只 import 这一个。 */
export const itools: ItoolsAPI = window.itools ?? createMock();

/** 是否运行在真实 iTools 宿主内（而非浏览器 dev mock）。 */
export const isHost = Boolean(window.itools);
