/**
 * What kiln's server always sends of a session's parts, as Rust has them when there's nothing to
 * say (crates/kiln/src/harness): spread under what a mock sets, like Rust's `..Default::default()`.
 * The types are the server's own (ui/src/api/wire, made from the Rust), so a field Rust adds is one
 * TypeScript asks the mocks for. Inventories are reported (`ready`): a mock's lists are what it has.
 * Each call makes them anew: a mock changes its lists in place.
 */
import type { AgentInfo, Meta, State, Task } from "../src/api/harness.ts";
import type { Agent } from "../src/api/types.ts";

export const defaultInfo = (): Pick<AgentInfo, "goal" | "summary" | "todos_done" | "todos_total" | "files_changed" | "lines_added" | "lines_removed" | "starts"> => ({
  goal: "",
  summary: "",
  todos_done: 0,
  todos_total: 0,
  files_changed: 0,
  lines_added: 0,
  lines_removed: 0,
  // kiln's record of what it started for the session (server-sent, always): none in these mocks.
  starts: [],
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

/**
 * A Claude Code agent's permission modes, models and efforts, as its harness lists them (every mode, its
 * bypass, "Don't ask", among them): the mock's Claude Code sessions say these, and set each (set_mode,
 * set_model, set_effort).
 */
export const CLAUDE_CODE: Pick<Meta, "modes" | "models" | "efforts"> = {
  modes: [
    { id: "plan", label: "Plan", description: "Reads and plans; changes nothing" },
    { id: "default", label: "Ask", description: "Asks before edits and commands" },
    { id: "acceptEdits", label: "Accept edits", description: "Edits files without asking; asks before commands" },
    { id: "auto", label: "Auto", description: "Decides for itself what needs asking" },
    { id: "bypassPermissions", label: "Don't ask", description: "Runs every command and edit without asking", dangerous: true },
  ],
  models: [
    { id: "default", label: "Default (recommended)", description: "Opus 5.5" },
    { id: "claude-opus-5-5", label: "claude-opus-5-5", description: "" },
    { id: "claude-fable-5-2", label: "claude-fable-5-2", description: "" },
    { id: "claude-sonnet-5-5", label: "claude-sonnet-5-5", description: "" },
    { id: "claude-haiku-5-5", label: "claude-haiku-5-5", description: "" },
  ],
  efforts: ["low", "medium", "high", "xhigh", "max"].map((id) => ({ id, label: id === "xhigh" ? "XHigh" : id.charAt(0).toUpperCase() + id.slice(1) })),
};
