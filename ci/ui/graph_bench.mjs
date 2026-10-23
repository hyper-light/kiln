// W21-AC03 (docs/ci.md): the run graph of a 1,000-leg run renders in < 200 ms and pans at 60 fps
// on this machine, in Chromium, against graph_setup.py's two runs (a 1,000-leg run of four
// 250-leg matrices and two jobs after them, and one of 200 jobs of five legs). Rendering: from the last of the data the
// graph needs (the run's jobs, its legs, its graph) arriving in the page to the first frame drawn with every
// node in it, the median of three loads. Panning: the page scrolled through the graph a frame at a
// time for two seconds (a graph wider than its space stacks, so nothing scrolls sideways), each
// frame's time from requestAnimationFrame; a Chromium trace of it is kept for review.
//
//     docker run --rm --network host -e GRAPH="$(cat /tmp/graph.json)" \
//       -v $PWD/ci/ui:/check:ro -v <dir with node_modules/playwright-core>:/pw:ro -v /tmp/graph-shots:/shots \
//       mcr.microsoft.com/playwright:v1.64.1-noble node /check/graph_bench.mjs
import { chromium } from "/pw/node_modules/playwright-core/index.mjs";

const cfg = JSON.parse(process.env.GRAPH);
const shots = "/shots";
const report = [];
let failed = 0;
const check = (what, ok, detail = "") => {
  report.push(`${ok ? "ok  " : "FAIL"} ${what}${detail ? ` (${detail})` : ""}`);
  if (!ok) failed++;
};
const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

// When the data arrives, and the first frame with the whole graph in it.
const TIMING = () => {
  const t = { run: 0, detail: 0, graph: 0, painted: 0, nodes: 0 };
  window.__graph = t;
  const fetch0 = window.fetch;
  window.fetch = async (...a) => {
    const r = await fetch0(...a);
    const u = String(a[0]?.url ?? a[0]);
    if (/\/api\/runs\/[^/?]+(\?|$)/.test(u)) t.run = performance.now();
    if (/\/api\/ci\/runs\/[^/?]+(\?|$)/.test(u)) t.detail = performance.now();
    if (/\/api\/ci\/runs\/[^/?]+\/graph(\?|$)/.test(u)) t.graph = performance.now();
    return r;
  };
  const watch = new MutationObserver(() => {
    const g = document.querySelector('[aria-label="Job dependency graph"]');
    const n = g ? g.querySelectorAll(".dag-node").length : 0;
    if (n && n >= (window.__want ?? 1) && !t.painted) {
      t.nodes = n;
      requestAnimationFrame(() => requestAnimationFrame(() => (t.painted = performance.now())));
      watch.disconnect();
    }
  });
  watch.observe(document, { childList: true, subtree: true });
};

const browser = await chromium.launch();
try {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const r = await ctx.request.post(`${cfg.base}/api/users/lead/link`, { headers: { Authorization: `Bearer ${cfg.admin}` } });
  const signIn = await ctx.newPage();
  await signIn.goto((await r.json()).url);
  await signIn.close();
  for (const [kind, run] of Object.entries(cfg.runs)) {
    // How many nodes its graph draws: a matrix job's legs are one node, and a job with no legs yet
    // (what it needs isn't done) one from the run's plan.
    const graph = await (await ctx.request.get(`${cfg.base}/api/ci/runs/${run}/graph`, { headers: { Authorization: `Bearer ${cfg.admin}` } })).json();
    const made = (job) => graph.nodes.some((n) => n.job === job || n.job.startsWith(`${job}/`));
    const want = graph.nodes.length + (graph.jobs ?? []).filter((j) => !made(j.job)).length;
    const legs = graph.nodes.reduce((n, j) => n + j.legs.length, 0);
    const times = [];
    let page;
    for (let i = 0; i < 3; i++) {
      page = await ctx.newPage();
      await page.addInitScript(`window.__want = ${want}; (${TIMING.toString()})();`);
      await page.goto(`${cfg.base}/runs/${run}`);
      await page.waitForFunction(() => window.__graph?.painted > 0, null, { timeout: 60000 });
      const t = await page.evaluate(() => window.__graph);
      times.push(t.painted - Math.max(t.run, t.detail, t.graph));
      if (i < 2) await page.close();
    }
    check(`${kind}: ${legs} legs, ${want} nodes: the graph renders in < 200 ms`, legs >= 1000 && median(times) < 200, `median ${median(times).toFixed(1)} ms of ${times.map((x) => x.toFixed(1)).join(", ")}`);
    // What W21-R04 asks of it: each matrix job's legs as a grid, a cell a leg, its axes said; and
    // the jobs after them, whose legs aren't made yet, as what they wait for.
    const dag = page.locator('[aria-label="Job dependency graph"]');
    const grids = await dag.locator(".dag-grid").count();
    const cells = await dag.locator(".dag-grid .dag-cell:not([data-empty])").count();
    const axes = [...new Set(await dag.locator(".dag-axes").allInnerTexts())];
    const matrices = graph.nodes.filter((n) => n.legs.length > 1).length;
    check(`${kind}: each matrix job's legs as a grid, its axes said`, grids === matrices && cells === legs && axes.length === 1 && axes[0] === (kind === "wide" ? "x → · y ↓" : "n →"), `${grids} grids of ${matrices}, ${cells} cells, ${axes.join(" | ")}`);
    if (kind === "wide") {
      const waits = (await dag.locator(".dag-node[data-waiting]").allInnerTexts()).map((t) => t.replace(/\s+/g, " "));
      check(`${kind}: the jobs after them, as what they wait for`, waits.length === 2 && waits.some((t) => /merge waits for build-a, build-b \+2/.test(t)) && waits.some((t) => /ship waits for merge/.test(t)), waits.join(" | "));
    }
    // Panning: through the graph, a frame at a time, for two seconds.
    await browser.startTracing(page, { path: `${shots}/graph-${kind}-trace.json`, screenshots: false });
    const frames = await page.evaluate(async () => {
      const g = document.querySelector('[aria-label="Job dependency graph"]');
      window.scrollTo(0, (g?.getBoundingClientRect().top ?? 0) + window.scrollY - 100);
      const span = Math.max(0, (g?.getBoundingClientRect().height ?? 0) - window.innerHeight / 2);
      const at = [];
      const start = performance.now();
      await new Promise((done) => {
        const step = (now) => {
          at.push(now);
          window.scrollBy(0, Math.max(4, span / 120));
          if (now - start < 2000) requestAnimationFrame(step);
          else done();
        };
        requestAnimationFrame(step);
      });
      return at;
    });
    await browser.stopTracing();
    const gaps = frames.slice(1).map((t, i) => t - (frames[i] ?? t));
    const fps = (gaps.length * 1000) / (frames[frames.length - 1] - frames[0]);
    const slow = gaps.filter((g) => g > 25).length;
    check(`${kind}: pans at 60 fps`, fps >= 57 && slow <= gaps.length * 0.05, `${fps.toFixed(1)} fps, ${slow} of ${gaps.length} frames over 25 ms, the slowest ${Math.max(...gaps).toFixed(1)} ms`);
    await page.screenshot({ path: `${shots}/graph-${kind}.png` });
    await page.close();
  }
} finally {
  await browser.close();
}

console.log(report.join("\n"));
process.exit(failed ? 1 : 0);
