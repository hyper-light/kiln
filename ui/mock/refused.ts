/**
 * Messages kiln couldn't read (docs/wire.md, "What can't be read"), in the mock: what the server
 * keeps of them for a runner's and an agent's pages (`refused`: how many by wire and code, and the
 * newest 16), planted on one of each, and noted as `Hub::note_unread` notes them, for the browser
 * checks (`POST /api/mock/refuse`, ci/ui/refused_check.mjs).
 *
 *   /runners?agent=metal-ryzen-01   what the server couldn't read of it, of the server's it couldn't, and reach's
 *   /agents/fam-tests               its runner's updates of it, and one of its MCP calls
 */
import type { RefusedMessages, Unread } from "../src/api/wire/index.ts";

/** How many of a page's newest are kept (`crate::wire::unread::NEWEST`). */
const NEWEST = 16;
const MIN = 60_000;

/** What's kept with `u` noted at `at` (ms): counted by its wire and code, and the newest of it. */
export function note(kept: RefusedMessages | undefined, u: Unread, at: number): RefusedMessages {
  const counts = [...(kept?.counts ?? [])];
  const i = counts.findIndex((c) => c.wire === u.wire && c.code === u.code);
  const was = counts[i];
  if (was) counts[i] = { ...was, n: was.n + 1 };
  else counts.push({ wire: u.wire, code: u.code, n: 1 });
  return { counts, newest: [...(kept?.newest ?? []), { at, unread: u }].slice(-NEWEST) };
}

const runner = (name: string) => ({ runner: { name, instance: "main" } });

/** A frame of a runner a release ahead, as the server reads it: a kind of message it doesn't know. */
export const ahead = (name: string, what: string): Unread => ({ wire: "runner", from: runner(name), what, code: "unknown", why: "AgentMsg has 51 variants" });

/** A runner's update of an agent session the server couldn't read. */
export const unreadUpdate = (name: string, what: string): Unread => ({ wire: "runner_payload", from: runner(name), what, code: "unknown", why: "no such update" });

/** Each noted that many ms ago, the oldest first. */
function kept(said: [number, Unread][]): RefusedMessages {
  const now = Date.now();
  let k: RefusedMessages = { counts: [], newest: [] };
  for (const [ago, u] of said) k = note(k, u, now - ago);
  return k;
}

/** metal-ryzen-01's, the newest 25 minutes ago. */
export const runnerRefused = () =>
  kept([
    [2 * 24 * 60 * MIN, { wire: "reach_link", from: runner("metal-epyc-01"), what: "Say variant 9", code: "malformed", why: "cut short" }],
    [3 * 60 * MIN, { wire: "server_item", from: runner("metal-ryzen-01"), what: "item 7", code: "malformed", why: "bytes left over" }],
    [70 * MIN, ahead("metal-ryzen-01", "variant 51")],
    [40 * MIN, ahead("metal-ryzen-01", "variant 51")],
    [25 * MIN, ahead("metal-ryzen-01", "variant 52")],
  ]);

/** fam-tests', the newest 20 minutes ago. */
export const agentRefused = () =>
  kept([
    [60 * MIN, { wire: "mcp_http", from: { session: { id: "fam-tests" } }, what: "tools/call goals_add", code: "malformed", why: "unknown field `golas` at /golas" }],
    [35 * MIN, unreadUpdate("metal-ryzen-01", 'update kind "handoff"')],
    [20 * MIN, unreadUpdate("metal-ryzen-01", 'update kind "handoff"')],
  ]);
