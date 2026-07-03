# iTools

一款 macOS 风格的键盘驱动效率启动器，基于 **Tauri 2 + Rust + HTML**，目标平台 **Windows 11**。

全局快捷键唤起居中搜索框，输入即匹配、上下键选择、回车执行、Esc 即走——「即用即走」。

## 功能（MVP）

- **全局唤起**：`Alt + Space` 在任意界面弹出/收起搜索框（若被占用，自动回退到 `Alt + Shift + Space` → `Ctrl + Alt + Space`）
- **应用启动器**（四源合一，默认模式）：
  - 开始菜单 .lnk（取**本地化显示名**——「远程桌面连接」中文可搜）+ 注册表 App Paths + 内置系统命令（卸载或更新程序/控制面板/设备管理器/服务…）+ `shell:AppsFolder` 枚举（**UWP/MSIX 应用**全覆盖）
  - **拼音搜索**：`weixin` / `wx` 都能搜到「微信」（多音字自动展开，`yinyue`/`yinle` 均可）
  - 卸载/帮助类条目降权沉底（可搜到但不抢排序）
- **全盘文件秒搜**（`/f` 前缀）：直连 Windows Search Index（OLE DB），按文件名全盘实时搜索；Search 服务不可用时自动降级为 walkdir 索引用户目录
- **真实系统图标**：`SHGetFileInfoW`（文件/lnk）+ `IShellItemImageFactory`（UWP），启动预热 + 按需加载 + 缓存
- **设置界面**（托盘菜单/点击头像进入）：背景透明度（实时生效）· 自定义背景图片 · 唤起快捷键录制 · 手动添加程序 · 开机自启
- **内置即时小工具**：输入即得，回车复制结果
  - 计算器（`1+2*3`）· 进制转换（`255` / `0xFF`）· 时间戳（`now` / `1700000000`）· 颜色（`#ff8800` / `rgb(255,136,0)`）· URL 直达（`github.com`）
- **macOS 观感**：无边框透明窗口 + Acrylic 毛玻璃 + DWM 圆角 + SF/苹方字体 + Spotlight 式动态增高
- **常驻托盘**：左键唤起，右键菜单显示/退出；失焦或 Esc 自动隐藏

## 技术栈

| 模块 | 选型 |
|---|---|
| 应用框架 | Tauri 2.x（Rust 后端 + WebView2 前端） |
| 前端 | Vanilla TypeScript + Vite |
| 全局快捷键 | `tauri-plugin-global-shortcut`（可视化录制，动态换绑） |
| 毛玻璃 / 圆角 | SWCA Acrylic（透明度实时可调，Win11 22H2+ 唯一生效路径）+ DWM 圆角 |
| 应用枚举 | 开始菜单 .lnk + App Paths（`winreg`）+ `shell:AppsFolder`（`windows` 0.61 COM） |
| 拼音搜索 | `pinyin`（多音字 heteronym 展开）+ `fuzzy-matcher` |
| 全盘搜索 | Windows Search Index（`windows` 0.61 OLE DB 直连）+ `fuzzy-matcher` 兜底 |
| 图标提取 | `SHGetFileInfoW` / `IShellItemImageFactory` → `png` → base64 |
| 设置 | `settings.json` 持久化 + `rfd` 文件对话框 + `tauri-plugin-autostart` |
| 计算求值 | `fasteval`（f64 语义） |
| 剪贴板 / 启动 | `arboard`（复制）/ `explorer.exe` 中转（进程与 stdio 完全脱钩） |

## 目录结构

```
itools/
├── index.html / vite.config.ts / tsconfig.json
├── src/                      # 前端
│   ├── main.ts               # 输入/防抖/invoke/键盘导航/动态高度
│   ├── styles.css            # macOS 风格样式
│   └── types.ts              # SearchItem 类型
└── src-tauri/
    ├── tauri.conf.json       # 无边框/透明/居中/置顶/隐藏启动
    ├── capabilities/         # 权限声明
    └── src/
        ├── main.rs / lib.rs  # 启动、插件、托盘、快捷键（含回退链）
        ├── window.rs         # 显隐/居中/毛玻璃/圆角
        ├── commands.rs       # search / execute / load_icons 命令
        └── search/           # 搜索管线
            ├── mod.rs        # 融合排序：命令 → 应用 → 文件
            ├── apps.rs       # 开始菜单应用索引
            ├── builtins.rs   # 内置即时小工具
            ├── winsearch.rs  # Windows Search Index 全盘秒搜（MTA worker）
            ├── files.rs      # walkdir 兜底索引
            └── icon.rs       # 系统图标提取转 PNG
```

## 开发运行

```bash
npm install
npm run tauri dev
```

## 打包

```bash
npm run tauri build      # 生成 Windows 安装包（.msi / NSIS）
```

## 交互速查

| 操作 | 快捷键 |
|---|---|
| 唤起 / 收起 | `Alt + Space`（占用则回退 `Alt + Shift + Space` / `Ctrl + Alt + Space`） |
| 上 / 下选择 | `↑` / `↓` |
| 执行选中项 | `Enter` |
| 快速执行第 N 项 | `Ctrl + 1..9` |
| 隐藏 | `Esc` / 点击窗口外 |

## 路线图（后续）

- 剪贴板历史
- 更大尺寸图标（SHIL_JUMBO / GetImage 更大尺寸）以适配 HiDPI
- 插件系统：配置驱动的轻量插件 → 可加载第三方 HTML 插件
- 打包发布（`npm run tauri build` 出 .msi / NSIS 安装包）
