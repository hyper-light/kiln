import assert from "node:assert/strict";
import { test } from "node:test";
import type { PlanItem } from "../src/api/plans.ts";
import type { Finding, Research } from "../src/api/research.ts";
import { allLooks, asItems, changesSince, changeWords, commentLine, digest, flagsOf, headingOf, introOf, runsOf, sameWords, stepBasis, restsOn, stepsWord } from "../src/views/plans/review-model.ts";

const grade = (score: number) => (score >= 85 ? "A" : score >= 70 ? "B" : score >= 50 ? "C" : score >= 30 ? "D" : "F") as Finding["confidence"]["grade"];

function finding(id: string, score: number, more: { against?: number; failed?: number; notFound?: boolean } = {}): Finding {
  return {
    id,
    claim: `claim ${id}`,
    cites: [{ source: "s1", quote: "q", check: { state: more.notFound ? "not_found" : "found", at: 0 } }],
    against: [],
    at: 0,
    confidence: {
      score,
      grade: grade(score),
      words: "",
      support: 0,
      against: more.against ?? 0,
      reliability: 0,
      citation: 0,
      independent: 1,
      cites: 1,
      found: 1,
      failed: more.failed ?? 0,
      why: "",
    },
    against_weights: [],
  };
}

function research(findings: Finding[]): Research {
  return {
    id: "r",
    question: "q",
    agent: null,
    by: "admin",
    plan: "p",
    sources: [],
    findings,
    skipped: {},
    summary: { findings: findings.length, grades: {}, sources: 3, internal: 2, external: 1, checking: 0 },
    created_at: 0,
    updated_at: 0,
  };
}

const step = (title: string, basis: string[] = [], more: Partial<PlanItem> = {}): PlanItem => ({ title, description: "", examples: "", acceptance: [], testing: [], basis, ...more });

test("a finding is worth a look when it's weak, contested or quoted where kiln couldn't find the words", () => {
  assert.deepEqual(flagsOf(finding("f1", 95)), []);
  assert.deepEqual(flagsOf(finding("f2", 41)), ["weak"]);
  assert.deepEqual(flagsOf(finding("f3", 22)), ["weak"]);
  assert.deepEqual(flagsOf(finding("f4", 60)), [], "C isn't weak: plans_ready warns of D and F");
  assert.deepEqual(flagsOf(finding("f5", 72, { against: 0.25 })), ["contested"]);
  assert.deepEqual(flagsOf(finding("f6", 72, { against: 0.2 })), [], "a light counterclaim isn't");
  assert.deepEqual(flagsOf(finding("f7", 55, { notFound: true })), ["unfound"]);
  assert.deepEqual(flagsOf(finding("f8", 20, { failed: 1, against: 0.4 })), ["weak", "contested", "unfound"]);
});

test("however much research there is, the digest is counts and the findings its steps rest on worth a look, worst first", () => {
  const many = Array.from({ length: 40 }, (_, i) => finding(`f${i + 1}`, 90));
  const weak = finding("f41", 41);
  const worst = finding("f42", 22, { notFound: true });
  const contested = finding("f43", 72, { against: 0.35 });
  const unused = finding("f44", 10);
  const r = research([...many, weak, worst, contested, unused]);
  const items = [step("one", ["f1", "f2", "f41"]), step("two", ["f43", "f3"]), step("three", ["f42", "f41"]), step("four")];
  const d = digest(items, r);
  assert.equal(d.findings, 44);
  assert.deepEqual(
    d.look.map((l) => [l.f.id, l.flags, l.steps]),
    [
      ["f42", ["weak", "unfound"], [3]],
      ["f41", ["weak"], [1, 3]],
      ["f43", ["contested"], [2]],
    ],
    "a weak finding no step rests on isn't listed",
  );
  assert.deepEqual(d.bare, [4], "a step resting on nothing is said");
  assert.equal(d.least, null, "with some to look at, the least sure isn't said");
  assert.equal(d.unused, 44 - 6);
  // Nothing worth a look: the least sure finding its steps rest on is said instead.
  const calm = digest([step("one", ["a", "b"])], research([finding("a", 95), finding("b", 66)]));
  assert.deepEqual([calm.look.length, calm.least?.f.id, calm.least?.steps], [0, "b", [1]]);
  // Every finding on asking: those resting under a step first, worst first, then the rest.
  const all = allLooks(items, r);
  assert.equal(all.used[0]?.f.id, "f42");
  assert.equal(all.unused[0]?.f.id, "f44");
});

test("a step resting on many findings lists those worth a look first, then the rest as named", () => {
  const r = research([finding("f1", 95), finding("f2", 40), finding("f3", 88), finding("f4", 72, { against: 0.3 })]);
  const items = [step("one", ["f1", "f2", "f3", "f4", "f9"])];
  const b = stepBasis(items[0]!, r, restsOn(items));
  assert.deepEqual(
    b.all.map((l) => l.f.id),
    ["f2", "f4", "f1", "f3"],
  );
  assert.deepEqual(
    b.flagged.map((l) => l.f.id),
    ["f2", "f4"],
  );
  assert.deepEqual(b.grades, [
    ["A", 2],
    ["B", 1],
    ["D", 1],
  ]);
  assert.deepEqual(b.missing, ["f9"], "an id its research doesn't hold is said");
});

test("what changed since you looked: steps matched by title, rewritten in place, added, taken out", () => {
  const before = [step("Settle where the diff lives", ["f5"]), step("Changes under the lanes", ["f1"], { description: "draw it" }), step("Comment"), step("Diff button"), step("Checks")];
  const after = [
    step("Mount ReviewFiles under the lanes", ["f1"]),
    step("Changes under the lanes", ["f1", "f5"], { description: "draw it, and move Files" }),
    step("Comment"),
    step("Checks"),
    step("Docs"),
  ];
  const c = changesSince(before, after);
  assert.deepEqual(c.list.map(changeWords), [
    "Step 1 rewritten: Mount ReviewFiles under the lanes (was: Settle where the diff lives)",
    "Step 2 changed: what to do, what it rests on",
    "Step 5 added: Docs",
    "Step 4 taken out: Diff button",
  ]);
  assert.deepEqual(c.same, [3, 4], "a step taken out before it moves none of the rest");
  // Steps that change places among the others say so; the others, renumbered by it, don't.
  const moved = changesSince([step("a"), step("b"), step("c"), step("d")], [step("a"), step("d"), step("b"), step("c")]);
  assert.deepEqual(moved.list.map(changeWords), ["Step 2 changed: its place (was step 4)"]);
  assert.deepEqual(moved.same, [1, 3, 4]);
  const first = changesSince(
    Array.from({ length: 9 }, (_, i) => step(`s${i}`)),
    [step("new"), ...Array.from({ length: 9 }, (_, i) => step(`s${i}`))],
  );
  assert.deepEqual(first.list.map(changeWords), ["Step 1 added: new"], "a step put first moves none of the nine after it");
  assert.equal(sameWords([3, 4, 5]), "Steps 3–5 unchanged");
  assert.equal(sameWords([2]), "Step 2 unchanged");
  assert.equal(sameWords([]), "");
  // A plan of words alone: its steps as titles.
  assert.deepEqual(changesSince(asItems(undefined, ["a", "b"]), asItems(undefined, ["a", "c", "b"])).list.map(changeWords), ["Step 2 added: c"]);
});

test("numbers read as words: runs of three or more, pairs and the rest listed", () => {
  assert.equal(runsOf([3, 4, 5]), "3–5");
  assert.equal(runsOf([3, 5, 6]), "3, 5 and 6");
  assert.equal(runsOf([6, 2, 7, 8, 10]), "2, 6–8 and 10");
  assert.equal(runsOf([1, 2]), "1 and 2");
  assert.equal(runsOf([4]), "4");
  assert.equal(stepsWord([2]), "step 2");
  assert.equal(stepsWord([2, 4]), "steps 2 and 4");
});

test("a plan's words: its title from its first heading, what it's for from the words before its steps", () => {
  const md = "# Fix: a mount dropped a store\n\nThe successor's store starts fresh.\n\n1. Reproduce it\n2. Fix it\n";
  assert.equal(headingOf(md), "Fix: a mount dropped a store");
  assert.equal(introOf(md), "The successor's store starts fresh.");
  assert.equal(headingOf("no heading"), undefined);
  assert.equal(introOf("1. only steps\n2. more"), "");
  assert.equal(introOf("# T\n## Why\nBecause.\n## Steps\n- a"), "Because.", "its headings are left out, and the next one ends it");
});

test("Comment on step n starts an answer with the step named, after what's written already", () => {
  assert.equal(commentLine("", 2, " Changes "), "Step 2 (Changes): ");
  assert.equal(commentLine("Step 1 (A): drop it  ", 3, "C"), "Step 1 (A): drop it\nStep 3 (C): ");
});
