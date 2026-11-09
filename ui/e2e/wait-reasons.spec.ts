// W05-R13's reasons that backfilling and gangs brought (docs/ci.md W05-R39, R40; docs/needs.md
// E4.7): a waiting leg says, as one sentence, that room is kept for it, that it waits behind room
// kept for another, or that it waits for the rest of its gang; and the queue's breakdown names each.
// The engine's view of a run of the mock's is served here, three of its legs waiting so.
import { expect, test, type Page } from "@playwright/test";

const AT = Date.UTC(2026, 10, 1, 17, 0);

/** Each new reason, and the sentence its leg's panel says it in. */
const WAITS: { wait: Record<string, unknown>; says: RegExp }[] = [
  {
    wait: { reason: "reserved", runner: "fedora-2", at: AT },
    says: /^No runner has room for it now: room is kept for it on fedora-2 from .+, when the work there is expected to have left it room\.$/,
  },
  {
    wait: { reason: "behind", leg: "gate (linux)", runner: "fedora-2", at: AT },
    says: /^It would fit on fedora-2, but only in room kept there for gate \(linux\) from .+, past which it would still run\.$/,
  },
  { wait: { reason: "gang", gang: "sim", have: 2, of: 3 }, says: /^Waiting for the rest of its gang sim, which starts together: 2 of 3 queued\.$/ },
];

type Job = { id: string; key: string; name?: string | null; status: string; started_at?: number | null; finished_at?: number | null };

/** A run of the mock's with three jobs at least, its first three waiting, served with the engine's view of it: a finished
 * one first, which the mock's events no longer change. */
async function waitingRun(page: Page): Promise<{ id: string; jobs: Job[] }> {
  const listed: { id: string; status: string }[] = await (await page.request.get("/api/runs?limit=50")).json();
  const active = (r: { status: string }) => ["pending", "queued", "running"].includes(r.status);
  for (const r of [...listed.filter((r) => !active(r)), ...listed.filter(active)]) {
    const detail: { run: Record<string, unknown>; jobs: Job[] } = await (await page.request.get(`/api/runs/${encodeURIComponent(r.id)}`)).json();
    if (detail.jobs.length < 3) continue;
    const jobs = detail.jobs.slice(0, 3).map((j) => ({ ...j, status: "queued", started_at: null, finished_at: null }));
    const all = [...jobs, ...detail.jobs.slice(3)];
    const legs = jobs.map((j, i) => ({
      id: j.id,
      key: j.key,
      name: j.name ?? null,
      job: j.key,
      matrix: null,
      status: "queued",
      conclusion: null,
      runner: null,
      executor: null,
      attempt: 1,
      wait_reason: WAITS[i]?.wait,
      queued_at: AT,
      started_at: null,
      finished_at: null,
      error: null,
    }));
    const run = {
      id: r.id,
      number: 1,
      repo: "acme/sim",
      workflow: ".kiln/ci.yaml",
      name: "ci",
      run_name: null,
      event: "push",
      branch: "main",
      sha: "0123456789abcdef0123456789abcdef01234567",
      actor: "ada",
      status: "running",
      conclusion: null,
      trust: "trusted",
      attempt: 1,
      created_at: AT,
      started_at: AT,
      finished_at: null,
      error: null,
    };
    const parts: Record<string, unknown> = {
      graph: { nodes: legs.map((l) => ({ job: l.key, legs: [{ ...l, reused: false }], started_at: null, finished_at: null })), edges: [], critical_path: [] },
      summary: { summaries: [] },
      annotations: { annotations: [] },
      artifacts: { artifacts: [] },
      tests: { counts: {}, tests: [] },
      metrics: { metrics: [] },
      flow: { outputs: [], exports: [] },
    };
    await page.route(`**/api/runs/${r.id}`, (route) => route.fulfill({ json: { ...detail, run: { ...detail.run, status: "running", finished_at: null }, jobs: all } }));
    await page.route(`**/api/ci/runs/${r.id}**`, (route) => {
      const part = new URL(route.request().url()).pathname.split("/").slice(5)[0];
      return route.fulfill({ json: part ? (parts[part] ?? {}) : { run, legs, actions: [] } });
    });
    await page.route("**/api/ci/legs/**", (route) => {
      const [leg, part] = new URL(route.request().url()).pathname.split("/").slice(4).map(decodeURIComponent);
      const l = legs.find((x) => x.id === leg);
      if (!l) return route.fulfill({ status: 404, json: { error: "no such leg" } });
      return route.fulfill({ json: part === "routing" ? { leg, known: true, wait_reason: l.wait_reason, runners: [] } : { attempts: [], outputs: {} } });
    });
    return { id: r.id, jobs };
  }
  throw new Error("no run of the mock's has three jobs");
}

test("a waiting leg says room is kept for it, that it waits behind another's, or for its gang", async ({ page }, info) => {
  // Three pages loaded in turn.
  test.slow();
  const { id, jobs } = await waitingRun(page);
  for (const [i, job] of jobs.entries()) {
    await page.goto(`/runs/${encodeURIComponent(id)}/jobs/${encodeURIComponent(job.id)}`);
    const says = WAITS[i]?.says ?? /./;
    // The first page a browser loads takes a while on a busy machine.
    await expect(page.getByText(says)).toBeVisible({ timeout: 30_000 });
    // The run's graph says it too, beside the leg (cut short there: whole on hover).
    await expect(page.locator(`[title]`).filter({ hasText: /^· / }).first()).toBeVisible();
    await page.screenshot({ path: info.outputPath(`leg-${String(WAITS[i]?.wait.reason)}.png`), fullPage: true });
  }
});

test("the queue's breakdown names room kept, waiting behind it, and waiting for a gang", async ({ page }, info) => {
  const insight = {
    count: 12,
    success: 10,
    failure: 2,
    cancelled: 0,
    skipped: 0,
    flaky: 1,
    retried: 1,
    success_rate: 0.83,
    flaky_rate: 0.08,
    p50: 600,
    p90: 900,
    p99: 1000,
    mean: 650,
    queue_p50: 30,
    queue_p90: 400,
    queue_seconds: 1860,
    run_seconds: 7200,
    cpu_seconds: 7000,
    peak_rss: 0,
    runner_minutes: 120,
    failures: {},
  };
  const waits = { reserved: 840, behind: 420, gang: 300, capacity: 300 };
  await page.route("**/api/ci/insights/workflows/**", (route) =>
    route.fulfill({
      json: {
        since: Math.floor(AT / 1000) - 30 * 86_400,
        days: 30,
        grain: "day",
        currency: "",
        ...insight,
        waits,
        repo: { id: "acme/sim", name: "acme/sim" },
        workflow: ".kiln/ci.yaml",
        name: "ci",
        series: [],
        jobs: [{ ...insight, waits, job: "sim", series: [] }],
        steps: [],
        executors: [],
        cache: [],
        critical_path: null,
      },
    }),
  );
  await page.goto(`/insights?repo=${encodeURIComponent("acme/sim")}&workflow=${encodeURIComponent(".kiln/ci.yaml")}`);
  for (const name of ["Room kept for it", "Behind room kept for another", "Waiting for its gang"]) {
    await expect(page.getByText(name, { exact: true })).toBeVisible({ timeout: 30_000 });
  }
  await page.screenshot({ path: info.outputPath("breakdown.png"), fullPage: true });
});
