import assert from "node:assert/strict";
import { test } from "node:test";
import { amountsSaid, answerSaid, cpusSaid, groupSaid, healthSaid, lineSaid, needSaid, startSaid } from "../src/views/offers/words.ts";
import { current } from "../src/views/offers/data.ts";
import type { OfferRunner, OffersView } from "../src/api/wire/index.ts";

const GiB = 1024 ** 3;

test("amounts read as people say them, only the dimensions asked", () => {
  assert.equal(amountsSaid({ cpu_milli: 4000, mem: 8 * GiB, disk: 0, gpu: 0 }), "4 CPUs · 8 GiB");
  assert.equal(amountsSaid({ cpu_milli: 500, mem: 512 * 1024 ** 2, disk: 50 * GiB, gpu: 1 }), "0.5 CPU · 512 MiB · 50 GiB disk · 1 GPU");
  assert.equal(amountsSaid({ cpu_milli: 0, mem: 0, disk: 0, gpu: 0 }), "any size");
  assert.equal(cpusSaid(1000), "1 CPU");
  assert.equal(cpusSaid(1500), "1.5 CPUs");
});

test("a start reads in the unit the server shows it in, counted from when it was said", () => {
  assert.equal(startSaid({ unit: "s", count: 40, from: 0 }), "within a minute");
  assert.equal(startSaid({ unit: "min", count: 1, from: 0 }), "in about 1 minute");
  assert.equal(startSaid({ unit: "h", count: 2, from: 0 }), "in about 2 hours");
  assert.equal(startSaid({ unit: "d", count: 3, from: 0 }), "in about 3 days");
});

test("an answer says now, why it waits and when it starts, or why never", () => {
  const ask = { cpu_milli: 4000, mem: 0, disk: 0, gpu: 0 };
  assert.equal(answerSaid({ ask, answer: "now", derived: false }), "starts now");
  assert.equal(answerSaid({ ask, answer: "never", reason: "past its budget of GPUs", derived: true }), "never: past its budget of GPUs");
  assert.equal(
    answerSaid({ ask, answer: "waits", reason: "kept", soonest: { unit: "min", count: 3, from: 0 }, latest: { unit: "h", count: 1, from: 0 }, derived: false }),
    "waits: room kept for the first in line, starts in about 3 minutes (at the latest in about 1 hour)",
  );
  assert.equal(answerSaid({ ask, answer: "waits", reason: "some-new-reason", derived: false }), "waits: some new reason");
});

test("a runner's line and health, and a group of waiting work, in words", () => {
  const r = {
    queued: [
      { key: "interactive", count: 1 },
      { key: "batch", count: 2 },
    ],
    health: { taking: true },
    connected: true,
    said: true,
  } as unknown as OfferRunner;
  assert.equal(lineSaid(r), "3 wait: 1 interactive, 2 batch");
  assert.equal(lineSaid({ ...r, queued: [] }), "nothing waits");
  assert.equal(healthSaid(r), "takes work");
  assert.equal(healthSaid({ ...r, health: { taking: false, why: "offline" } }), "offline");
  const g = {
    need: "",
    ask: { cpu_milli: 0, mem: 0, disk: 0, gpu: 0 },
    runs: 3,
    by_class: [],
    reasons: [
      { key: "capacity", count: 2 },
      { key: "retry", count: 1 },
    ],
    cursor: "x",
  };
  assert.equal(needSaid(g), "any runner");
  assert.equal(groupSaid(g), "3 wait: 2 every runner that could is busy, 1 waiting to try again");
  assert.equal(groupSaid({ ...g, runs: 2, reasons: [{ key: "capability", count: 2 }] }), "2 wait: no runner has what it needs");
});

test("an offers event of a version the page has leaves it be; a newer one reads it again", () => {
  const v = { version: 7 } as OffersView;
  assert.equal(current(v, { type: "offers", version: 7 }), v);
  assert.equal(current(v, { type: "offers", version: 6 }), v);
  assert.equal(current(v, { type: "offers", version: 8 }), undefined);
  assert.equal(current(v, { type: "resync" }), undefined);
});
