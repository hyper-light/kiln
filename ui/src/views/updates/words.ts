/**
 * What the System page's Updates and Builds say (docs/releases.md): each machine's state in a word
 * and a line, the platforms runners download and what the release pipeline is building of them,
 * the Rollout figure's headline and readout, how long kiln waits, and the first message of an
 * agent asked why a runner doesn't upgrade. Pure: words and numbers from the overview, no React.
 */
import type {
  FleetTarget,
  Job,
  KilnBinary,
  ReleaseBuild,
  Run,
  RunnerFinding,
  RunnerInstance,
  RunnerUpdate,
  ServerUpdate,
  UpdateJob,
  UpdatePolicy,
  UpdatesOverview,
  UpgradeWaits,
  UpgradeWay,
  UpgradeWayOpen,
} from "@/api/types";
import { formatBytes, relativeTime, shortSha } from "@/lib/format";

/** A state's colour: sage, amber, rose, lilac, or faint. */
export type Tone = "ok" | "warn" | "bad" | "run" | "off";
/** A cable's look when nothing moves on it: behind, failed, away, waiting; still and plain otherwise. */
export type CableTone = "warn" | "bad" | "off" | "wait" | undefined;

/** Finished upgrades stay shown under their runner this long. */
export const RECENT_S = 3600;

export const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** "builds 94f0847a1b2c itself" → "Builds 94f0847a1b2c itself". */
export function sentence(s: string) {
  return s.charAt(0).toUpperCase() + s.slice(1);
}

/** "a, b and c". */
export function listed(items: string[]) {
  return items.length < 2 ? (items[0] ?? "") : `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/** What a runner process still holds: "2 terminals, 1 agent session" (`short`: "2 terminals · 1 agent"), or "nothing". */
export function holding(i: RunnerInstance, short = false) {
  const parts = [i.terminals ? plural(i.terminals, "terminal") : "", i.agents ? plural(i.agents, short ? "agent" : "agent session") : "", i.jobs ? plural(i.jobs, "job") : ""].filter(Boolean);
  return parts.length ? parts.join(short ? " · " : ", ") : "nothing";
}

/** What a runner does with a newer kiln. */
export const POLICIES: { value: UpdatePolicy; label: string }[] = [
  { value: "auto", label: "Automatic" },
  { value: "notify", label: "Notify" },
  { value: "off", label: "Off" },
];

/* ---------------------------- the ways a runner is upgraded ---------------------------- */

/** The way an upgrade went: a build on the machine, or a binary downloaded. */
export function wayOf(j: UpdateJob): UpgradeWay {
  return j.method === "build" ? "build" : "download";
}

/** The ways a runner can be upgraded now. An older server doesn't say: the way it would take, when it would. */
export function upgradeWays(r: RunnerUpdate): UpgradeWayOpen[] {
  if (r.ways) return r.ways;
  if (!r.how || r.how === "wait" || r.how === "upgrading" || r.how === "left" || r.how === "off") return [];
  const build = r.how === "build";
  return [{ way: build ? "build" : "download", ok: (r.method === "native" && !build) || r.terminal, why: "" }];
}

/** A way's button: the first is the upgrade itself, the one that failed is tried again, any other is the alternative. */
export function wayLabel(way: UpgradeWay, first: boolean, failedBefore: boolean) {
  if (failedBefore) return way === "build" ? "Retry build" : "Retry";
  if (first) return way === "build" ? "Build and upgrade" : "Upgrade";
  return way === "build" ? "Build it instead" : "Download instead";
}

/** A runner's upgrade, as its row offers it: whether it can be asked for, the ways open (the one to try first, first), and why none is. */
export function upgradeOffer(r: RunnerUpdate, job: UpdateJob | undefined) {
  const failed = job?.state === "failed" ? job : undefined;
  const behind = r.connected && !r.up_to_date && r.how !== "left" && r.policy !== "off" && !r.locked;
  const ways = upgradeWays(r);
  const open = ways.filter((w) => w.ok);
  const first: UpgradeWay = failed ? wayOf(failed) : r.how === "build" ? "build" : "download";
  open.sort((a, b) => Number(b.way === first) - Number(a.way === first));
  return {
    behind,
    /** It can be upgraded from its row now (nothing under way). */
    actionable: behind && job?.state !== "running" && r.how !== "upgrading",
    open,
    failed,
    /** Behind, with no way open: each way says why. */
    stuck: behind && open.length === 0 && ways.length > 0 ? ways.filter((w) => !w.ok) : [],
    /** kiln has no binary of the newest kiln for its platform. */
    missing: Boolean(r.connected && !r.up_to_date && !r.target_build && r.target),
  };
}

/** Upgrade jobs of one machine: the one under way, or the last one, while it's recent. */
export function latestJob(jobs: UpdateJob[], kind: UpdateJob["kind"], target: string, nowS: number): UpdateJob | undefined {
  return jobs.find((j) => j.kind === kind && j.target === target && (j.state === "running" || nowS - (j.ended_at ?? j.started_at) < RECENT_S));
}

/* ---------------------------- platforms ---------------------------- */

const OS_ORDER: Record<string, number> = { linux: 0, macos: 1, windows: 2 };
const ARCH_ORDER: Record<string, number> = { x86_64: 0, aarch64: 1 };

/** "x86_64-unknown-linux-musl" → linux, x86_64, musl. */
export function platformOf(target: string): { os: string; arch: string; abi: string } {
  const parts = target.split("-");
  const arch = parts[0] || target;
  const os = target.endsWith("-apple-darwin") ? "macos" : target.includes("-windows-") ? "windows" : target.includes("-linux-") ? "linux" : (parts[2] ?? "unknown");
  const abi = os === "linux" || os === "windows" ? (parts[parts.length - 1] ?? "") : "";
  return { os, arch, abi };
}

/** The release pipeline's job that builds `target`, on a machine of that platform (crate::fleet::platform_job, `.kiln/release.yaml`). */
export function platformJob(target: string): "linux" | "macos" | "windows" {
  if (target.endsWith("-apple-darwin")) return "macos";
  if (target.includes("-windows-")) return "windows";
  return "linux";
}

/** The release run is under way. */
export function isBuilding(last: Run | null | undefined): boolean {
  return last != null && (last.status === "queued" || last.status === "running" || last.status === "pending");
}

export type BuildState = "current" | "behind" | "building" | "queued" | "failed" | "missing";

/** A platform's build as the Builds section shows it: what runners of it download, and where it stands. */
export interface PlatformView {
  /** Its target: one platform, one build runners get. */
  key: string;
  target: string;
  os: string;
  arch: string;
  abi: string;
  /** "linux · x86_64 · musl": its target in words, so no two platforms read alike. */
  label: string;
  /** What runners of it download: the server's own for the server's platform, else the one kept for it. */
  binary: KilnBinary | null;
  /** Builds kept for it that runners don't get: an older one beside the server's own. */
  shadowed: KilnBinary[];
  /** The commit it is built from, when kiln knows it. */
  commit: string | null;
  state: BuildState;
  tone: Tone;
  /** Its job of the release run under way, when kiln read it. */
  job: Job | null;
  /** That job runs now: its cable lights. */
  lit: boolean;
  /** The runners of this platform. */
  runners: string[];
}

/** "x86_64 · musl": a platform's arch and its ABI, what follows its OS. */
export function archWords(p: { arch: string; abi: string }): string {
  return p.abi ? `${p.arch} · ${p.abi}` : p.arch;
}

/**
 * Every platform with a build here, every platform kiln's release pipeline is behind on, and every
 * platform a runner waits for a build of: the build runners of each get (the server's own for its
 * platform, which shadows any other kept for it), and what the release run under way (its `jobs`,
 * when read) does about each: its job of the platform's family queued, running or failed, whatever
 * commit the run builds.
 */
export function platformsOf(data: UpdatesOverview, jobs: Job[] | null): PlatformView[] {
  const build = data.build;
  const last = build?.last ?? null;
  const building = isBuilding(last);
  const behind = new Set(build?.behind ?? []);
  const lastOfIt = last !== null && build?.commit != null && last.sha.toLowerCase() === build.commit.toLowerCase();
  const newestOfServer = data.newest?.from === "server" ? data.newest.commit : null;
  const wanted = [...behind, ...data.runners.filter((r) => r.connected && !r.up_to_date && !r.target_build && r.target).map((r) => r.target as string)];
  const targets = [...new Set([...data.binaries.map((b) => b.target), ...wanted])];
  const out = targets.map((target): PlatformView => {
    const kept = data.binaries.filter((b) => b.target === target);
    const binary = kept.find((b) => b.origin === "server") ?? kept.reduce<KilnBinary | null>((a, b) => (a === null || b.at > a.at ? b : a), null);
    const { os, arch, abi } = platformOf(target);
    const job = building ? (jobs?.find((j) => j.key === platformJob(target)) ?? null) : null;
    const s = job?.status;
    let state: BuildState;
    if (s === "queued" || s === "pending") state = "queued";
    else if (s === "running") state = "building";
    else if (s === "failed" || s === "cancelled" || s === "timed_out") state = "failed";
    // The run's jobs aren't read yet: what it is to build, building.
    else if (building && jobs === null && behind.has(target) && binary?.origin !== "server") state = "building";
    else if (binary && behind.has(target)) state = lastOfIt && last?.status === "failed" ? "failed" : "behind";
    else if (binary) state = "current";
    else state = lastOfIt && last?.status === "failed" ? "failed" : "missing";
    const runners = data.runners.filter((r) => r.target === target).map((r) => r.name);
    const tone: Tone =
      state === "current" ? "ok" : state === "behind" ? "warn" : state === "building" ? "run" : state === "failed" ? "bad" : state === "queued" ? "off" : runners.length ? "warn" : "off";
    const commit = binary ? (binary.sha ?? (binary.origin === "server" ? newestOfServer : null)) : null;
    return {
      key: target,
      target,
      os,
      arch,
      abi,
      label: `${os} · ${archWords({ arch, abi })}`,
      binary,
      shadowed: kept.filter((b) => b !== binary),
      commit,
      state,
      tone,
      job: state === "current" || state === "behind" || state === "missing" ? null : job,
      lit: s === "running",
      runners,
    };
  });
  out.sort((a, b) => (OS_ORDER[a.os] ?? 9) - (OS_ORDER[b.os] ?? 9) || (ARCH_ORDER[a.arch] ?? 9) - (ARCH_ORDER[b.arch] ?? 9) || a.abi.localeCompare(b.abi));
  return out;
}

/** Where a build came from, in a few words (`short`: as the figure has room for). */
export function originWords(b: KilnBinary, short = false): string {
  switch (b.origin) {
    case "server":
      return short ? "server's own" : "the server's own";
    case "built":
      return short ? `run #${b.run ?? "?"}` : `release run #${b.run ?? "?"}`;
    case "machine":
      return `built by ${b.by ?? "a machine"}`;
    case "upload":
      return short ? "uploaded" : "an upload";
    default:
      return "GitHub release";
  }
}

/** What a platform's build is, short: its commit when kiln knows it, else its build. */
export function buildOf(p: PlatformView): string {
  return p.commit ? shortSha(p.commit) : (p.binary?.build ?? "none");
}

/** A platform's line in the figure. */
export function platformLine(p: PlatformView, last: Run | null): string {
  const run = last ? `run #${last.number}` : "the release run";
  switch (p.state) {
    case "building":
      return last ? `building ${last.sha.slice(0, 7)} · ${run}` : "building";
    case "queued":
      return `queued · ${run}`;
    case "failed":
      return p.binary ? `${buildOf(p)} · build failed` : `build failed · ${run}`;
    case "behind":
      return `${buildOf(p)} · behind`;
    case "missing":
      return "no build yet";
    default:
      return `${buildOf(p)} · ${p.binary ? originWords(p.binary, true) : ""}`;
  }
}

/** The cable from a platform's build to the server, when nothing moves on it. */
export function platformCable(p: PlatformView): CableTone {
  return p.state === "behind" ? "warn" : p.state === "failed" ? "bad" : p.state === "queued" || p.state === "missing" ? "off" : p.state === "building" && !p.lit ? "wait" : undefined;
}

/** How far the release pipeline is: what the Builds section says beside its actions, and (`more`) what it explains under its title. */
export interface BuildStatus {
  tone: Tone;
  /** Words around its run's link: `before`, run #N, `after`. */
  before: string;
  run: Run | null;
  after: string;
  more: string | null;
}

/** How far a release run's jobs are, as its words say it: "1 of 3 running", "2 of 3 queued", "3 of 3 built", and how many failed. */
export function runProgress(jobs: Job[]): string {
  const n = jobs.length;
  const count = (...s: string[]) => jobs.filter((j) => s.includes(j.status)).length;
  const [running, queued, done, failed] = [count("running"), count("queued", "pending"), count("succeeded", "skipped"), count("failed", "cancelled", "timed_out")];
  const words = running ? `${running} of ${n} running` : queued ? `${queued} of ${n} queued` : `${done} of ${n} built`;
  return failed ? `${words} · ${failed} failed` : words;
}

export function buildStatus(build: ReleaseBuild, platforms: PlatformView[], nowMs: number, jobs: Job[] | null = null): BuildStatus {
  const last = build.last ?? null;
  const commit = build.commit ? build.commit.slice(0, 7) : null;
  const behind = build.behind ?? [];
  const lastOfIt = last !== null && build.commit != null && last.sha.toLowerCase() === build.commit.toLowerCase();
  if (build.error) return { tone: "warn", before: "Can't build here", run: null, after: "", more: build.error };
  if (last && isBuilding(last)) {
    // What the run does, from its jobs: whatever commit it builds, the server's or a newer one.
    return { tone: "run", before: "Release ", run: last, after: ` building ${last.sha.slice(0, 7)}${jobs?.length ? ` · ${runProgress(jobs)}` : ""}`, more: null };
  }
  if (build.in_step) return { tone: "ok", before: `All built from ${commit}`, run: null, after: "", more: null };
  if (commit) {
    const named = platforms.filter((p) => behind.includes(p.target)).map((p) => `${p.os}/${p.arch}`);
    const what = named.length && named.length <= 2 ? `${named.join(", ")} behind` : `${plural(behind.length, "platform")} behind`;
    if (lastOfIt && last?.status === "failed") {
      return {
        tone: "bad",
        before: "Build failed · ",
        run: last,
        after: "",
        more: `${plural(behind.length, "platform")} ${behind.length === 1 ? "isn't" : "aren't"} built from ${commit}, the newest kiln${build.keeps_in_step ? "; kiln tries again in a while" : ""}.`,
      };
    }
    return { tone: "warn", before: what, run: null, after: "", more: `Not built from ${commit}, the newest kiln${build.keeps_in_step ? ": kiln builds them on its own" : ""}.` };
  }
  if (last?.status === "succeeded") {
    return {
      tone: "ok",
      before: `Built ${relativeTime(last.finished_at ?? last.created_at, nowMs)} · `,
      run: last,
      after: "",
      more: `Built from ${last.branch} ${last.sha.slice(0, 7)}. The server doesn't know which commit it runs (built without KILN_COMMIT), so kiln can't keep them in step with it.`,
    };
  }
  if (last) return { tone: "bad", before: `Last build ${last.status === "failed" ? "failed" : "didn't finish"} · `, run: last, after: "", more: null };
  return {
    tone: "warn",
    before: "Not built here yet",
    run: null,
    after: "",
    more: `kiln hasn't built every platform here yet: ${build.repo}'s ${build.pipeline} pipeline does, on the server's own runner.`,
  };
}

/* ---------------------------- the server ---------------------------- */

export interface ServerView {
  tone: Tone;
  word: string;
  sub: string;
  /** Its line in the figure. */
  scene: string;
  /** Its build, short: the commit when kiln knows it runs the newest. */
  build: string;
  /** "Deployment ns/name", "a service (kiln supervise)", "a standalone process". */
  runsAs: string;
  /** How it is upgraded, in its Updates column. */
  policy: string;
  /** Its note says something to act on (shown under its row; else in the readout). */
  notable: boolean;
}

export function serverView(s: ServerUpdate, job: UpdateJob | undefined, pending: boolean, newest: FleetTarget | null, nowMs: number): ServerView {
  const build = newest?.from === "server" ? shortSha(newest.commit) : s.build;
  const runsAs = s.mode === "kubernetes" ? `Deployment ${s.namespace}/${s.deployment}` : s.mode === "service" ? "a service (kiln supervise)" : "a standalone process";
  const policy = s.mode === "kubernetes" ? "Follows its image" : s.mode === "service" ? "Installed from here" : "Upgraded by hand";
  const error = s.image?.error || (s.mode !== "kubernetes" ? s.release.error : null);
  const checkedAt = s.mode === "kubernetes" ? (s.image?.checked_at ?? 0) : s.release.checked_at;
  let tone: Tone;
  let word: string;
  let sub: string;
  if (pending) [tone, word, sub] = ["run", "Restarting", "reconnects when it's back"];
  else if (job?.state === "running") [tone, word, sub] = ["run", "Upgrading", job.method === "restart" ? "restarting onto the new image" : "installing the new build"];
  else if (s.newer) {
    tone = "warn";
    word = "Upgrade ready";
    sub =
      s.mode === "kubernetes"
        ? `the image holds ${s.image?.build ?? "a newer build"}`
        : s.replacement?.origin === "upload"
          ? `uploaded build ${s.replacement.build ?? ""}`
          : `kiln ${s.replacement?.version ?? ""} released`;
  } else if (error) [tone, word, sub] = ["warn", "Can't check", error];
  else {
    tone = "ok";
    word = !newest || newest.from === "server" ? "Newest" : "Up to date";
    sub = s.checking || !checkedAt ? "checking…" : `checked ${relativeTime(checkedAt, nowMs)}`;
  }
  const state = pending ? "restarting" : s.newer ? "upgrade ready" : word.toLowerCase();
  return { tone, word, sub, scene: `${s.version} · ${build} · ${state}`, build, runsAs, policy, notable: pending || job?.state === "running" || s.newer || Boolean(error) };
}

/* ---------------------------- runners ---------------------------- */

export interface RunnerView {
  tone: Tone;
  /** Its state in a word or two: "Up to date", "Upgrading", "Behind"… */
  word: string;
  /** What that means for it now. */
  sub: string;
  /** Its line in the figure. */
  scene: string;
  cable: CableTone;
  /** Its upgrade runs now: its cable lights. */
  lit: boolean;
  /** A new runner process runs beside the old one, which keeps what it holds until that ends. */
  switching: boolean;
  /** The build it runs, short, and the one it would be brought to. */
  from: string;
  to: string | null;
  /** It is to move from one to the other (connected, behind, not left alone). */
  moving: boolean;
}

/** A failed upgrade, in a few words: what ran out, or the first of what went wrong. */
export function failWords(j: UpdateJob, waits: UpgradeWaits | null | undefined): string {
  if (j.timed_out === "upgrade") return waits ? `didn't report in within ${waits.upgrade_minutes} min` : "didn't report in in time";
  if (j.timed_out === "jobs") return waits ? `its jobs ran past ${waits.wait_for_jobs_minutes} min` : "its jobs ran too long";
  if (j.timed_out === "build") return waits ? `didn't build within ${waits.build_minutes} min` : "didn't build in time";
  const first = j.error?.split(/[;(]/)[0]?.trim();
  return first || (j.method === "build" ? "the build failed" : "see its output");
}

export function runnerView(r: RunnerUpdate, job: UpdateJob | undefined, newest: FleetTarget | null, waits: UpgradeWaits | null | undefined, nowMs: number): RunnerView {
  const from = r.commit ? shortSha(r.commit) : (r.build ?? "unknown");
  const to = newest ? shortSha(newest.commit) : (r.target_build ?? null);
  const moving = r.connected && !r.up_to_date && r.how !== "left";
  const old = r.instances.filter((i) => !i.newest);
  const switching = r.instances.length > 1;
  const oldHolds = old.length ? old.map((i) => holding(i, true)).join(" · ") : "nothing";
  const base = { from, to, moving, switching, lit: false, cable: undefined as CableTone };
  if (job?.state === "running" || (r.connected && r.how === "upgrading")) {
    const build = job?.method === "build";
    const toWords = to ?? "the new build";
    return {
      ...base,
      tone: "run",
      word: build ? "Building itself" : "Upgrading",
      sub: switching ? `old runner holds ${oldHolds}` : build ? `building ${toWords} itself` : `upgrading to ${toWords}`,
      scene: switching ? "switching · nothing stops" : build ? `building ${toWords} itself` : `upgrading to ${toWords}`,
      lit: true,
    };
  }
  if (!r.connected) {
    return { ...base, tone: "off", word: "Offline", sub: moving || !r.up_to_date ? "needs to reconnect to be upgraded" : `runs ${from}`, scene: `offline · ${from}`, cable: "off" };
  }
  if (r.up_to_date) {
    const recent = job?.state === "done" && job.ended_at ? `switched ${relativeTime(job.ended_at, nowMs)}` : null;
    const newestOne = r.instances.find((i) => i.newest);
    const sub = switching ? `old runner holds ${oldHolds}` : (recent ?? (newestOne ? `holds ${holding(newestOne, true)}` : ""));
    return { ...base, tone: "ok", word: "Up to date", sub, scene: `${from} · current` };
  }
  if (job?.state === "failed") {
    const build = job.method === "build";
    return {
      ...base,
      tone: "bad",
      word: build ? "Build failed" : "Upgrade failed",
      sub: failWords(job, waits),
      scene: job.timed_out ? "upgrade timed out" : build ? "build failed" : "upgrade failed",
      cable: "bad",
    };
  }
  if (r.how === "left") return { ...base, tone: "warn", word: r.standing === "local" ? "Local build" : "Branch build", sub: r.next ?? "left alone", scene: `${from} · left alone` };
  if (r.how === "off")
    return { ...base, tone: "warn", word: "Updates off", sub: r.locked ? "its machine refuses upgrades" : `${to ?? "a newer kiln"} is available`, scene: `${from} · updates off`, cable: "warn" };
  if (r.how === "wait") {
    return {
      ...base,
      tone: "run",
      word: "Waiting",
      sub: r.waits_on ? `for the ${r.os}/${r.arch} build` : (r.next ?? "for a build"),
      scene: r.waits_on ? `waits for the ${r.os} build` : (r.next ?? "waits"),
      cable: "wait",
    };
  }
  const word = r.standing ? "Behind" : r.build ? "Upgrade available" : "Older runner";
  return { ...base, tone: "warn", word, sub: r.next ?? "ready to upgrade", scene: `${from} → ${to ?? "a newer build"}`, cable: "warn" };
}

/* ---------------------------- the fleet as a whole ---------------------------- */

export interface RunnerEntry {
  r: RunnerUpdate;
  job: UpdateJob | undefined;
  view: RunnerView;
}

export interface Fleet {
  runners: RunnerEntry[];
  /** Up to date. */
  current: number;
  upgrading: RunnerEntry[];
  failed: RunnerEntry[];
  waiting: RunnerEntry[];
  offline: RunnerEntry[];
  /** What Upgrade all upgrades: connected, behind, theirs to upgrade (not left alone, waiting, off or locked). */
  ready: RunnerUpdate[];
  /** Behind and connected, whatever stands in the way. */
  behind: RunnerEntry[];
}

export function fleetOf(data: UpdatesOverview, jobs: UpdateJob[], nowMs: number): Fleet {
  const nowS = nowMs / 1000;
  const newest = data.newest ?? null;
  const runners = data.runners.map((r) => {
    const job = latestJob(jobs, "runner", r.name, nowS);
    return { r, job, view: runnerView(r, job, newest, data.waits?.now, nowMs) };
  });
  return {
    runners,
    current: data.runners.filter((r) => r.up_to_date).length,
    upgrading: runners.filter((e) => e.view.lit),
    failed: runners.filter((e) => e.view.tone === "bad"),
    waiting: runners.filter((e) => e.view.word === "Waiting"),
    offline: runners.filter((e) => !e.r.connected),
    // Runners on a branch or a local build run it on purpose: "Upgrade all" leaves them be.
    ready: data.runners.filter((r) => r.connected && !r.up_to_date && r.how !== "left" && r.how !== "wait" && r.policy !== "off" && !r.locked),
    behind: runners.filter((e) => e.r.connected && !e.r.up_to_date && e.r.how !== "left" && !e.view.lit),
  };
}

/** The line under "Server and runners". */
export function fleetSummary(data: UpdatesOverview, fleet: Fleet): string {
  const n = data.runners.length;
  if (n === 0) return "No machines have joined yet.";
  const newest = data.newest ? shortSha(data.newest.commit) : null;
  const parts = [
    newest ? `${fleet.current} of ${plural(n, "runner")} on ${newest}` : `${fleet.current} of ${n} up to date`,
    fleet.upgrading.length ? `${fleet.upgrading.length} upgrading` : null,
    fleet.ready.length ? `${fleet.ready.length} ready to upgrade` : null,
    fleet.waiting.length ? `${fleet.waiting.length} waiting on a build` : null,
    fleet.offline.length ? `${fleet.offline.length} offline` : null,
  ].filter(Boolean);
  return parts.join(" · ") + (data.follow && fleet.current === n ? " · each follows the newest kiln on its own" : "");
}

/** The figure's headline: the one thing to know now, and its tone. */
export function headline(data: UpdatesOverview, fleet: Fleet, server: ServerView, platforms: PlatformView[]): { tone: Tone; text: string } {
  const newest = data.newest ? shortSha(data.newest.commit) : null;
  const [failed] = fleet.failed;
  if (failed)
    return { tone: "bad", text: fleet.failed.length > 1 ? `${fleet.failed.length} upgrades failed` : `${failed.r.name}'s ${failed.view.word === "Build failed" ? "build" : "upgrade"} failed` };
  if (fleet.upgrading.length) return { tone: "run", text: `Upgrading ${plural(fleet.upgrading.length, "runner")}` };
  if (server.tone === "run") return { tone: "run", text: server.word === "Restarting" ? "Restarting the server" : "Upgrading the server" };
  const building = platforms.filter((p) => p.state === "building" || p.state === "queued");
  if (building.length) return { tone: "run", text: `Building ${plural(building.length, "platform")}` };
  if (data.server.newer) return { tone: "warn", text: "Server upgrade ready" };
  if (fleet.behind.length) return { tone: "warn", text: `${plural(fleet.behind.length, "runner")} behind` };
  if (fleet.current === data.runners.length) return { tone: "ok", text: newest ? `Every machine runs ${newest}` : "Every machine is up to date" };
  return { tone: "ok", text: newest ? `${fleet.current} of ${plural(data.runners.length, "runner")} on ${newest}` : "Up to date" };
}

export interface Readout {
  title: string;
  note: string;
}

/** What the readout says with nothing picked: the fleet as it stands, and what happens next. */
export function readoutAll(data: UpdatesOverview, fleet: Fleet, platforms: PlatformView[]): Readout {
  const newest = data.newest ? shortSha(data.newest.commit) : null;
  const last = data.build?.last ?? null;
  const building = platforms.filter((p) => p.state === "building" || p.state === "queued");
  const stale = platforms.filter((p) => p.state === "behind" || p.state === "missing" || (p.state === "failed" && !isBuilding(last)));
  if (fleet.failed.length) {
    const said = fleet.failed.map((e) => `${e.r.name}'s ${e.view.word === "Build failed" ? "build" : "upgrade"} ${e.job?.timed_out ? "timed out" : "failed"}`);
    if (fleet.offline.length) said.push(fleet.offline.length === 1 ? `${fleet.offline[0]?.r.name} is offline` : `${fleet.offline.length} runners are offline`);
    const waiting = fleet.waiting.map((e) => e.r.name);
    const note =
      stale.length && waiting.length
        ? `The ${listed(stale.map((p) => `${p.os}/${p.arch}`))} build ${stale.length === 1 ? "is" : "are"} behind, so ${listed(waiting)} ${waiting.length === 1 ? "waits" : "wait"} for a new one.`
        : "Retry an upgrade, or Diagnose to see why it failed.";
    // It starts with a machine's name: as it is, never capitalised.
    return { title: `${said.join("; ")}.`, note };
  }
  if (building.length && last) {
    return {
      title: `Release run #${last.number} builds ${listed(building.map((p) => `${p.os}/${p.arch}`))} from ${last.sha.slice(0, 7)}.`,
      note: "Runners waiting on a build upgrade as soon as it lands on the server.",
    };
  }
  if (fleet.upgrading.length) {
    const names = fleet.upgrading.map((e) => e.r.name);
    return {
      title: `${listed(names)} ${names.length > 1 ? "are" : "is"} switching to ${newest ?? "the new build"}.`,
      note: "A runner starts the new build beside the old one; the old one keeps its terminals, agents and jobs until they end.",
    };
  }
  if (fleet.behind.length) {
    const names = fleet.behind.map((e) => e.r.name);
    const notify = fleet.behind.every((e) => e.r.policy === "notify");
    const auto = fleet.behind.every((e) => e.r.policy === "auto");
    return {
      title:
        names.length <= 2 && fleet.current > 0 && fleet.current + names.length === data.runners.length && newest
          ? `Every runner but ${listed(names)} runs ${newest}.`
          : `${plural(names.length, "runner")} ${names.length === 1 ? "is" : "are"} behind${newest ? ` ${newest}` : ""}.`,
      note: notify
        ? `${names.length === 1 ? "Its updates are" : "Their updates are"} set to notify, so ${names.length === 1 ? "it waits" : "they wait"} for you.`
        : auto
          ? `${names.length === 1 ? "It upgrades" : "They upgrade"} on ${names.length === 1 ? "its" : "their"} own, or now with Upgrade.`
          : "Upgrade each when you're ready, or all of them at once.",
    };
  }
  if (data.server.newer) return { title: "A newer server build is ready.", note: data.server.note };
  return {
    title:
      fleet.current === data.runners.length
        ? newest
          ? `Every machine runs ${newest}.`
          : "Every machine is up to date."
        : `${fleet.current} of ${plural(data.runners.length, "runner")} run${fleet.current === 1 ? "s" : ""} ${newest ?? "the newest kiln"}.`,
    note: "New builds reach the server; runners download theirs and switch without stopping what they run.",
  };
}

/** What the readout says of a picked runner. */
export function readoutRunner(e: RunnerEntry, waits: UpgradeWaits | null | undefined): Readout {
  const { r, view, job } = e;
  const host = r.hostname.toLowerCase() !== r.name.toLowerCase() ? ` (${r.hostname})` : "";
  const title = `${r.name}${host} · ${r.os}/${r.arch} · ${view.moving ? `${view.from} → ${view.to ?? "a newer build"}` : view.from}`;
  const source = view.moving && r.from ? ` Its build: ${r.from}.` : "";
  const old = r.instances.filter((i) => !i.newest);
  let note: string;
  if (view.tone === "bad" && job) note = `${sentence(failWords(job, waits))}. Retry, or Diagnose to see why.`;
  else if (!r.connected) note = "Offline. It upgrades when it's back, if its updates say so.";
  else if (view.lit)
    note = view.switching
      ? `Its old runner holds ${old.map((i) => holding(i)).join("; ")} while it switches; none of it stops.`
      : "A new runner starts beside the old one and takes new work; nothing it runs stops.";
  else if (view.word === "Waiting") note = r.waits_on ? `Waits for the ${r.os}/${r.arch} build; it upgrades as soon as that lands.` : `${sentence(r.next ?? "waits for a build")}.`;
  else if (view.moving && r.policy === "notify") note = "Its updates are set to notify: Upgrade when you're ready.";
  else if (view.moving && r.next) note = `${sentence(r.next)}.`;
  else if (r.up_to_date) {
    const newest = r.instances.find((i) => i.newest);
    note = view.switching ? `Its old runner still holds ${old.map((i) => holding(i)).join("; ")}; it ends when that does.` : newest ? `Holds ${holding(newest)}.` : `Runs kiln ${r.version}.`;
  } else note = r.next ? `${sentence(r.next)}.` : `Runs kiln ${r.version}.`;
  return { title, note: note + source };
}

/** What the readout says of the server, picked. */
export function readoutServer(s: ServerUpdate, view: ServerView): Readout {
  return { title: `kiln server · ${view.build} · ${s.version} · ${view.runsAs}`, note: s.note };
}

/** What the readout says of a platform's build, picked. */
export function readoutPlatform(p: PlatformView, build: ReleaseBuild | undefined, nowMs: number): Readout {
  const last = build?.last ?? null;
  const commit = build?.commit ? build.commit.slice(0, 7) : null;
  const size = p.binary ? ` · ${formatBytes(p.binary.size)}` : "";
  const title = `${p.os}/${p.arch}${p.abi ? ` (${p.abi})` : ""} · ${buildOf(p)}${size}`;
  const step = p.job?.steps.find((s) => s.status === "running");
  const who = p.runners.length ? `Runners on ${p.os}/${p.arch} download it from the server.` : "No runner of this platform has joined.";
  let note: string;
  if (p.state === "building")
    note = `Release run #${last?.number ?? "?"} is building it${last ? ` from ${last.sha.slice(0, 7)}` : ""}${step ? `: its ${step.name} step${p.job?.agent ? `, on ${p.job.agent}` : ""}` : ""}.`;
  else if (p.state === "queued") note = `Queued in release run #${last?.number ?? "?"}.`;
  else if (p.state === "failed") note = `The release run didn't build it${commit ? ` from ${commit}` : ""}: see its run.`;
  else if (p.state === "behind") note = `Built from an older commit: Build every platform brings it up to ${commit ?? "the newest kiln"}.`;
  else if (p.state === "missing") note = "No build of it here yet: Build every platform, or upload one.";
  else note = `From ${p.binary ? originWords(p.binary) : "?"}, ${relativeTime(p.binary?.at, nowMs)}. ${who}`;
  return { title, note };
}

/* ---------------------------- how long kiln waits ---------------------------- */

export const WAITS: { key: keyof UpgradeWaits; which: NonNullable<UpdateJob["timed_out"]>; label: string; said: string; hint: string; min: number; max: number }[] = [
  {
    key: "upgrade_minutes",
    which: "upgrade",
    label: "Upgrade",
    said: "for an upgrade",
    hint: "for a runner to switch to its new build and report in, once it has no jobs running",
    min: 1,
    max: 24 * 60,
  },
  {
    key: "wait_for_jobs_minutes",
    which: "jobs",
    label: "Running jobs",
    said: "for running jobs",
    hint: "while a runner finishes the jobs it is running; it switches once they end",
    min: 1,
    max: 7 * 24 * 60,
  },
  { key: "build_minutes", which: "build", label: "Build", said: "for a build", hint: "for a machine to build kiln itself, install included", min: 5, max: 24 * 60 },
];

/** "an hour", "6 hours", "90 minutes". */
export function minutesSaid(m: number) {
  if (m === 1) return "a minute";
  if (m === 60) return "an hour";
  return m >= 120 && m % 60 === 0 ? `${m / 60} hours` : `${m} minutes`;
}

export type WaitsDraft = Record<keyof UpgradeWaits, string>;

export const draftOf = (w: UpgradeWaits): WaitsDraft => ({
  upgrade_minutes: String(w.upgrade_minutes),
  wait_for_jobs_minutes: String(w.wait_for_jobs_minutes),
  build_minutes: String(w.build_minutes),
});

/** The waits a draft says, or what's wrong with it. */
export function readDraft(d: WaitsDraft): UpgradeWaits | string {
  const out: UpgradeWaits = { upgrade_minutes: 0, wait_for_jobs_minutes: 0, build_minutes: 0 };
  for (const f of WAITS) {
    const v = Number(d[f.key]);
    if (!Number.isInteger(v) || v < f.min || v > f.max) return `${f.label}: a whole number of minutes from ${f.min} to ${f.max}`;
    out[f.key] = v;
  }
  return out;
}

/* ---------------------------- diagnosing with an agent ---------------------------- */

/** The first message of an agent started on a runner to find out why it isn't upgrading, and fix that. */
export function diagnosisPrompt(r: RunnerUpdate, job: UpdateJob | undefined, newest: FleetTarget | null, findings?: RunnerFinding[]): string {
  const failed = job?.state === "failed" ? job : undefined;
  const at = (t: number) => new Date(t * 1000).toISOString().replace(/\.\d+Z$/, "Z");
  const tail = failed ? failed.log.slice(-40) : [];
  const stuck = (r.ways ?? []).filter((w) => !w.ok);
  const lines = [
    failed
      ? `kiln couldn't upgrade its runner on ${r.name} (${r.hostname}, ${r.os}/${r.arch}). Find out why, and fix what's wrong on the machine so the next upgrade goes through.`
      : `kiln can't upgrade its runner on ${r.name} (${r.hostname}, ${r.os}/${r.arch}). Find out what's in the way, and fix it if it's on the machine.`,
    "",
    "What kiln knows:",
    `- ${r.name} runs kiln ${r.version}${r.commit ? `, commit ${r.commit}` : ""}${r.build ? `, build ${r.build}` : ""}.`,
    newest ? `- The newest kiln is commit ${newest.commit}.` : "",
    failed
      ? `- Its upgrade, ${failed.method === "build" ? `a build of ${failed.commit ? shortSha(failed.commit) : "the newest kiln"} on the machine` : `to build ${failed.to_build ?? "the new build"}`}, started ${at(failed.started_at)} and failed: ${failed.error ?? "kiln wasn't told why"}`
      : "",
    ...stuck.map((w) => `- ${w.way === "build" ? "It can't build kiln itself now" : "It can't download a build now"}: ${w.why}.`),
    r.next ? `- What kiln does next: it ${r.next}.` : "",
    tail.length ? `- Its last lines, as kiln kept them:\n\n${tail.map((l) => `    ${l}`).join("\n")}` : "",
    ...(findings ?? [])
      .filter((f) => f.level !== "ok")
      .map((f) => `- kiln's checks (${f.level}): ${f.title}${f.detail && !f.detail.includes("\n") ? `. ${f.detail}` : ""}${f.fix ? ` Kiln can repair it: ${f.fix.label}.` : ""}`),
    "",
    "Where to look:",
    `- You should be on ${r.name}. If you aren't, run commands there with kiln reach (on: "${r.name}").`,
    r.os === "linux"
      ? `- The runner's log: journalctl --user -u 'kiln-agent*'${failed ? ` --since '${at(failed.started_at).replace("T", " ").replace("Z", "")} UTC'` : ""} (without --user for a system install).`
      : r.os === "macos"
        ? "- The runner's log: kiln.log in its state directory, usually ~/.local/share/kiln/agent/kiln.log."
        : "- The runner's log, in its state directory.",
    failed?.method === "build" ? `- The build's whole output: ~/.cache/kiln/self-build/build-${failed.id}.log (the build works in ~/.cache/kiln/self-build).` : "",
    "- Free disk and memory, and for a build the Rust toolchain (rustup show, cargo --version) and npm.",
    "",
    "Rules:",
    "- Don't stop, restart or reinstall the kiln runner: everything on this machine runs through it, you included.",
    "- Don't upgrade kiln yourself. Fix the cause, then say what was wrong and what you changed; the upgrade is then tried again from System → Updates.",
    "- Ask before anything you can't undo, like deleting files you didn't make or changing system settings.",
  ];
  return lines
    .filter((l, i, all) => l !== "" || (i > 0 && all[i - 1] !== ""))
    .join("\n")
    .trim();
}
