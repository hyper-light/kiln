/**
 * U05: the actual Ship LogViewer preserves an incomplete producer prefix when its socket closes.
 * These cases run at every configured browser/device project. One selected commit's read-model
 * input is pinned; only its chosen job/step is controlled. Other routes and sockets stay normal.
 */
import { expect, test, type Route, type WebSocketRoute } from "@playwright/test";
import type { Job } from "../src/api/types";
import type { ShipDetail, ShipPage } from "../src/api/ship";

test("required output ends only on End, including bytes queued behind timing metadata", async ({ page }) => {
  const commits = (await (await page.request.get("/api/ship/commits")).json()) as ShipPage;
  const commit = commits.commits[0];
  if (!commit) throw new Error("the mock has no Ship commit");
  const repo = commit.repo.split("/").map(encodeURIComponent).join("/");
  const detailPath = "/api/ship/commits/" + repo + "/" + encodeURIComponent(commit.sha);
  const detail = (await (await page.request.get(detailPath)).json()) as ShipDetail;
  const picked = (detail.checks ?? []).flatMap((check) => check.legs.map((leg) => ({ ...leg, shown: check.pipeline + " · " + leg.name }))).find((leg) => leg.job);
  if (!picked?.job) throw new Error("the mock's first commit has no job output");
  const jobId = picked.job;
  const shown = picked.shown;
  const jobPath = "/api/jobs/" + encodeURIComponent(jobId);
  const original = (await (await page.request.get(jobPath)).json()) as Job;
  const first = original.steps[0];
  if (!first) throw new Error("the chosen mock job has no step");
  const step = first.idx;
  const begun = original.started_at ?? 1_790_000_000;
  const fixedJob: Job = {
    ...original,
    status: "running",
    started_at: begun,
    finished_at: null,
    exit_code: null,
    steps: [{ ...first, status: "running", started_at: begun, finished_at: null, exit_code: null }],
  };
  const logPath = jobPath + "/logs";
  const pending = "Output is not complete yet.";
  const incomplete = "Output is not complete yet. Reload to try again.";

  for (const scenario of [
    { name: "binary prefix survives close without End", marker: "u05-once-A", complete: false, holdTimes: false },
    { name: "zero-byte End clears Pending before close", marker: "", complete: true, holdTimes: false },
    { name: "close preserves bytes queued behind timing metadata", marker: "u05-once-C", complete: false, holdTimes: true },
  ]) {
    await test.step(scenario.name, async () => {
      const view = await page.context().newPage();
      try {
        const sockets: WebSocketRoute[] = [];
        const times: Route[] = [];
        const fallbackGets: string[] = [];
        // Observe the real fetch call synchronously: a zero network counter before an async
        // request arrives would not prove that close avoided the fallback. Delegate unchanged.
        await view.addInitScript(
          ({ logPath, step }) => {
            const observed = window as unknown as { __producerLogGets: string[] };
            observed.__producerLogGets = [];
            const originalFetch = window.fetch.bind(window);
            window.fetch = (input, init) => {
              const url = new URL(input instanceof Request ? input.url : String(input), window.location.href);
              const method = (init?.method ?? (input instanceof Request ? input.method : "GET")).toUpperCase();
              if (method === "GET" && url.pathname === logPath && url.searchParams.get("step") === String(step)) observed.__producerLogGets.push(url.pathname + url.search);
              return originalFetch(input, init);
            };
          },
          { logPath, step },
        );
        // Keep only the selected row's read-model input stable while the mock's live run
        // advances. Other commit rows, event sockets and job routes are left alone.
        await view.route(
          (url) => url.pathname === detailPath,
          async (route) => {
            if (route.request().method() !== "GET") return route.fallback();
            await route.fulfill({ json: detail });
          },
        );
        await view.route(
          (url) => url.pathname === jobPath,
          async (route) => {
            if (route.request().method() !== "GET") return route.fallback();
            await route.fulfill({ json: fixedJob });
          },
        );
        await view.route(
          (url) => url.pathname === logPath + "/times" && url.searchParams.get("step") === String(step),
          async (route) => {
            if (route.request().method() !== "GET") return route.fallback();
            times.push(route);
            if (!scenario.holdTimes) await route.fulfill({ json: { times: [], size: null } });
          },
        );
        await view.route(
          (url) => url.pathname === logPath && url.searchParams.get("step") === String(step),
          async (route) => {
            if (route.request().method() !== "GET") return route.fallback();
            fallbackGets.push(route.request().url());
            await route.fulfill({ contentType: "text/plain", body: "unexpected fallback response\n" });
          },
        );
        await view.routeWebSocket(
          (url) => url.pathname === logPath + "/ws" && url.searchParams.get("step") === String(step),
          (socket) => {
            sockets.push(socket);
          },
        );

        await view.goto("/ship");
        const row = view.getByRole("list", { name: "Commits", exact: true }).locator(":scope > li").first();
        await row.getByRole("button", { name: /^Open / }).click();
        await row.getByRole("list", { name: "Checks jobs" }).getByRole("button", { name: shown, exact: true }).first().click();
        const out = row.locator(".ship-output");
        await expect(out).toHaveAttribute("data-job", jobId);
        await expect(out).toHaveAttribute("data-step", String(step));
        await expect(out.locator(".xterm")).toBeVisible();
        await expect.poll(() => sockets.length).toBe(1);
        await expect.poll(() => times.length).toBe(1);
        const socket = sockets[0];
        const timing = times[0];
        if (!socket || !timing) throw new Error("the chosen step did not open its socket and timing request");
        const rendered = async () => (await out.locator(".xterm-rows > div").allTextContents()).join("\n");
        const copies = async () => (await rendered()).split(scenario.marker).length - 1;

        socket.send(JSON.stringify({ type: "pending", message: pending }));
        await expect(out.getByRole("status")).toHaveText(pending);
        if (scenario.complete) {
          // End0 sends no binary prefix: closure must still avoid the legacy empty fallback.
          socket.send(JSON.stringify({ type: "end" }));
        } else {
          socket.send(Buffer.from("\x1b[32m" + scenario.marker + "\x1b[0m\r\n", "utf8"));
          if (!scenario.holdTimes) {
            await expect.poll(copies).toBe(1);
            await expect(out.getByRole("status")).toBeHidden();
          }
        }
        // Playwright's awaited page-side close dispatch is the explicit message/close barrier.
        // The DOM checks below wait for React and xterm; no wall-clock sleep is involved.
        await socket.close({ code: 1000, reason: "fixture stream closed" });
        if (scenario.complete) {
          await expect(out.getByRole("status")).toBeHidden();
          expect((await rendered()).trim()).toBe("");
        } else {
          await expect(out.getByRole("status")).toHaveText(incomplete);
          if (scenario.holdTimes) {
            expect(await rendered()).not.toContain(scenario.marker);
            expect(await view.evaluate(() => (window as unknown as { __producerLogGets: string[] }).__producerLogGets)).toEqual([]);
            expect(fallbackGets).toEqual([]);
            await timing.fulfill({ json: { times: [], size: null } });
          }
          await expect.poll(copies).toBe(1);
          await expect(out.getByRole("status")).toHaveText(incomplete);
        }
        expect(await rendered()).not.toContain("(no output)");
        expect(await view.evaluate(() => (window as unknown as { __producerLogGets: string[] }).__producerLogGets)).toEqual([]);
        expect(fallbackGets).toEqual([]);
      } finally {
        await view.close();
      }
    });
  }
});
