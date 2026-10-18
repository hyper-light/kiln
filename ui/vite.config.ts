import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import rsc from "@vitejs/plugin-rsc";
import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import { fileURLToPath, URL } from "node:url";

// The kiln server the dev server sends /api to.
const target = process.env.KILN_URL ?? "http://127.0.0.1:7070";

/**
 * React's Flight client (what turns a server component's stream into elements) loads when a page
 * first shows a server component, not with every page: Start imports it up front in the browser.
 */
function flightOnDemand(): Plugin {
  const id = "virtual:tanstack-rsc-browser-decode";
  const resolved = "\0kiln:flight-on-demand";
  return {
    name: "kiln:flight-on-demand",
    enforce: "pre",
    applyToEnvironment: (env) => env.config.consumer === "client",
    resolveId: (source) => (source === id ? resolved : undefined),
    load: (loading) =>
      loading === resolved
        ? `const flight = () => import("@vitejs/plugin-rsc/browser");
export const createFromReadableStream = (stream, options) => flight().then((m) => m.createFromReadableStream(stream, options));
export const createFromFetch = (response, options) => flight().then((m) => m.createFromFetch(response, options));`
        : undefined,
  };
}

export default defineConfig({
  // TanStack Start renders pages and server components (React Server Components); React's plugin
  // goes after it.
  plugins: [flightOnDemand(), tailwindcss(), tanstackStart({ rsc: { enabled: true }, spa: { enabled: true, prerender: { outputPath: "/_shell.html" } } }), rsc(), react()],
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
