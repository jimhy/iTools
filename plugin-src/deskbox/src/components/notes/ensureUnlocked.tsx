/**
 * 确保加密会话已解锁：已解锁直接放行；未设主密码则弹「创建主密码」；已设则弹「解锁」。
 * 环境不支持 Web Crypto 时诚实提示并返回 false。返回 Promise<boolean>——true=已解锁，false=用户取消/不可用。
 * （密码库界面会有一份等价实现，重复无妨，整合期由主 agent 提取到共享层。）
 */
import { useCryptoStore } from "../../state/cryptoStore";
import { toast } from "../../state/toastStore";
import { createMasterDialog } from "./dialogs/CreateMasterDialog";
import { unlockDialog } from "./dialogs/UnlockDialog";

export function ensureUnlocked(): Promise<boolean> {
  const cs = useCryptoStore.getState();
  if (cs.isUnlocked()) return Promise.resolve(true);
  if (!cs.cryptoOk) {
    toast("当前环境不支持加密（缺少 Web Crypto），无法使用加密功能");
    return Promise.resolve(false);
  }
  if (!cs.hasMaster()) return createMasterDialog();
  return unlockDialog();
}
