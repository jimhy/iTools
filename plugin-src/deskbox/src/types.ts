/** DeskBox 领域模型（三个功能共用的数据类型）。 */

/** AES-GCM 密文块（base64 的 iv 与密文）。 */
export interface EncBlob {
  iv: string;
  ct: string;
}

/** 笔记树节点（文件夹或笔记）。正文单独按 id 存储（明文或密文）。 */
export interface NoteNode {
  id: string;
  parentId: string | null;
  type: "folder" | "note";
  title: string;
  order: number;
  /** 笔记是否加密上锁（正文用主密钥 AES-GCM 加密存储）。 */
  locked: boolean;
  /** 是否收藏（星标）。仅笔记有意义。 */
  starred?: boolean;
  /**
   * 明文摘要（侧栏卡片的预览行用）。仅明文笔记维护并落盘；加密笔记恒为空字符串，
   * 卡片改为展示「已加密」占位，杜绝密文明文摘要泄漏。缺省（旧数据）时首次加载回填。
   */
  excerpt?: string;
  createdAt: number;
  updatedAt: number;
}

/** 笔记正文的存储形态：明文或密文。 */
export type NoteBody = { enc: false; text: string } | { enc: true; blob: EncBlob };

/** 待办项。priority/category 为对齐设计稿新增的字段。 */
export interface Todo {
  id: string;
  text: string;
  done: boolean;
  priority: "high" | "mid" | "low";
  category: "work" | "personal" | null;
  order: number;
  createdAt: number;
  doneAt: number | null;
}

/** 待办分类视图（今天/本周为按创建时间的虚拟视图，工作/个人按 category，已完成按 done）。 */
export type TodoCat = "today" | "week" | "work" | "personal" | "done";

/** 密码条目。secret/note 为 AES-GCM 密文；strength 为展示用强度等级（0 弱 /1 中 /2 强）。 */
export interface VaultEntry {
  id: string;
  title: string;
  username: string;
  url: string;
  category: VaultCat;
  strength: 0 | 1 | 2;
  secret: EncBlob | null;
  note: EncBlob | null;
  createdAt: number;
  updatedAt: number;
}

/** 密码分类 id（内置或用户自定义；不再是固定枚举）。 */
export type VaultCat = string;

/** 密码分类（可增 / 删 / 改）。icon 为图标标识（见 vaultMeta 的 ICONS）。 */
export interface VaultCategory {
  id: string;
  name: string;
  icon: string;
}

/** 主密码配置（存于本地；verifier 用于校验解锁口令是否正确）。 */
export interface SecConfig {
  v: number;
  salt: string;
  iter: number;
  verifier: EncBlob;
}
