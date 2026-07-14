/**
 * 笔记图片资源服务：把粘贴 / 拖入 / 路径读入的图片**本地化**存储到插件沙盒（assets/<id>，
 * 内容为 data URL 文本），正文里只引用逻辑地址 `itasset://<id>`。
 *
 * 设计意图：
 * - 绝不引用源图片路径——源文件可能被删；一律复制字节到项目目录。
 * - 正文（getHTML）只存 `itasset://<id>`，体积小、可移植、为下一期云同步预留（届时把 assets/ 文件推服务端）。
 * - 显示时经 NodeView 异步把 `itasset://<id>` 解析成 data URL（带内存缓存）。
 */
import { itools } from "./itools";
import { uid } from "./store";

const PREFIX = "itasset://";
/** id → data URL 的内存缓存，避免同一图片反复读盘。 */
const cache = new Map<string, string>();

const assetFile = (id: string) => `assets/${id}`;

function toU8(data: ArrayBuffer | Uint8Array): Uint8Array {
  return data instanceof Uint8Array ? data : new Uint8Array(data);
}

function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    bin += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(bin);
}

/** 由文件扩展名推断 image MIME（路径 / 文件名用）。 */
export function mimeFromName(name: string): string {
  const ext = (name.split(".").pop() || "").toLowerCase();
  const map: Record<string, string> = {
    png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif",
    bmp: "image/bmp", webp: "image/webp", svg: "image/svg+xml", ico: "image/x-icon",
    tif: "image/tiff", tiff: "image/tiff",
  };
  return map[ext] || "image/png";
}

/** 存一张图片到沙盒，返回资源 id。data URL 全文落盘，读回即可直接当 img src。 */
export async function putImage(data: ArrayBuffer | Uint8Array, mime: string): Promise<string> {
  const dataUrl = `data:${mime};base64,${bytesToBase64(toU8(data))}`;
  const id = uid();
  await itools.writeFile(assetFile(id), dataUrl);
  cache.set(id, dataUrl);
  return id;
}

/** 解析资源 id → 可显示的 data URL（带缓存）。文件缺失则抛错。 */
export async function getImageUrl(id: string): Promise<string> {
  const hit = cache.get(id);
  if (hit) return hit;
  const url = await itools.readFile(assetFile(id));
  cache.set(id, url);
  return url;
}

/** 删除一张图片资源（幂等）。 */
export async function deleteImage(id: string): Promise<void> {
  cache.delete(id);
  try {
    await itools.removeFile(assetFile(id));
  } catch {
    /* 不存在视为已删 */
  }
}

export const idToSrc = (id: string) => PREFIX + id;
export const isAssetSrc = (src: string) => src.startsWith(PREFIX);
export const srcToId = (src: string) => src.slice(PREFIX.length);

/** img src → 可显示 URL：itasset:// 走本地资源解析；http/https/data 原样返回。 */
export async function resolveDisplaySrc(src: string): Promise<string> {
  return isAssetSrc(src) ? getImageUrl(srcToId(src)) : src;
}

/** 从 HTML 收集所有 itasset:// 资源 id（删除笔记时据此清理图片文件）。 */
export function collectAssetIds(html: string): string[] {
  const ids = new Set<string>();
  const re = /itasset:\/\/([A-Za-z0-9]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(html))) ids.add(m[1]);
  return [...ids];
}
