import assert from "node:assert/strict";
import { test } from "node:test";
import { HighlightClient, highlightResource, type SyntaxWorker } from "../src/lib/syntax/async-client.ts";
import { Highlighter, type Highlight, type Tok } from "../src/lib/syntax/engine.ts";
import { HighlightQueue, type WorkerHighlighter } from "../src/lib/syntax/worker-queue.ts";
import { MAX_SYNTAX_JOBS, MAX_SYNTAX_JOB_CHARS, MAX_SYNTAX_RESULT_TOKENS, MAX_SYNTAX_TEXTS, textSize, type HighlightRequest, type HighlightResponse } from "../src/lib/syntax/worker-protocol.ts";
import { highlighted, nodeAssets } from "./syntax-node.ts";

const deferred = <T>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => (resolve = done));
  return { promise, resolve };
};
const linesOf = (text: string): Tok[][] => text.split("\n").map((line) => (line ? [[0, line.length, "hl-kw"]] : []));
const valueOf = (text: string): Highlight => ({ lines: linesOf(text), wants: [] });

class FakeWorker extends EventTarget {
  sent: HighlightRequest[] = [];
  stopped = false;
  postMessage(request: HighlightRequest) {
    this.sent.push(structuredClone(request));
  }
  terminate() {
    this.stopped = true;
  }
  answer(response: HighlightResponse) {
    this.dispatchEvent(new MessageEvent("message", { data: response }));
  }
  fail() {
    this.dispatchEvent(new Event("error", { cancelable: true }));
  }
  port() {
    return this as unknown as SyntaxWorker;
  }
}

test("w26_r15_syntax_worker_associates_results_with_the_live_source_and_retains_completed_disabled_colors", () => {
  const worker = new FakeWorker();
  const client = new HighlightClient(() => worker.port());
  const old = highlightResource(client, "rust", ["old source"]);
  const oldOff = old.subscribe(() => {});
  const oldId = worker.sent[0]!.id;
  oldOff();
  assert.deepEqual(worker.sent[1], { type: "cancel", id: oldId });
  const now = highlightResource(client, "rust", ["new source", "second piece"]);
  let notices = 0;
  const off = now.subscribe(() => notices++);
  const id = worker.sent[2]!.id;
  assert.notEqual(id, oldId);
  worker.answer({ type: "highlight", id: oldId, lines: [linesOf("old source")] });
  assert.equal(old.read(), null);
  assert.equal(now.read(), null);
  const tokens = [linesOf("new source"), linesOf("second piece")];
  worker.answer({ type: "highlight", id, lines: tokens });
  assert.deepEqual(now.read(), tokens);
  assert.equal(notices, 1);
  const snapshot = now.read();
  off(); // Disabled/offscreen: the finished derived output is still exactly the same object.
  assert.equal(now.read(), snapshot);
  const sent = worker.sent.length;
  now.subscribe(() => {})();
  assert.equal(worker.sent.length, sent, "re-entering a completed source does not parse again");
});

test("w26_r15_syntax_worker_cancelled_lease_can_restart_but_wrong_piece_count_never_publishes", () => {
  const worker = new FakeWorker();
  const client = new HighlightClient(() => worker.port());
  const source = highlightResource(client, "typescript", ["one", "two"]);
  source.subscribe(() => {})();
  const old = worker.sent[0]!.id;
  const off = source.subscribe(() => {});
  const id = worker.sent[2]!.id;
  assert.notEqual(id, old);
  worker.answer({ type: "highlight", id: old, lines: [linesOf("one"), linesOf("two")] });
  assert.equal(source.read(), null);
  worker.answer({ type: "highlight", id, lines: [linesOf("one")] });
  assert.equal(source.read(), null);
  off();
});

test("w26_r15_syntax_worker_failure_finishes_pending_readers_plain_and_only_new_requests_retry", () => {
  const workers: FakeWorker[] = [];
  const client = new HighlightClient(() => {
    const worker = new FakeWorker();
    workers.push(worker);
    return worker.port();
  });
  const a = highlightResource(client, "rust", ["a"]);
  const b = highlightResource(client, "tsx", ["b"]);
  let notices = 0;
  const offA = a.subscribe(() => notices++);
  const offB = b.subscribe(() => notices++);
  workers[0]!.fail();
  assert.equal(workers[0]!.stopped, true);
  assert.equal(notices, 2);
  assert.equal(a.read(), null);
  assert.equal(b.read(), null);
  offA();
  offB();
  a.subscribe(() => {})();
  assert.equal(workers.length, 1, "an unavailable source does not create a render/retry loop");
  const c = highlightResource(client, "rust", ["c"]);
  const offC = c.subscribe(() => {});
  assert.equal(workers.length, 2);
  const id = workers[1]!.sent[0]!.id;
  workers[0]!.answer({ type: "highlight", id, lines: [linesOf("wrong worker")] });
  assert.equal(c.read(), null);
  workers[1]!.answer({ type: "highlight", id, lines: [linesOf("c")] });
  assert.deepEqual(c.read(), [linesOf("c")]);
  offC();
});

test("w26_r15_syntax_worker_unsupported_or_blocked_workers_leave_plain_text_without_retrying_a_lease", () => {
  for (const create of [
    () => null,
    () => {
      throw new Error("CSP blocked");
    },
  ]) {
    let attempts = 0;
    const client = new HighlightClient(() => {
      attempts++;
      return create();
    });
    const resource = highlightResource(client, "rust", ["text"]);
    resource.subscribe(() => {})();
    resource.subscribe(() => {})();
    assert.equal(resource.read(), null);
    assert.equal(attempts, 1);
  }
});

test("w26_r15_syntax_worker_bounds_pending_jobs_and_source_size_before_cloning", () => {
  assert.equal(textSize(["x".repeat(MAX_SYNTAX_JOB_CHARS)]), MAX_SYNTAX_JOB_CHARS);
  assert.equal(textSize(["x".repeat(MAX_SYNTAX_JOB_CHARS + 1)]), null);
  assert.equal(textSize(Array(MAX_SYNTAX_TEXTS + 1).fill("")), null);
  const worker = new FakeWorker();
  const client = new HighlightClient(() => worker.port());
  const cancel: (() => void)[] = [];
  for (let i = 0; i < MAX_SYNTAX_JOBS; i++) cancel.push(client.request("rust", [String(i)], () => {}));
  let refused: unknown = "not answered";
  client.request("rust", ["overflow"], (value) => (refused = value));
  assert.equal(refused, null);
  assert.equal(worker.sent.length, MAX_SYNTAX_JOBS);
  cancel[0]!();
  client.request("rust", ["replacement"], () => {});
  assert.equal(worker.sent.length, MAX_SYNTAX_JOBS + 2, "cancel frees one bounded slot");
  cancel.forEach((stop) => stop());
});

test("w26_r15_syntax_worker_cancels_during_grammar_load_and_never_runs_a_cancelled_queued_piece", { timeout: 5000 }, async () => {
  const loading = deferred<boolean>();
  const finished = deferred<void>();
  const parsed: string[] = [];
  const responses: HighlightResponse[] = [];
  const highlighter: WorkerHighlighter = {
    load: () => loading.promise,
    highlight: (_language, text) => {
      parsed.push(text);
      return valueOf(text);
    },
  };
  const queue = new HighlightQueue(
    highlighter,
    (response) => {
      responses.push(response);
      finished.resolve();
    },
    async () => {},
  );
  queue.receive({ type: "highlight", id: 1, language: "rust", texts: ["cancelled during load"] });
  queue.receive({ type: "highlight", id: 2, language: "rust", texts: ["cancelled in queue"] });
  queue.receive({ type: "highlight", id: 3, language: "rust", texts: ["live"] });
  queue.receive({ type: "cancel", id: 1 });
  queue.receive({ type: "cancel", id: 2 });
  loading.resolve(true);
  await finished.promise;
  assert.deepEqual(parsed, ["live"]);
  assert.deepEqual(responses, [{ type: "highlight", id: 3, lines: [linesOf("live")] }]);
});

test("w26_r15_syntax_worker_yields_between_pieces_so_inflight_cancellation_stops_the_rest", { timeout: 5000 }, async () => {
  const stopped = deferred<void>();
  const parsed: string[] = [];
  const responses: HighlightResponse[] = [];
  const queue = new HighlightQueue(
    {
      load: async () => true,
      highlight: (_language, text) => {
        parsed.push(text);
        return valueOf(text);
      },
    },
    (response) => responses.push(response),
    async () => {
      if (parsed.length) {
        queue.receive({ type: "cancel", id: 1 });
        stopped.resolve();
      }
    },
  );
  queue.receive({ type: "highlight", id: 1, language: "rust", texts: ["first", "must not run"] });
  await stopped.promise;
  assert.deepEqual(parsed, ["first"]);
  assert.deepEqual(responses, []);
});

test("w26_r15_syntax_worker_serializes_mutable_parsers_and_converges_nested_injections", { timeout: 5000 }, async () => {
  const gate = deferred<boolean>();
  const done = deferred<void>();
  const loaded = new Set<string>();
  const parsed: string[] = [];
  const output: HighlightResponse[] = [];
  const queue = new HighlightQueue(
    {
      load: async (language) => {
        if (language === "jsdoc") await gate.promise;
        loaded.add(language);
        return true;
      },
      highlight: (language, text) => {
        parsed.push(`${language}:${text}`);
        return { lines: linesOf(text), wants: language === "typescript" && !loaded.has("jsdoc") ? ["jsdoc"] : [] };
      },
    },
    (response) => {
      output.push(response);
      if (output.length === 2) done.resolve();
    },
    async () => {},
  );
  queue.receive({ type: "highlight", id: 1, language: "typescript", texts: ["/** documentation */"] });
  queue.receive({ type: "highlight", id: 2, language: "rust", texts: ["fn main() {}"] });
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(
    parsed.some((text) => text.startsWith("rust:")),
    false,
    "another parser does not run during dependency convergence",
  );
  gate.resolve(true);
  await done.promise;
  assert.deepEqual(parsed, ["typescript:/** documentation */", "typescript:/** documentation */", "rust:fn main() {}"]);
  assert.deepEqual(
    output.map((response) => response.id),
    [1, 2],
  );
});

test("w26_r15_syntax_worker_refuses_an_unbounded_token_response_before_cloning_it_to_the_page", { timeout: 5000 }, async () => {
  const complete = deferred<HighlightResponse>();
  const tokens: Tok[] = Array.from({ length: MAX_SYNTAX_RESULT_TOKENS + 1 }, (_, i) => [i, i + 1, "hl-kw"]);
  const queue = new HighlightQueue({ load: async () => true, highlight: () => ({ lines: [tokens], wants: [] }) }, complete.resolve, async () => {});
  queue.receive({ type: "highlight", id: 1, language: "rust", texts: ["source"] });
  assert.deepEqual(await complete.promise, { type: "error", id: 1 });
});

test("w26_r15_syntax_worker_actual_rust_tsx_and_markdown_tokens_match_the_existing_engine", { timeout: 30000 }, async () => {
  const highlighter = new Highlighter(nodeAssets, { budgetMs: 30_000 });
  const samples = [
    { language: "rust", texts: ['fn main() { println!("hello"); }', "let x: u32 = 7;"] },
    { language: "tsx", texts: ["/** @param {string} name */\nconst View = ({name}: Props) => <h1>{name}</h1>;"] },
    { language: "markdown", texts: ["# Heading\n\n```python\nimport os\n```\n\nSome **bold** words."] },
  ];
  for (const sample of samples) {
    const complete = deferred<HighlightResponse>();
    const queue = new HighlightQueue(highlighter, complete.resolve, async () => {});
    queue.receive({ type: "highlight", id: 1, ...sample });
    const response = await complete.promise;
    assert.equal(response.type, "highlight", sample.language);
    if (response.type !== "highlight") continue;
    const expected = await Promise.all(sample.texts.map(async (text) => (await highlighted(sample.language, text)).lines));
    assert.deepEqual(response.lines, expected, `${sample.language}: exact original text/piece order and resolved injected colors`);
  }
});
