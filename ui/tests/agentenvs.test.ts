// The Engine tab's Environments, decided (views/harness/agentenv-logic.ts, agentenv-scope.ts; docs/agents.md,
// "Environments", "Scopes, stacks, and applied on its own"): scopes matched as the server matches them (the
// shared testdata/scope-cases.json), the filter by chips, the facets and sorts, a stack reordered, a card's
// draft to what agentenvs.create and .update take; and the composer's stack carried into the start (`agentenvs`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { Agentenv, AgentenvChip, AgentenvFacts } from "../src/api/agentenvs.ts";
import { agentenvOf } from "../src/api/agentenvs.ts";
import { startAsk } from "../src/api/harness.ts";
import {
  askOf,
  AUTO_ORDER,
  blankVar,
  changeOf,
  chipsOf,
  draftOf,
  ENV_ORDER,
  envFacets,
  envWords,
  facetsFor,
  filterShows,
  givesOf,
  moved,
  movedTo,
  problemOf,
  shows,
  suggestionsFor,
  varOf,
  withChip,
} from "../src/views/harness/agentenv-logic.ts";
import { factsOf, scopeMatches } from "../src/views/harness/agentenv-scope.ts";
import { specOf } from "../src/views/harness/spawn-setup.ts";

const env = (id: string, over: Partial<Agentenv> = {}): Agentenv => ({
  id,
  owner: "ada",
  name: id,
  base: "user",
  vars: [],
  tags: [],
  scope: [],
  auto: false,
  version: 1,
  created_at: 0,
  updated_at: 0,
  sessions: [],
  ...over,
});

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

const CASES = JSON.parse(readFileSync(new URL("../../crates/kiln/src/agentenvs/testdata/scope-cases.json", import.meta.url), "utf8")) as {
  name: string;
  scope: AgentenvChip[];
  facts: AgentenvFacts;
  matches: boolean;
}[];

test("a scope matches a session as the server's does: each of the shared cases", () => {
  assert.ok(CASES.length > 0);
  for (const c of CASES) assert.equal(scopeMatches(c.scope, c.facts), c.matches, c.name);
});

const mac: AgentenvFacts = { machine: "m-mac", os: "macos", arch: "arm64", harness: "claude", agent: "kiln:reviewer", model: "claude-opus-5-5", permission: "claude:plan", effort: "claude:high" };
const chip = (key: AgentenvChip["key"], value: string): AgentenvChip => ({ key, value });

test("the filter by chips: started from a session's facts it shows what matches it; a key it has no chip of is any; none, every one", () => {
  const own = chipsOf(mac);
  assert.deepEqual(
    own.map((c) => c.key),
    ["machine", "os", "arch", "harness", "agent", "model", "permission", "effort"],
  );
  // From the session's own facts, the filter agrees with the matcher on every shared case of that session.
  for (const c of CASES) assert.equal(filterShows(c.scope, chipsOf(c.facts)), c.matches || [...new Set(c.scope.map((x) => x.key))].some((k) => !c.facts[k]), c.name);
  // A key taken out of the filter: any value of it.
  const noOs = own.filter((c) => c.key !== "os");
  assert.equal(filterShows([chip("os", "linux")], own), false);
  assert.equal(filterShows([chip("os", "linux")], noOs), true);
  // Two of a key: either.
  assert.equal(filterShows([chip("os", "linux")], [chip("os", "macos"), chip("os", "linux")]), true);
  // A permission of a harness the filter's harnesses leave out never shows, whatever its own key says.
  assert.equal(filterShows([chip("permission", "codex:plan")], [chip("harness", "claude")]), false);
  assert.equal(filterShows([chip("permission", "codex:plan")], [chip("harness", "codex")]), true);
  // No chips: every one; everywhere: always.
  assert.equal(filterShows([chip("machine", "m-lin")], []), true);
  assert.equal(filterShows([], own), true);
});

test("a chip added at its key's place, once", () => {
  const chips = [chip("os", "linux"), chip("model", "gpt-6")];
  assert.deepEqual(withChip(chips, chip("harness", "codex")), [chip("os", "linux"), chip("harness", "codex"), chip("model", "gpt-6")]);
  assert.deepEqual(withChip(chips, chip("os", "macos")), [chip("os", "linux"), chip("os", "macos"), chip("model", "gpt-6")]);
  assert.deepEqual(withChip(chips, chip("os", "linux")), chips);
  assert.deepEqual(withChip(chips, chip("effort", "codex:high")).at(-1), chip("effort", "codex:high"));
});

test("a session to be, as scopes see it: its machine with its system and OCI architecture, its permissions and effort named by their harness", () => {
  const machines = [{ name: "box", os: "linux", arch: "x86_64" }];
  assert.deepEqual(factsOf({ target: "agent:box", machines, harness: "codex", model: "gpt-6", mode: "on-request", effort: "high" }), {
    machine: "box",
    os: "linux",
    arch: "amd64",
    harness: "codex",
    model: "gpt-6",
    permission: "codex:on-request",
    effort: "codex:high",
  });
  const local = factsOf({ target: "local", machines, harness: "claude", model: "m", mode: "plan", effort: "" });
  assert.equal(local.machine, "local");
  assert.equal("effort" in local, false);
});

test("a chip's suggestions: machines, harnesses, agents, models; permissions and efforts named by their harness", () => {
  const known = {
    machines: [{ name: "box" }],
    harnesses: [
      { id: "claude", models: [{ id: "opus" }], modes: [{ id: "plan" }], efforts: [{ id: "high" }] },
      { id: "codex", modes: [{ id: "plan" }] },
    ],
    agents: [{ id: "kiln:reviewer" }],
  };
  assert.deepEqual(suggestionsFor("machine", known), ["box", "local"]);
  assert.deepEqual(suggestionsFor("harness", known), ["claude", "codex"]);
  assert.deepEqual(suggestionsFor("permission", known), ["claude:plan", "codex:plan"]);
  assert.deepEqual(suggestionsFor("effort", known), ["claude:high"]);
  assert.deepEqual(suggestionsFor("model", known), ["opus"]);
  assert.deepEqual(suggestionsFor("agent", known), ["kiln:reviewer"]);
  assert.deepEqual(suggestionsFor("arch", known), ["amd64", "arm64"]);
});

test("a stack: a row dragged to its place, the others in order; those applied on their own join more specific first, then by name", () => {
  assert.deepEqual(movedTo(["a", "b", "c"], 0, 2), ["b", "c", "a"]);
  assert.deepEqual(movedTo(["a", "b", "c"], 2, 0), ["c", "a", "b"]);
  assert.deepEqual(movedTo(["a", "b", "c"], 1, 1), ["a", "b", "c"]);
  assert.deepEqual(movedTo(["a", "b", "c"], 5, 0), ["a", "b", "c"]);
  const autos = [env("b", { scope: [chip("os", "linux")] }), env("a", { scope: [chip("os", "linux")] }), env("z", { scope: [chip("os", "linux"), chip("harness", "claude")] })];
  assert.deepEqual(
    autos.sort(AUTO_ORDER).map((e) => e.id),
    ["z", "a", "b"],
  );
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

test("the composer's stack goes into the start as `agentenvs`, top first; none, nothing", () => {
  const base = { kind: "claude", cwd: "~/src", shell: "", mode: "", model: "", effort: "", speed: "", name: "", prompt: "hi", reach: "", volume: false };
  const spec = specOf({ ...base, agentenvs: ["env-release", "env-aws"] });
  assert.deepEqual(spec.agentenvs, ["env-release", "env-aws"]);
  assert.deepEqual(startAsk("agent:m", spec, 1).agentenvs, ["env-release", "env-aws"]);
  assert.equal("agentenvs" in startAsk("agent:m", specOf(base), 1), false);
  assert.equal("agentenvs" in startAsk("agent:m", specOf({ ...base, agentenvs: [] }), 1), false);
});
