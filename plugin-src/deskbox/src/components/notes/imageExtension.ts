/**
 * 自定义 TipTap 图片扩展：文档里图片 src 存逻辑地址 `itasset://<id>`（getHTML 精简、云同步就绪），
 * NodeView 把它异步解析成 data URL 显示。交互：**双击预览**（灯箱可缩放/拖动）、**右键菜单**
 * 「预览 / 另存为 / 删除」。
 *
 * 删除策略：从文档移除该图片节点，不立刻删盘文件——避免「删了再撤销」时图片破损；
 * 沙盒里的图片文件在**整条笔记被删除**时统一清理（见 notesStore.deleteNode）。
 */
import Image from "@tiptap/extension-image";
import { itools } from "../../services/itools";
import { resolveDisplaySrc } from "../../services/assets";
import { toast } from "../../state/toastStore";
import { openContextMenu } from "../../state/contextMenuStore";
import { openImageViewer } from "../../state/imageViewerStore";

export const AssetImage = Image.extend({
  // 保持默认 name="image"：粘贴/HTML 里的 <img> 天然映射到本扩展。
  addNodeView() {
    return ({ node, editor, getPos }) => {
      let curSrc: string = node.attrs.src || "";
      let displayUrl = "";

      const dom = document.createElement("div");
      dom.className = "noteImg";
      const img = document.createElement("img");
      img.draggable = false;
      img.title = "双击预览 · 右键更多";
      if (node.attrs.alt) img.alt = node.attrs.alt;
      dom.appendChild(img);

      const render = (src: string) => {
        curSrc = src;
        resolveDisplaySrc(src).then(
          (u) => {
            displayUrl = u;
            img.src = u;
            img.removeAttribute("data-broken");
          },
          () => {
            displayUrl = "";
            img.alt = "（图片已丢失）";
            img.setAttribute("data-broken", "1");
          },
        );
      };
      render(curSrc);

      const preview = () => displayUrl && openImageViewer(displayUrl);
      const saveAs = () => {
        const url = displayUrl || curSrc;
        if (!url) return;
        void itools
          .saveImage(url, "笔记图片.png")
          .then((p) => p && toast("已另存图片"))
          .catch(() => toast("保存失败"));
      };
      const remove = () => {
        const pos = typeof getPos === "function" ? getPos() : undefined;
        if (pos == null) return;
        editor.chain().focus().deleteRange({ from: pos, to: pos + 1 }).run();
      };

      img.addEventListener("dblclick", (e) => {
        e.preventDefault();
        preview();
      });
      dom.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        e.stopPropagation();
        openContextMenu(e.clientX, e.clientY, [
          { label: "预览", onSelect: preview },
          { label: "另存为…", onSelect: saveAs },
          { label: "删除", danger: true, onSelect: remove },
        ]);
      });

      return {
        dom,
        update: (updated) => {
          if (updated.type.name !== node.type.name) return false;
          if (updated.attrs.src !== curSrc) render(updated.attrs.src || "");
          return true;
        },
        // NodeView 内部（img.src）DOM 变动不触发 ProseMirror 重解析。
        ignoreMutation: () => true,
      };
    };
  },
});
