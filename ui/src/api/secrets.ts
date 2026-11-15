/**
 * A person's own secrets (`/api/me/secrets`, `secrets.mine.*`; crates/kiln/src/secrets/mine.rs; kiln
 * plan 01a1cd8e862e7169a074db3d5fa33f3d, P4.7): theirs alone, never read back. A list names each with
 * its size and when it was set and last used; an Agentfile of theirs names one a build is given as
 * `LABEL kiln.secret.<ID>=user:<NAME>`.
 */
import type { Res } from "@/lib/data";
import { request } from "./client";

const enc = encodeURIComponent;

export interface MySecret {
  /** Upper-case, as the server keeps it. */
  name: string;
  /** Its value's length, in bytes. */
  size: number;
  /** Unix seconds. */
  created_at: number;
  updated_at: number;
  /** None until a build reads it. */
  last_used_at: number | null;
}

export const mySecretsApi = {
  list: (signal?: AbortSignal) => request<{ secrets: MySecret[] }>("GET", "/api/me/secrets", undefined, { signal }).then((r) => r.secrets),
  set: (name: string, value: string) => request<{ set: boolean; name: string }>("PUT", `/api/me/secrets/${enc(name)}`, { value }),
  remove: (name: string) => request<{ deleted: boolean }>("DELETE", `/api/me/secrets/${enc(name)}`),
};

export const mySecretsRes: Res<MySecret[]> = { key: "me/secrets", fetch: (s) => mySecretsApi.list(s) };
