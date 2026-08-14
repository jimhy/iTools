# PixShot — iTools 截图插件（PixPin 风格）

区域/全屏截图 + 标注编辑 + 贴图（把图片钉在屏幕最前面），全部在 iTools 插件面板内完成。**v2 起改用 iTools 原生能力，不再依赖 PowerShell。**

## 功能

- **区域截图**：冻结主屏 → 拖动框选（原生透明置顶覆盖层）→ 松开即完成；右键 / Esc 取消。截图时自动隐藏 iTools 面板，截完自动恢复。
- **全屏截图**：一键截取整个主屏。
- **标注编辑器**：矩形 / 椭圆 / 直线 / 箭头 / 画笔 / 荧光笔 / 文字 / 序号 / 马赛克 / 取色器 / 裁剪，8 色 + 粗细可调，撤销/重做（Ctrl+Z / Ctrl+Y）。
- **贴图（Pin）**：把编辑结果或剪贴板中的图片钉成无边框置顶小窗——拖动移动、滚轮缩放、按 `1` 回原始大小、双击或 Esc 关闭。
- **输出**：复制到剪贴板（Ctrl+C）、原生「另存为」保存 PNG（Ctrl+S）；截图完成后原图自动进剪贴板。
- **全局快捷键**（可选）：勾选后注册 `Ctrl+Shift+A`，任意界面按下直接开始区域截图。
- **最近截图**：保留最近 3 张，可点击继续编辑。

## 安装 / 权限

1. 把 `pixshot` 目录放进 iTools 的插件目录（安装版 `%LOCALAPPDATA%\iTools\plugins\`，开发版项目 `plugins/`），托盘「重新加载插件」。
2. **需授权**：iTools「插件管理」→ pixshot → 打开 **screen-capture**（截图/贴图必需）与 **hotkey**（用全局快捷键时）。

> 需要 iTools 本体 **v0.1+（含原生截图能力那一版）**。旧版 iTools 没有 `captureRegion` 等原生 API，本插件将无法工作。

## 使用

- 主搜索栏输入 **截图 / 截屏 / screenshot / pixpin** → 回车，默认立即区域框选（可在面板关掉「立即截图」）。
- 输入 **贴图 / 钉图 / pin** → 把剪贴板里的图片直接钉在屏幕上。
- 编辑器内：`Ctrl+C` 复制、`Ctrl+S` 保存、📌 贴图、`Esc` 收起面板。

## 实现（备忘）

面板是 WebView2 网页，系统能力全部经 `window.itools` 原生 API：

- 截图：`itools.captureRegion()` / `captureFull()`（iTools 内部走 xcap → Windows.Graphics.Capture，**无 PowerShell、无杀软木马指纹**，返回 PNG 的 ArrayBuffer）。
- 复制/保存/贴图：`itools.writeImage()` / `saveImage()` / `createPin()`。
- 剪贴板贴图：`itools.readImage()` → `createPin()`。
- 快捷键：`itools.registerHotkey("ctrl+shift+a")` + `itools.onHotkey()`。
- 标注是纯前端 canvas 矢量 op 列表按序重绘；马赛克=区域像素化、裁剪=拍扁成新底图、撤销/重做为状态快照。图片显示用 `URL.createObjectURL`（blob:）。

## v1 → v2 变化

v1 曾用「隐藏 + base64 编码的 PowerShell 抓屏 + 剪贴板标记通道」，被 Windows Defender 误报为 `Trojan:Win32/Bearfoos.A!ml`（隐藏编码 PowerShell + 抓屏 + 读剪贴板 = 窃密木马指纹）。v2 全部改成 iTools 原生能力，**误报根除**，且更快更稳，顺带修掉了 v1 状态机（剪贴板轮询/marker/busy 死锁）的一批时序 bug。

## 尚未实现（路线图）

长截图（滚动拼接）、mp4 录屏（含系统声，需 iTools 侧接 ffmpeg）、公式识别。GIF 录屏与离线 OCR 已由 iTools 提供原生 API（`startGifRecord`/`ocr`），本插件后续可接入。
