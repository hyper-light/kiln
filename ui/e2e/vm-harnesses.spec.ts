// Every harness in a Shards microVM, from kiln's UI (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, steps
// 34 and 35; crates/kiln/tests/vm_e2e/README.md). Against the mock: kiln's ten Agentfiles listed read
// only; Claude Code and Codex started in a microVM from kiln's, refused where the machine starts none;
// Codex from a person's own. Against ROOT's server (E2E_BASE_URL, E2E_TOKEN, E2E_VM_MACHINE, the name
// of its runner that starts microVMs): each harness started from kiln's Agentfile and from a person's
// own, its task's turn done, a message steered in, a turn stopped, the agent ended and its conversation
// picked up again from the composer, all in the page, with kiln's word on each read from its API. A
// harness the composer can't pick up (crates/kiln/tests/vm_e2e/gaps.json's ui_pick_up) says its gap.
// Each is its vendor's own CLI, which takes ROOT's task alone: that half skips until each of its words
// is given (VM_E2E_PROMPT, VM_E2E_RESUMED_PROMPT, VM_E2E_STEERED_PROMPT, VM_E2E_STOPPED_PROMPT,
// VM_E2E_DONE_MARKER). Where it ran, its links' suites, its MCP round trip and the agents it starts are
// the Rust drive's checks (crates/kiln/tests/vm_e2e/step35.rs).
import { readFileSync } from "node:fs";
import { expect as expectNow, test, type Page } from "@playwright/test";

/** Each wait on the server gives it 15 s, as the other specs' do; a harness's turn, VM_E2E_TURN_SECS. */
const expect = expectNow.configure({ timeout: 15_000 });
const TURN_MS = Number(process.env.VM_E2E_TURN_SECS ?? 600) * 1000;

const REAL = !!process.env.E2E_BASE_URL;
const TOKEN = process.env.E2E_TOKEN;
const VM_MACHINE = process.env.E2E_VM_MACHINE;

/** The ten harnesses of steps 19 to 29: kiln's id, and its name as the composer shows it. */
const HARNESSES = [
  ["claude", "Claude Code"],
  ["codex", "Codex"],
  ["gemini", "Gemini CLI"],
  ["opencode", "opencode"],
  ["goose", "Goose"],
  ["amp", "Amp"],
  ["devin", "Devin"],
  ["antigravity", "Antigravity CLI"],
  ["cursor", "Cursor CLI"],
  ["pi", "Pi"],
] as const;

/** The mock's one machine that starts microVMs (ui/mock/data.ts MICROVM_RUNNERS). */
const MOCK_VM = "metal-mac-mini";

const sideways = (page: Page) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);

/** What kiln recorded of how session `id` is held. */
async function isolationOf(page: Page, id: string) {
  const list = (await (await page.request.get("/api/harness/sessions")).json()) as { id: string; isolation?: { kind: string; image?: string } }[];
  return list.find((a) => a.id === id)?.isolation;
}

/** Picks harness `name` in Mission control's composer by a model of its, as a harness switch does. */
async function toHarness(page: Page, name: string) {
  const model = page.locator(".mc-how").locator('.rs-v[data-kind="model"]');
  if ((await model.getAttribute("aria-expanded")) !== "true") await model.click();
  await page
    .locator(".mc-how")
    .getByRole("group", { name: `${name} models` })
    .getByRole("radio")
    .first()
    .click();
}

/** In More options: the Agentfile its microVM is made from (by its image), then machine `machine`. */
async function chooseAgentfileAndMachine(page: Page, name: string, image: string, machine: RegExp) {
  await page.getByRole("button", { name: /^More options/ }).click();
  await page.getByLabel(`The Agentfile ${name}'s microVM is made from`).selectOption(image);
  await page
    .locator(".mc-opt")
    .filter({ has: page.locator(".mc-opt-key", { hasText: /^Machine$/ }) })
    .locator("button.mc-pick")
    .click();
  await page.getByRole("radiogroup", { name: "Machine" }).getByRole("radio", { name: machine }).click();
}

/** Starts it from the composer: its id, from the page it opens. */
async function start(page: Page): Promise<string> {
  await page.locator(".mc-start").click();
  await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
  return new URL(page.url()).pathname.split("/").pop() ?? "";
}

test.describe("against the mock", () => {
  test.skip(REAL, "the mock's machines and Agentfiles");

  test("step35: kiln's ten Agentfiles are listed read-only, one for each harness", async ({ page }) => {
    await page.goto("/agents/agentfiles");
    await expect(page.getByRole("heading", { level: 1, name: "Agentfiles" })).toBeVisible();
    const kilns = page.getByRole("list", { name: "kiln's Agentfiles" });
    await expect(kilns.getByRole("listitem")).toHaveCount(HARNESSES.length);
    for (const [kind] of HARNESSES) {
      const row = kilns.getByRole("listitem").filter({ hasText: `harness-${kind}` });
      await expect(row.getByRole("button", { name: "Read" })).toBeVisible();
      await expect(row.getByRole("button", { name: "Copy as yours" })).toBeVisible();
    }
    expect(await sideways(page)).toBe(0);
  });

  test("step34: Claude Code and Codex each start in a microVM from kiln's Agentfile, which the server itself can't hold", async ({ page }) => {
    for (const [kind, name] of [
      ["claude", "Claude Code"],
      ["codex", "Codex"],
    ] as const) {
      await page.goto("/agents");
      if (kind !== "claude") await toHarness(page, name);
      await page.getByRole("button", { name: /^More options/ }).click();
      await page.getByLabel(`The Agentfile ${name}'s microVM is made from`).selectOption(`harness-${kind}`);
      // The server's own kiln starts none: said under the row, and nothing starts there.
      await expect(page.locator(".mc-opts .mc-note")).toContainText("kiln's server starts no microVMs itself");
      await page
        .locator(".mc-opt")
        .filter({ has: page.locator(".mc-opt-key", { hasText: /^Machine$/ }) })
        .locator("button.mc-pick")
        .click();
      await page
        .getByRole("radiogroup", { name: "Machine" })
        .getByRole("radio", { name: new RegExp(MOCK_VM) })
        .click();
      await expect(page.locator(".mc-opts .mc-note")).toHaveCount(0);
      await expect(page.locator(".mc-start")).toHaveAttribute("aria-label", new RegExp(`^Start: ${name} on ${MOCK_VM} .*in a microVM from harness-${kind}$`));
      expect(await sideways(page)).toBe(0);
      const id = await start(page);
      await expect.poll(() => isolationOf(page, id)).toEqual({ kind: "microvm", image: `harness-${kind}` });
      await expect(page.getByRole("textbox", { name: "Message to the agent" })).toBeVisible();
    }
  });

  test("step35: Codex starts in a microVM from a person's own Agentfile, and the choice is kept for Codex there", async ({ page }) => {
    const mine = ((await (await page.request.get("/api/agentfiles")).json()) as { agentfiles: { id: string; name: string }[] }).agentfiles.find((a) => a.name === "claude-rust");
    expect(mine).toBeTruthy();
    await page.goto("/agents");
    await toHarness(page, "Codex");
    await chooseAgentfileAndMachine(page, "Codex", `agentfile:${mine?.id}`, new RegExp(MOCK_VM));
    await expect(page.locator(".mc-start")).toHaveAttribute("aria-label", /^Start: Codex on .*in a microVM from claude-rust$/);
    const id = await start(page);
    await expect.poll(() => isolationOf(page, id)).toEqual({ kind: "microvm", image: `agentfile:${mine?.id}` });
    await page.goto("/agents");
    await toHarness(page, "Codex");
    await page.getByRole("button", { name: /^More options/ }).click();
    await expect(page.getByLabel("The Agentfile Codex's microVM is made from")).toHaveValue(`agentfile:${mine?.id}`);
  });
});

/** ROOT's task, a word for each turn (crates/kiln/tests/vm_e2e/mod.rs TASK_WORDS): a vendor's own CLI takes these alone, never a stand-in's words. */
const TASK_WORDS = ["VM_E2E_PROMPT", "VM_E2E_RESUMED_PROMPT", "VM_E2E_STEERED_PROMPT", "VM_E2E_STOPPED_PROMPT", "VM_E2E_DONE_MARKER"] as const;
const UNTASKED = TASK_WORDS.filter((v) => !process.env[v]);

test.describe("against ROOT's server", () => {
  test.skip(!REAL || !TOKEN || !VM_MACHINE, "a real kiln server and its runner that starts microVMs: E2E_BASE_URL, E2E_TOKEN, E2E_VM_MACHINE (crates/kiln/tests/vm_e2e/README.md)");
  test.skip(UNTASKED.length > 0, `each harness here is its vendor's own CLI (kiln's Agentfile, or a person's own made from it), which takes ROOT's task alone: ${UNTASKED.join(", ")} not given`);
  test.setTimeout(6 * TURN_MS);

  test.beforeEach(async ({ page }) => {
    expectNow((await page.request.post("/api/auth/login", { data: { token: TOKEN } })).ok()).toBe(true);
  });

  /** ROOT's words for a turn, `{kind}` the harness and `{file}` a file of the run's own (README.md); each carries the run's own word, so its rows and lines are told from any other's. */
  const words = (v: (typeof TASK_WORDS)[number], kind: string, own: string) => `${(process.env[v] ?? "").replaceAll("{kind}", kind).replaceAll("{file}", `vm-e2e-${own}.txt`)} ${own}`;
  const marker = process.env.VM_E2E_DONE_MARKER ?? "";

  /** What each harness's composer can pick up again (gaps.json's ui_pick_up), and why not where it can't. */
  const gaps = JSON.parse(readFileSync(new URL("../../crates/kiln/tests/vm_e2e/gaps.json", import.meta.url), "utf8")) as Record<
    string,
    { ui_pick_up?: boolean; gaps?: { what: string; why: string; cites: string[] }[] }
  >;
  const pickUpGap = (kind: string) => gaps[kind]?.gaps?.find((g) => g.what === "ui_pick_up");

  /** What session `id`'s harness said, its replies and its tools' output. */
  async function said(page: Page, id: string): Promise<string> {
    const r = await page.request.get(`/api/harness/sessions/${id}/events?limit=1000`);
    const events = (await r.json()) as { kind?: string; text?: string; output?: string }[];
    return events.map((e) => (e.kind === "assistant" ? e.text : e.kind === "tool" ? e.output : "") ?? "").join("\n");
  }

  /** Session `id` as kiln lists it, or `gone <status>`. */
  async function infoOf(page: Page, id: string): Promise<{ status?: string; harness_session?: string; isolation?: { kind: string; image?: string } } | string> {
    const r = await page.request.get(`/api/harness/sessions/${id}`);
    return r.ok() ? ((await r.json()) as { status?: string; harness_session?: string; isolation?: { kind: string; image?: string } }) : `gone ${r.status()}`;
  }
  const statusOf = async (page: Page, id: string) => {
    const i = await infoOf(page, id);
    return typeof i === "string" ? i : (i.status ?? "");
  };

  for (const [kind, name] of HARNESSES) {
    test(`step35: ${name} from kiln's Agentfile and from a person's own, in a microVM: started, steered, stopped, ended and picked up again in the page`, async ({ page }, info) => {
      test.skip(info.project.name !== "desktop", "once, on the desktop: each start is a real harness's");
      // A person's own, made from kiln's for it, as Copy as yours makes one.
      const listing = (await (await page.request.get("/api/agentfiles")).json()) as { builtin: { harness: string; text: string }[] };
      const text = listing.builtin.find((b) => b.harness === kind)?.text ?? "";
      const kept = await page.request.post("/api/agentfiles", { data: { name: `${kind}-own-${Date.now()}`, text } });
      expectNow(kept.ok(), await kept.text()).toBe(true);
      const own = `agentfile:${((await kept.json()) as { agentfile: { id: string } }).agentfile.id}`;

      for (const image of [`harness-${kind}`, own]) {
        const nonce = `e2e-${Date.now().toString(36)}`;
        await page.goto("/agents");
        await toHarness(page, name);
        await chooseAgentfileAndMachine(page, name, image, new RegExp(VM_MACHINE ?? ""));
        await page.getByLabel("Its first message").fill(words("VM_E2E_PROMPT", kind, nonce));
        const id = await start(page);
        await expect.poll(() => isolationOf(page, id)).toEqual({ kind: "microvm", image });
        await expect.poll(() => said(page, id), { timeout: TURN_MS }).toContain(marker);

        // A long turn, a message steered into it, then the turn stopped from the page.
        const field = page.getByRole("textbox", { name: "Message to the agent" });
        const send = page.locator(".hx-dock .cp-send");
        await field.fill(words("VM_E2E_STOPPED_PROMPT", kind, nonce));
        await send.click();
        await expect.poll(() => statusOf(page, id), { timeout: TURN_MS }).toBe("running");
        await field.fill(`STEER ${nonce}`);
        await send.click();
        await page.getByRole("button", { name: "Stop the current turn" }).click();
        await expect.poll(() => statusOf(page, id), { timeout: 60_000 }).toBe("idle");

        // Ended from its actions, after the question asked in place; its conversation is kept.
        const was = await infoOf(page, id);
        const conversation = typeof was === "string" ? undefined : was.harness_session;
        expectNow(conversation, `kiln knows no conversation of ${id} to pick up again`).toBeTruthy();
        await page.getByRole("button", { name: /^Actions for / }).click();
        await page.getByRole("menuitem", { name: "End agent…" }).click();
        await page.getByRole("button", { name: "End agent", exact: true }).click();
        await expect.poll(() => statusOf(page, id), { timeout: 60_000 }).toMatch(/^gone 404/);

        // Picked up again from the composer's past sessions, in a microVM as it ran: where the
        // composer lists this harness's conversations; else its gap, cited.
        const gap = pickUpGap(kind);
        if (gap) {
          info.annotations.push({ type: "gap", description: `${kind}: ui_pick_up: ${gap.why} (${gap.cites.join(", ")})` });
          continue;
        }
        await page.goto("/agents");
        await toHarness(page, name);
        await chooseAgentfileAndMachine(page, name, image, new RegExp(VM_MACHINE ?? ""));
        await page.getByRole("button", { name: /^A past session on / }).click();
        await page.locator(".ps-list .ps-row").filter({ hasText: nonce }).first().click();
        await expect(page).toHaveURL(/\/agents\/[0-9a-f]{32}$/);
        const again = new URL(page.url()).pathname.split("/").pop() ?? "";
        await expect
          .poll(
            async () => {
              const i = await infoOf(page, again);
              return typeof i === "string" ? i : { conversation: i.harness_session, kind: i.isolation?.kind };
            },
            { timeout: TURN_MS },
          )
          .toEqual({ conversation, kind: "microvm" });
        await page.getByRole("button", { name: /^Actions for / }).click();
        await page.getByRole("menuitem", { name: "End agent…" }).click();
        await page.getByRole("button", { name: "End agent", exact: true }).click();
        await expect.poll(() => statusOf(page, again), { timeout: 60_000 }).toMatch(/^gone 404/);
      }
    });
  }
});
