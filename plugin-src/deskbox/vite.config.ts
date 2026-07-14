import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";

// 构建产物直接输出到插件加载目录 plugins/deskbox。
// - base './'：插件页在 itplugin://<id>/ 下加载，assets 必须走相对路径（CSP 'self' 放行，绝不走 CDN）。
// - emptyOutDir false：保留插件目录里的 plugin.json / logo.png / README.md，只覆盖 index.html 与 assets。
// - 固定 bundle 文件名（不带 hash）：产物提交进 git，避免每次构建文件名变动造成噪音。
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    outDir: resolve(__dirname, "../../plugins/deskbox"),
    emptyOutDir: false,
    target: "esnext",
    rollupOptions: {
      output: {
        entryFileNames: "assets/deskbox.js",
        chunkFileNames: "assets/deskbox-[name].js",
        assetFileNames: "assets/deskbox.[ext]",
      },
    },
  },
});
