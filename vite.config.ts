import { defineConfig } from "vite";

// Tauri 期望前端资源从固定端口提供，且构建产物输出到 dist
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5180,
    strictPort: true,
    watch: {
      // dev-server 只该盯着**前端源码**。下面这些目录都不是前端源码，却含有
      // index.html / tsconfig.json 之类会被 vite 认作「源码变了」的文件：
      //
      // - src-tauri/**：Rust 侧与其 target 产物
      // - plugins/**、dev-plugins/**：插件目录（每个插件都有自己的 index.html）
      //
      // 其中 `plugins/.staging/**` 是**必须**排除的：插件安装会把下载的包解压到那里，
      // vite 一看到新出现的 index.html / tsconfig.json 就强制整页 reload，
      // 而管理中心的「安装」弹窗是页面里的 DOM——页面一重载弹窗当场消失，
      // 用户永远点不到「安装」按钮，表现为「点了没反应、插件装不上」。
      // 这个坑排查了很久：后端日志显示预览成功、哈希也校验通过，问题却出在 dev-server。
      ignored: ["**/src-tauri/**", "**/plugins/**", "**/dev-plugins/**"],
    },
  },
  build: {
    // WebView2 (Chromium) 支持较新特性，无需过度降级
    target: "esnext",
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: "index.html",
        admin: "admin.html",
        // 更新窗口是独立窗口（tauri.conf.json 里 label=update），需要自己的入口，
        // 漏了它打包后窗口会白屏
        update: "update.html",
      },
    },
  },
});
