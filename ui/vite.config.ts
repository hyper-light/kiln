import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import rsc from "@vitejs/plugin-rsc";
import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import { fileURLToPath, URL } from "node:url";

// The kiln server the dev server sends /api to.
const target = process.env.KILN_URL ?? "http://127.0.0.1:7070";

export default defineConfig({
  // TanStack Start renders pages and server components (React Server Components); React's plugin
  // goes after it.
  plugins: [tailwindcss(), tanstackStart({ rsc: { enabled: true }, spa: { enabled: true, prerender: { outputPath: "/_shell.html" } } }), rsc(), react()],
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
  build: {
    target: "es2022",
    chunkSizeWarningLimit: 600,
    sourcemap: false,
  },
});
