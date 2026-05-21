import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// 构建产物输出到仓库根目录的 static/dist，由 Rust 后端的 axum 服务托管。
// 开发模式下 vite 跑在 5173，把 WebSocket /ws 代理到 Rust 后端 (默认 127.0.0.1:3000)。
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: "../static/dist",
    emptyOutDir: true,
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      output: {
        manualChunks: {
          "react-vendor": ["react", "react-dom"],
          "markdown": ["react-markdown", "remark-gfm"],
          "highlight": ["react-syntax-highlighter"],
        },
      },
    },
  },
  server: {
    port: 5300,
    proxy: {
      "/ws": {
        target: "ws://127.0.0.1:3000",
        ws: true,
        changeOrigin: true,
      },
      "/api": {
        target: "http://127.0.0.1:3000",
        changeOrigin: true,
        cookieDomainRewrite: "",
      },
    },
  },
});
