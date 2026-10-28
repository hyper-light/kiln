import assert from "node:assert/strict";
import { test } from "node:test";
import { events } from "../src/api/events.ts";
import { waitSentence } from "../src/api/ci.ts";
import { placementWords, retryChoices, type RetryOptions } from "../src/api/retry.ts";
import type { Agent, ServerEvent } from "../src/api/types.ts";
import { agentsRes, byLabel, labelOf, noteLabels, shownName, useMachineLabels } from "../src/lib/data.ts";

/** A machine kiln knows by its machine id (docs/machines.md): the id is its name, its key everywhere. */
const ID = "5f2a9c3e0b1d4e6f8a7c9b0d1e2f3a4b";
const MBP = "adas-macbook-pro-3.local";

const agent = (over: Partial<Agent>): Agent => ({ name: "fedora", hostname: "fedora", os: "linux", arch: "x86_64", connected: true, labels: [], ...over }) as Agent;

/** What the event stream hands its listeners, as if the server had sent it. */
const emit = (ev: ServerEvent) => (events as unknown as { handlers: Set<(e: ServerEvent) => void> }).handlers.forEach((h) => h(ev));

test("a machine known by its id is shown by its label; one known by its name, by that name", () => {
  useMachineLabels.setState({}, true);
  noteLabels([{ name: ID, label: MBP }, { name: "fedora" }, { name: "pi", label: null }, { name: "gpu", label: "" }]);
  assert.equal(labelOf(ID), MBP);
  assert.equal(labelOf("fedora"), "fedora");
  assert.equal(labelOf("pi"), "pi");
  assert.equal(labelOf("never-seen"), "never-seen");
  // Only the labels are kept.
  assert.deepEqual(useMachineLabels.getState(), { [ID]: MBP });
  // A name that is also a property every object has is no label.
  assert.equal(labelOf("constructor"), "constructor");
  assert.equal(labelOf("toString"), "toString");
});

test("the labels change only when one does: noting the same ones again leaves them be", () => {
  useMachineLabels.setState({}, true);
  noteLabels([{ name: ID, label: MBP }]);
  const was = useMachineLabels.getState();
  let changes = 0;
  const stop = useMachineLabels.subscribe(() => changes++);
  try {
    noteLabels([{ name: ID, label: MBP }, { name: "fedora" }]);
    noteLabels([]);
    assert.equal(useMachineLabels.getState(), was);
    assert.equal(changes, 0);
    // Renamed: one change for however many it brings.
    noteLabels([
      { name: ID, label: "studio" },
      { name: "6e1b0d2c4a3f5e7d9c8b0a1f2e3d4c5b", label: "pi" },
    ]);
    assert.equal(changes, 1);
    assert.equal(labelOf(ID), "studio");
    assert.equal(labelOf("6e1b0d2c4a3f5e7d9c8b0a1f2e3d4c5b"), "pi");
  } finally {
    stop();
  }
});

test("every load of the agents notes their labels, and a rename's agent event the new one", async () => {
  useMachineLabels.setState({}, true);
  const was = globalThis.fetch;
  globalThis.fetch = async () => new Response(JSON.stringify([agent({ name: ID, label: MBP }), agent({ name: "fedora" })]), { status: 200 });
  try {
    const list = await agentsRes.fetch(new AbortController().signal);
    assert.deepEqual(
      list.map((a) => a.name),
      [ID, "fedora"],
    );
  } finally {
    globalThis.fetch = was;
  }
  assert.equal(labelOf(ID), MBP);
  emit({ type: "agent", agent: agent({ name: ID, label: "studio" }) });
  assert.equal(labelOf(ID), "studio");
});

test("shown by its label, put in order by it, still kept by its name", () => {
  const mbp = agent({ name: ID, label: MBP });
  assert.equal(shownName(mbp), MBP);
  assert.equal(shownName(agent({ name: "fedora" })), "fedora");
  assert.equal(shownName({ name: "pi", label: null }), "pi");
  // Labels handed in (a component's) are the ones used.
  const labels = { [ID]: "zz-last", "6e1b0d2c4a3f5e7d9c8b0a1f2e3d4c5b": "aa-first" };
  assert.deepEqual([ID, "fedora", "6e1b0d2c4a3f5e7d9c8b0a1f2e3d4c5b"].sort(byLabel(labels)), ["6e1b0d2c4a3f5e7d9c8b0a1f2e3d4c5b", "fedora", ID]);
  assert.equal(labelOf(ID, labels), "zz-last");
});

test("why a leg waits names its runners by their labels, not their machine ids", () => {
  useMachineLabels.setState({ [ID]: MBP }, true);
  assert.equal(waitSentence({ reason: "no-runner", offline: [ID, "fedora"] }), `No runner that can take it is connected: ${MBP}, fedora.`);
  assert.equal(waitSentence({ reason: "pinned", runner: ID, state: "offline" }), `Waiting for ${MBP}, where its retry asked it to run, to connect.`);
  assert.equal(waitSentence({ reason: "other-runner", avoided: ID, asked: true }), `Waiting for a runner other than ${MBP}, as its retry asked.`);
  assert.equal(waitSentence({ reason: "warm", runner: ID, saves_s: 30 }), `Waiting for ${MBP}, where it runs warm: about 30 s sooner.`);
  // A component's labels, handed in.
  assert.equal(waitSentence({ reason: "capability", runners: [ID] }, { [ID]: "studio" }), "The runners that match need a newer kiln: studio.");
});

test("a retry offers runners by their labels and asks for them by their names", () => {
  useMachineLabels.setState({ [ID]: MBP }, true);
  const o: RetryOptions = {
    job: "j1",
    run: "r1",
    key: "build",
    kind: "pipeline",
    ran_on: ID,
    elsewhere: true,
    retry: { can: true, why: null },
    runners: [{ name: ID, connected: true, draining: false, busy: 0, slots: 2, able: true, why: [] }],
  };
  assert.deepEqual(retryChoices(o), [
    { value: "", label: "Any runner that can run it" },
    { value: "elsewhere", label: `Any runner but ${MBP}` },
    { value: `runner:${ID}`, label: `On ${MBP} (ran here)` },
  ]);
  assert.equal(placementWords({ runner: ID }), `on ${MBP}`);
  assert.equal(placementWords({ avoid: ID }), `anywhere but ${MBP}`);
});
