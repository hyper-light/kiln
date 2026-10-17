// Fake-but-plausible data for the kiln mock server: repos, pipelines, logs.

export function mulberry32(seed: number) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function hashStr(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}

export const pick = <T>(rnd: () => number, arr: readonly T[]): T => arr[Math.floor(rnd() * arr.length)]!;

export type Toolchain = "rust" | "ts" | "python";

export interface RepoSeed {
  name: string;
  branches: string[];
  toolchain: Toolchain;
  weight: number;
}

export const REPOS: RepoSeed[] = [
  { name: "hyper-light/hyper-raft", branches: ["main", "release/*"], toolchain: "rust", weight: 5 },
  { name: "hyper-light/slates", branches: ["main"], toolchain: "rust", weight: 3 },
  { name: "hyper-light/focal", branches: ["main", "next"], toolchain: "rust", weight: 3 },
  { name: "hyper-light/mantle", branches: ["main"], toolchain: "rust", weight: 2 },
  { name: "hyper-light/shards", branches: ["main"], toolchain: "rust", weight: 2 },
  { name: "hyper-light/sylk", branches: ["main"], toolchain: "rust", weight: 2 },
  { name: "hyper-light/hyperscale", branches: ["main", "dev"], toolchain: "python", weight: 3 },
  { name: "hyper-light/vorpal", branches: ["main"], toolchain: "rust", weight: 4 },
  { name: "hyper-light/hyper-tls", branches: ["main"], toolchain: "rust", weight: 1 },
  { name: "hyper-light/hyperlight-site", branches: ["main"], toolchain: "ts", weight: 3 },
  { name: "hyper-light/kiln", branches: ["main"], toolchain: "rust", weight: 3 },
];

export const AUTHORS = ["ada", "mira.k", "jonas", "tsubasa", "lena-p", "dependabot[bot]", "ravi", "ines"];

const SUBJECTS: Record<Toolchain, string[]> = {
  rust: [
    "raft: pipeline AppendEntries under backpressure",
    "transport: coalesce QUIC datagrams on the send path",
    "fix: off-by-one in snapshot chunk offsets",
    "perf: avoid clone in hot log-replay loop",
    "deps: bump tokio to 1.47",
    "tls: rotate session tickets every 12h",
    "refactor: split storage engine into segments",
    "ci: cache sccache across matrix legs",
    "bench: add criterion group for leader election",
    "fix: clippy::needless_borrow in codec",
    "feat: zero-copy decode for framed messages",
    "docs: explain membership change protocol",
    "test: property tests for log compaction",
    "chore: deny unwrap/expect at workspace level",
  ],
  ts: [
    "site: tighten hero tracking on mobile",
    "blog: post on vorpal benchmarks",
    "fix: focus ring offset on pill buttons",
    "perf: lazy-load study gallery artwork",
    "deps: bump next to 16.3",
    "a11y: honour prefers-contrast in cards",
  ],
  python: [
    "scheduler: adaptive batch sizing",
    "fix: race in worker heartbeat",
    "perf: vectorise histogram merge",
    "deps: bump numpy to 2.3",
    "feat: graceful drain on SIGTERM",
  ],
};

export function commitMessage(rnd: () => number, tc: Toolchain): string {
  const s = pick(rnd, SUBJECTS[tc]);
  return rnd() < 0.3 ? `${s}\n\nSigned-off-by: kiln-bot` : s;
}

export function sha(rnd: () => number): string {
  let s = "";
  for (let i = 0; i < 40; i++) s += "0123456789abcdef"[Math.floor(rnd() * 16)];
  return s;
}

/* ---------------------------- pipelines ---------------------------- */

export interface StepTpl {
  name: string;
  run: string;
  dur: [number, number];
  kind: string;
}
export interface JobTpl {
  key: string;
  needs: string[];
  labels: string[];
  steps: StepTpl[];
  toolchain: string | null;
}

const checkout: StepTpl = { name: "checkout", run: "git fetch --depth=1 origin $KILN_SHA && git checkout $KILN_SHA", dur: [2, 5], kind: "checkout" };
const cache: StepTpl = { name: "restore cache", run: "kiln cache restore cargo-registry target", dur: [2, 8], kind: "cache" };

export function pipelineFor(tc: Toolchain, branch: string): { name: string; jobs: JobTpl[] }[] {
  if (tc === "rust") {
    const jobs: JobTpl[] = [
      { key: "fmt", needs: [], labels: ["k8s"], toolchain: "rust", steps: [checkout, { name: "cargo fmt", run: "cargo fmt --all -- --check", dur: [3, 8], kind: "fmt" }] },
      { key: "clippy", needs: [], labels: ["k8s"], toolchain: "rust", steps: [checkout, cache, { name: "cargo clippy", run: "cargo clippy --workspace --all-targets -- -D warnings", dur: [30, 90], kind: "clippy" }] },
      {
        key: "test (profile=dev)",
        needs: ["fmt"],
        labels: ["k8s"],
        toolchain: "rust",
        steps: [checkout, cache, { name: "build tests", run: "cargo build --workspace --tests", dur: [40, 120], kind: "compile" }, { name: "cargo test", run: "cargo test --workspace --profile dev", dur: [20, 80], kind: "test" }],
      },
      {
        key: "test (profile=release)",
        needs: ["fmt"],
        labels: ["k8s"],
        toolchain: "rust",
        steps: [checkout, cache, { name: "build tests", run: "cargo build --workspace --tests --release", dur: [60, 160], kind: "compile" }, { name: "cargo test", run: "cargo test --workspace --profile release", dur: [20, 60], kind: "test" }],
      },
      {
        key: "build",
        needs: ["clippy", "test (profile=dev)"],
        labels: ["k8s"],
        toolchain: "rust",
        steps: [checkout, cache, { name: "cargo build", run: "cargo build --release --locked", dur: [60, 150], kind: "compile" }, { name: "package", run: "tar czf dist.tgz -C target/release .", dur: [3, 8], kind: "package" }],
      },
    ];
    if (branch === "main" || branch.startsWith("release/"))
      jobs.push({
        key: "bench",
        needs: ["test (profile=release)"],
        labels: ["baremetal", "bench"],
        toolchain: "rust",
        steps: [checkout, { name: "cargo bench", run: "cargo bench -- --save-baseline $KILN_SHA", dur: [60, 180], kind: "bench" }, { name: "upload artifacts", run: "kiln artifacts upload target/criterion/**", dur: [2, 6], kind: "upload" }],
      });
    return [{ name: "ci", jobs }];
  }
  if (tc === "ts") {
    const jobs: JobTpl[] = [
      { key: "install", needs: [], labels: ["k8s"], toolchain: "node", steps: [checkout, { name: "npm ci", run: "npm ci", dur: [10, 30], kind: "npm" }] },
      { key: "lint", needs: ["install"], labels: ["k8s"], toolchain: "node", steps: [checkout, { name: "eslint", run: "npm run lint", dur: [6, 18], kind: "lint" }] },
      { key: "typecheck", needs: ["install"], labels: ["k8s"], toolchain: "node", steps: [checkout, { name: "tsc", run: "npm run typecheck", dur: [8, 20], kind: "tsc" }] },
      { key: "build", needs: ["lint", "typecheck"], labels: ["k8s"], toolchain: "node", steps: [checkout, { name: "next build", run: "npm run build", dur: [25, 60], kind: "vite" }] },
      { key: "e2e", needs: ["build"], labels: ["baremetal"], toolchain: "node", steps: [checkout, { name: "playwright", run: "npx playwright test", dur: [30, 90], kind: "e2e" }] },
    ];
    if (branch === "main") jobs.push({ key: "deploy", needs: ["e2e"], labels: ["k8s"], toolchain: null, steps: [{ name: "argocd sync", run: "argocd app sync hyperlight-site", dur: [6, 20], kind: "deploy" }] });
    return [{ name: "ci", jobs }];
  }
  return [
    {
      name: "ci",
      jobs: [
        { key: "lint", needs: [], labels: ["k8s"], toolchain: "python", steps: [checkout, { name: "ruff", run: "ruff check . && ruff format --check .", dur: [3, 9], kind: "ruff" }] },
        { key: "test (python=3.12)", needs: ["lint"], labels: ["k8s"], toolchain: "python", steps: [checkout, { name: "pytest", run: "uv run pytest -q", dur: [20, 70], kind: "pytest" }] },
        { key: "test (python=3.13)", needs: ["lint"], labels: ["k8s"], toolchain: "python", steps: [checkout, { name: "pytest", run: "uv run pytest -q", dur: [20, 70], kind: "pytest" }] },
        { key: "wheel", needs: ["test (python=3.12)", "test (python=3.13)"], labels: ["k8s"], toolchain: "python", steps: [checkout, { name: "build wheel", run: "uv build", dur: [8, 20], kind: "wheel" }] },
      ],
    },
  ];
}

/* ---------------------------- ANSI log lines ---------------------------- */

const E = "\x1b[";
const dim = (s: string) => `${E}2m${s}${E}0m`;
const bold = (s: string) => `${E}1m${s}${E}0m`;
const green = (s: string) => `${E}32m${s}${E}0m`;
const bgreen = (s: string) => `${E}1;32m${s}${E}0m`;
const red = (s: string) => `${E}31m${s}${E}0m`;
const bred = (s: string) => `${E}1;31m${s}${E}0m`;
const yellow = (s: string) => `${E}33m${s}${E}0m`;
const byellow = (s: string) => `${E}1;33m${s}${E}0m`;
const blue = (s: string) => `${E}34m${s}${E}0m`;
const bblue = (s: string) => `${E}1;34m${s}${E}0m`;
const cyan = (s: string) => `${E}36m${s}${E}0m`;
const magenta = (s: string) => `${E}35m${s}${E}0m`;

const CRATES = ["serde", "tokio", "bytes", "quinn", "rustls", "ring", "tracing", "futures-util", "hashbrown", "parking_lot", "smallvec", "crossbeam-channel", "prost", "mio", "socket2", "libc", "memchr", "itoa", "anyhow", "thiserror", "clap", "rand", "criterion", "proptest"];
const MODS = ["raft::log", "raft::election", "raft::snapshot", "transport::quic", "codec::frame", "storage::segment", "membership", "tls::session", "net::pool", "replica::apply"];

export interface LogPlan {
  /** lines (may include \r progress updates) */
  lines: string[];
  failLines: string[];
}

export function stepLog(kind: string, run: string, seed: number, ctx: { repo: string; sha: string; branch: string; job: string }): LogPlan {
  const rnd = mulberry32(seed);
  const L: string[] = [];
  const head = `${dim("$")} ${bold(run)}`;
  L.push(head);
  const fail: string[] = [];
  switch (kind) {
    case "checkout": {
      L.push(`Initialized empty Git repository in /work/${ctx.repo.split("/")[1]}/.git/`);
      L.push(`${dim("remote:")} Enumerating objects: ${200 + Math.floor(rnd() * 3000)}, done.`);
      L.push(`${dim("remote:")} Counting objects: 100% (${Math.floor(rnd() * 900)}/${Math.floor(rnd() * 900) + 900}), done.`);
      L.push(`Receiving objects: 100% (${Math.floor(rnd() * 3000)}), ${(rnd() * 20 + 1).toFixed(2)} MiB | ${(rnd() * 80 + 20).toFixed(1)} MiB/s, done.`);
      L.push(`HEAD is now at ${yellow(ctx.sha.slice(0, 7))} on ${cyan(ctx.branch)}`);
      break;
    }
    case "cache": {
      L.push(`${cyan("kiln")} cache key ${dim(`cargo-${ctx.sha.slice(0, 8)}-linux-x86_64`)}`);
      const hit = rnd() < 0.8;
      L.push(hit ? `${green("✓")} restored cargo-registry ${dim(`(${(rnd() * 900 + 100).toFixed(0)} MiB in ${(rnd() * 4 + 1).toFixed(1)}s)`)}` : `${yellow("!")} cache miss for target, falling back to ${dim("cargo-main-linux-x86_64")}`);
      L.push(`${green("✓")} restored target ${dim(`(${(rnd() * 3 + 0.5).toFixed(1)} GiB)`)}`);
      break;
    }
    case "fmt": {
      L.push(`${dim("Checking formatting of")} ${Math.floor(rnd() * 200 + 40)} files`);
      L.push(green("All files formatted correctly."));
      fail.push(`${bred("Diff in")} /work/src/raft/log.rs at line ${Math.floor(rnd() * 400)}:`);
      fail.push(`${red("-    let idx = self.entries.len() as u64 ;")}`);
      fail.push(`${green("+    let idx = self.entries.len() as u64;")}`);
      break;
    }
    case "compile":
    case "clippy": {
      const n = 14 + Math.floor(rnd() * 22);
      for (let i = 0; i < n; i++) {
        const c = pick(rnd, CRATES);
        L.push(`${bgreen(kind === "clippy" && i > n - 4 ? "    Checking" : "   Compiling")} ${c} v${Math.floor(rnd() * 2)}.${Math.floor(rnd() * 40)}.${Math.floor(rnd() * 20)}`);
        if (i % 5 === 4) L.push(`\r${bblue("    Building")} [${"=".repeat(Math.floor((i / n) * 30))}>${" ".repeat(30 - Math.floor((i / n) * 30))}] ${i}/${n}: ${c}`);
      }
      if (kind === "clippy" && rnd() < 0.5) {
        L.push(`${byellow("warning")}${bold(": this expression creates a reference which is immediately dereferenced")}`);
        L.push(`  ${bblue("-->")} src/codec/frame.rs:${Math.floor(rnd() * 300)}:${Math.floor(rnd() * 40)}`);
        L.push(`   ${bblue("|")}`);
        L.push(`${bblue(String(Math.floor(rnd() * 300)).padStart(3))}${bblue(" |")}     let n = decode(&buf[..]);`);
        L.push(`   ${bblue("|")}                    ${byellow("^^^^^^^^^ help: change this to: `buf`")}`);
      }
      L.push(`${bgreen("    Finished")} \`${run.includes("release") ? "release" : "dev"}\` profile [${run.includes("release") ? "optimized" : "unoptimized + debuginfo"}] target(s) in ${(rnd() * 80 + 10).toFixed(2)}s`);
      fail.push(`${bred("error[E0308]")}${bold(": mismatched types")}`);
      fail.push(`  ${bblue("-->")} src/raft/snapshot.rs:${Math.floor(rnd() * 400)}:${Math.floor(rnd() * 30)}`);
      fail.push(`   ${bblue("|")}     ${red("expected `u64`, found `usize`")}`);
      fail.push(`${bred("error")}: could not compile \`${ctx.repo.split("/")[1]}\` (lib) due to 1 previous error`);
      break;
    }
    case "test": {
      const n = 18 + Math.floor(rnd() * 30);
      L.push(`${bgreen("     Running")} unittests src/lib.rs (target/debug/deps/${ctx.repo.split("/")[1]?.replace(/-/g, "_")}-${ctx.sha.slice(0, 16)})`);
      L.push("");
      L.push(`running ${n} tests`);
      for (let i = 0; i < n; i++) {
        L.push(`test ${pick(rnd, MODS)}::tests::${pick(rnd, ["append", "truncate", "elect_leader", "vote_split", "install_snapshot", "replay", "rejects_stale_term", "roundtrip", "handshake", "pipelining"])}_${i} ... ${green("ok")}`);
      }
      L.push("");
      L.push(`test result: ${green("ok")}. ${n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in ${(rnd() * 9 + 0.3).toFixed(2)}s`);
      fail.push(`test raft::election::tests::vote_split_${n} ... ${bred("FAILED")}`);
      fail.push("");
      fail.push("failures:");
      fail.push("");
      fail.push(`---- raft::election::tests::vote_split_${n} stdout ----`);
      fail.push(`thread 'raft::election::tests::vote_split_${n}' panicked at src/raft/election.rs:${Math.floor(rnd() * 500)}:9:`);
      fail.push(`assertion \`left == right\` failed: ${red("leader elected twice in term 7")}`);
      fail.push(`  left: ${red("Some(NodeId(2))")}`);
      fail.push(` right: ${green("Some(NodeId(3))")}`);
      fail.push("");
      fail.push(`test result: ${bred("FAILED")}. ${n - 1} passed; 1 failed; 0 ignored`);
      break;
    }
    case "bench": {
      const benches = ["append_entries/batch=64", "append_entries/batch=1024", "election/5-nodes", "snapshot/install/64MiB", "codec/decode/frame"];
      for (const b of benches) {
        const t = rnd() * 3 + 0.2;
        const ch = (rnd() - 0.6) * 8;
        L.push(`${bold(b.padEnd(34))}time:   [${(t * 0.98).toFixed(4)} ms ${bold(t.toFixed(4) + " ms")} ${(t * 1.02).toFixed(4)} ms]`);
        L.push(`${" ".repeat(34)}change: [${(ch - 1).toFixed(2)}% ${bold(ch.toFixed(2) + "%")} ${(ch + 1).toFixed(2)}%] (p = 0.0${Math.floor(rnd() * 9)} < 0.05)`);
        L.push(`${" ".repeat(34)}${ch < 0 ? green("Performance has improved.") : ch > 2 ? red("Performance has regressed.") : "No change in performance detected."}`);
        if (rnd() < 0.4) L.push(`${yellow(`Found ${Math.floor(rnd() * 8) + 1} outliers among 100 measurements`)}`);
      }
      break;
    }
    case "upload":
    case "package": {
      L.push(`${cyan("kiln")} artifacts: ${Math.floor(rnd() * 40 + 3)} files, ${(rnd() * 120 + 4).toFixed(1)} MiB`);
      L.push(`${green("✓")} uploaded ${dim(`sha256:${ctx.sha.slice(0, 24)}…`)}`);
      break;
    }
    case "npm": {
      L.push(`${dim("npm")} ${yellow("warn")} deprecated inflight@1.0.6`);
      L.push(`\nadded ${Math.floor(rnd() * 600 + 200)} packages, and audited ${Math.floor(rnd() * 600 + 200)} packages in ${Math.floor(rnd() * 20 + 3)}s`);
      L.push(`${bold(String(Math.floor(rnd() * 200 + 40)))} packages are looking for funding`);
      L.push(`found ${bold(green("0"))} vulnerabilities`);
      break;
    }
    case "lint":
    case "tsc":
    case "ruff": {
      L.push(`${dim(">")} ${run.replace("npm run ", "")}`);
      L.push(kind === "ruff" ? `All checks passed!` : `${green("✔")} No problems found in ${Math.floor(rnd() * 120 + 40)} files`);
      fail.push(`${dim("src/components/header.tsx")}`);
      fail.push(`  ${dim("42:7")}  ${red("error")}  'pathname' is assigned a value but never used  ${dim("@typescript-eslint/no-unused-vars")}`);
      fail.push(`\n${bred("✖ 1 problem (1 error, 0 warnings)")}`);
      break;
    }
    case "vite": {
      L.push(`   ${magenta("▲ Next.js 16.3.4")}`);
      L.push(`   Creating an optimized production build ...`);
      L.push(` ${green("✓")} Compiled successfully in ${(rnd() * 20 + 5).toFixed(1)}s`);
      for (const r of ["/", "/projects", "/projects/[slug]", "/blog", "/blog/[slug]", "/about"]) {
        L.push(`${dim("├")} ${r.includes("[") ? blue("●") : "○"} ${r.padEnd(22)} ${(rnd() * 9 + 1).toFixed(2)} kB ${dim(`${(rnd() * 60 + 100).toFixed(0)} kB`)}`);
      }
      break;
    }
    case "e2e": {
      L.push(`Running ${Math.floor(rnd() * 40 + 20)} tests using 4 workers`);
      for (let i = 0; i < 12; i++) L.push(`  ${green("✓")}  ${i + 1} [chromium] › ${pick(rnd, ["home.spec.ts", "nav.spec.ts", "blog.spec.ts", "a11y.spec.ts"])}:${Math.floor(rnd() * 90)} › ${pick(rnd, ["renders hero", "motion toggle pauses", "mobile nav opens", "focus visible", "search filters"])} ${dim(`(${Math.floor(rnd() * 900 + 100)}ms)`)}`);
      L.push(`\n  ${green(`${Math.floor(rnd() * 40 + 20)} passed`)} ${dim(`(${(rnd() * 40 + 10).toFixed(1)}s)`)}`);
      break;
    }
    case "pytest": {
      L.push(`${bold("============================= test session starts ==============================")}`);
      L.push(`platform linux -- Python 3.1${Math.floor(rnd() * 2) + 2}.4, pytest-8.4.1`);
      L.push(`collected ${Math.floor(rnd() * 300 + 50)} items\n`);
      for (const f of ["tests/test_scheduler.py", "tests/test_worker.py", "tests/test_histogram.py", "tests/test_api.py"]) L.push(`${f} ${green(".".repeat(Math.floor(rnd() * 40 + 10)))} ${dim(`[${Math.floor(rnd() * 100)}%]`)}`);
      L.push(`${bgreen(`========================= ${Math.floor(rnd() * 300 + 50)} passed in ${(rnd() * 30 + 2).toFixed(2)}s =========================`)}`);
      break;
    }
    case "wheel":
      L.push(`Building source distribution...`);
      L.push(`Building wheel from source distribution...`);
      L.push(`Successfully built ${green(`dist/hyperscale-0.${Math.floor(rnd() * 9)}.0-py3-none-any.whl`)}`);
      break;
    case "deploy":
      L.push(`${cyan("argocd")} syncing application hyperlight-site (revision ${ctx.sha.slice(0, 7)})`);
      L.push(`Deployment.apps/hyperlight-site  ${green("Synced")}  ${green("Healthy")}`);
      L.push(`${green("✓")} rollout complete: 3/3 replicas updated`);
      break;
    default:
      L.push("ok");
  }
  if (fail.length === 0) fail.push(bred("error: step failed"));
  return { lines: L, failLines: fail };
}

/* ---------------------------- discovery (Add machines) ---------------------------- */

export type MockOs = "linux" | "macos" | "windows";

/** This server as discovery knows it. */
export const SERVER_INSTANCE_ID = "c0ffee12d4e5f6071829a3b4c5d6e7f8";
/** First 16 hex digits of the SHA-256 of a CA certificate: ours, and the one of the NAS's kiln. */
export const SERVER_CA = "9f3c2a77d1e04b58";
export const OTHER_CA = "a71e09c4b23f8d16";
export const SERVER_ADDR = "192.168.1.50";
export const SERVER_URL = `http://${SERVER_ADDR}:7070`;
/** The LAN subnets the server sits on (a scan's default). */
export const SUBNETS = ["192.168.1.0/24", "10.0.8.0/24"];
export const DNS_SD_DOMAINS = ["lan.example.com", "corp.example.net"];

export interface InstanceSeed {
  id: string;
  name: string;
  role: "server" | "agent";
  version: string;
  os: string;
  arch: string;
  port: number;
  quic: number;
  ca: string | null;
  enrolled: string | null;
  addresses: string[];
  via: ("mdns" | "native" | "dns-sd" | "scan" | "probe" | "tailscale" | "seed")[];
  /** Seconds ago it was first and last heard. */
  firstAgo: number;
  lastAgo: number;
  invited?: boolean;
}

/** What the network holds besides this server: runners already here, machines not set up yet,
 * another kiln server and one of its runners. `version` "" means this server's version. */
export const INSTANCE_SEEDS: InstanceSeed[] = [
  { id: "4b1d9e0c7a2f45e8b6c3d1a0f9e8d7c6", name: "ryzen-01.lan", role: "agent", version: "", os: "linux", arch: "x86_64", port: 0, quic: 0, ca: null, enrolled: SERVER_CA, addresses: ["192.168.1.21"], via: ["mdns", "native"], firstAgo: 86400 * 3, lastAgo: 9 },
  { id: "7e2a6c1f9b0d4e3a8c5b2f1e0d9c8b7a", name: "mac-mini.lan", role: "agent", version: "", os: "macos", arch: "aarch64", port: 0, quic: 0, ca: null, enrolled: SERVER_CA, addresses: ["192.168.1.27", "100.88.12.4"], via: ["mdns", "native", "tailscale"], firstAgo: 86400 * 3, lastAgo: 14 },
  { id: "1a9f3e7d2c6b4a5f8e0d1c2b3a4f5e6d", name: "pi-04.lan", role: "agent", version: "0.0.9", os: "linux", arch: "aarch64", port: 0, quic: 0, ca: null, enrolled: SERVER_CA, addresses: ["192.168.1.44"], via: ["mdns"], firstAgo: 86400 * 3, lastAgo: 21 },
  { id: "d3c8b2a19f0e4d7c6b5a4f3e2d1c0b9a", name: "studio-mbp.lan", role: "agent", version: "", os: "macos", arch: "aarch64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["192.168.1.34", "fe80::14d2:7cff:fe0a:3b91"], via: ["mdns", "native"], firstAgo: 640, lastAgo: 6 },
  { id: "5f0e9d8c7b6a4f3e2d1c0b9a8f7e6d5c", name: "ryzen-03.lan", role: "agent", version: "", os: "linux", arch: "x86_64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["192.168.1.23"], via: ["mdns", "scan"], firstAgo: 2400, lastAgo: 11 },
  { id: "9a8b7c6d5e4f43a2b1c0d9e8f7a6b5c4", name: "pi-05.lan", role: "agent", version: "0.0.9", os: "linux", arch: "aarch64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["192.168.1.38"], via: ["scan"], firstAgo: 760, lastAgo: 731, invited: true },
  { id: "2c3d4e5f6a7b48c9d0e1f2a3b4c5d6e7", name: "win-build-01", role: "agent", version: "", os: "windows", arch: "x86_64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["10.0.8.31"], via: ["probe"], firstAgo: 5400, lastAgo: 5400 },
  { id: "e6f5a4b3c2d14e0f9a8b7c6d5e4f3a2b", name: "nas.lan", role: "server", version: "0.0.9", os: "linux", arch: "x86_64", port: 7070, quic: 7071, ca: OTHER_CA, enrolled: null, addresses: ["192.168.1.40"], via: ["mdns", "dns-sd"], firstAgo: 86400 * 3, lastAgo: 17 },
  { id: "0f1e2d3c4b5a46978a9b0c1d2e3f4a5b", name: "lab-runner-7", role: "agent", version: "0.0.9", os: "linux", arch: "x86_64", port: 0, quic: 0, ca: null, enrolled: OTHER_CA, addresses: ["100.101.12.7"], via: ["tailscale"], firstAgo: 86400, lastAgo: 48 },
];

/** Machines the mock "finds" when asked: on a scan of 192.168.1.0/24, and by address. */
export const SCAN_FIND: InstanceSeed = { id: "8c7d6e5f4a3b42c1d0e9f8a7b6c5d4e3", name: "nuc-lab.lan", role: "agent", version: "", os: "linux", arch: "x86_64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["192.168.1.61"], via: ["scan"], firstAgo: 0, lastAgo: 0 };
export const PROBE_FIND: InstanceSeed = { id: "3e4f5a6b7c8d49e0f1a2b3c4d5e6f7a8", name: "build-box", role: "agent", version: "", os: "linux", arch: "aarch64", port: 0, quic: 0, ca: null, enrolled: null, addresses: ["10.0.4.12"], via: ["probe"], firstAgo: 0, lastAgo: 0 };
/** The machine that "runs the one-liner" a while after the Add machines page asked for it. */
export const AUTOJOIN = { name: "metal-pi-06", hostname: "pi-06.lan", id: "6a5b4c3d2e1f40a9b8c7d6e5f4a3b2c1", address: "192.168.1.46", os: "linux", arch: "aarch64" };

/** Interfaces the server's multicast DNS runs on, by the server's OS. */
export function mdnsInterfaces(os: MockOs): string {
  if (os === "macos") return `en0 (${SERVER_ADDR}), en7 (10.0.8.5)`;
  if (os === "windows") return `Ethernet (${SERVER_ADDR}), vEthernet (10.0.8.5)`;
  return `enp5s0 (${SERVER_ADDR}), enp6s0 (10.0.8.5)`;
}

/** The DNS-SD instance label and the zone-file lines that would publish this server. */
export function dnsSdRecords(host: string, version: string, os: string, arch: string, zone: string): string[] {
  const label = `kiln\\032server\\032on\\032${host}\\032(${SERVER_INSTANCE_ID.slice(0, 8)})`;
  const inst = `${label}._kiln._tcp.${zone}.`;
  return [
    `b._dns-sd._udp.${zone}. 3600 IN PTR ${zone}.`,
    `_kiln._tcp.${zone}. 3600 IN PTR ${inst}`,
    `${inst} 3600 IN SRV 0 0 7070 ${host}.${zone}.`,
    `${inst} 3600 IN TXT "id=${SERVER_INSTANCE_ID}" "name=${host}" "role=server" "version=${version}" "quic=7071" "ca=${SERVER_CA}" "os=${os}" "arch=${arch}"`,
    `${host}.${zone}. 3600 IN A ${SERVER_ADDR}`,
  ];
}

/** Why multicast DNS hears less than a scan does, and the fix on each system. */
export const SILENT_NETWORK_FIXES = [
  { os: "linux" as const, text: "On each Linux machine with ufw:", command: "sudo ufw allow from 192.168.1.0/24 to any port 5353 proto udp" },
  { os: "linux" as const, text: "With firewalld:", command: "sudo firewall-cmd --permanent --add-service=mdns && sudo firewall-cmd --reload" },
  { os: "macos" as const, text: "On each Mac, let kiln take incoming connections:", command: "sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add ~/.local/bin/kiln && sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp ~/.local/bin/kiln" },
  { os: "windows" as const, text: "On each Windows machine, in an elevated PowerShell:", command: 'New-NetFirewallRule -DisplayName "kiln mDNS" -Direction Inbound -Protocol UDP -LocalPort 5353 -Action Allow' },
  { text: "On Wi-Fi, turn off client (AP) isolation for this network in the router." },
];

/* ------------------------------------------------------------------ */
/* Folders on machines (GET /api/machines/:target/dirs)                */
/* ------------------------------------------------------------------ */

/** A folder's folders by name; "file" for a file, "locked" for a folder kiln's user can't read. */
type Tree = { [name: string]: Tree } | "file" | "locked";

const leaves = (...names: string[]): { [name: string]: Tree } => Object.fromEntries(names.map((n) => [n, {}]));

const RUST_CRATE: { [name: string]: Tree } = { src: {}, tests: {}, benches: {}, "Cargo.toml": "file" };
const project = (extra: { [name: string]: Tree } = {}): Tree => ({
  ".git": leaves("hooks", "objects", "refs"),
  ".github": leaves("workflows"),
  docs: {},
  "README.md": "file",
  ...extra,
});

/** A Linux user's home: the projects they work on, dotfiles and the usual clutter. */
const LINUX_HOME: { [name: string]: Tree } = {
  ".cache": leaves("pip", "go-build", "ms-playwright", "kiln"),
  ".cargo": leaves("bin", "registry", "git"),
  ".claude": leaves("projects", "todos", "commands", "agents"),
  ".codex": leaves("sessions", "log"),
  ".config": leaves("nvim", "fish", "git", "gh", "zed", "kiln", "htop", "systemd"),
  ".local": { bin: {}, share: leaves("fonts", "applications", "kiln"), state: {} },
  ".npm": leaves("_cacache", "_logs"),
  ".rustup": leaves("toolchains", "downloads", "update-hashes"),
  ".ssh": "locked",
  Desktop: {},
  Documents: leaves("notes", "papers", "taxes-2025", "talks"),
  Downloads: leaves("node-v24.4.0-linux-x64", "zig-0.15.1"),
  Pictures: leaves("Screenshots", "wallpapers"),
  Projects: {
    kiln: project({
      ".kiln": {},
      crates: { kiln: { ...RUST_CRATE, assets: {} }, "kiln-proto": RUST_CRATE },
      deploy: leaves("k8s", "systemd", "docker"),
      ui: {
        src: { api: {}, components: leaves("ui", "charts"), lib: {}, views: leaves("harness", "settings", "runs", "agents") },
        mock: {},
        public: {},
        node_modules: Object.fromEntries(Array.from({ length: 60 }, (_, i) => [`pkg-${String(i).padStart(2, "0")}`, {}])),
      },
      target: leaves("debug", "release", "tmp"),
      "Cargo.toml": "file",
    }),
    "hyper-raft": project({ crates: leaves("hyper-rt", "hyper-quic", "hyper-transport", "hyper-tls", "raft"), benches: {}, examples: {} }),
    "hyperlight-site": project({ app: leaves("about", "blog", "projects", "share"), components: {}, content: leaves("posts", "projects"), public: {}, node_modules: {} }),
    slates: project({ src: {}, tests: {} }),
    focal: project({ src: {}, examples: {} }),
    mantle: project({ src: {} }),
    shards: project({ src: {}, bench: {} }),
    sylk: project({ src: {} }),
    hyperscale: project({ hyperscale: {}, tests: {}, scripts: {} }),
    vorpal: project({ crates: leaves("vorpal-core", "vorpal-cli"), docs: {} }),
    "hyper-tls": project({ src: {}, fuzz: {} }),
    monorepo: {
      packages: Object.fromEntries(Array.from({ length: 260 }, (_, i) => [`package-${String(i).padStart(3, "0")}`, {}])),
      apps: leaves("web", "admin", "mobile"),
      tools: {},
    },
    "a-folder-with-a-rather-long-name-to-see-how-the-words-wrap-on-a-phone": {},
    "Screenshots 2026": {},
    scratch: {},
    "notes.md": "file",
  },
  go: leaves("bin", "pkg", "src"),
  src: leaves("linux", "llvm-project", "postgres"),
  "todo.txt": "file",
};

const LINUX_ROOT = (user: string): { [name: string]: Tree } => ({
  boot: {},
  dev: leaves("pts", "shm", "disk"),
  etc: leaves("nginx", "systemd", "ssh", "kiln", "apt", "docker", "ssl"),
  home: { [user]: LINUX_HOME, kiln: "locked" },
  mnt: { data: leaves("datasets", "models", "scratch"), nfs: leaves("shared", "backups") },
  opt: leaves("kiln", "containerd", "google"),
  proc: {},
  root: "locked",
  srv: leaves("git", "www"),
  tmp: leaves("kiln-build-1f3c", "systemd-private-7e1", ".X11-unix", ".ICE-unix"),
  usr: { bin: {}, lib: {}, local: leaves("bin", "lib", "share", "src"), share: {}, src: {} },
  var: { lib: leaves("kiln", "docker", "postgresql"), log: leaves("journal", "nginx"), cache: {}, tmp: {} },
});

const MAC_ROOT: { [name: string]: Tree } = {
  Applications: leaves("Xcode.app", "Ghostty.app", "Zed.app"),
  Library: leaves("Developer", "Fonts", "LaunchDaemons"),
  System: "locked",
  Users: {
    ada: {
      ...LINUX_HOME,
      Applications: {},
      Library: leaves("Application Support", "Caches", "Developer", "Preferences"),
      Movies: {},
      Music: {},
      Public: {},
    },
    Shared: {},
  },
  Volumes: leaves("Macintosh HD", "Backups"),
  opt: { homebrew: leaves("bin", "Cellar", "opt", "share") },
  private: leaves("etc", "tmp", "var"),
};

/** How long ago (seconds) a folder was last changed: what people work on now is fresh, the rest older. */
const FRESH: Record<string, number> = {
  kiln: 4 * 60,
  ui: 6 * 60,
  crates: 11 * 60,
  Projects: 4 * 60,
  harness: 9 * 60,
  "hyper-raft": 3 * 3600,
  "hyperlight-site": 26 * 3600,
  ".claude": 2 * 60,
  ".cache": 30 * 60,
  Downloads: 5 * 3600,
};
function age(abs: string, name: string): number {
  const rnd = mulberry32(hashStr(abs));
  return FRESH[name] ?? Math.floor(3600 + rnd() * 86400 * 120);
}

export interface MockDirListing {
  path: string;
  abs: string;
  parent: string | null;
  exists: boolean;
  dirs: { name: string; modified: number }[];
  truncated: boolean;
  error?: string;
}

/** The folders in the folder `typed` names on a mock machine, by the server's rules (see crates/kiln/src/dirs.rs). */
export function mockDirList(os: "linux" | "macos", typed: string, user = "ada"): MockDirListing {
  const home = os === "macos" ? `/Users/${user}` : `/home/${user}`;
  const root = os === "macos" ? MAC_ROOT : LINUX_ROOT(user);
  const t = typed.trim();
  const raw = t === "" || t === "~" ? home : t.startsWith("~/") ? `${home}/${t.slice(2)}` : t.startsWith("/") ? t : `${home}/${t}`;
  const parts: string[] = [];
  for (const s of raw.split("/")) {
    if (s === "" || s === ".") continue;
    if (s === "..") parts.pop();
    else parts.push(s);
  }
  const abs = `/${parts.join("/")}`;
  const tilde = (p: string) => (p === home ? "~" : p.startsWith(`${home}/`) ? `~${p.slice(home.length)}` : p);
  const out: MockDirListing = {
    path: tilde(abs),
    abs,
    parent: parts.length ? tilde(`/${parts.slice(0, -1).join("/")}`) : null,
    exists: false,
    dirs: [],
    truncated: false,
  };
  let node: Tree = root;
  for (const [i, name] of parts.entries()) {
    if (node === "locked") return { ...out, exists: true, error: "permission denied" };
    if (node === "file") return { ...out, error: "no such folder" };
    const next: Tree | undefined = node[name];
    if (next === undefined) return { ...out, error: "no such folder" };
    if (next === "file" && i === parts.length - 1) return { ...out, error: "not a folder" };
    node = next;
  }
  if (node === "file") return { ...out, error: "no such folder" };
  if (node === "locked") return { ...out, exists: true, error: "permission denied" };
  const last = t.replace(/\/+$/, "").split("/").pop() ?? "";
  const hidden = last.startsWith(".") && last !== "..";
  const now = Math.floor(Date.now() / 1000);
  const dirs = Object.entries(node)
    .filter(([name, sub]) => sub !== "file" && (hidden || !name.startsWith(".")))
    .map(([name]) => ({ name, modified: now - age(`${abs}/${name}`, name) }))
    .sort((a, b) => b.modified - a.modified || (a.name < b.name ? -1 : 1));
  return { ...out, exists: true, dirs: dirs.slice(0, 200), truncated: dirs.length > 200 };
}
