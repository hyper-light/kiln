/**
 * A person's verdicts and comments, signed by them in this browser (docs/ci.md W27-R04). The
 * browser makes a key of its own the first time a person gives a verdict or comment here, in that
 * click (WebCrypto's ECDSA P-256, never extractable), and keeps it in IndexedDB. kiln certifies it
 * as the person signed in, from a CSR (`POST /api/me/identity`: never the key; never for an agent, a
 * job or a peer), and again once half its certificate's life has gone. For each verdict or comment
 * kiln says what to sign (`POST /api/diffs/:id/statement`), and the signature goes with it, a
 * Sigstore bundle. Where kiln doesn't require it and this browser can't sign (a page that isn't
 * secure), it goes unsigned.
 */
import { ApiError, request } from "@/api/client";

/** What kiln asks a giver to sign. */
interface ToSign {
  payload_type: string;
  statement: { body?: { by?: string } };
  /** The statement's bytes, base64. */
  payload: string;
  /** The bytes signed (DSSE's PAE), base64. */
  pae: string;
  required: boolean;
}

/** This browser's identity, as kept: its keys, whom kiln certified them as, and the certificate. */
interface Kept {
  keys: CryptoKeyPair;
  uri: string;
  cert: string;
  notBefore: number;
  notAfter: number;
}

const DB = "kiln-identity";
const STORE = "device";
const BUNDLE = "application/vnd.dev.sigstore.bundle.v0.3+json";

/** One request of the identity store: its answer. */
function idb<T>(mode: IDBTransactionMode, run: (s: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const open = indexedDB.open(DB, 1);
    open.onupgradeneeded = () => open.result.createObjectStore(STORE);
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const tx = open.result.transaction(STORE, mode);
      const r = run(tx.objectStore(STORE));
      r.onsuccess = () => resolve(r.result);
      r.onerror = () => reject(r.error);
      tx.oncomplete = () => open.result.close();
    };
  });
}

const bytes = (...b: number[]) => new Uint8Array(b);

/** A DER value: `tag`, its length, then `parts`. */
function der(tag: number, ...parts: Uint8Array[]): Uint8Array<ArrayBuffer> {
  const len = parts.reduce((n, p) => n + p.length, 0);
  const head = len < 0x80 ? [len] : len < 0x100 ? [0x81, len] : [0x82, len >> 8, len & 0xff];
  const out = new Uint8Array(1 + head.length + len);
  out[0] = tag;
  out.set(head, 1);
  let at = 1 + head.length;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** ecdsa-with-SHA256 (1.2.840.10045.4.3.2) and commonName (2.5.4.3). */
const ECDSA_SHA256 = bytes(0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02);
const CN = bytes(0x06, 0x03, 0x55, 0x04, 0x03);

/** An ECDSA signature as WebCrypto gives it (r and s, 32 bytes each) in DER, as X.509 and kiln take it. */
export function sigDer(raw: Uint8Array): Uint8Array {
  const int = (x: Uint8Array) => {
    let i = 0;
    while (i < x.length - 1 && x[i] === 0) i++;
    const v = x.slice(i);
    return (v[0] ?? 0) & 0x80 ? der(0x02, bytes(0), v) : der(0x02, v);
  };
  return der(0x30, int(raw.slice(0, 32)), int(raw.slice(32)));
}

const b64 = (b: Uint8Array) => {
  let s = "";
  for (const x of b) s += String.fromCharCode(x);
  return btoa(s);
};
const unb64 = (s: string) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const pem = (label: string, b: Uint8Array) => `-----BEGIN ${label}-----\n${(b64(b).match(/.{1,64}/g) ?? []).join("\n")}\n-----END ${label}-----\n`;
const derOf = (p: string) => unb64(p.replace(/-----[^-]+-----/g, "").replace(/\s+/g, ""));

const ECDSA = { name: "ECDSA", hash: "SHA-256" };

/** A PKCS #10 request for `keys`, signed by them: what kiln certifies a key from. */
export async function csrFor(keys: CryptoKeyPair): Promise<string> {
  const spki = new Uint8Array(await crypto.subtle.exportKey("spki", keys.publicKey));
  const name = der(0x30, der(0x31, der(0x30, CN, der(0x0c, new TextEncoder().encode("kiln-identity")))));
  const info = der(0x30, bytes(0x02, 0x01, 0x00), name, spki, bytes(0xa0, 0x00));
  const sig = new Uint8Array(await crypto.subtle.sign(ECDSA, keys.privateKey, info));
  return pem("CERTIFICATE REQUEST", der(0x30, info, der(0x30, ECDSA_SHA256), der(0x03, bytes(0), sigDer(sig))));
}

const fresh = () => crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);

const certify = async (keys: CryptoKeyPair) => request<{ uri: string; cert_pem: string; not_before: number; not_after: number }>("POST", "/api/me/identity", { csr: await csrFor(keys) });

/** This browser's identity as `by`: kept, or certified again (once half its life has gone, or for
 * someone else signed in here now), with a new key where kiln won't certify the one kept. */
async function identity(by: string): Promise<Kept> {
  const now = Date.now() / 1000;
  const kept = await idb<Kept | undefined>("readonly", (s) => s.get("me") as IDBRequest<Kept | undefined>).catch(() => undefined);
  if (kept && kept.uri === by && now < kept.notBefore + (kept.notAfter - kept.notBefore) / 2) return kept;
  let keys = kept?.keys ?? (await fresh());
  let issued;
  try {
    issued = await certify(keys);
  } catch (e) {
    // Revoked, or certifying someone else: a new key.
    if (!(e instanceof ApiError) || e.status !== 409 || !kept) throw e;
    keys = await fresh();
    issued = await certify(keys);
  }
  const k: Kept = { keys, uri: issued.uri, cert: issued.cert_pem, notBefore: issued.not_before, notAfter: issued.not_after };
  await idb("readwrite", (s) => s.put(k, "me"));
  return k;
}

/** What `ask` gives, signed in this browser, kiln saying what to sign at `statement` (a diff's
 * `/statement`, for a verdict or comment, or a comment's, for its new words): the bundle that goes
 * with it. None where kiln doesn't require it and this browser can't sign, or kiln is from before
 * signatures. */
export async function signed(statement: string, ask: Record<string, unknown>): Promise<object | undefined> {
  let toSign: ToSign;
  try {
    toSign = await request<ToSign>("POST", statement, ask);
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405)) return undefined;
    throw e;
  }
  if (!globalThis.crypto?.subtle) {
    if (toSign.required) throw new Error("This page isn't secure (https), so this browser can't sign, and its repository requires it.");
    return undefined;
  }
  const me = await identity(toSign.statement.body?.by ?? "");
  const raw = new Uint8Array(await crypto.subtle.sign(ECDSA, me.keys.privateKey, unb64(toSign.pae)));
  return {
    mediaType: BUNDLE,
    verificationMaterial: { certificate: { rawBytes: b64(derOf(me.cert)) } },
    dsseEnvelope: { payloadType: toSign.payload_type, payload: toSign.payload, signatures: [{ sig: b64(sigDer(raw)) }] },
  };
}

/** Whether this browser signs as its person: WebCrypto and IndexedDB, which a page that isn't secure (https) lacks. */
export const browserSigns = () => typeof globalThis.crypto?.subtle !== "undefined" && typeof indexedDB !== "undefined";
