import { defineConfig } from "vite";

// Tauri 期望前端资源从固定端口提供，且构建产物输出到 dist
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // 忽略 Rust 侧，避免前端 dev-server 无谓重载
      ignored: ["**/src-tauri/**"],
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
      },
    },
  },
});
