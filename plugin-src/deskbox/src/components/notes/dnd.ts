/** 侧栏拖拽的共享类型：当前被拖节点 id 的可变引用 + 放置标记（放入文件夹 / 排到其后）。 */
export type DragRef = { current: string | null };
export type DropMark = { id: string; kind: "into" | "after" } | null;
