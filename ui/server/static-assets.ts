/** W21-R02: standalone previews serve the same immutable bytes with negotiated encodings. */
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";

type Encoding = "br" | "gzip" | "identity";
interface Variant {
  file: string;
  bytes: number;
  sha256: string;
}
interface Asset {
  type: string;
  bytes: number;
  sha256: string;
  gzip?: Variant;
  br?: Variant;
}
export interface StaticFile {
  body: BodyInit;
  bytes: number;
  type?: string;
}
export type OpenStaticFile = (url: URL) => Promise<StaticFile | null>;
const hash = (bytes: Uint8Array) => createHash("sha256").update(bytes).digest("hex");
const isDigest = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);

/** Explicit refusals beat wildcards. Identity is acceptable unless explicitly refused. */
export function encodingQuality(header: string | null, encoding: Encoding): number {
  if (!header) return encoding === "identity" ? 1 : 0;
  const values = new Map<string, number>();
  for (const part of header.split(",")) {
    const [token, ...parameters] = part.trim().toLowerCase().split(";");
    if (!token) continue;
    let q = 1;
    for (const parameter of parameters) {
      const match = /^\s*q\s*=\s*(.*?)\s*$/.exec(parameter);
      if (match) q = /^(?:0(?:\.\d{0,3})?|1(?:\.0{0,3})?)$/.test(match[1] ?? "") ? Number(match[1]) : 0;
    }
    values.set(token, Math.max(values.get(token) ?? 0, q));
  }
  const explicit = values.get(encoding);
  if (explicit !== undefined) return explicit;
  if (encoding === "identity") return values.get("*") === 0 ? 0 : 1;
  return values.get("*") ?? 0;
}

export function chooseEncoding(header: string | null, asset: Pick<Asset, "gzip" | "br">): Encoding | null {
  let chosen: Encoding | null = null;
  let quality = 0;
  for (const encoding of ["br", "gzip", "identity"] as const) {
    if (encoding !== "identity" && !asset[encoding]) continue;
    const q = encodingQuality(header, encoding);
    if (q > quality) {
      quality = q;
      chosen = encoding;
    }
  }
  return chosen;
}

async function nodeFile(url: URL): Promise<StaticFile | null> {
  try {
    const bytes = await readFile(url);
    return { body: new Blob([new Uint8Array(bytes)]), bytes: bytes.length };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT" || (error as NodeJS.ErrnoException).code === "EISDIR") return null;
    throw error;
  }
}

/** Verify the immutable package at startup, before accepting requests; never compress on demand. */
export async function staticAssets(client: URL, encoded: URL, open: OpenStaticFile = nodeFile) {
  const assets = new Map<string, Asset>();
  let manifest: { version?: unknown; assets?: Record<string, Asset> } | null = null;
  try {
    const bytes = await readFile(new URL("manifest.json", encoded));
    if (bytes.length > 4 * 1024 * 1024) throw new Error("UI asset manifest exceeds its size budget");
    manifest = JSON.parse(bytes.toString());
  } catch (error) {
    // Older carried bundles did not package compressed files. Identity remains available.
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
  if (manifest) {
    if (manifest.version !== 1 || !manifest.assets || typeof manifest.assets !== "object" || Array.isArray(manifest.assets) || Object.keys(manifest.assets).length > 4096)
      throw new Error("Unsupported UI asset manifest");
    for (const [path, asset] of Object.entries(manifest.assets)) {
      if (
        !/^\/assets\/[A-Za-z0-9_-][A-Za-z0-9_.-]*$/.test(path) ||
        !asset ||
        !isDigest(asset.sha256) ||
        !Number.isSafeInteger(asset.bytes) ||
        asset.bytes < 0 ||
        typeof asset.type !== "string" ||
        /[\r\n]/.test(asset.type)
      )
        throw new Error("Invalid UI asset entry");
      const original = await readFile(new URL(path.slice(1), client));
      if (original.length !== asset.bytes || hash(original) !== asset.sha256) throw new Error("UI asset manifest does not match " + path);
      for (const encoding of ["br", "gzip"] as const) {
        const variant = asset[encoding];
        if (!variant) continue;
        if (!/^[A-Za-z0-9_-][A-Za-z0-9_.-]*$/.test(variant.file) || !isDigest(variant.sha256) || !Number.isSafeInteger(variant.bytes) || variant.bytes < 0) throw new Error("Invalid encoded UI asset");
        const body = await readFile(new URL(variant.file, encoded));
        if (body.length !== variant.bytes || hash(body) !== variant.sha256) throw new Error("Encoded UI asset manifest does not match " + path);
      }
      assets.set(path, asset);
    }
  }
  return async (req: Request): Promise<Response | null> => {
    if (req.method !== "GET" && req.method !== "HEAD") return null;
    const path = new URL(req.url).pathname;
    if (path === "/" || /(?:\.\.|%|:|\\|\/\/)/.test(path) || path.startsWith("/_")) return null;
    const asset = assets.get(path);
    const immutable = path.startsWith("/assets/");
    const headers = new Headers({ "cache-control": immutable ? "public, max-age=31536000, immutable" : "no-cache", "x-content-type-options": "nosniff" });
    let selected = new URL(path.slice(1), client);
    if (asset) {
      headers.set("vary", "Accept-Encoding");
      headers.set("content-type", asset.type);
      const accepted = req.headers.get("accept-encoding");
      const encoding = req.headers.has("range") && encodingQuality(accepted, "identity") > 0 ? "identity" : chooseEncoding(accepted, asset);
      if (!encoding) return new Response(null, { status: 406, headers: { vary: "Accept-Encoding", "cache-control": "no-store" } });
      const variant = encoding === "identity" ? null : asset[encoding];
      const digest = variant?.sha256 ?? asset.sha256;
      const etag = `"${digest}-${encoding}"`;
      headers.set("etag", etag);
      if (variant) {
        selected = new URL(variant.file, encoded);
        headers.set("content-encoding", encoding);
        headers.set("accept-ranges", "none");
      }
      const condition = req.headers.get("if-none-match");
      if (condition?.split(",").some((value) => value.trim() === "*" || value.trim().replace(/^W\//, "") === etag)) return new Response(null, { status: 304, headers });
    }
    const file = await open(selected);
    if (!file) return immutable ? new Response("No such UI asset", { status: 404, headers: { "cache-control": "no-store" } }) : null;
    if (!asset && encodingQuality(req.headers.get("accept-encoding"), "identity") === 0) return new Response(null, { status: 406, headers: { vary: "Accept-Encoding", "cache-control": "no-store" } });
    if (!headers.has("content-type") && file.type) headers.set("content-type", file.type);
    headers.set("content-length", String(file.bytes));
    return new Response(req.method === "HEAD" ? null : file.body, { headers });
  };
}
