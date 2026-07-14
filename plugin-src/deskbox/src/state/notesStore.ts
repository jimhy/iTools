/**
 * 笔记状态：树（文件夹/笔记）+ 展开集合 + 当前编辑的笔记草稿 + 正文加载/保存（明文/密文两种形态）。
 * 数据与持久化、加解密全在本层；组件只做展示与交互。正文按 id 单独存储（notes.body.<id>）。
 *
 * 分层：落盘经 services/store（itools.db）；加解密经 services/crypto + state/cryptoStore；
 * 解锁弹窗经 components/notes/ensureUnlocked（本功能的 UI 编排）。
 */
import { create } from "zustand";
import type { NoteBody, NoteNode } from "../types";
import { KEY, store, uid } from "../services/store";
import { decWith, encWith } from "../services/crypto";
import { useCryptoStore } from "./cryptoStore";
import { toast } from "./toastStore";
import { ensureUnlocked } from "../components/notes/ensureUnlocked";
import { errMsg, makeExcerpt } from "../components/notes/util";
import { mdToHtml } from "../components/notes/markdown";
import { collectAssetIds, deleteImage } from "../services/assets";

// ---- 纯函数树工具（作用于给定的 tree 数组） ----
const childrenOf = (tree: NoteNode[], pid: string | null) =>
  tree.filter((n) => n.parentId === pid).sort((a, b) => a.order - b.order);

/** 重排某父级下子节点的 order 为 0..n（原地改 node.order）。 */
const reindex = (tree: NoteNode[], pid: string | null) => childrenOf(tree, pid).forEach((n, i) => (n.order = i));

const findNode = (tree: NoteNode[], id: string | null) => (id ? tree.find((n) => n.id === id) ?? null : null);

/** nodeId 的任一祖先是否为 ancestorId（防止把节点拖进自己的子树）。 */
function isAncestor(tree: NoteNode[], ancestorId: string, nodeId: string | null): boolean {
  let cur = findNode(tree, nodeId);
  while (cur && cur.parentId != null) {
    if (cur.parentId === ancestorId) return true;
    cur = findNode(tree, cur.parentId);
  }
  return false;
}

/** 收集以 id 为根的整棵子树的 id（含自身）。 */
function collectSubtree(tree: NoteNode[], id: string, acc: string[]): string[] {
  acc.push(id);
  childrenOf(tree, id).forEach((c) => collectSubtree(tree, c.id, acc));
  return acc;
}

// ---- 正文的加密/明文读写 ----
async function loadBody(node: NoteNode): Promise<string> {
  const raw = await store.get<NoteBody>(KEY.body(node.id));
  if (!raw) return "";
  if (!raw.enc) return raw.text || "";
  const cs = useCryptoStore.getState();
  if (!cs.isUnlocked()) {
    const ok = await ensureUnlocked();
    if (!ok) throw new Error("locked");
  }
  const key = useCryptoStore.getState().masterKey;
  if (!key) throw new Error("locked");
  return decWith(key, raw.blob);
}

async function saveBody(node: NoteNode, text: string): Promise<void> {
  if (node.locked) {
    const key = useCryptoStore.getState().masterKey;
    if (!key) throw new Error("已锁定，无法保存加密笔记");
    const blob = await encWith(key, text);
    await store.set(KEY.body(node.id), { enc: true, blob });
  } else {
    await store.set(KEY.body(node.id), { enc: false, text });
  }
}

const persistTree = (tree: NoteNode[]) => store.set(KEY.tree, tree);
const persistExpanded = (expanded: Set<string>) => store.set(KEY.expanded, Array.from(expanded));

/**
 * 删除若干笔记时清理其正文里引用的本地图片文件（itasset://）。
 * 明文笔记从存储/当前草稿取 HTML 收集资源 id；加密笔记未解锁时无法读正文，其图片文件本次不清理（可接受）。
 */
async function purgeAssetsFor(nodeIds: string[], curNote: string | null, draftBody: string): Promise<void> {
  for (const id of nodeIds) {
    let html: string | null = null;
    if (id === curNote) {
      html = draftBody;
    } else {
      const raw = await store.get<NoteBody>(KEY.body(id));
      if (raw && !raw.enc) html = raw.text;
    }
    if (html) {
      for (const aid of collectAssetIds(html)) await deleteImage(aid);
    }
  }
}

export interface NoteHit {
  node: NoteNode;
  snippet: string;
}

interface NotesState {
  /** 笔记树（供 StatusBar 计数，字段名固定）。 */
  tree: NoteNode[];
  expanded: Set<string>;
  /** 编辑器打开的笔记 id（决定保存/删除/上锁目标）。 */
  curNote: string | null;
  /** 树里高亮/“新建到此”的目标节点 id（笔记或文件夹）。 */
  selId: string | null;
  draftTitle: string;
  /** 正文 HTML（TipTap 富文本）。旧版 Markdown 正文在 openNote 时迁移为 HTML。 */
  draftBody: string;
  /** 每次新建笔记自增，触发编辑器聚焦标题。 */
  titleFocusTick: number;

  load: () => Promise<void>;
  toggleFolder: (id: string) => void;
  newNode: (type: "folder" | "note") => Promise<void>;
  openNote: (id: string, isNew?: boolean) => Promise<void>;
  rename: (id: string, title: string) => void;
  moveInto: (dragId: string, folderId: string) => void;
  moveAfter: (dragId: string, targetId: string) => void;
  moveToRoot: (dragId: string) => void;
  setDraftTitle: (v: string) => void;
  setDraftBody: (v: string) => void;
  toggleLock: () => Promise<void>;
  /** 切换某笔记的星标（收藏）。 */
  toggleStar: (id: string) => void;
  subtreeCount: (id: string) => number;
  deleteCurrentNote: () => Promise<void>;
  /** 删除任意节点（文件夹级联删其内笔记与各自正文）。供右键菜单用。 */
  deleteNode: (id: string) => Promise<void>;
  /** 立即落盘待保存的草稿（切换/卸载/退出前调）。 */
  flush: () => void;
  searchNotes: (q: string) => Promise<NoteHit[]>;
}

// 防抖保存计时器（非响应式，放模块作用域）。
let saveTimer: ReturnType<typeof setTimeout> | null = null;

export const useNotesStore = create<NotesState>((set, get) => {
  const scheduleSave = () => {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      saveTimer = null;
      const { curNote, tree, draftBody } = get();
      const node = findNode(tree, curNote);
      if (!node) return;
      saveBody(node, draftBody)
        .then(() => persistTree(get().tree))
        .catch((e) => toast("保存失败：" + errMsg(e)));
    }, 400);
  };

  return {
    tree: [],
    expanded: new Set<string>(),
    curNote: null,
    selId: null,
    draftTitle: "",
    draftBody: "",
    titleFocusTick: 0,

    load: async () => {
      const tree = (await store.get<NoteNode[]>(KEY.tree)) ?? [];
      const exp = (await store.get<string[]>(KEY.expanded)) ?? [];
      // 回填旧数据缺失的明文摘要（侧栏卡片预览用）：明文笔记读正文取摘要，加密笔记恒空（显示占位）。
      let changed = false;
      for (const n of tree) {
        if (n.type !== "note" || n.excerpt != null) continue;
        if (n.locked) {
          n.excerpt = "";
        } else {
          const raw = await store.get<NoteBody>(KEY.body(n.id));
          n.excerpt = raw && !raw.enc ? makeExcerpt(raw.text || "") : "";
        }
        changed = true;
      }
      if (changed) void persistTree(tree);
      set({ tree, expanded: new Set(exp) });
    },

    toggleFolder: (id) => {
      const exp = new Set(get().expanded);
      if (exp.has(id)) exp.delete(id);
      else exp.add(id);
      set({ expanded: exp, selId: id });
      void persistExpanded(exp);
    },

    newNode: async (type) => {
      const { selId, tree, expanded } = get();
      const cur = findNode(tree, selId);
      // 文件夹只允许一层：文件夹始终建在根级；笔记建在选中文件夹内或其同级。
      const parentId = type === "folder" ? null : cur ? (cur.type === "folder" ? cur.id : cur.parentId) : null;
      const node: NoteNode = {
        id: uid(),
        parentId,
        type,
        title: type === "folder" ? "新建文件夹" : "新建笔记",
        order: childrenOf(tree, parentId).length,
        locked: false,
        starred: false,
        excerpt: "",
        createdAt: Date.now(),
        updatedAt: Date.now(),
      };
      const nextTree = [...tree, node];
      const nextExp = new Set(expanded);
      if (parentId) nextExp.add(parentId);
      set({ tree: nextTree, expanded: nextExp });
      void persistTree(nextTree);
      void persistExpanded(nextExp);
      if (type === "note") {
        await get().openNote(node.id, true);
        set((s) => ({ titleFocusTick: s.titleFocusTick + 1 }));
      } else {
        set({ selId: node.id });
      }
    },

    openNote: async (id, isNew = false) => {
      get().flush();
      const node = findNode(get().tree, id);
      if (!node || node.type !== "note") return;
      let text = "";
      try {
        text = isNew ? "" : await loadBody(node);
      } catch {
        toast("未解锁，无法打开此加密笔记");
        return;
      }
      // 旧版 Markdown 正文迁移为 HTML（新笔记为空；已是 HTML 的原样返回）。
      set({ curNote: id, selId: id, draftTitle: node.title || "", draftBody: mdToHtml(text) });
    },

    rename: (id, title) => {
      const tree = get().tree;
      const node = findNode(tree, id);
      if (!node) return;
      node.title = title.trim() || (node.type === "folder" ? "未命名文件夹" : "未命名笔记");
      node.updatedAt = Date.now();
      set({ tree: [...tree], ...(get().curNote === id ? { draftTitle: node.title } : {}) });
      void persistTree(get().tree);
    },

    moveInto: (dragId, folderId) => {
      const tree = get().tree;
      const n = findNode(tree, dragId);
      if (!n) return;
      // 文件夹只允许一层：文件夹不能拖进另一个文件夹（只有笔记能进文件夹）。
      if (n.type === "folder") {
        toast("文件夹不能嵌套到文件夹里");
        return;
      }
      if (dragId === folderId || isAncestor(tree, dragId, folderId)) {
        toast("不能移动到自身或其子级");
        return;
      }
      const oldParent = n.parentId;
      n.parentId = folderId;
      n.order = childrenOf(tree, folderId).length;
      reindex(tree, folderId);
      if (oldParent !== folderId) reindex(tree, oldParent);
      const exp = new Set(get().expanded);
      exp.add(folderId);
      set({ tree: [...tree], expanded: exp });
      void persistExpanded(exp);
      void persistTree(get().tree);
    },

    moveAfter: (dragId, targetId) => {
      const tree = get().tree;
      const t = findNode(tree, targetId);
      const n = findNode(tree, dragId);
      if (!t || !n) return;
      if (dragId === t.parentId || isAncestor(tree, dragId, t.parentId)) {
        toast("不能移动到自身或其子级");
        return;
      }
      const oldParent = n.parentId;
      const sibs = childrenOf(tree, t.parentId).filter((x) => x.id !== dragId);
      const idx = sibs.findIndex((x) => x.id === targetId);
      n.parentId = t.parentId;
      const arr = [...sibs.slice(0, idx + 1), n, ...sibs.slice(idx + 1)];
      arr.forEach((x, i) => (x.order = i));
      if (oldParent !== t.parentId) reindex(tree, oldParent);
      set({ tree: [...tree] });
      void persistTree(get().tree);
    },

    moveToRoot: (dragId) => {
      const tree = get().tree;
      const n = findNode(tree, dragId);
      if (!n || n.parentId === null) return;
      const old = n.parentId;
      n.parentId = null;
      n.order = childrenOf(tree, null).length;
      reindex(tree, null);
      reindex(tree, old);
      set({ tree: [...tree] });
      void persistTree(get().tree);
    },

    setDraftTitle: (v) => {
      const { curNote, tree } = get();
      if (!curNote) return;
      const node = findNode(tree, curNote);
      if (node) {
        node.title = v.trim() || "未命名笔记";
        node.updatedAt = Date.now();
      }
      set({ draftTitle: v, tree: [...tree] });
      scheduleSave();
    },

    setDraftBody: (v) => {
      const { curNote, tree } = get();
      if (!curNote) return;
      const node = findNode(tree, curNote);
      if (node) {
        node.updatedAt = Date.now();
        // 明文笔记同步维护卡片摘要；加密笔记恒空（卡片显示占位，不落明文摘要）。
        node.excerpt = node.locked ? "" : makeExcerpt(v);
      }
      set({ draftBody: v, tree: [...tree] });
      scheduleSave();
    },

    toggleLock: async () => {
      const { curNote, tree, draftBody } = get();
      const node = findNode(tree, curNote);
      if (!node) return;
      const ok = await ensureUnlocked();
      if (!ok) return;
      const willLock = !node.locked;
      const prevExcerpt = node.excerpt;
      node.locked = willLock;
      node.updatedAt = Date.now();
      // 上锁清除明文摘要；解锁按当前正文重建摘要。
      node.excerpt = willLock ? "" : makeExcerpt(draftBody);
      try {
        await saveBody(node, draftBody);
        await persistTree(get().tree);
      } catch (e) {
        node.locked = !willLock; // 回滚
        node.excerpt = prevExcerpt;
        toast("操作失败：" + errMsg(e));
        return;
      }
      set({ tree: [...tree] });
      toast(willLock ? "已加密上锁" : "已取消加密");
    },

    toggleStar: (id) => {
      const tree = get().tree;
      const node = findNode(tree, id);
      if (!node) return;
      node.starred = !node.starred;
      node.updatedAt = Date.now();
      set({ tree: [...tree] });
      void persistTree(get().tree);
    },

    subtreeCount: (id) => collectSubtree(get().tree, id, []).length - 1,

    deleteCurrentNote: async () => {
      const { curNote, tree, expanded, selId } = get();
      const node = findNode(tree, curNote);
      if (!node) return;
      if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
      }
      const ids = collectSubtree(tree, node.id, []);
      const pid = node.parentId;
      const nextTree = tree.filter((n) => !ids.includes(n.id));
      const exp = new Set(expanded);
      await purgeAssetsFor(ids, curNote, get().draftBody); // 清理正文引用的本地图片
      for (const id of ids) {
        await store.remove(KEY.body(id));
        exp.delete(id);
      }
      reindex(nextTree, pid);
      set({
        tree: [...nextTree],
        expanded: exp,
        curNote: null,
        draftTitle: "",
        draftBody: "",
        selId: selId && ids.includes(selId) ? null : selId,
      });
      void persistTree(get().tree);
      void persistExpanded(exp);
    },

    deleteNode: async (id) => {
      const { tree, expanded, selId, curNote } = get();
      const node = findNode(tree, id);
      if (!node) return;
      const ids = collectSubtree(tree, id, []);
      const pid = node.parentId;
      const nextTree = tree.filter((n) => !ids.includes(n.id));
      const exp = new Set(expanded);
      await purgeAssetsFor(ids, curNote, get().draftBody); // 清理正文引用的本地图片
      for (const bid of ids) {
        await store.remove(KEY.body(bid));
        exp.delete(bid);
      }
      reindex(nextTree, pid);
      const clearCur = curNote != null && ids.includes(curNote);
      if (clearCur && saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
      }
      set({
        tree: [...nextTree],
        expanded: exp,
        ...(clearCur ? { curNote: null, draftTitle: "", draftBody: "" } : {}),
        selId: selId != null && ids.includes(selId) ? null : selId,
      });
      void persistTree(get().tree);
      void persistExpanded(exp);
    },

    flush: () => {
      if (!saveTimer) return;
      clearTimeout(saveTimer);
      saveTimer = null;
      const { curNote, tree, draftBody } = get();
      const node = findNode(tree, curNote);
      if (!node) return;
      if (node.locked && !useCryptoStore.getState().masterKey) {
        toast("已锁定，未保存的修改需解锁后再编辑");
        return;
      }
      saveBody(node, draftBody)
        .then(() => persistTree(get().tree))
        .catch((e) => toast("保存失败：" + errMsg(e)));
    },

    searchNotes: async (q) => {
      const query = q.toLowerCase();
      const notes = get().tree.filter((n) => n.type === "note");
      const cs = useCryptoStore.getState();
      const hits: NoteHit[] = [];
      for (const n of notes) {
        const title = n.title || "";
        let snippet = "";
        let matched = title.toLowerCase().includes(query);
        const raw = await store.get<NoteBody>(KEY.body(n.id));
        if (raw) {
          let text: string | null = null;
          if (!raw.enc) text = raw.text || "";
          else if (cs.isUnlocked() && cs.masterKey) {
            try {
              text = await decWith(cs.masterKey, raw.blob);
            } catch {
              text = null;
            }
          }
          if (text) {
            const i = text.toLowerCase().indexOf(query);
            if (i >= 0) {
              matched = true;
              snippet = text.slice(Math.max(0, i - 12), i + 40);
            }
          }
        }
        if (matched) hits.push({ node: n, snippet });
      }
      return hits;
    },
  };
});

// 响应本体锁定：Topbar 直接调 cryptoStore.lock() 清除主密钥后，若正编辑加密笔记则清空编辑器，
// 避免明文残留 & 阻止旧防抖计时器以空密钥再保存。（先 flush 的时机由主 agent 在 lock 前调 notesStore.flush 更佳，见交付说明。）
let prevMasterKey = useCryptoStore.getState().masterKey;
useCryptoStore.subscribe((s) => {
  const cur = s.masterKey;
  if (prevMasterKey && !cur) {
    const st = useNotesStore.getState();
    const node = findNode(st.tree, st.curNote);
    if (node && node.locked) {
      if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
      }
      useNotesStore.setState({
        curNote: null,
        draftTitle: "",
        draftBody: "",
        selId: st.selId === node.id ? null : st.selId,
      });
    }
  }
  prevMasterKey = cur;
});
