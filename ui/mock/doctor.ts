/**
 * The mock's runner diagnoses and repairs (`GET /api/agents/:name/diagnose`, `POST
 * /api/agents/:name/repair`, crate::doctor): the Mac whose self-build failed finds the reasons
 * (signing, a full disk under a big self-build cache) with repairs for each; the others are
 * mostly fine. A repair says what it did, and the next look finds that fixed.
 */
import type http from "node:http";
import type { RunnerDiagnosis, RunnerFinding } from "../src/api/types.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
  /** The runners, by name: whether each is connected and its OS. */
  runners: () => { name: string; connected: boolean; os: string }[];
}

/** Repairs made, per runner: what the next look finds fixed. */
const repaired = new Map<string, Set<string>>();

function findingsOf(name: string, os: string): RunnerFinding[] {
  const done = repaired.get(name) ?? new Set<string>();
  const out: RunnerFinding[] = [{ id: "connection", level: "ok", title: "Connected to this kiln" }];
  if (os === "macos") {
    out.push({
      id: "upgrade.last",
      level: "fail",
      title: "Its last upgrade failed 2 h ago (build)",
      detail: "kiln did not build (its full output is ~/.cache/kiln/self-build/build-9d2f61c08b7e4a55.log on this machine)\n\n   Compiling kiln v0.2.0 (/Users/ada/.cache/kiln/self-build/src)\nerror: linking with `cc` failed: exit status: 1\n  = note: ld: write failed: No space left on device\nerror: could not compile `kiln` (bin \"kiln\") due to 1 previous error",
      fix: { action: "retry-upgrade", label: "Upgrade it the other way (download)", way: "download" },
    });
    out.push(
      done.has("clear-self-build-cache")
        ? { id: "disk.work", level: "ok", title: "182 GiB free of 460 GiB where jobs and agents work" }
        : { id: "disk.work", level: "fail", title: "820 MiB free of 460 GiB where jobs and agents work", detail: "/Users/ada/.local/share/kiln/work", fix: { action: "clear-self-build-cache", label: "Clear the self-build cache (23.4 GiB)" } },
    );
    if (!done.has("clear-self-build-cache"))
      out.push({ id: "self_build.cache", level: "warn", title: "Self-builds keep 23.4 GiB of compiled crates", detail: "It's a cache: clearing it frees the room, and the next self-build compiles from scratch.", fix: { action: "clear-self-build-cache", label: "Clear the self-build cache" } });
    out.push(
      done.has("re-sign")
        ? { id: "signing", level: "ok", title: "kiln is signed here, so macOS keeps letting it onto the local network after upgrades" }
        : {
            id: "signing",
            level: "warn",
            title: "kiln isn't signed here: after each upgrade macOS may cut it off from the local network until someone allows it again",
            detail: "The installed kiln is ad hoc signed. Run `kiln signing setup` on the Mac once (macOS asks to trust kiln's certificate there), then repair with Sign kiln again.",
            fix: { action: "re-sign", label: "Sign kiln again" },
          },
    );
  } else {
    out.push({ id: "kiln.build", level: "ok", title: "Runs the newest kiln (4f3c2b1a9e8d)" });
    out.push({ id: "disk.work", level: "ok", title: "1.2 TiB free of 1.8 TiB where jobs and agents work" });
    out.push({ id: "self_build.cache", level: "info", title: "Self-builds keep 6.1 GiB of compiled crates" });
  }
  out.push({ id: "clock", level: "ok", title: "Its clock agrees with the server's" });
  const order = { fail: 0, warn: 1, info: 2, ok: 3 };
  return out.sort((a, b) => order[a.level] - order[b.level]);
}

const SAID: Record<string, string> = {
  "clear-self-build-cache": "Freed 23.4 GiB of self-build cache; its 3 logs stay.",
  "prune-versions": "Removed older kiln builds kept under ~/.local/share/kiln/runner/versions: 312 MiB freed; the one running and the newest few stay.",
  "re-sign": "Signed /Users/ada/.local/bin/kiln again as kiln.",
};

export async function handleDoctor(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const m = url.pathname.match(/^\/api\/agents\/([^/]+)\/(diagnose|repair)$/);
  if (!m) return false;
  const name = decodeURIComponent(m[1] ?? "");
  const r = ctx.runners().find((x) => x.name === name);
  if (!r) return (ctx.err(res, 404, `kiln has no runner called ${name}`), true);
  if (m[2] === "diagnose" && req.method === "GET") {
    // A machine takes a moment to look at itself.
    await new Promise((ok) => setTimeout(ok, 700));
    const findings: RunnerFinding[] = r.connected
      ? findingsOf(name, r.os)
      : [{ id: "connection", level: "fail", title: "Not connected; last heard from 2 days ago", detail: "Until it's back, kiln can't look at the machine itself." }];
    const d: RunnerDiagnosis = { runner: name, at: Date.now(), worst: findings[0]?.level ?? "ok", findings, machine_checked: r.connected, why: r.connected ? null : "it isn't connected" };
    ctx.json(res, 200, d);
    return true;
  }
  if (m[2] === "repair" && req.method === "POST") {
    const b = await ctx.body<{ action?: string; way?: string }>(req);
    const action = b.action ?? "";
    if (action === "retry-upgrade") return (ctx.json(res, 200, { ok: true, action, job: { id: "upg-mock", kind: "runner", target: name, method: b.way === "build" ? "build" : "native", state: "running", started_at: Math.floor(Date.now() / 1000), log: [] } }), true);
    const said = SAID[action];
    if (!said) return (ctx.err(res, 400, `${JSON.stringify(action)} isn't a repair`), true);
    if (action === "re-sign" && r.os !== "macos") return (ctx.err(res, 409, "only a Mac signs kiln: there's nothing to sign here"), true);
    await new Promise((ok) => setTimeout(ok, 500));
    repaired.set(name, new Set([...(repaired.get(name) ?? []), action]));
    ctx.json(res, 200, { ok: true, action, message: said });
    return true;
  }
  return false;
}
