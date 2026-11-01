import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 本番は Rust のサーバが dist を配信する。開発時だけ /api を検証用サーバへ中継する。
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8766",
    },
  },
});
