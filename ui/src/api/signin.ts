/**
 * Signing in (docs/ci.md W10, W23-R08): passkeys made in this browser and signed in with, one's
 * own password, and the hosts one may sign in with. The server's WebAuthn options and the
 * browser's answers travel as JSON, their binary parts base64url.
 */
import { request, setAuthed } from "@/api/client";

export type Passkey = { id: string; name: string; rp_id: string; created_at: number; last_used_at: number | null; backed_up: boolean };
export type SsoProvider = { provider: string; name: string; start: string };

const fromB64 = (s: string): ArrayBuffer => {
  const bin = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out.buffer;
};

const toB64 = (buf: ArrayBuffer): string => {
  let s = "";
  for (const x of new Uint8Array(buf)) s += String.fromCharCode(x);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
};

/** Whether this browser makes and uses passkeys here: WebAuthn, on a secure page. */
export const passkeysHere = () => typeof window !== "undefined" && window.isSecureContext && "PublicKeyCredential" in window;

type CreateOptions = {
  challenge: string;
  rp: PublicKeyCredentialRpEntity;
  user: { id: string; name: string; displayName: string };
  pubKeyCredParams: PublicKeyCredentialParameters[];
  timeout: number;
  attestation: AttestationConveyancePreference;
  authenticatorSelection: AuthenticatorSelectionCriteria;
  excludeCredentials: { type: "public-key"; id: string }[];
};

type GetOptions = { challenge: string; rpId: string; timeout: number; userVerification: UserVerificationRequirement };

export const signin = {
  /** The hosts this server signs people in with. */
  sso: (signal?: AbortSignal) => request<{ providers: SsoProvider[] }>("GET", "/api/auth/sso", undefined, { signal }).then((r) => r.providers),

  passkeys: (signal?: AbortSignal) => request<{ passkeys: Passkey[] }>("GET", "/api/me/passkeys", undefined, { signal }).then((r) => r.passkeys),
  /** Makes a passkey with this browser's authenticator (which asks its person), kept as `name`. */
  addPasskey: async (name: string) => {
    const o = await request<CreateOptions>("POST", "/api/me/passkeys/options", {});
    const made = (await navigator.credentials.create({
      publicKey: { ...o, challenge: fromB64(o.challenge), user: { ...o.user, id: fromB64(o.user.id) }, excludeCredentials: o.excludeCredentials.map((c) => ({ ...c, id: fromB64(c.id) })) },
    })) as PublicKeyCredential | null;
    if (!made) throw new Error("No passkey was made.");
    const r = made.response as AuthenticatorAttestationResponse;
    return request<Passkey>("POST", "/api/me/passkeys", {
      name,
      id: toB64(made.rawId),
      clientDataJSON: toB64(r.clientDataJSON),
      attestationObject: toB64(r.attestationObject),
      transports: typeof r.getTransports === "function" ? r.getTransports() : [],
    });
  },
  removePasskey: (id: string) => request<void>("DELETE", `/api/me/passkeys/${encodeURIComponent(id)}`),
  /** Signs in with a passkey of this domain, the one its person picks. */
  withPasskey: async () => {
    const o = await request<GetOptions>("POST", "/api/auth/passkey/options", {});
    const got = (await navigator.credentials.get({ publicKey: { ...o, challenge: fromB64(o.challenge), allowCredentials: [] } })) as PublicKeyCredential | null;
    if (!got) throw new Error("No passkey was chosen.");
    const r = got.response as AuthenticatorAssertionResponse;
    await request<void>("POST", "/api/auth/passkey", {
      id: toB64(got.rawId),
      clientDataJSON: toB64(r.clientDataJSON),
      authenticatorData: toB64(r.authenticatorData),
      signature: toB64(r.signature),
      userHandle: r.userHandle ? toB64(r.userHandle) : null,
    });
    setAuthed(true);
  },

  /** Whether one keeps a password of one's own. */
  myPassword: (signal?: AbortSignal) => request<{ set: boolean }>("GET", "/api/me/password", undefined, { signal }),
  /** One's own password, set (or, empty, removed); `current` when one is set already. */
  setMyPassword: (password: string, current: string) => request<{ set: boolean }>("PUT", "/api/me/password", { password, current }),
  /** A person's sign-in with their name and password. */
  withName: async (name: string, password: string) => {
    await request<void>("POST", "/api/auth/password", { name, password });
    setAuthed(true);
  },
};
