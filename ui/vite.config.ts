import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

const target = process.env.KILN_URL ?? "http://127.0.0.1:7070";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  server: {
    proxy: {
      "/api": { target, changeOrigin: true, ws: true },
      "/metrics": { target, changeOrigin: true },
      "/healthz": { target, changeOrigin: true },
      "/readyz": { target, changeOrigin: true },
    },
  },
  preview: {
    proxy: {
      "/api": { target, changeOrigin: true, ws: true },
      "/metrics": { target, changeOrigin: true },
      "/healthz": { target, changeOrigin: true },
    },
  },
  build: {
    target: "es2022",
    chunkSizeWarningLimit: 600,
    sourcemap: false,
  },
});
