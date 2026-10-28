/**
 * The mock's simulated trunk (plan 01a175b7f1fe7207be20d6176e2c974f, step 2), ported from the CI/CD
 * prototype's engine: commits land through the queue, CI checks them, the release pipeline builds
 * every platform (a newer push supersedes a build under way), staging takes the newest commit that
 * passed and built, production waits for a person, kiln restarts into it under a ten-minute guard,
 * then the runners replant onto it, blue/green. Seeded, so every start tells the same story; it runs
 * on the wall clock, `speed` times faster. Rows and details come out as the server's JSON
 * (crates/kiln/src/ship.rs), typed by ui/src/api/ship.ts.
 */
import type { ShipAction, ShipCommit, ShipDetail, ShipPart, ShipPip, ShipStand } from "../src/api/ship.ts";

// ---- randomness, the same on every start ----
function mulberry32(seed: number) {
  let a = seed;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rnd = mulberry32(20261025);
const R = (a: number, b: number) => a + (b - a) * rnd();
const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rnd() * xs.length)] as T;
const hex = (n: number) => Array.from({ length: n }, () => "0123456789abcdef"[Math.floor(rnd() * 16)]).join("");

interface Who {
  name: string;
  kind: "person" | "agent";
}
const PEOPLE: Who[] = [
  { name: "ada", kind: "person" },
  { name: "ravi", kind: "person" },
  { name: "mei", kind: "person" },
];
const AGENTS: Who[] = ["Kiln Distro", "Visual Bugs", "Merge Work", "CI/CD Integration", "Quiver"].map((name) => ({ name, kind: "agent" }));
const DOCTOR: Who = { name: "ci-doctor", kind: "agent" };
const MSGS = [
  "expose-lan: TCP_NODELAY on the HTTP forwarder",
  "login: phones get the rings below the form",
  "A step's live log keeps up: the masker holds back only what could start a secret",
  "CI: the facade answers a person's permission on the repository (W14-R11)",
  "CI: Docker inside a leg keeps the memory and CPUs it was given (W05-R03)",
  "Runners: a leg's steps find what the user's login shell has on PATH",
  "A finished job runs again elsewhere, or on a runner named (W05-R28)",
  "CP-T3.2: a message reaches its agent once, however it's sent again",
  "kiln-cd's git tests run git free of the machine's config",
  "ui-perf: the UI server's brotli gate",
  "Nodes keep system headroom beyond the reservation",
  "rust auto pipeline: line-tables-only debuginfo, bounded jobs",
  "python auto pipeline: no compile sweep when a repo has no tests",
  "observe e2e: each scenario from a blank page",
  "goal-stack: a session that goes fences its inbox first",
  "deploy: a rollback holds until git changes what the app renders",
  "Releases: a v* tag builds every platform",
  "Mission control: rows hold still while a menu is open",
  "CI scheduler: an exclusive leg holds its CPUs and memory",
  "hyper-rt: the HTTP/2 client keeps one connection per origin",
  "sync: a move carries its folder's homes along",
  "store: group commit rides the next write",
  "docs: node headroom, reconnect and reclaim semantics",
  "review: a stack's checks show one line per diff",
  "updates: a Mac without tools gets its build over the wire",
  "harness: Devin's mid-turn prompts steer without cancelling",
  "kilndb: snapshots stream while writes go on",
];
const FAILS = [
  "rust lint + test: assertion failed in sync::reconnect",
  "rust lint + test: tests/reuse.rs w08_ac06 took no result for the same inputs",
  "rust lint + test: clippy: indexing may panic in ci/plan.rs",
  "ui typecheck + lint + build: Property 'sha' is missing in type 'RunRow'",
  "playwright: Mission control rows hold still (WebKit, phone)",
];
const FLAKES = ["tests/restart.rs w05_r05 failed, then passed on retry", "playwright: login rings settle failed once (WebKit)", "kiln-sim seed 4079 timed out once, passed on retry"];
const ERRS = ["metal-ryzen-02 lost the leg (OOM killer); retried on metal-epyc-01", "the k8s pod was evicted; retried on a worker node"];
const GUARD_FAILS = ["server restarted (OOM)", "memory over 80% for 2m", "2 of 6 runners came back"];
const RUNNERS = ["metal-ryzen-01", "metal-ryzen-02", "metal-epyc-01", "metal-mac-mini", "metal-pi-04", "metal-nuc-old"];
const PLATS = [
  { id: "linux", name: "Linux", dur: [180, 260] as const },
  { id: "windows", name: "Windows", dur: [200, 280] as const },
  { id: "macos", name: "macOS", dur: [300, 460] as const },
];
const SOAK = 90;
const GUARD = 600;

interface Job {
  name: string;
  dur: number;
  state: string;
  start: number;
  end: number;
  note: string;
  fate: string;
  retried: boolean;
}
interface Pipe {
  name: string;
  state: string;
  reused: boolean;
  jobs: Job[];
}
interface Part {
  id: string;
  state: string;
  start: number;
  end: number;
  fate?: string;
}
interface Fate {
  checks: string;
  guard?: string;
  replant?: string;
}
interface Spec {
  author?: Who;
  msg?: string;
  fate?: Partial<Fate>;
  fixFor?: string;
}
interface Commit {
  id: string;
  seq: number;
  sha: string;
  short: string;
  msg: string;
  author: Who;
  at: number;
  tags: string[];
  fixFor: string | null;
  fixedBy: string | null;
  land: { state: string; start: number; end: number };
  checks: { state: string; start: number; end: number; pipes: Pipe[] };
  build: { state: string; by: string | null; plats: Part[]; image: Part };
  staging: { state: string; since: number; with: string | null };
  /** `back`: what production went back to when this one's guard failed. */
  production: { state: string; since: number; with: string | null; by: string | null; start: number; end: number; why: string; back: string | null };
  replant: { state: string };
  fate: Fate;
  handled: { by: string; kind: string } | null;
}
interface Runner {
  name: string;
  on: string | null;
  state: string;
  start: number;
  end: number;
  target: string | null;
  why: string;
  fail: boolean;
}

class Sim {
  t = -10800;
  commits: Commit[] = [];
  byId = new Map<string, Commit>();
  queue: string[] = [];
  staging = { at: null as string | null, next: null as string | null, end: 0 };
  production = { at: null as string | null, next: null as string | null, phase: null as "delivering" | "guard" | null, end: 0, waiting: null as string | null, hold: null as string | null };
  runners: Runner[] = RUNNERS.map((name) => ({
    name,
    on: null,
    state: name === "metal-nuc-old" ? "offline" : "current",
    start: 0,
    end: 0,
    target: null,
    why: name === "metal-nuc-old" ? "offline since 3d" : "",
    fail: false,
  }));
  fixes: { at: number; of: string; author: Who }[] = [];
  pending: { at: number; spec: Spec }[] = [];
  auto: { until: number; delay: number; tags: string[] } | null = null;
  waitingSince = 0;
  approvals = 0;
  msgI = 0;
  nextArrival = Infinity;
  drawFate: () => Fate = () => {
    const x = rnd();
    return { checks: x < 0.09 ? "fail" : x < 0.17 ? "flaky" : x < 0.21 ? "error" : "pass" };
  };

  idx(c: Commit | null) {
    return c ? this.commits.indexOf(c) : -1;
  }
  get(id: string | null | undefined): Commit | null {
    return (id && this.byId.get(id)) || null;
  }
  ok(c: Commit) {
    return c.checks.state === "passed" || c.checks.state === "flaky" || c.checks.state === "reused";
  }

  add(spec: Spec = {}): Commit {
    const id = `c${this.commits.length + 1}`;
    const author = spec.author ?? (rnd() < 0.72 ? pick(AGENTS) : pick(PEOPLE));
    const sha = hex(40);
    const msg = spec.msg ?? MSGS[this.msgI++ % MSGS.length] ?? "";
    const fate: Fate = { ...this.drawFate(), ...spec.fate };
    const docs = /^docs:/.test(msg);
    const failAt = fate.checks === "pass" ? -1 : Math.floor(rnd() * 3);
    const pipes: [string, [string, number][]][] = [
      [
        "ci",
        [
          ["rust lint + test", R(260, 400)],
          ["ui typecheck + lint + build", R(90, 140)],
        ],
      ],
      ["e2e", [["playwright", R(160, 260)]]],
      ["sim", [["kiln-sim seeds", R(110, 170)]]],
    ];
    const c: Commit = {
      id,
      seq: this.commits.length + 1,
      sha,
      short: sha.slice(0, 7),
      msg,
      author,
      at: this.t,
      tags: [],
      fixFor: spec.fixFor ?? null,
      fixedBy: null,
      land: { state: "queued", start: 0, end: 0 },
      checks: {
        state: "none",
        start: 0,
        end: 0,
        pipes: pipes.map(([name, jobs], i) => ({
          name,
          state: "none",
          reused: docs && name !== "e2e",
          jobs: jobs.map(([jn, dur], j) => ({ name: jn, dur, state: "none", start: 0, end: 0, note: "", fate: i === failAt && j === 0 ? fate.checks : "pass", retried: false })),
        })),
      },
      build: { state: "none", by: null, plats: PLATS.map((p) => ({ id: p.id, state: "none", start: 0, end: 0 })), image: { id: "image", state: "none", start: 0, end: 0 } },
      staging: { state: "none", since: 0, with: null },
      production: { state: "none", since: 0, with: null, by: null, start: 0, end: 0, why: "", back: null },
      replant: { state: "none" },
      fate,
      handled: null,
    };
    const failing = c.checks.pipes[failAt]?.jobs[0];
    if (fate.checks === "fail" && failing) failing.note = failAt === 0 ? pick(FAILS.slice(0, 3)) : failAt === 1 ? (FAILS[4] ?? "") : "kiln-sim seed 385 found a lost write after a torn page";
    const mac = c.build.plats[2];
    if (mac && rnd() < 0.06) mac.fate = "fail";
    this.commits.push(c);
    this.byId.set(id, c);
    this.queue.push(id);
    return c;
  }

  /** One step of `dt` simulated seconds. */
  step(dt: number) {
    this.t += dt;
    this.stepArrivals();
    this.stepLand();
    this.stepChecks();
    this.stepBuild();
    this.stepStaging();
    this.stepProduction();
    this.stepReplant();
  }

  stepArrivals() {
    if (this.t >= this.nextArrival) {
      this.add();
      this.nextArrival = this.t + R(260, 480);
    }
    this.pending = this.pending.filter((p) => (this.t < p.at ? true : (this.add(p.spec), false)));
    this.fixes = this.fixes.filter((f) => {
      if (this.t < f.at) return true;
      const of = this.get(f.of);
      const area = (of?.msg.split(":")[0] ?? "ci").toLowerCase().slice(0, 18);
      this.add({ author: f.author, msg: `fix(${area}): what ${of?.short ?? ""} broke`, fixFor: f.of, fate: { checks: "pass" } });
      return false;
    });
  }

  stepLand() {
    const c = this.get(this.queue[0]);
    if (!c) return;
    if (c.land.state === "queued") {
      c.land.state = "running";
      c.land.start = this.t;
      c.land.end = this.t + R(25, 50);
    } else if (c.land.state === "running" && this.t >= c.land.end) {
      c.land.state = "passed";
      this.queue.shift();
      this.landed(c);
    }
  }

  landed(c: Commit) {
    // A newer push supersedes release builds still under way (cancel-in-progress), tags excepted.
    for (const o of this.commits) {
      if (o === c || o.build.state !== "running" || o.tags.length) continue;
      let any = false;
      for (const p of o.build.plats)
        if (p.state !== "passed" && p.state !== "failed") {
          p.state = "superseded";
          any = true;
        }
      if (any) o.build.by = c.id;
    }
    c.checks.state = "running";
    c.checks.start = this.t;
    for (const p of c.checks.pipes) {
      if (p.reused) {
        p.state = "reused";
        for (const j of p.jobs) {
          j.state = "reused";
          j.start = j.end = this.t;
        }
        continue;
      }
      p.state = "running";
      const t0 = this.t + R(3, 20);
      for (const j of p.jobs) {
        j.state = "queued";
        j.start = t0;
        j.end = t0 + j.dur;
      }
    }
    c.build.state = "running";
    for (const p of c.build.plats) {
      const spec = PLATS.find((x) => x.id === p.id);
      p.state = "queued";
      p.start = this.t + R(2, 18);
      p.end = p.start + R(spec?.dur[0] ?? 200, spec?.dur[1] ?? 300);
    }
    const img = c.build.image;
    img.state = "queued";
    img.start = this.t + R(4, 12);
    img.end = img.start + R(110, 160);
  }

  stepChecks() {
    for (const c of this.commits) {
      if (c.checks.state !== "running") continue;
      for (const p of c.checks.pipes) {
        if (p.reused) continue;
        for (const j of p.jobs) {
          if (j.state === "queued" && this.t >= j.start) j.state = "running";
          if (j.state === "running" && this.t >= j.end) {
            if (j.fate === "fail") j.state = "failed";
            else if ((j.fate === "flaky" || j.fate === "error") && !j.retried) {
              j.retried = true;
              j.note = j.fate === "flaky" ? pick(FLAKES) : pick(ERRS);
              j.end = this.t + R(60, 110);
            } else j.state = j.fate === "flaky" ? "flaky" : "passed";
          }
        }
        const js = p.jobs.map((j) => j.state);
        p.state = js.includes("failed") ? "failed" : js.some((s) => s === "running" || s === "queued") ? "running" : js.includes("flaky") ? "flaky" : "passed";
      }
      const ps = c.checks.pipes.map((p) => p.state);
      if (ps.includes("failed")) {
        c.checks.state = "failed";
        c.checks.end = this.t;
        if (c.author.kind === "agent") {
          c.handled = { by: c.author.name, kind: "fixing" };
          this.fixes.push({ at: this.t + R(150, 260), of: c.id, author: c.author });
        }
        if (c.staging.state === "none") c.staging.state = "skipped";
      } else if (!ps.includes("running")) {
        c.checks.state = ps.includes("flaky") ? "flaky" : ps.every((s) => s === "reused") ? "reused" : "passed";
        c.checks.end = this.t;
        const of = this.get(c.fixFor);
        if (of) of.fixedBy = c.id;
      }
    }
  }

  stepBuild() {
    for (const c of this.commits) {
      if (c.build.state !== "running") continue;
      const parts = [...c.build.plats, c.build.image];
      for (const p of parts) {
        if (p.state === "queued" && this.t >= p.start) p.state = "running";
        if (p.state === "running" && this.t >= p.end) p.state = p.fate === "fail" ? "failed" : "passed";
      }
      if (parts.every((p) => p.state === "passed" || p.state === "failed" || p.state === "superseded"))
        c.build.state = c.build.image.state === "failed" ? "failed" : c.build.plats.some((p) => p.state === "failed") ? "partial" : "passed";
    }
  }

  stepStaging() {
    const s = this.staging;
    const next = this.get(s.next);
    if (next) {
      if (this.t >= s.end) {
        const prev = this.get(s.at);
        for (let i = this.idx(prev) + 1; i < this.idx(next); i++) {
          const o = this.commits[i];
          if (o && this.ok(o) && (o.staging.state === "none" || o.staging.state === "waiting")) {
            o.staging.state = "rode";
            o.staging.with = next.id;
          }
        }
        if (prev) prev.staging.state = "superseded";
        s.at = next.id;
        s.next = null;
        next.staging.state = "soaking";
        next.staging.since = this.t;
      }
    } else {
      // The newest commit that passed and built (every platform done or superseded, the image too).
      const cand = [...this.commits].reverse().find((o) => o.land.state === "passed" && this.ok(o) && (o.build.state === "passed" || o.build.state === "partial"));
      if (cand && cand.id !== s.at && this.idx(cand) > this.idx(this.get(s.at))) {
        s.next = cand.id;
        s.end = this.t + R(25, 45);
        cand.staging.state = "running";
      }
    }
    const at = this.get(s.at);
    if (at && at.staging.state === "soaking" && this.t - at.staging.since >= SOAK) at.staging.state = "healthy";
  }

  stepProduction() {
    const p = this.production;
    const st = this.get(this.staging.at);
    if (!p.phase) {
      const eligible = st && st.staging.state === "healthy" && this.idx(st) > this.idx(this.get(p.at)) && !(p.hold && this.idx(st) <= this.idx(this.get(p.hold)));
      if (st && eligible && p.waiting !== st.id) {
        const was = this.get(p.waiting);
        if (was) was.production.state = "none";
        p.waiting = st.id;
        this.waitingSince = this.waitingSince && was ? this.waitingSince : this.t;
        st.production.state = "needs";
        this.markWaiting();
      }
      if (p.waiting && this.auto && this.t < this.auto.until && this.t - this.waitingSince >= this.auto.delay) this.approve("ada");
    } else if (p.phase === "delivering" && this.t >= p.end) {
      const c = this.get(p.next);
      p.phase = "guard";
      p.end = this.t + GUARD;
      if (c) {
        c.production.state = "guard";
        c.production.start = this.t;
        c.production.end = p.end;
      }
    } else if (p.phase === "guard" && this.t >= p.end) {
      const c = this.get(p.next);
      p.phase = null;
      p.next = null;
      if (!c) return;
      if (c.fate.guard === "rollback") {
        c.production.state = "rolledback";
        c.production.why = pick(GUARD_FAILS);
        c.production.back = p.at;
        p.hold = p.at;
        for (const o of this.commits)
          if (o.production.state === "queued" && o.production.with === c.id) {
            o.production.state = "none";
            o.production.with = null;
          }
      } else {
        const prev = this.get(p.at);
        for (let i = this.idx(prev) + 1; i < this.idx(c); i++) {
          const o = this.commits[i];
          if (o && this.ok(o) && o.production.state !== "rolledback") {
            o.production.state = "rode";
            o.production.with = c.id;
          }
        }
        if (prev) prev.production.state = "shipped";
        p.at = c.id;
        p.hold = null;
        c.production.state = "healthy";
        c.production.since = this.t;
        this.startReplant(c);
        const tag = this.auto?.tags.length && rnd() < 0.5 ? this.auto.tags.shift() : undefined;
        if (tag) c.tags.push(tag);
      }
    }
  }

  markWaiting() {
    const w = this.get(this.production.waiting);
    for (const o of this.commits)
      if (o.production.state === "queued") {
        o.production.state = "none";
        o.production.with = null;
      }
    if (!w) return;
    for (let i = this.idx(this.get(this.production.at)) + 1; i < this.idx(w); i++) {
      const o = this.commits[i];
      if (o && this.ok(o) && o.production.state === "none") {
        o.production.state = "queued";
        o.production.with = w.id;
      }
    }
  }

  waitingCount() {
    const w = this.get(this.production.waiting);
    if (!w) return 0;
    let n = 0;
    for (let i = this.idx(this.get(this.production.at)) + 1; i <= this.idx(w); i++) {
      const o = this.commits[i];
      if (o && this.ok(o)) n++;
    }
    return n;
  }

  approve(by: string): boolean {
    const p = this.production;
    const c = this.get(p.waiting);
    if (!c || p.phase) return false;
    // Every fourth delivery fails its guard, so a rollback comes round.
    if (++this.approvals % 4 === 0) c.fate.guard = "rollback";
    p.waiting = null;
    this.waitingSince = 0;
    p.phase = "delivering";
    p.next = c.id;
    p.end = this.t + R(15, 25);
    c.production.state = "running";
    c.production.by = by;
    c.production.start = this.t;
    for (const o of this.commits) if (o.production.state === "queued") o.production.with = c.id;
    return true;
  }

  /** Hold: production keeps what it runs; the next commit staging finds healthy asks again. */
  hold(): boolean {
    const p = this.production;
    const c = this.get(p.waiting);
    if (!c || p.phase) return false;
    c.production.state = "none";
    p.hold = c.id;
    p.waiting = null;
    this.waitingSince = 0;
    this.markWaiting();
    return true;
  }

  startReplant(c: Commit) {
    const macBuilt = c.build.plats.find((p) => p.id === "macos")?.state === "passed";
    for (const o of this.commits) if (o !== c && o.replant.state === "running") o.replant.state = "superseded";
    for (const r of this.runners) {
      if (r.state === "offline") continue;
      r.state = "queued";
      r.target = c.id;
      r.start = this.t + R(5, 50);
      let dur = R(40, 100);
      r.why = "";
      if (r.name === "metal-mac-mini" && !macBuilt) {
        dur = R(200, 300);
        r.why = "builds kiln itself: no macOS build of this commit";
      }
      if (r.name === "metal-pi-04") dur = R(80, 150);
      r.end = r.start + dur;
      r.fail = r.name === "metal-pi-04" && c.fate.replant === "timeout";
    }
    c.replant.state = "running";
  }

  stepReplant() {
    let changed = false;
    for (const r of this.runners) {
      if (r.state === "queued" && this.t >= r.start) {
        r.state = "switching";
        changed = true;
      }
      if (r.state === "switching" && this.t >= r.end) {
        if (r.fail) {
          r.state = "failed";
          r.why = "upgrade timed out: no report within 15 minutes";
        } else {
          r.state = "current";
          r.on = r.target;
          r.why = "";
        }
        changed = true;
      }
    }
    const c = this.get(this.production.at);
    if (!changed || !c || c.replant.state !== "running") return;
    const live = this.runners.filter((r) => r.state !== "offline");
    if (live.every((r) => r.state === "current" || r.state === "failed")) c.replant.state = live.some((r) => r.state === "failed") ? "partial" : "passed";
  }

  retry(id: string): boolean {
    const c = this.get(id);
    if (!c || c.checks.state !== "failed") return false;
    for (const p of c.checks.pipes)
      for (const j of p.jobs)
        if (j.state === "failed") {
          j.state = "queued";
          j.fate = "pass";
          j.start = this.t + 3;
          j.end = this.t + 3 + j.dur * 0.8;
          j.note = "";
        }
    c.checks.state = "running";
    c.staging.state = "none";
    c.handled = { by: "you", kind: "retried" };
    return true;
  }

  handOff(id: string): boolean {
    const c = this.get(id);
    if (!c || c.checks.state !== "failed") return false;
    c.handled = { by: DOCTOR.name, kind: "fixing" };
    this.fixes.push({ at: this.t + R(120, 200), of: c.id, author: DOCTOR });
    return true;
  }

  retryRunner(name: string): boolean {
    const r = this.runners.find((x) => x.name === name);
    if (!r || r.state !== "failed") return false;
    r.state = "switching";
    r.start = this.t;
    r.end = this.t + R(40, 80);
    r.fail = false;
    r.why = "retrying the upgrade";
    const c = this.get(r.target);
    if (c) c.replant.state = "running";
    return true;
  }

  /** A burst of agent commits, `n` of them a few seconds apart. */
  burst(n: number) {
    for (let i = 0; i < n; i++) this.pending.push({ at: this.t + 4 + i * R(4, 10), spec: { author: AGENTS[i % AGENTS.length] } });
  }

  /** Where a commit stands, in Mission control's words. */
  stand(c: Commit): string {
    const p = this.production;
    if (p.waiting === c.id) return "needs";
    if (c.checks.state === "failed" && c.author.kind === "person" && !c.handled) return "needs";
    // A rollback needs someone until production ships past it; then it stands rolled back.
    if (c.production.state === "rolledback") return this.idx(this.get(p.at)) > this.idx(c) ? "rolledback" : "needs";
    if (p.at === c.id && this.runners.some((r) => r.state === "failed" && r.target === c.id)) return "needs";
    if (c.checks.state === "failed") return c.fixedBy ? "superseded" : "failing";
    if (c.land.state !== "passed") return "landing";
    if (c.checks.state === "running") return "checking";
    if (c.production.state === "running" || c.production.state === "guard") return "shipping";
    if (p.at === c.id && c.replant.state === "running") return "replanting";
    if (c.production.state === "healthy" || c.production.state === "shipped" || c.production.state === "rode") return "shipped";
    if (c.production.state === "queued") return "waiting";
    if (["running", "soaking", "healthy", "rode"].includes(c.staging.state)) return "staging";
    if (c.build.state === "running") return "building";
    return "staging";
  }

  /** The one line that matters for this commit now. */
  line(c: Commit): string {
    const p = this.production;
    const s = this.stand(c);
    const short = (id: string | null) => this.get(id)?.short ?? "";
    const live = this.runners.filter((r) => r.state !== "offline");
    if (p.waiting === c.id) {
      const n = this.waitingCount();
      return `Approve for production · ${n} commit${n === 1 ? "" : "s"}`;
    }
    if (c.checks.state === "failed") {
      const j = c.checks.pipes.flatMap((x) => x.jobs).find((x) => x.state === "failed");
      const why = j ? j.note.replace(/^[^:]+: /, "") : "checks failed";
      if (c.fixedBy) return `Fixed in ${short(c.fixedBy)}`;
      if (c.handled?.kind === "fixing") return `${c.handled.by} fixing · ${why}`;
      return `${j?.name ?? "checks"} · ${why}`;
    }
    if (c.production.state === "rolledback") return c.production.back ? `Rolled back to ${short(c.production.back)} · ${c.production.why}` : `Rolled back · ${c.production.why}`;
    const fr = this.runners.find((r) => r.state === "failed" && r.target === c.id);
    if (fr && p.at === c.id) return `${fr.name.replace("metal-", "")} didn't replant · ${fr.why.split(":")[0]}`;
    if (s === "landing") return c.land.state === "running" ? "Testing against trunk" : `Queued · ${this.queue.indexOf(c.id)} ahead`;
    if (s === "checking") {
      const retry = c.checks.pipes.flatMap((x) => x.jobs).find((x) => x.retried && x.state === "running");
      if (retry) return `Retrying · ${retry.note.split(";")[0]}`;
      return (
        c.checks.pipes
          .filter((x) => x.state === "running")
          .map((x) => x.name)
          .join(" · ") || "Starting"
      );
    }
    if (s === "shipping") return c.production.state === "running" ? "Restarting into it" : "Guard watching";
    if (s === "replanting") return `Runners ${live.filter((r) => r.state === "current" && r.on === c.id).length}/${live.length}`;
    if (s === "shipped") {
      if (c.production.state === "rode") return `With ${short(c.production.with)}`;
      if (c.production.state === "healthy") return `In production · runners ${live.filter((r) => r.on === c.id).length}/${live.length}`;
      return "Superseded";
    }
    if (s === "waiting") return `Rides with ${short(c.production.with)}`;
    if (s === "staging") {
      if (c.staging.state === "running") return "Syncing";
      if (c.staging.state === "rode") return `With ${short(c.staging.with)}`;
      if (c.staging.state === "soaking") return "Soaking";
      if (this.staging.at === c.id) return "Healthy";
      return c.staging.state === "superseded" ? "Passed" : "Built";
    }
    if (s === "building")
      return `Building ${c.build.plats
        .filter((x) => x.state !== "passed")
        .map((x) => PLATS.find((y) => y.id === x.id)?.name ?? x.id)
        .join(", ")}`;
    return "";
  }

  /** The seven pips of a commit's journey, as the server names them: Review first (none on trunk). */
  pips(c: Commit): ShipPip[] {
    const carrier = this.get(c.production.with) ?? c;
    const prog = (from: number, len: number) => Math.max(0, Math.min(1, (this.t - from) / len));
    const st = c.staging.state;
    const staging = ["running", "soaking", "healthy", "rode"].includes(st) ? st : st === "superseded" ? "passed" : "none";
    const ps = c.production.state;
    const production = ps === "shipped" ? "passed" : ["needs", "running", "guard", "healthy", "rode", "queued", "rolledback"].includes(ps) ? ps : "none";
    const rs = carrier.replant.state;
    const replant = ["healthy", "passed", "rode"].includes(production)
      ? rs === "running"
        ? "replanting"
        : rs === "passed" || rs === "superseded"
          ? "replanted"
          : rs === "partial"
            ? "partial"
            : "none"
      : "none";
    const pip = (stage: string, state: string, p?: number): ShipPip => (p === undefined ? { stage, state } : { stage, state, p });
    return [
      pip("review", "none"),
      pip("land", c.land.state),
      pip("checks", c.checks.state),
      pip("build", c.build.state),
      pip("staging", staging, st === "soaking" ? prog(c.staging.since, SOAK) : undefined),
      pip("production", production, ps === "guard" ? prog(c.production.start, GUARD) : undefined),
      pip("replant", replant),
    ];
  }

  /** Fills the trunk's history, then the live minutes before now: production one approval behind,
   * a person's commit failing, an agent fixing its own, a runner that didn't replant. */
  seed() {
    this.auto = { until: -1900, delay: 520, tags: ["v0.13.0", "v0.14.0"] };
    let t = this.t;
    while (t < -2200) {
      this.pending.push({ at: t, spec: {} });
      t += R(360, 640);
    }
    const a = (i: number) => AGENTS[i];
    const live: [number, Spec][] = [
      [-2100, { author: a(0), fate: { checks: "pass", replant: "timeout" } }],
      [-1500, { author: a(1), fate: { checks: "flaky" } }],
      [-1180, { author: PEOPLE[2], fate: { checks: "fail" } }],
      [-980, { author: a(2), fate: { checks: "pass" } }],
      [-760, { author: a(4), fate: { checks: "fail" } }],
      [-520, { author: a(3), fate: { checks: "pass" } }],
      [-300, { author: PEOPLE[0], fate: { checks: "pass" }, msg: "docs: what the guard watches, and for how long" }],
      [-90, { author: a(0), fate: { checks: "pass" } }],
      [-25, { author: a(1), fate: { checks: "pass" } }],
    ];
    for (const [at, spec] of live) this.pending.push({ at, spec });
    this.pending.sort((x, y) => x.at - y.at);
    // The history's commits fail and flake less, so its story reads plainly.
    const draw = this.drawFate;
    this.drawFate = () => ({ checks: rnd() < 0.06 ? "flaky" : "pass" });
    while (this.t < 0) this.step(5);
    this.drawFate = draw;
    this.auto = null;
    this.nextArrival = this.t + R(160, 240);
    for (const c of this.commits) if (c.checks.state === "failed" && c.author.kind === "person") c.handled = null;
    // One runner that didn't take the last deploy, so replant has something to say at once.
    const at = this.get(this.production.at);
    const pi = this.runners.find((r) => r.name === "metal-pi-04");
    if (at && pi && pi.state !== "failed") {
      pi.state = "failed";
      pi.target = at.id;
      pi.end = this.t - 240;
      pi.why = "upgrade timed out: no report within 15 minutes";
      at.replant.state = "partial";
    }
  }
}

const STAND: Record<string, ShipStand> = {
  needs: "needs_you",
  failing: "failing",
  landing: "landing",
  checking: "checking",
  building: "building",
  staging: "in_staging",
  waiting: "waiting",
  shipping: "shipping",
  replanting: "replanting",
  shipped: "shipped",
  rolledback: "rolled_back",
  superseded: "fixed",
};
const part = (s: string): ShipPart => (s === "running" || s === "passed" || s === "failed" || s === "superseded" ? s : "queued");

/** kiln's own trunk, simulated: rows and details as the server sends them, and what people do. */
export class Trunk {
  private readonly sim = new Sim();
  private wall = Date.now();

  constructor(
    readonly repo: string,
    private readonly speed: number,
  ) {
    this.sim.seed();
  }

  /** Catches the simulation up with the wall clock (at most six simulated hours at a time). */
  private advance() {
    const now = Date.now();
    let dt = Math.min(((now - this.wall) / 1000) * this.speed, 6 * 3600);
    this.wall = now;
    while (dt > 0) {
      this.sim.step(Math.min(5, dt));
      dt -= 5;
    }
  }

  private find(sha: string): Commit | null {
    return sha.length >= 7 ? (this.sim.commits.find((c) => c.sha.startsWith(sha)) ?? null) : null;
  }

  private row(c: Commit): ShipCommit {
    const sim = this.sim;
    const run = `ship-run-${c.id}`;
    const actions: ShipAction[] = [];
    if (sim.production.waiting === c.id && !sim.production.phase) {
      actions.push({ name: "approve", method: "POST", path: `/api/deploy/releases/ship-rel-${c.id}/approve`, body: {} });
      actions.push({ name: "hold", method: "POST", path: `/api/deploy/releases/ship-rel-${c.id}/reject`, body: {} });
    }
    if (c.checks.state === "failed" && !c.fixedBy) {
      actions.push({ name: "retry", method: "POST", path: `/api/ci/runs/${run}/rerun`, body: { mode: "failed" } });
      actions.push({ name: "hand_off", method: "POST", path: "/api/attachments", body: { repo: this.repo, run } });
    }
    if (sim.production.at === c.id)
      for (const r of sim.runners) if (r.state === "failed" && r.target === c.id) actions.push({ name: "retry_runner", method: "POST", path: `/api/updates/runners/${r.name}`, body: null });
    const environments = [
      ...(sim.staging.at === c.id ? [{ environment: "staging", production: false }] : []),
      ...(sim.production.at === c.id ? [{ environment: "production", production: true }] : []),
    ];
    return {
      repo: this.repo,
      sha: c.sha,
      short: c.short,
      message: c.msg,
      author: c.author.name,
      branch: "main",
      at: Math.floor(Date.now() / 1000) - Math.round(sim.t - c.at),
      stand: STAND[sim.stand(c)] ?? "waiting",
      line: sim.line(c),
      pips: sim.pips(c),
      environments,
      tags: [...c.tags],
      runs: c.land.state === "passed" ? [{ id: run, pipeline: "ci" }] : [],
      cursor: 1000 + c.seq,
      kind: "commit",
      host: null,
      change: null,
      fold_under: null,
      actions,
    };
  }

  /** Every commit, newest first. */
  commits(): ShipCommit[] {
    this.advance();
    return this.sim.commits.map((c) => this.row(c)).reverse();
  }

  /** One commit with the records its stand came from, or null when it isn't the trunk's. */
  detail(sha: string): ShipDetail | null {
    this.advance();
    const c = this.find(sha);
    if (!c) return null;
    const sim = this.sim;
    const now = Math.floor(Date.now() / 1000);
    const wall = (t: number) => now - Math.round(sim.t - t);
    const leg = (j: Job) => (j.state === "none" ? "queued" : j.state);
    const deployments = [
      ...(["running", "soaking", "healthy", "rode", "superseded"].includes(c.staging.state)
        ? [{ environment: "staging", production: false, state: c.staging.state === "superseded" ? "inactive" : "success", description: c.staging.state, at: wall(c.staging.since || c.at) }]
        : []),
      ...(["running", "guard", "healthy", "shipped", "rolledback"].includes(c.production.state)
        ? [{ environment: "production", production: true, state: c.production.state === "rolledback" ? "failure" : "success", description: c.production.state, at: wall(c.production.start || c.at) }]
        : []),
    ];
    const releases = [
      ...(sim.production.waiting === c.id ? [{ state: "waiting", message: `Approve for production · ${sim.waitingCount()} commits`, rolled_back: false }] : []),
      ...(c.production.by ? [{ state: c.production.state === "rolledback" ? "failed" : "approved", message: `approved by ${c.production.by}`, rolled_back: c.production.state === "rolledback" }] : []),
    ];
    return {
      ...this.row(c),
      checks: c.checks.pipes.map((p) => ({
        pipeline: p.name,
        legs: p.jobs.map((j, i) => ({
          name: j.name,
          state: leg(j),
          attempts: j.retried ? 2 : 1,
          note: j.note,
          // Once it has run, the job it ran as and that job's run (Output streams its log).
          ...(j.state === "none" || j.state === "queued"
            ? {}
            : { job: `ship-job-${c.id}-${p.name}-${i}`, run: `ship-run-${c.id}`, start: wall(j.start), ...(j.state === "running" ? {} : { end: wall(j.end) }) }),
        })),
      })),
      build: {
        platforms: c.build.plats.map((p) => ({ name: PLATS.find((x) => x.id === p.id)?.name ?? p.id, state: part(p.state) })),
        image: { state: part(c.build.image.state), digest: c.build.image.state === "passed" ? `sha256:${c.sha.slice(0, 12)}` : null },
      },
      deployments,
      releases,
      replant: sim.runners.filter((r) => r.target === c.id).map((r) => ({ name: r.name, state: r.state, why: r.why })),
      // A trunk commit's Review lane has the one mark, its push — unsigned until W27-R03.
      review: [{ id: `push:${c.sha}`, kind: "push", at: wall(c.at), end: wall(c.at), by: c.author.name, signer: null, revision: null, state: "passed" }],
    };
  }

  approve(release: string) {
    const c = this.sim.get(release.replace(/^ship-rel-/, ""));
    this.advance();
    return !!c && this.sim.production.waiting === c.id && this.sim.approve("you");
  }
  hold(release: string) {
    const c = this.sim.get(release.replace(/^ship-rel-/, ""));
    this.advance();
    return !!c && this.sim.production.waiting === c.id && this.sim.hold();
  }
  retry(run: string) {
    this.advance();
    return this.sim.retry(run.replace(/^ship-run-/, ""));
  }
  handOff(run: string) {
    this.advance();
    return this.sim.handOff(run.replace(/^ship-run-/, ""));
  }
  retryRunner(name: string) {
    this.advance();
    return this.sim.retryRunner(name);
  }
  /** Whether `name` is one of the trunk's runners whose upgrade failed (so its retry is ours). */
  failedRunner(name: string) {
    return this.sim.runners.some((r) => r.name === name && r.state === "failed");
  }
  burst(n: number) {
    this.advance();
    this.sim.burst(n);
  }
}
