/**
 * People, their grants, roles and tokens (docs/ci.md W10-R01 to R04): for Settings → People and
 * Settings → Tokens. A grant is written `[!]<role or capability>[@<scope>]`: `developer@repo:acme/app`,
 * `viewer` (the whole server), `!secrets.manage@org:acme` (a denial).
 */
import { request } from "@/api/client";

export type Person = { id: string; name: string; created_at: number; created_by: string; link_expires_at: number | null; grants: string[] };
export type GrantRow = { id: number; principal: string; user: string; what: string; scope: string; deny: boolean; created_at: number; created_by: string };
export type Role = { name: string; builtin: boolean; capabilities: string[] };
export type Token = {
  id: string;
  kind: "pat" | "agent";
  principal: string;
  on_behalf_of: string | null;
  note: string | null;
  grants: string[];
  masked: string;
  created_at: number;
  expires_at: number | null;
  last_used_at: number | null;
  revoked_at: number | null;
};
export type Minted = { id: string; token: string; grants: string[]; expires_at: number };

const enc = encodeURIComponent;

export const people = {
  list: (signal?: AbortSignal) => request<{ users: Person[] }>("GET", "/api/users", undefined, { signal }).then((r) => r.users),
  add: (name: string, grants: string[], email: string | null) => request<{ id: string; name: string; grants: string[] }>("POST", "/api/users", { name, grants, email }),
  /** Disables them: they sign in no more, their tokens are revoked. */
  remove: (id: string) => request<void>("DELETE", `/api/users/${enc(id)}`),
  link: (id: string) => request<{ user: string; url: string; expires_at: number }>("POST", `/api/users/${enc(id)}/link`, {}),
  signOut: (id: string) => request<void>("POST", `/api/users/${enc(id)}/sign-out`, {}),
  grants: (user: string, signal?: AbortSignal) => request<{ grants: GrantRow[] }>("GET", `/api/grants?user=${enc(user)}`, undefined, { signal }).then((r) => r.grants),
  grant: (user: string, grant: string) => request<{ id: number; user: string; grant: string }>("POST", "/api/grants", { user, grant }),
  ungrant: (id: number) => request<void>("DELETE", `/api/grants/${id}`),

  roles: (signal?: AbortSignal) => request<{ roles: Role[]; capabilities: string[] }>("GET", "/api/roles", undefined, { signal }),
  putRole: (name: string, capabilities: string[]) => request<Role>("PUT", `/api/roles/${enc(name)}`, { capabilities }),
  removeRole: (name: string) => request<void>("DELETE", `/api/roles/${enc(name)}`),

  /** One's own tokens; everyone's with tokens.manage. */
  tokens: (signal?: AbortSignal) => request<{ tokens: Token[] }>("GET", "/api/tokens", undefined, { signal }).then((r) => r.tokens),
  mint: (note: string, grants: string[], days: number) => request<Minted>("POST", "/api/tokens", { kind: "pat", note: note || null, grants, expires_in_days: days }),
  revoke: (id: string) => request<void>("DELETE", `/api/tokens/${enc(id)}`),
};
