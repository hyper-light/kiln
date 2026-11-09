import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import rsc from "@vitejs/plugin-rsc";
import tailwindcss from "@tailwindcss/vite";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import { fileURLToPath, URL } from "node:url";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join as joinPath, resolve as resolvePath } from "node:path";
import { gunzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { entryPreloads } from "./scripts/entry-preloads";

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

/**
 * The grammars go out as plain WebAssembly, which kiln sends Brotli-encoded (packed at build
 * time, crates/kiln/build.rs) and the browser decodes itself, rather than the gzip files kept in
 * the source, inflated in the page (goal 8, plan 01a1a832ebfd step 3): Brotli is a third smaller.
 * Built only; the dev server serves the gzip files, which lib/syntax/browser.ts inflates.
 */
function plainGrammars(worker = false): Plugin {
  const cache = fileURLToPath(new URL("./node_modules/.cache/kiln-grammars/", import.meta.url));
  return {
    name: "kiln:plain-grammars",
    enforce: "pre",
    apply: "build",
    applyToEnvironment: worker ? undefined : (env) => env.config.consumer === "client",
    resolveId(source, importer) {
      const m = /^(.*\/grammars\/([\w-]+)\.wasm\.gz)\?url&no-inline$/.exec(source);
      if (!m || !importer) return undefined;
      const gz = resolvePath(dirname(importer.split("?")[0]), m[1]);
      const wasm = joinPath(cache, `${m[2]}.wasm`);
      const plain = gunzipSync(readFileSync(gz));
      if (!existsSync(wasm) || !readFileSync(wasm).equals(plain)) {
        mkdirSync(cache, { recursive: true });
        writeFileSync(wasm, plain);
      }
      return `${wasm}?url&no-inline`;
    },
  };
}

/**
 * What each client chunk is, for scripts/stable-names.mjs (goal 8, plan 01a1a832ebfd step 4): its
 * name and a digest of the modules in it (`.kiln-chunks.json`, read and removed after the build),
 * so a chunk keeps the same key from build to build however its content or its imports' change.
 */
function chunkKeys(): Plugin {
  return {
    name: "kiln:chunk-keys",
    apply: "build",
    applyToEnvironment: (env) => env.config.consumer === "client",
    generateBundle: {
      order: "post",
      handler(_options, bundle) {
        const root = fileURLToPath(new URL(".", import.meta.url));
        const chunks: Record<string, { name: string; modules: string }> = {};
        for (const out of Object.values(bundle)) {
          if (out.type !== "chunk") continue;
          const ids = (out.facadeModuleId ? [out.facadeModuleId] : [...out.moduleIds]).map((id) => id.replace(root, "").replace(/\0/g, "")).sort();
          chunks[out.fileName] = { name: out.name, modules: createHash("sha256").update(ids.join("\n")).digest("base64url").slice(0, 6) };
        }
        this.emitFile({ type: "asset", fileName: ".kiln-chunks.json", source: JSON.stringify(chunks) });
      },
    },
  };
}

/** W21-R02: reuse the shared-icon grouping from 946890305b8 in this build's module graph. */
function iconsTogether(): Plugin {
  return {
    name: "kiln:icons-together",
    apply: "build",
    applyToEnvironment: (env) => env.config.consumer === "client",
    outputOptions(options) {
      if (options.codeSplitting === false) return options;
      const splitting = typeof options.codeSplitting === "object" ? options.codeSplitting : {};
      const sanitize = options.sanitizeFileName;
      return {
        ...options,
        // W21-R02: retained assets must remain loadable by an already-open client after a
        // later deploy. The native retention lookup accepts plain ASCII names, not Rolldown's ~.
        sanitizeFileName: (name) => (typeof sanitize === "function" ? sanitize(name) : name).replace(/[^A-Za-z0-9_./-]/g, "_"),
        codeSplitting: {
          ...splitting,
          groups: [
            ...(splitting.groups ?? []),
            { name: "icons", test: /node_modules[\\/]lucide-react[\\/]/, entriesAware: true, minShareCount: 2, entriesAwareMergeThreshold: 8 * 1024, includeDependenciesRecursively: false },
          ],
        },
      };
    },
  };
}

export default defineConfig({
  // TanStack Start renders pages and server components (React Server Components); React's plugin
  // goes after it.
  plugins: [
    flightOnDemand(),
    plainGrammars(),
    chunkKeys(),
    iconsTogether(),
    entryPreloads(),
    tailwindcss(),
    tanstackStart({ rsc: { enabled: true }, spa: { enabled: true, prerender: { outputPath: "/_shell.html" } } }),
    rsc(),
    react(),
  ],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  // W26-R15: workers have no document import map. Keep their entire ES graph in a separate
  // flat namespace, outside chunkKeys/stable-names, including identical raw-query chunks.
  // Flat ASCII names also fit the native retained-assets and precompression contracts.
  worker: {
    format: "es",
    plugins: () => [plainGrammars(true)],
    rollupOptions: {
      output: {
        entryFileNames: "assets/syntax-worker-[name]-[hash].js",
        chunkFileNames: "assets/syntax-worker-[name]-[hash].js",
        assetFileNames: "assets/syntax-worker-[name]-[hash][extname]",
        sanitizeFileName: (name) => name.replace(/[^A-Za-z0-9_./-]/g, "_"),
      },
    },
  },
  // W21-R02: the carried Bun app has no node_modules or package-install step. RSC is a
  // separate server environment; the ordinary SSR setting alone does not bundle its imports.
  ssr: { noExternal: true },
  environments: { rsc: { resolve: { noExternal: true } } },
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
    // Source maps only to attribute bytes to modules (ci/ui/attribute.mjs, goal 8 step 6).
    sourcemap: process.env.KILN_UI_SOURCEMAP === "1" ? "hidden" : false,
  },
});
