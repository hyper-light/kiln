import assert from "node:assert/strict";
import { test } from "node:test";
import type { Job, KilnBinary, Run, RunnerUpdate, ServerUpdate, UpdateJob, UpdatesOverview } from "../src/api/types.ts";
import {
  buildStatus,
  diagnosisPrompt,
  failWords,
  fleetOf,
  fleetSummary,
  headline,
  holding,
  minutesSaid,
  platformCable,
  platformJob,
  platformLine,
  platformOf,
  platformsOf,
  readDraft,
  readoutAll,
  readoutRunner,
  runnerView,
  serverView,
  upgradeOffer,
  wayLabel,
} from "../src/views/updates/words.ts";

const NOW = 1_800_000_000_000;
const S = NOW / 1000;
const NEWEST = "b0de36d1234567890abcdef1234567890abcdef0";
const OLD = "56f4409aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

function runner(name: string, over: Partial<RunnerUpdate> = {}): RunnerUpdate {
  return {
    name,
    policy: "auto",
    locked: false,
    from: null,
    hostname: name,
    os: "linux",
    arch: "x86_64",
    connected: true,
    version: "0.2.0",
    target: "x86_64-unknown-linux-musl",
    target_build: "a1b2c3d4e5f6",
    build: "a1b2c3d4e5f6",
    up_to_date: true,
    method: "native",
    terminal: true,
    instances: [],
    commit: NEWEST,
    standing: "current",
    next: null,
    how: null,
    ways: [],
    ...over,
  };
}

const behind = (name: string, over: Partial<RunnerUpdate> = {}) =>
  runner(name, {
    up_to_date: false,
    commit: OLD,
    build: "0f0f0f0f0f0f",
    standing: "behind",
    how: "download",
    next: "downloads build a1b2c3d4e5f6 (b0de36d12345)",
    ways: [{ way: "download", ok: true, why: "downloads build a1b2c3d4e5f6" }],
    ...over,
  });

function job(target: string, over: Partial<UpdateJob> = {}): UpdateJob {
  return {
    id: `j-${target}`,
    kind: "runner",
    target,
    method: "native",
    state: "running",
    started_at: S - 60,
    ended_at: null,
    from_build: "0f0f",
    to_build: "a1b2",
    commit: NEWEST,
    reached: false,
    log: [],
    error: null,
    ...over,
  };
}

const server: ServerUpdate = {
  version: "0.2.0",
  build: "4a1c0f2e9b3d",
  target: "x86_64-unknown-linux-gnu",
  mode: "kubernetes",
  deployment: "kiln-production",
  namespace: "default",
  image: { image: "kiln", node: "adacachy", build: "4a1c0f2e9b3d", newer: false, error: null, checked_at: S - 120 },
  checking: false,
  newer: false,
  action: "restart",
  replacement: null,
  release: { latest: "0.2.0", newer: false, url: null, error: null, checked_at: S - 120 },
  note: "Up to date: the kiln image on adacachy holds this build.",
};

const bin = (target: string, over: Partial<KilnBinary> = {}): KilnBinary => ({
  target,
  sha256: "ab".repeat(32),
  size: 41_800_000,
  build: "a1b2c3d4e5f6",
  origin: "built",
  at: S - 600,
  run: 412,
  sha: NEWEST,
  ...over,
});

const run = (over: Partial<Run> = {}): Run => ({
  id: "run-413",
  repo_id: "r",
  repo_name: "hyper-light/kiln",
  number: 413,
  pipeline: "release",
  event: "manual",
  branch: "main",
  sha: NEWEST,
  message: "",
  author: "",
  status: "running",
  created_at: S - 100,
  started_at: S - 90,
  finished_at: null,
  error: null,
  concurrency_group: null,
  ...over,
});

function overview(over: Partial<UpdatesOverview> = {}): UpdatesOverview {
  return {
    server,
    runners: [runner("adacachy"), runner("fedora")],
    jobs: [],
    binaries: [bin("x86_64-unknown-linux-gnu", { origin: "server", sha: undefined, run: undefined }), bin("aarch64-unknown-linux-musl"), bin("aarch64-apple-darwin")],
    targets: ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"],
    build: { repo: "hyper-light/kiln", pipeline: "release", last: run({ status: "succeeded", finished_at: S - 700 }), commit: NEWEST, in_step: true, behind: [], keeps_in_step: true },
    newest: { commit: NEWEST, from: "server", since: S - 840 },
    follow: true,
    waits: { now: { upgrade_minutes: 15, wait_for_jobs_minutes: 60, build_minutes: 45 }, config: { upgrade_minutes: 15, wait_for_jobs_minutes: 60, build_minutes: 45 }, changed: false },
    ...over,
  };
}

test("platforms read from their targets; each has the release job that builds it", () => {
  assert.deepEqual(platformOf("x86_64-unknown-linux-musl"), { os: "linux", arch: "x86_64", abi: "musl" });
  assert.deepEqual(platformOf("aarch64-apple-darwin"), { os: "macos", arch: "aarch64", abi: "" });
  assert.deepEqual(platformOf("x86_64-pc-windows-msvc"), { os: "windows", arch: "x86_64", abi: "msvc" });
  assert.equal(platformJob("aarch64-apple-darwin"), "macos");
  assert.equal(platformJob("x86_64-pc-windows-msvc"), "windows");
  assert.equal(platformJob("aarch64-unknown-linux-musl"), "linux");
});

test("every platform in step: ordered linux, macos, windows, x86_64 first, the server's own named so", () => {
  const ps = platformsOf(overview(), null);
  assert.deepEqual(
    ps.map((p) => p.label),
    ["linux · x86_64", "linux · aarch64", "macos · aarch64"],
  );
  assert.ok(ps.every((p) => p.state === "current" && p.tone === "ok"));
  assert.equal(platformLine(ps[0]!, null), "b0de36d · server's own", "the server's own build runs the newest commit");
  assert.equal(platformLine(ps[1]!, null), "b0de36d · run #412");
  const s = buildStatus(overview().build!, ps, NOW);
  assert.equal(s.tone, "ok");
  assert.equal(s.before, "All built from b0de36d");
});

test("two builds of one platform's arch read with their ABI", () => {
  const ps = platformsOf(overview({ binaries: [bin("x86_64-unknown-linux-gnu", { origin: "server", sha: undefined }), bin("x86_64-unknown-linux-musl")] }), null);
  assert.deepEqual(
    ps.map((p) => p.label),
    ["linux · x86_64 · gnu", "linux · x86_64 · musl"],
  );
});

test("a release run under way: each platform says what its job does, and only a running job lights", () => {
  const data = overview({
    binaries: [bin("x86_64-unknown-linux-gnu", { origin: "server", sha: undefined }), bin("aarch64-unknown-linux-musl", { sha: OLD }), bin("aarch64-apple-darwin", { sha: OLD })],
    build: {
      repo: "hyper-light/kiln",
      pipeline: "release",
      last: run(),
      commit: NEWEST,
      in_step: false,
      behind: ["aarch64-unknown-linux-musl", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"],
      keeps_in_step: true,
    },
  });
  const jobs = [
    { key: "linux", status: "running", steps: [] },
    { key: "macos", status: "queued", steps: [] },
    { key: "windows", status: "failed", steps: [] },
  ] as unknown as Job[];
  const ps = platformsOf(data, jobs);
  const by = Object.fromEntries(ps.map((p) => [p.target, p]));
  assert.equal(by["x86_64-unknown-linux-gnu"]!.state, "current");
  assert.equal(by["aarch64-unknown-linux-musl"]!.state, "building");
  assert.equal(by["aarch64-unknown-linux-musl"]!.lit, true);
  assert.equal(by["aarch64-apple-darwin"]!.state, "queued");
  assert.equal(by["aarch64-apple-darwin"]!.lit, false);
  assert.equal(platformCable(by["aarch64-apple-darwin"]!), "off");
  assert.equal(by["x86_64-pc-windows-msvc"]!.state, "failed", "a platform without a build, whose job failed");
  assert.equal(by["x86_64-pc-windows-msvc"]!.binary, null);
  assert.equal(platformLine(by["aarch64-unknown-linux-musl"]!, data.build!.last!), "building · run #413");
  const s = buildStatus(data.build!, ps, NOW);
  assert.equal(s.tone, "run");
  assert.equal(`${s.before}run #${s.run?.number}${s.after}`, "Release run #413 · 0 of 3 built");
  // Without the run's jobs read yet: building, but nothing lit.
  const blind = platformsOf(data, null);
  assert.ok(blind.filter((p) => p.target !== "x86_64-unknown-linux-gnu").every((p) => p.state === "building" && !p.lit));
});

test("behind, failed, never built: what the Builds section says", () => {
  const b = { repo: "hyper-light/kiln", pipeline: "release", commit: NEWEST, in_step: false, behind: ["aarch64-apple-darwin"], keeps_in_step: true };
  const ps = platformsOf(overview({ build: { ...b, last: null } }), null);
  const behindOne = buildStatus({ ...b, last: null }, ps, NOW);
  assert.equal(behindOne.tone, "warn");
  assert.equal(behindOne.before, "macos/aarch64 behind");
  assert.equal(behindOne.more, "Not built from b0de36d, the newest kiln: kiln builds them on its own.");
  const failed = buildStatus({ ...b, last: run({ status: "failed", finished_at: S - 30 }) }, ps, NOW);
  assert.equal(failed.tone, "bad");
  assert.equal(failed.before, "Build failed · ");
  assert.match(failed.more ?? "", /kiln tries again in a while/);
  const never = buildStatus({ repo: "hyper-light/kiln", pipeline: "release", last: null, commit: null }, ps, NOW);
  assert.equal(never.before, "Not built here yet");
  assert.match(never.more ?? "", /hyper-light\/kiln's release pipeline does/);
  assert.equal(buildStatus({ ...b, last: null, error: "no repository here" }, ps, NOW).more, "no repository here");
});

test("a runner's state: current, behind, upgrading, switching, failed, waiting, offline", () => {
  const waits = { upgrade_minutes: 15, wait_for_jobs_minutes: 60, build_minutes: 45 };
  const target = { commit: NEWEST, from: "server", since: S - 840 };
  const cur = runnerView(
    runner("a", { instances: [{ id: "x", legacy: false, version: "0.2.0", created_at: 0, connected_at: 0, pid: 1, newest: true, terminals: 6, agents: 3, jobs: 0 }] }),
    undefined,
    target,
    waits,
    NOW,
  );
  assert.deepEqual([cur.tone, cur.word, cur.sub, cur.scene], ["ok", "Up to date", "holds 6 terminals · 3 agents", "b0de36d · current"]);
  const recent = runnerView(runner("a"), job("a", { state: "done", ended_at: S - 240 }), target, waits, NOW);
  assert.equal(recent.sub, "switched 4m ago");
  const b = runnerView(behind("b"), undefined, target, waits, NOW);
  assert.deepEqual([b.tone, b.word, b.cable, b.scene, b.moving], ["warn", "Behind", "warn", "56f4409 → b0de36d", true]);
  assert.equal(b.sub, "downloads build a1b2c3d4e5f6 (b0de36d12345)");
  const up = runnerView(behind("c"), job("c"), target, waits, NOW);
  assert.deepEqual([up.tone, up.word, up.lit, up.sub], ["run", "Upgrading", true, "upgrading to b0de36d"]);
  const two = [
    { id: "n", legacy: false, version: "0.2.0", created_at: 0, connected_at: 0, pid: 2, newest: true, terminals: 0, agents: 0, jobs: 0 },
    { id: "o", legacy: false, version: "0.1.0", created_at: 0, connected_at: 0, pid: 1, newest: false, terminals: 2, agents: 0, jobs: 1 },
  ];
  const sw = runnerView(behind("d", { instances: two }), job("d"), target, waits, NOW);
  assert.deepEqual([sw.switching, sw.sub, sw.scene], [true, "old runner holds 2 terminals · 1 job", "switching · nothing stops"]);
  const failed = runnerView(behind("e"), job("e", { state: "failed", ended_at: S - 100, timed_out: "upgrade", error: "the new runner didn't report in within 15 minutes" }), target, waits, NOW);
  assert.deepEqual([failed.tone, failed.word, failed.sub, failed.scene, failed.cable], ["bad", "Upgrade failed", "didn't report in within 15 min", "upgrade timed out", "bad"]);
  const built = runnerView(behind("f"), job("f", { method: "build", state: "failed", error: "kiln did not build (its full output is x.log)" }), target, waits, NOW);
  assert.deepEqual([built.word, built.sub], ["Build failed", "kiln did not build"]);
  const wait = runnerView(behind("g", { os: "macos", arch: "aarch64", how: "wait", waits_on: { run: "r", number: 413, step: "build", since: S - 60, on: "mac" } }), undefined, target, waits, NOW);
  assert.deepEqual([wait.word, wait.sub, wait.scene, wait.cable], ["Waiting", "for the macos/aarch64 build", "waits for the macos build", "wait"]);
  const off = runnerView(behind("h", { connected: false }), undefined, target, waits, NOW);
  assert.deepEqual([off.tone, off.word, off.cable, off.scene], ["off", "Offline", "off", "offline · 56f4409"]);
});

test("the ways a runner is upgraded: the failed one is tried again first, the other offered instead", () => {
  const r = behind("mac", {
    ways: [
      { way: "download", ok: true, why: "downloads build x" },
      { way: "build", ok: true, why: "builds it" },
    ],
  });
  const offer = upgradeOffer(r, job("mac", { method: "build", state: "failed" }));
  assert.deepEqual(
    offer.open.map((w) => w.way),
    ["build", "download"],
  );
  assert.equal(wayLabel("build", true, true), "Retry build");
  assert.equal(wayLabel("download", false, false), "Download instead");
  assert.equal(offer.actionable, true);
  const stuck = upgradeOffer(behind("x", { ways: [{ way: "download", ok: false, why: "no binary yet" }], target_build: null }), undefined);
  assert.equal(stuck.stuck.length, 1);
  assert.equal(stuck.missing, true);
  assert.equal(upgradeOffer(behind("y"), job("y")).actionable, false, "not while it upgrades");
  assert.equal(upgradeOffer(behind("z", { locked: true }), undefined).behind, false, "a locked one isn't upgraded from here");
});

test("the server: newest, an upgrade ready, restarting", () => {
  const ok = serverView(server, undefined, false, { commit: NEWEST, from: "server", since: S }, NOW);
  assert.deepEqual([ok.tone, ok.word, ok.sub, ok.build, ok.policy, ok.notable], ["ok", "Newest", "checked 2m ago", "b0de36d", "Follows its image", false]);
  assert.equal(ok.scene, "0.2.0 · b0de36d · newest");
  const ready = serverView({ ...server, newer: true, image: { ...server.image!, build: "9f9f9f9f9f9f", newer: true } }, undefined, false, null, NOW);
  assert.deepEqual([ready.tone, ready.word, ready.sub, ready.notable], ["warn", "Upgrade ready", "the image holds 9f9f9f9f9f9f", true]);
  const restarting = serverView(server, undefined, true, null, NOW);
  assert.deepEqual([restarting.tone, restarting.word], ["run", "Restarting"]);
  const svc = serverView({ ...server, mode: "service", image: null, release: { ...server.release, error: "rate limited" } }, undefined, false, null, NOW);
  assert.deepEqual([svc.word, svc.sub, svc.policy], ["Can't check", "rate limited", "Installed from here"]);
});

test("the fleet: its line, its headline and its readout", () => {
  const all = overview();
  const fleet = fleetOf(all, [], NOW);
  const sv = serverView(server, undefined, false, all.newest ?? null, NOW);
  const ps = platformsOf(all, null);
  assert.equal(fleetSummary(all, fleet), "2 of 2 runners on b0de36d · each follows the newest kiln on its own");
  assert.deepEqual(headline(all, fleet, sv, ps), { tone: "ok", text: "Every machine runs b0de36d" });
  assert.equal(readoutAll(all, fleet, ps).title, "Every machine runs b0de36d.");

  const mixed = overview({
    runners: [runner("adacachy"), behind("fedora", { policy: "notify" }), behind("jupiter", { connected: false })],
    jobs: [job("adacachy", { state: "done", ended_at: S - 30 })],
  });
  const f2 = fleetOf(mixed, mixed.jobs, NOW);
  assert.equal(fleetSummary(mixed, f2), "1 of 3 runners on b0de36d · 1 ready to upgrade · 1 offline");
  assert.deepEqual(headline(mixed, f2, sv, ps), { tone: "warn", text: "1 runner behind" });
  const ro = readoutAll(mixed, f2, ps);
  assert.equal(ro.title, "1 runner is behind b0de36d.");
  assert.equal(ro.note, "Its updates are set to notify, so it waits for you.");

  const failing = overview({ runners: [runner("a"), behind("jupiter")], jobs: [job("jupiter", { state: "failed", ended_at: S - 50, timed_out: "upgrade" })] });
  const f3 = fleetOf(failing, failing.jobs, NOW);
  assert.deepEqual(headline(failing, f3, sv, ps), { tone: "bad", text: "jupiter's upgrade failed" });
  assert.equal(readoutAll(failing, f3, ps).title, "jupiter's upgrade timed out.", "a machine's name is never capitalised");
  const picked = readoutRunner(f3.runners[1]!, failing.waits?.now);
  assert.equal(picked.title, "jupiter · linux/x86_64 · 56f4409 → b0de36d");
  assert.equal(picked.note, "Didn't report in within 15 min. Retry, or Diagnose to see why.");

  const rolling = overview({ runners: [runner("a"), behind("fedora")], jobs: [job("fedora")] });
  const f4 = fleetOf(rolling, rolling.jobs, NOW);
  assert.deepEqual(headline(rolling, f4, sv, ps), { tone: "run", text: "Upgrading 1 runner" });
  assert.equal(readoutAll(rolling, f4, ps).title, "fedora is switching to b0de36d.");
});

test("how long kiln waits: whole minutes in range, said in words", () => {
  assert.deepEqual(readDraft({ upgrade_minutes: "15", wait_for_jobs_minutes: "60", build_minutes: "45" }), { upgrade_minutes: 15, wait_for_jobs_minutes: 60, build_minutes: 45 });
  assert.equal(readDraft({ upgrade_minutes: "0", wait_for_jobs_minutes: "60", build_minutes: "45" }), "Upgrade: a whole number of minutes from 1 to 1440");
  assert.equal(readDraft({ upgrade_minutes: "15", wait_for_jobs_minutes: "60", build_minutes: "4" }), "Build: a whole number of minutes from 5 to 1440");
  assert.equal(readDraft({ upgrade_minutes: "1.5", wait_for_jobs_minutes: "60", build_minutes: "45" }), "Upgrade: a whole number of minutes from 1 to 1440");
  assert.deepEqual([minutesSaid(1), minutesSaid(60), minutesSaid(360), minutesSaid(90)], ["a minute", "an hour", "6 hours", "90 minutes"]);
  assert.equal(failWords(job("x", { timed_out: "jobs" }), { upgrade_minutes: 15, wait_for_jobs_minutes: 60, build_minutes: 45 }), "its jobs ran past 60 min");
});

test("an agent asked why a runner won't upgrade is told where it's retried: System → Updates", () => {
  const p = diagnosisPrompt(behind("jupiter"), job("jupiter", { state: "failed", started_at: S - 600, error: "timed out" }), { commit: NEWEST, from: "server", since: S });
  assert.match(p, /the upgrade is then tried again from System → Updates\./);
  assert.match(p, /kiln couldn't upgrade its runner on jupiter/);
  assert.doesNotMatch(p, /\n\n\n/);
  assert.equal(holding({ id: "x", legacy: false, version: "", created_at: 0, connected_at: 0, pid: null, newest: false, terminals: 1, agents: 2, jobs: 0 }), "1 terminal, 2 agent sessions");
});
