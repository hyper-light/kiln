import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { brotliDecompressSync, gunzipSync } from "node:zlib";
import { test } from "node:test";
import { compressAssets } from "../scripts/compress-assets.mjs";
import { chooseEncoding, staticAssets } from "../server/static-assets";

const raw = Buffer.from("export const message = 'A full immutable script, with its original bytes preserved.';\n".repeat(80));

async function fixture() {
  const root = await mkdtemp(join(homedir(), ".cache", "kiln-static-assets-"));
  const client = join(root, "client");
  const encoded = join(root, "encoded");
  await mkdir(join(client, "assets"), { recursive: true });
  await writeFile(join(client, "assets", "app-abc123.js"), raw);
  await writeFile(join(client, "assets", "small-abc123.css"), "body{color:blue}");
  const manifest = await compressAssets(client, encoded);
  const clientUrl = pathToFileURL(client + "/");
  const encodedUrl = pathToFileURL(encoded + "/");
  return { root, client, encoded, manifest, clientUrl, encodedUrl, cleanup: () => rm(root, { recursive: true, force: true }) };
}

test("W21-R02 encoding negotiation respects refusals, qualities, wildcard, and identity", () => {
  const variants = { br: { file: "a.br", bytes: 1, sha256: "a" }, gzip: { file: "a.gz", bytes: 1, sha256: "a" } };
  for (const [header, expected] of [
    [null, "identity"],
    ["gzip, br", "br"],
    ["br;q=0, gzip", "gzip"],
    ["gzip;q=0.5, identity;q=0.1", "gzip"],
    ["gzip;q=0.5", "identity"],
    ["*;q=1, br;q=0", "gzip"],
    ["*;q=0", null],
    ["br;q=0, gzip;q=0, identity;q=0", null],
    ["br;q=invalid, gzip", "gzip"],
  ] as const)
    assert.equal(chooseEncoding(header, variants), expected, String(header));
});

test("W21-R02 build encodings decode to exact source bytes; GET, HEAD and validators select the same representation", async () => {
  const f = await fixture();
  try {
    const serve = await staticAssets(f.clientUrl, f.encodedUrl);
    for (const encoding of ["identity", "gzip", "br"] as const) {
      const headers = { "accept-encoding": encoding };
      const response = await serve(new Request("http://preview/assets/app-abc123.js", { headers }));
      assert.ok(response);
      assert.equal(response.status, 200);
      assert.equal(response.headers.get("content-type"), "text/javascript; charset=utf-8");
      assert.equal(response.headers.get("vary"), "Accept-Encoding");
      assert.equal(response.headers.get("cache-control"), "public, max-age=31536000, immutable");
      assert.equal(response.headers.get("content-encoding"), encoding === "identity" ? null : encoding);
      const bytes = Buffer.from(await response.arrayBuffer());
      assert.deepEqual(encoding === "gzip" ? gunzipSync(bytes) : encoding === "br" ? brotliDecompressSync(bytes) : bytes, raw);
      const head = await serve(new Request("http://preview/assets/app-abc123.js", { method: "HEAD", headers }));
      assert.ok(head);
      assert.equal(head.headers.get("content-length"), String(bytes.length));
      assert.equal((await head.arrayBuffer()).byteLength, 0);
      assert.equal(head.headers.get("etag"), response.headers.get("etag"));
      const unchanged = await serve(new Request("http://preview/assets/app-abc123.js", { headers: { ...headers, "if-none-match": "W/" + response.headers.get("etag") } }));
      assert.equal(unchanged?.status, 304);
      assert.equal((await unchanged?.arrayBuffer())?.byteLength, 0);
    }
    const identity = await serve(new Request("http://preview/assets/app-abc123.js"));
    const otherEncoding = await serve(new Request("http://preview/assets/app-abc123.js", { headers: { "accept-encoding": "gzip", "if-none-match": identity?.headers.get("etag") ?? "" } }));
    assert.equal(otherEncoding?.status, 200);
    const refused = await serve(new Request("http://preview/assets/app-abc123.js", { headers: { "accept-encoding": "*;q=0" } }));
    assert.equal(refused?.status, 406);
    assert.equal(refused?.headers.get("cache-control"), "no-store");
    assert.equal((await serve(new Request("http://preview/assets/missing.js")))?.status, 404);
    assert.equal(await serve(new Request("http://preview/api/me")), null);
    assert.equal(await serve(new Request("http://preview/_shell.html")), null);
    assert.equal(await serve(new Request("http://preview/assets/%2fsecret.js")), null);
    assert.equal(await serve(new Request("http://preview/assets/app-abc123.js", { method: "POST" })), null);
  } finally {
    await f.cleanup();
  }
});

test("W21-R02 stale or damaged compressed artifacts fail before the preview accepts requests", async () => {
  const f = await fixture();
  try {
    const variant = f.manifest.assets["/assets/app-abc123.js"].gzip;
    assert.ok(variant);
    const path = join(f.encoded, variant.file);
    const original = await readFile(path);
    await writeFile(path, "damaged encoding");
    await assert.rejects(staticAssets(f.clientUrl, f.encodedUrl), /Encoded UI asset manifest/);
    await writeFile(path, original);
    await writeFile(join(f.client, "assets", "app-abc123.js"), "changed source");
    await assert.rejects(staticAssets(f.clientUrl, f.encodedUrl), /UI asset manifest does not match/);
  } finally {
    await f.cleanup();
  }
});

test("W21-R02 older uncompressed packages remain usable without a sidecar manifest", async () => {
  const f = await fixture();
  try {
    const serve = await staticAssets(f.clientUrl, new URL("missing/", f.encodedUrl));
    const response = await serve(new Request("http://preview/assets/app-abc123.js", { headers: { "accept-encoding": "gzip" } }));
    assert.equal(response?.headers.get("content-encoding"), null);
    assert.deepEqual(Buffer.from((await response?.arrayBuffer()) ?? []), raw);
  } finally {
    await f.cleanup();
  }
});
