/**
 * What kiln's server always sends of a session's parts, as Rust has them when there's nothing to
 * say (crates/kiln/src/harness): spread under what a mock sets, like Rust's `..Default::default()`.
 * The types are the server's own (ui/src/api/wire, made from the Rust), so a field Rust adds is one
 * TypeScript asks the mocks for. Inventories are reported (`ready`): a mock's lists are what it has.
 * Each call makes them anew: a mock changes its lists in place.
 */
import type { AgentInfo, Meta, State, Task } from "../src/api/harness.ts";
import type { Agent } from "../src/api/types.ts";

export const defaultInfo = (): Pick<AgentInfo, "goal" | "summary" | "todos_done" | "todos_total" | "files_changed" | "lines_added" | "lines_removed"> => ({
  goal: "",
  summary: "",
  todos_done: 0,
  todos_total: 0,
  files_changed: 0,
  lines_added: 0,
  lines_removed: 0,
});

export const defaultState = (): Pick<
  State,
  | "goal"
  | "summary"
  | "files_changed"
  | "lines_added"
  | "lines_removed"
  | "approvals"
  | "queue"
  | "delivery"
  | "tasks"
  | "todos"
  | "usage"
  | "dropped"
  | "control_values"
  | "limits"
  | "harness_status"
  | "mcp"
  | "skills"
  | "plugins"
  | "inventory"
  | "left_behind"
  | "move_plan"
> => ({
  goal: "",
  summary: "",
  files_changed: 0,
  lines_added: 0,
  lines_removed: 0,
  approvals: [],
  queue: [],
  delivery: {},
  tasks: [],
  todos: [],
  usage: { cost_usd: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, turns: 0 },
  dropped: 0,
  control_values: {},
  limits: [],
  harness_status: [],
  mcp: [],
  skills: [],
  plugins: [],
  inventory: { mcp: "ready", skills: "ready", plugins: "ready", limits: "ready" },
  left_behind: 0,
  move_plan: {},
});

export const defaultTask = (): Pick<Task, "kind" | "background" | "depth" | "tokens" | "tool_uses" | "duration_ms" | "stoppable"> => ({
  kind: "",
  background: false,
  depth: 0,
  tokens: 0,
  tool_uses: 0,
  duration_ms: 0,
  stoppable: false,
});

export const defaultMeta = (): Pick<Meta, "efforts" | "controls" | "hooks"> => ({
  efforts: [],
  controls: [],
  hooks: [],
});

/** A runner as `GET /api/agents` lists it, its counts and flags at their start, its kiln the
 *  server's (`full`, none behind). */
export const defaultRunner = (): Pick<Agent, "running" | "busy" | "draining" | "sample" | "terminal" | "outpost" | "skew"> => ({
  running: [],
  busy: 0,
  draining: false,
  sample: null,
  terminal: true,
  outpost: false,
  skew: { standing: "full", behind: 0 },
});
