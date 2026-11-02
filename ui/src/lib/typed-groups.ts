/**
 * A workflow's stages and uses as typed (docs/ci.md W29-R05, as the approved still pl-3 draws a draft):
 * read from the text as it's written, each line the server's where it had one; a job or a use the text
 * adds is a line of its own, marked new (`state: "added"`), and one it takes out is gone. The other
 * groups stay as the server gave them.
 */
import { parseDocument } from "yaml";
import type { ShipPipelineGroup, ShipPipelineLine } from "@/api/ship";

type Js = Record<string, unknown>;
const obj = (v: unknown): Js => (v && typeof v === "object" && !Array.isArray(v) ? (v as Js) : {});
const strs = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : v === undefined || v === null ? [] : [String(v)]);

/** Jobs by stage: those that need nothing first, then each job after the last of what it needs (as the server's stages). */
export function stagesOf(jobs: Js): string[][] {
  const depth = new Map<string, number>();
  const needsOf = (id: string) => strs(obj(jobs[id]).needs).filter((n) => n in jobs);
  for (let pass = 0; pass < 64; pass++)
    for (const id of Object.keys(jobs)) {
      if (depth.has(id)) continue;
      const n = needsOf(id);
      if (n.every((x) => depth.has(x))) depth.set(id, n.length ? Math.max(...n.map((x) => depth.get(x) ?? 0)) + 1 : 0);
    }
  // A cycle's jobs: in the first stage, where the YAML's problems say why.
  for (const id of Object.keys(jobs)) if (!depth.has(id)) depth.set(id, 0);
  const out: string[][] = [];
  for (const id of Object.keys(jobs)) (out[depth.get(id) ?? 0] ??= []).push(id);
  return out.filter(Boolean);
}

/** How a job's last finished run went, and its commit (the strip's `its last run passed in 18m 40s on a3f2c1d`). */
export interface JobLast {
  outcome: "passed" | "failed" | "cancelled";
  sha: string;
}

/**
 * A job's line as the server words it (`kiln_ci::pipelines::job_line`, the approved stills'): its id;
 * its legs where a matrix makes more than one (`3 platforms`), where it runs (bare metal unless a
 * container is asked for) and its labels beyond that, or the workflow it calls; its time in the
 * last finished run; and what it needs. Its note is what the strip says of it picked: where it
 * runs, what it reserves and its last run (pl-2).
 */
export function jobLine(id: string, j: Js, secs?: number, last?: JobLast): ShipPipelineLine {
  let legs: string | null = null;
  const at: string[] = [];
  if (j.uses) at.push(`calls ${String(j.uses)}`);
  else {
    const m = obj(obj(j.strategy).matrix);
    const count = (a: unknown) => (Array.isArray(a) ? Math.max(1, a.length) : 1);
    // kiln's `platforms`, or an axis named for them (`platform`, `os`): pl-2's `3 platforms`.
    const named = Object.entries(m).find(([k, a]) => ["platform", "platforms", "os"].includes(k) && count(a) > 1);
    if (named) legs = `${count(named[1])} platforms`;
    else {
      const n = Object.entries(m)
        .filter(([k, a]) => Array.isArray(a) && !["include", "exclude", "platforms"].includes(k))
        .reduce((x, [, a]) => x * count(a), 1);
      if (n > 1) legs = `${n} legs`;
    }
    at.push(j.container || j.image ? "in a container" : "bare metal");
    const on = j["runs-on"];
    if (on && typeof on === "object" && !Array.isArray(on)) {
      const g = obj(on);
      if (g.group) at.push(`group ${String(g.group)}`);
      at.push(...strs(g.labels).filter((t) => !["baremetal", "bare-metal"].includes(t)));
    } else at.push(...strs(on).filter((t) => !["baremetal", "bare-metal"].includes(t)));
  }
  // A job only a branch runs: pl-2's `on main`.
  const branch = onBranch(j.if === undefined ? "" : String(j.if));
  const on = branch ? `on ${branch}` : null;
  const parts = [...(legs ? [legs] : []), ...at, ...(on ? [on] : [])];
  // Its time in the last finished run (pl-2's `18m`).
  if (secs !== undefined) parts.push(spanWords(secs));
  if (strs(j.needs).length) parts.push(`needs ${strs(j.needs).join(", ")}`);
  const said = [...(legs ? [legs] : []), at.join(", "), ...(on ? [on] : [])];
  const reserved = reservedWords(j);
  if (reserved) said.push(reserved);
  if (last && secs !== undefined) {
    const how = last.outcome === "passed" ? "passed in" : last.outcome === "failed" ? "failed after" : "was cancelled after";
    said.push(`its last run ${how} ${spanExact(secs)} on ${last.sha.slice(0, 7)}`);
  }
  return { key: `jobs.${id}`, kind: "job", name: id, value: parts.join(" · "), note: said.join(" · ") };
}

/** A quantity as kiln's routing reads it (`kiln_ci::sched::routing::number`): `16Gi`, `2G`, `512Mi`, bytes. */
function quantity(v: unknown): number | null {
  if (typeof v !== "string" && typeof v !== "number") return null;
  const s = String(v).trim();
  const m = /^(-?[\d.]+)(.*)$/.exec(s);
  const n = m ? Number(m[1]) : NaN;
  const unit: Record<string, number> = { "": 1, B: 1, s: 1, Ki: 2 ** 10, Mi: 2 ** 20, Gi: 2 ** 30, Ti: 2 ** 40, K: 1e3, k: 1e3, M: 1e6, G: 1e9, T: 1e12, m: 60, h: 3600, d: 86_400 };
  const k = m ? unit[m[2] ?? ""] : undefined;
  return Number.isFinite(n) && k !== undefined ? n * k : null;
}

/** A size as the server shows it (`kiln_ci::sched::needs::show_size`): `16 GiB`, `512 MiB`, `1.5 TiB`. */
function showSize(n: number): string {
  const unit = (
    [
      [2 ** 40, "TiB"],
      [2 ** 30, "GiB"],
      [2 ** 20, "MiB"],
      [2 ** 10, "KiB"],
    ] as const
  ).find(([k]) => n >= k);
  return unit ? `${Math.round((n / unit[0]) * 10) / 10} ${unit[1]}` : `${n} bytes`;
}

/** What a job reserves (`resources:`), as the server says it (`kiln_ci::pipelines::reserved_words`): `2 CPUs, 16 GiB`; nothing where one isn't a quantity. */
function reservedWords(j: Js): string | null {
  if (!j.resources || typeof j.resources !== "object") return null;
  const r = obj(j.resources);
  const text = (k: string) => (typeof r[k] === "string" || typeof r[k] === "number" ? String(r[k]).trim() : null);
  const cpuText = text("cpu");
  const cpu = cpuText === null ? 0 : cpuText.endsWith("m") ? Number(cpuText.slice(0, -1)) / 1000 : Number(cpuText);
  const [mem, disk] = [text("memory"), text("disk")].map((t) => (t === null ? 0 : quantity(t)));
  if (!Number.isFinite(cpu) || cpu < 0 || mem === null || disk === null || mem === undefined || disk === undefined || mem < 0 || disk < 0) return null;
  const gpu = Number.parseInt(text("gpu") ?? "0", 10) || 0;
  const parts: string[] = [];
  if (cpu > 0) parts.push(`${cpu} CPU${cpu === 1 ? "" : "s"}`);
  if (mem > 0) parts.push(showSize(Math.floor(mem)));
  if (disk > 0) parts.push(`${showSize(Math.floor(disk))} of disk`);
  if (gpu > 0) parts.push(gpu === 1 ? "1 GPU" : `${gpu} GPUs`);
  return parts.length ? parts.join(", ") : null;
}

/** A span in its two largest units, as the server says a job's time (`kiln_ci::pipelines::span_exact`): `40s`, `18m 40s`, `2h 5m`, `3d 4h`. */
export function spanExact(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  const two = (a: number, ua: string, b: number, ub: string) => (b ? `${a}${ua} ${b}${ub}` : `${a}${ua}`);
  return s < 60
    ? `${s}s`
    : s < 3600
      ? two(Math.floor(s / 60), "m", s % 60, "s")
      : s < 86_400
        ? two(Math.floor(s / 3600), "h", Math.floor((s % 3600) / 60), "m")
        : two(Math.floor(s / 86_400), "d", Math.floor((s % 86_400) / 3600), "h");
}

/** The branch a job's `if:` keeps it to, where that's all it says (`kiln_ci::pipelines::on_branch`). */
function onBranch(cond: string): string | null {
  const c = cond
    .trim()
    .replace(/^\$\{\{/, "")
    .replace(/\}\}$/, "")
    .trim();
  const at = c.indexOf("==");
  if (at < 0) return null;
  const v = c
    .slice(at + 2)
    .trim()
    .replace(/^['"]+|['"]+$/g, "");
  const l = c.slice(0, at).trim();
  const b = l === "github.ref" ? (v.startsWith("refs/heads/") ? v.slice("refs/heads/".length) : null) : l === "github.ref_name" ? v : null;
  return b && !b.includes(" ") ? b : null;
}

/** A span in its largest unit, as the server says it (`kiln_ci::pipelines::span_words`): `40s`, `18m`, `2h`, `3d`, `3w`. */
export function spanWords(secs: number): string {
  const s = Math.max(0, Math.floor(secs));
  return s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86_400 ? `${Math.floor(s / 3600)}h` : s < 7 * 86_400 ? `${Math.floor(s / 86_400)}d` : `${Math.floor(s / (7 * 86_400))}w`;
}

/** A job the text adds: its line, marked new. */
const newJob = (id: string, j: Js): ShipPipelineLine => ({ ...jobLine(id, j), state: "added" });

/** A use the text adds: new, and asked to be pinned unless it names a whole commit. */
function newUse(u: string): ShipPipelineLine {
  const ref = u.includes("@") ? u.slice(u.lastIndexOf("@") + 1) : "";
  return { key: `uses.${u}`, kind: "use", name: u, value: `typed · new${/^[0-9a-f]{40}$/.test(ref) || u.startsWith("./") ? "" : " · pin it?"}`, state: "added" };
}

/** Each job's `uses:` (a reusable workflow's call) and its steps', once each, in the file's order. */
function usesOf(jobs: Js): string[] {
  const out: string[] = [];
  for (const j of Object.values(jobs).map(obj)) {
    if (j.uses) out.push(String(j.uses));
    for (const s of Array.isArray(j.steps) ? j.steps.map(obj) : []) if (s.uses) out.push(String(s.uses));
  }
  return [...new Set(out)];
}

/** `groups` with their stages and uses as `text` has them; null where the text isn't YAML yet, or has no jobs to read. */
export function typedGroups(text: string, groups: ShipPipelineGroup[]): ShipPipelineGroup[] | null {
  const d = parseDocument(text);
  if (d.errors.length) return null;
  const jobs = obj(obj(d.toJS()).jobs);
  if (!Object.keys(jobs).length) return null;
  const had = new Map(groups.flatMap((g) => g.lines.map((l) => [l.key, l] as const)));
  const stages: ShipPipelineGroup[] = stagesOf(jobs).map((ids, i) => ({ name: `Stage ${i + 1}`, lines: ids.map((id) => had.get(`jobs.${id}`) ?? newJob(id, obj(jobs[id]))) }));
  const uses = usesOf(jobs).map((u) => had.get(`uses.${u}`) ?? newUse(u));
  const out: ShipPipelineGroup[] = [];
  let placed = false;
  for (const g of groups) {
    if (g.name.startsWith("Stage ")) {
      if (!placed) out.push(...stages);
      placed = true;
    } else out.push(g.name === "Uses" ? { ...g, lines: uses } : g);
  }
  // No stages before (a file that had no jobs): after its triggers.
  if (!placed) out.splice(out.findIndex((g) => g.name === "Triggers") + 1, 0, ...stages);
  return out;
}

/** What the text adds, as the strip says it: `helm-lint`, `azure/setup-helm@v4`. */
export const addedOf = (groups: ShipPipelineGroup[]): ShipPipelineLine[] => groups.flatMap((g) => g.lines).filter((l) => l.state === "added");
