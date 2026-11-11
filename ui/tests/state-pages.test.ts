import test from "node:test";
import assert from "node:assert/strict";
import {
  approvalItem,
  deferredState,
  pageHasItem,
  queuePageRows,
  stateCount,
  stateFieldRevision,
  stateMessageIds,
  stateMetadata,
  stateMutationError,
  stateReply,
  taskItem,
  type StatePage,
} from "../src/api/state-pages";

const metadata = {
  revision: "source-1",
  revisions: { queue: "queue-1" },
  deferred: { queue: { kind: "array", len: 564 } },
  counts: { queue: 564, approvals: 2, tasks: 3, tasks_running: 1, todos: 5, todos_done: 2 },
};

test("W20: projected empty arrays remain deferred and counts come from the owner", () => {
  const state = { status: "idle", queue: [], approvals: [], view: metadata };
  assert.equal(deferredState(state, "queue")?.len, 564);
  assert.equal(stateCount(state, "queue", state.queue.length), 564);
  assert.equal(stateCount(state, "approvals", state.approvals.length), 2);
  assert.equal(stateMetadata({ view: { ...metadata, counts: { ...metadata.counts, queue: -1 } } }), undefined);
  assert.equal(stateMetadata({ view: { ...metadata, revision: undefined } }), undefined, "owner metadata always requires its revision");
  assert.equal(stateMetadata({ status: "running" }), undefined, "legacy state remains complete");
  assert.equal(stateFieldRevision(state, "$queue_index"), "queue-1");
});

test("W20: state pages reject a stale response and preserve bounded node references", () => {
  assert.match(stateReply({ result: { type: "page", revision: "old", entries: [] } }, "source-1").error!, /older/);
  assert.match(stateReply({ error: { code: "stale" } }, "source-1").error!, /changed/);
  const result = { type: "page", revision: "source-1", entries: [{ key: { type: "index", index: 40 }, node: { kind: "object", len: 8 } }], next: 41 };
  assert.deepEqual(stateReply({ result }, "source-1").reply, result);
  assert.match(stateReply({ result: { ...result, next: -1 } }, "source-1").error!, /cursor/);
  assert.match(stateReply({ result: { ...result, entries: [{ key: { type: "index", index: 1 } }] } }, "source-1").error!, /unreadable/);
});

test("W20: queue pages use absolute indexes and partial rows never become editable text", () => {
  const page: StatePage = {
    phase: "ready",
    read: { revision: "source-1", field: "queue", path: [], offset: 40 },
    reply: {
      type: "page",
      revision: "source-1",
      next: 43,
      entries: [
        { key: { type: "index", index: 40 }, value: { id: "q40", text: "the complete source text", at: 1, attachments: [{ name: "input.txt", mime: "text/plain", file_id: "file-1" }] } },
        { key: { type: "index", index: 41 }, node: { kind: "object", len: 8 } },
        { key: { type: "index", index: 42 }, value: { id: "q42", text: { node: "partial" }, at: 1 } },
      ],
    },
  };
  const rows = queuePageRows(page);
  assert.deepEqual(
    rows.map((r) => r.index),
    [40, 41, 42],
  );
  assert.equal(rows[0]?.item?.text, "the complete source text");
  assert.equal(rows[0]?.item?.attachments?.[0]?.file_id, "file-1");
  assert.equal(rows[1]?.item, undefined);
  assert.equal(rows[2]?.item, undefined);
  assert.deepEqual(queuePageRows({ ...page, read: { ...page.read, path: [{ type: "index", index: 40 }] } }), [], "drilling into a row does not turn its fields into queue messages");
});

test("W20: editing a paged message requires the owner's revision fence", () => {
  assert.equal(stateMutationError({ result: { type: "applied" } }), undefined);
  assert.match(stateMutationError({ error: { code: "stale" } })!, /changed/);
  assert.match(stateMutationError(null)!, /did not apply/);
});

test("W20: a partial queue index proves only the entries actually supplied", () => {
  const view = { ...metadata, queue_index: { entries: [{ id: "known", digest: "0123456789abcdef", len: 12 }], complete: false, next: 1 } };
  const state = { queue: [], tasks: [], delivery: {}, view };
  assert.deepEqual([...stateMessageIds(state)], ["known"]);
  assert.equal(stateMessageIds(state).has("unseen"), false, "absence is not a delivery acknowledgement");
  assert.deepEqual(
    [...stateMessageIds({ queue: [{ id: "ordinary" }], given: [{ id: "handed", digest: "abc" }], delivery: { sent: "sent", cancelled: "cancelled" } })],
    ["ordinary", "handed", "sent", "cancelled"],
  );
  const idRead = { result: { type: "value", revision: "queue-1", value: "contents" } };
  assert.equal(stateReply(idRead, "queue-1").error, undefined);
});

test("W20: complete deferred approval items keep exact request data while previews remain inert", () => {
  const approval = {
    id: "approval-1",
    tool_id: null,
    tool_name: "Bash",
    tool: "shell",
    title: "Run the exact command",
    input: { command: "printf 'keep these words'" },
    command: "printf 'keep these words'",
    cwd: "/work",
    at: 1,
    always: [{ label: "This session", scope: "session", value: { scope: "session" } }],
  };
  assert.equal(approvalItem(approval, [{ type: "index", index: 50 }]), approval);
  assert.equal(approvalItem(approval, [{ type: "id", id: approval.id }]), approval);
  assert.equal(approvalItem(approval, [{ type: "id", id: "different" }]), undefined);
  assert.equal(
    approvalItem(approval, [
      { type: "index", index: 50 },
      { type: "key", key: "input" },
    ]),
    undefined,
  );
  assert.equal(approvalItem({ id: approval.id, title: "partial preview" }, [{ type: "index", index: 50 }]), undefined);
  assert.equal(approvalItem({ ...approval, command: { kind: "text", len: 900000 } }, [{ type: "index", index: 50 }]), undefined);
  assert.equal(approvalItem({ ...approval, questions: [{ question: "Question preview", options: { kind: "array", len: 3 } }] }, [{ type: "index", index: 50 }]), undefined);
});

test("W20: deferred task items retain typed stop identity only while their source page is current", () => {
  const task = { id: "task-1", description: "A real task", kind: "shell", status: "running", background: true, depth: 0, tokens: 0, tool_uses: 1, duration_ms: 10, started: 1, stoppable: true };
  const page: StatePage = {
    phase: "ready",
    read: { revision: "tasks-1", field: "tasks", path: [], offset: 50 },
    reply: { type: "page", revision: "tasks-1", entries: [{ key: { type: "index", index: 50 }, value: task }], next: 51 },
  };
  const state = { view: { ...metadata, revisions: { tasks: "tasks-1" }, deferred: { tasks: { kind: "array", len: 51 } } } };
  assert.equal(taskItem(task, [{ type: "index", index: 50 }]), task);
  assert.equal(pageHasItem(state, page, "tasks", task.id), true);
  assert.equal(pageHasItem({ view: { ...state.view, revision: "unrelated-status" } }, page, "tasks", task.id), true);
  assert.equal(pageHasItem({ view: { ...state.view, revisions: { tasks: "tasks-2" } } }, page, "tasks", task.id), false);
  assert.equal(pageHasItem(state, { ...page, phase: "loading" }, "tasks", task.id), false);
  assert.equal(
    pageHasItem(state, { ...page, reply: { type: "page", revision: "tasks-1", entries: [{ key: { type: "index", index: 50 }, value: task, node: { kind: "object", len: 20 } }] } }, "tasks", task.id),
    false,
    "a node reference never acquires actions even if a value accompanies it",
  );
  assert.equal(taskItem({ ...task, description: { kind: "text", len: 900000 } }, [{ type: "index", index: 50 }]), undefined);
});

test("W20: approval actions cannot survive an approval-field revision or path change", () => {
  const approval = { id: "approval-2", tool_id: "tool-2", tool_name: "Bash", tool: "shell", title: "Approve", input: null, cwd: "/work", at: 1 };
  const state = { view: { ...metadata, revisions: { approvals: "approvals-1" } } };
  const page: StatePage = {
    phase: "ready",
    read: { revision: "approvals-1", field: "approvals", path: [{ type: "id", id: approval.id }], offset: 0 },
    reply: { type: "value", revision: "approvals-1", value: approval },
  };
  assert.equal(pageHasItem(state, page, "approvals", approval.id), true);
  assert.equal(
    pageHasItem(
      state,
      {
        ...page,
        read: {
          ...page.read,
          path: [
            { type: "id", id: approval.id },
            { type: "key", key: "input" },
          ],
        },
      },
      "approvals",
      approval.id,
    ),
    false,
  );
  assert.equal(pageHasItem({ view: { ...state.view, revisions: { approvals: "approvals-2" } } }, page, "approvals", approval.id), false);
  assert.equal(pageHasItem(state, page, "tasks", approval.id), false);
});
