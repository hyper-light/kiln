// The Engine tab's Environments, decided (views/harness/agentenv-logic.ts; docs/agents.md, "Environments"):
// this session's, the facets and sorts, a card's draft to what agentenvs.create and .update take; and the
// composer's choice carried into the start (`agentenv`).
import { test } from "node:test";
import assert from "node:assert/strict";
import type { Agentenv } from "../src/api/agentenvs.ts";
import { agentenvOf } from "../src/api/agentenvs.ts";
import { startAsk } from "../src/api/harness.ts";
import { askOf, changeOf, draftOf, ENV_ORDER, envFacets, envWords, facetsFor, givesOf, mineOf, moved, problemOf, shows, varOf, blankVar } from "../src/views/harness/agentenv-logic.ts";
import { specOf } from "../src/views/harness/spawn-setup.ts";

const env = (id: string, over: Partial<Agentenv> = {}): Agentenv => ({ id, owner: "ada", name: id, base: "user", vars: [], tags: [], version: 1, created_at: 0, updated_at: 0, sessions: [], ...over });

const release = env("release", {
  created_at: 30,
  version: 3,
  vars: [
    { name: "GH_TOKEN", from: "secret", secret: "GH_TOKEN" },
    { name: "RUST_LOG", from: "value", value: "info" },
    { name: "CARGO_HOME", from: "machine" },
  ],
  sessions: ["s1"],
});
const aws = env("aws", { created_at: 10, vars: [{ name: "AWS_PROFILE", from: "machine", of: "AWS_DEFAULT_PROFILE" }], sessions: ["s2", "s3"] });
const clean = env("clean", { created_at: 20, base: "clean", vars: [{ name: "NODE_OPTIONS", from: "unset" }] });
const all = [release, aws, clean];

test("this session's: the one listing it, else the one its entry names", () => {
  assert.equal(mineOf(all, "s1", null)?.id, "release");
  assert.equal(mineOf(all, "s9", { id: "clean", name: "clean", version: 1 })?.id, "clean");
  assert.equal(mineOf(all, "s9", null), undefined);
  // The list wins: it's what an apply changes first.
  assert.equal(mineOf(all, "s1", { id: "clean", name: "clean", version: 1 })?.id, "release");
});

test("a session's entry read defensively: none from a server before environments", () => {
  assert.equal(agentenvOf({}), null);
  assert.equal(agentenvOf({ agentenv: "x" }), null);
  assert.deepEqual(agentenvOf({ agentenv: { id: "e", name: "n", version: 2, more: 1 } }), { id: "e", name: "n", version: 2 });
});

test("facets: base, what it uses (unset isn't a use), and where it runs from this session's view", () => {
  const f = envFacets("s1");
  const [base, uses, where] = f;
  assert.deepEqual(base!.of(clean), ["clean"]);
  assert.deepEqual(uses!.of(release).sort(), ["machine", "secret", "value"]);
  assert.deepEqual(uses!.of(clean), []);
  assert.deepEqual(where!.of(release), ["this"]);
  assert.deepEqual(where!.of(aws), ["others"]);
  assert.deepEqual(where!.of(clean), ["none"]);
  // Secrets chosen: only those that give one.
  assert.deepEqual(
    all.filter((e) => shows(e, envWords(e), "", { uses: ["secret"] }, f)).map((e) => e.id),
    ["release"],
  );
  // Each option counts what it would show, the other facets as they are, in the facet's own order.
  const facets = facetsFor(all, envWords, "", { base: ["user"] }, f);
  assert.deepEqual(
    facets[0]!.options.map((o) => [o.id, o.count]),
    [
      ["user", 2],
      ["clean", 1],
    ],
  );
  assert.deepEqual(
    facets[2]!.options.map((o) => [o.label, o.count]),
    [
      ["This session", 1],
      ["Other sessions", 1],
      ["Unused", 0],
    ],
  );
  // What's typed finds a variable's name and the machine's variable it takes.
  assert.ok(shows(aws, envWords(aws), "aws_default", {}, f));
});

test("sorts: newest, oldest, by name both ways, most variables, most sessions", () => {
  const ids = (k: keyof typeof ENV_ORDER) => [...all].sort(ENV_ORDER[k]).map((e) => e.id);
  assert.deepEqual(ids("newest"), ["release", "clean", "aws"]);
  assert.deepEqual(ids("oldest"), ["aws", "clean", "release"]);
  assert.deepEqual(ids("name"), ["aws", "clean", "release"]);
  assert.deepEqual(ids("name-desc"), ["release", "clean", "aws"]);
  assert.deepEqual(ids("vars"), ["release", "aws", "clean"]);
  assert.deepEqual(ids("sessions"), ["aws", "release", "clean"]);
});

test("a secret by its name, never a value; a machine variable by what it takes", () => {
  assert.equal(givesOf({ name: "GH_TOKEN", from: "secret", secret: "GH_TOKEN" }), "GH_TOKEN");
  assert.equal(givesOf({ name: "AWS_PROFILE", from: "machine", of: "AWS_DEFAULT_PROFILE" }), "$AWS_DEFAULT_PROFILE");
  assert.equal(givesOf({ name: "CARGO_HOME", from: "machine" }), "$CARGO_HOME");
  assert.equal(givesOf({ name: "X", from: "unset" }), "taken away");
});

test("a draft round-trips: unchanged, it changes nothing but says its version", () => {
  const d = draftOf(aws);
  assert.deepEqual(askOf(d).vars, aws.vars);
  assert.deepEqual(changeOf(aws, d), { version: 1 });
});

test("a draft's change: only what changed, on the version it was opened at", () => {
  const d = draftOf(release);
  d.name = "release-2";
  d.vars = moved(d.vars, 2, -1);
  assert.deepEqual(changeOf({ ...release, version: 4 }, d), {
    version: 3,
    name: "release-2",
    vars: [
      { name: "GH_TOKEN", from: "secret", secret: "GH_TOKEN" },
      { name: "CARGO_HOME", from: "machine" },
      { name: "RUST_LOG", from: "value", value: "info" },
    ],
  });
});

test("a machine variable names what it takes only when that isn't its own name; each kind keeps only its own field", () => {
  const v = { ...blankVar(), name: "AWS_PROFILE", from: "machine" as const, pick: "AWS_DEFAULT_PROFILE", value: "left over", secret: "GH_TOKEN" };
  assert.deepEqual(varOf(v), { name: "AWS_PROFILE", from: "machine", of: "AWS_DEFAULT_PROFILE" });
  assert.deepEqual(varOf({ ...v, pick: "AWS_PROFILE" }), { name: "AWS_PROFILE", from: "machine" });
  assert.deepEqual(varOf({ ...v, from: "unset" }), { name: "AWS_PROFILE", from: "unset" });
});

test("what's in the way, in words: a name, valid variables, a secret picked, a machine variable picked", () => {
  const d = draftOf();
  assert.equal(problemOf(d), "Name it");
  d.name = "x";
  assert.equal(problemOf(d), null);
  d.vars = [{ ...blankVar(), name: "1A" }];
  assert.match(problemOf(d) ?? "", /isn't a variable's name/);
  d.vars = [{ ...blankVar(), name: "A", from: "secret" }];
  assert.match(problemOf(d) ?? "", /Pick the secret/);
  d.vars = [{ ...blankVar(), name: "A", from: "machine" }];
  assert.match(problemOf(d) ?? "", /Pick the machine's variable/);
  assert.deepEqual(askOf({ ...d, description: "  " }), { name: "x", base: "user", vars: [{ name: "A", from: "machine" }] });
});

test("moving a variable past either end changes nothing", () => {
  assert.deepEqual(moved([1, 2, 3], 0, -1), [1, 2, 3]);
  assert.deepEqual(moved([1, 2, 3], 2, 1), [1, 2, 3]);
  assert.deepEqual(moved([1, 2, 3], 1, 1), [1, 3, 2]);
});

test("the composer's environment goes into the start as `agentenv`; none, nothing", () => {
  const base = { kind: "claude", cwd: "~/src", shell: "", mode: "", model: "", effort: "", speed: "", name: "", prompt: "hi", reach: "", volume: false };
  const spec = specOf({ ...base, agentenv: "env-release" });
  assert.equal(spec.agentenv, "env-release");
  assert.equal(startAsk("agent:m", spec, 1).agentenv, "env-release");
  assert.equal("agentenv" in startAsk("agent:m", specOf(base), 1), false);
});
