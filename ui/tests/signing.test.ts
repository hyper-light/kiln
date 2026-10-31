import assert from "node:assert/strict";
import { createPublicKey, verify } from "node:crypto";
import { test } from "node:test";
import { csrFor, sigDer } from "../src/lib/signing.ts";

/** One DER value at `at`: its tag, and where its content starts and ends. */
function tlv(b: Uint8Array, at: number) {
  const tag = b[at] ?? 0;
  let len = b[at + 1] ?? 0;
  let start = at + 2;
  if (len & 0x80) {
    const n = len & 0x7f;
    len = 0;
    for (let i = 0; i < n; i++) len = len * 256 + (b[at + 2 + i] ?? 0);
    start += n;
  }
  return { tag, start, end: start + len };
}

const ECDSA = { name: "ECDSA", namedCurve: "P-256" };

async function keysOf() {
  const keys = await crypto.subtle.generateKey(ECDSA, false, ["sign", "verify"]);
  const key = createPublicKey({ key: Buffer.from(await crypto.subtle.exportKey("spki", keys.publicKey)), format: "der", type: "spki" });
  return { keys, key };
}

test("a CSR this browser makes for its key is signed by that key, as kiln checks it before certifying (W27-R04)", async () => {
  const { keys, key } = await keysOf();
  const pem = await csrFor(keys);
  assert.match(pem, /^-----BEGIN CERTIFICATE REQUEST-----\n[A-Za-z0-9+/=\n]+\n-----END CERTIFICATE REQUEST-----\n$/);
  const der = Buffer.from(pem.replace(/-----[^-]+-----/g, "").replace(/\s+/g, ""), "base64");
  const outer = tlv(der, 0);
  const info = tlv(der, outer.start);
  const algorithm = tlv(der, info.end);
  const signature = tlv(der, algorithm.end);
  assert.deepEqual([outer.tag, info.tag, algorithm.tag, signature.tag, signature.end], [0x30, 0x30, 0x30, 0x03, der.length]);
  // Past the bit string's count of unused bits: the signature over the request's info.
  assert.ok(verify("sha256", der.subarray(outer.start, info.end), { key, dsaEncoding: "der" }, der.subarray(signature.start + 1, signature.end)));
});

test("a signature goes in DER, as X.509 and kiln take ECDSA's, whatever its integers' leading bytes", async () => {
  const { keys, key } = await keysOf();
  const message = new TextEncoder().encode("DSSEv1 39 application/vnd.kiln.review.verdict+json 2 {}");
  // Enough signatures that some r or s has its high bit set, or a leading zero.
  for (let i = 0; i < 32; i++) {
    const raw = new Uint8Array(await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, keys.privateKey, message));
    assert.ok(verify("sha256", message, { key, dsaEncoding: "der" }, sigDer(raw)), `signature ${i}`);
  }
});
