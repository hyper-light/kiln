import assert from "node:assert/strict";
import { test } from "node:test";
import type { AgentInfo, State, Status } from "../src/api/harness.ts";
import { defaultInfo, defaultState } from "../mock/defaults.ts";
import { sessionAvailability, sessionStatus, stateAtOpen } from "../src/api/session-availability.ts";

const info = (status: Status): AgentInfo => ({ ...defaultInfo(), status });
const state = (status: Status): State => ({ ...defaultState(), status });

test("W20: a command route does not imply Running or erase owner lifecycle", () => {
  for (const status of ["starting", "idle", "running", "waiting", "exited"] as const) {
    const snapshot = { conn: "open" as const, info: info(status), state: state("disconnected") };
    assert.equal(sessionAvailability(snapshot), "available", status);
    assert.equal(sessionStatus(snapshot), status, status);
  }
  const paused = { ...state("idle"), pause: { reason: "Usage exhausted", since: 1, auto: true, by: "harness" }, frozen: 1 };
  assert.equal(stateAtOpen(paused, info("idle")), paused, "a real pause and its details are retained");
  assert.equal(sessionStatus({ info: info("idle"), state: paused }), "idle");
});

test("W20: browser connection, owner availability and absent details stay distinct", () => {
  assert.equal(sessionAvailability({ conn: "open", info: null, state: null }), "connecting", "no owner lifecycle has arrived");
  assert.equal(sessionAvailability({ conn: "open", info: info("starting"), state: null }), "available", "details are not a command-route prerequisite");
  assert.equal(stateAtOpen(null, info("starting")), null, "no phantom default State");
  assert.equal(sessionAvailability({ conn: "open", info: info("disconnected"), state: state("running") }), "offline");
  assert.equal(sessionAvailability({ conn: "reconnecting", info: info("running"), state: state("running") }), "connecting");
  assert.equal(sessionAvailability({ conn: "gone", info: info("running"), state: state("running") }), "gone");
});

test("W20: an owner lifecycle update replaces no queue, delivery or body values", () => {
  const before = state("disconnected");
  const after = stateAtOpen(before, info("idle"));
  assert.equal(after?.status, "idle");
  assert.equal(after?.queue, before.queue);
  assert.equal(after?.tasks, before.tasks);
  assert.equal(after?.delivery, before.delivery);
  assert.equal(after?.view, before.view);
  assert.equal(stateAtOpen(before, null), before, "absent Info supplies no invented authority");
});
