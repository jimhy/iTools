/**
 * 弹窗 → Promise 的桥接：openModal 展示一个对话框组件，返回一个在用户确认/取消时 resolve 的 Promise。
 * resolve 是幂等的——对话框内的按钮、以及点击遮罩导致的卸载（组件在 useEffect 清理里兜底 resolve(false)）
 * 都只会生效一次，避免 await 悬挂或重复 resolve。
 */
import type { ReactNode } from "react";
import { openModal } from "../../../state/modalStore";

export function ask(render: (resolve: (v: boolean) => void) => ReactNode): Promise<boolean> {
  return new Promise<boolean>((res) => {
    let done = false;
    const settle = (v: boolean) => {
      if (done) return;
      done = true;
      res(v);
    };
    openModal(render(settle));
  });
}
