/**
 * The agents, lean, down the events socket (W21-R02; docs/wire.md, "The events socket"). An agent's every change
 * came as a `harness` event saying it whole, 1.2 KB: while one works that is three a second, of which a page
 * shows nothing new. A socket that asks (`?lean=1`) is sent each agent's entry once, lean (without where its
 * harness started and, while its turn runs, without its last words), and from then on only what changed of it
 * (`harness_patch`, `crates/kiln/src/wire/events/roster.rs`).
 *
 * What's kept here is each agent's entry as this socket last said it; what changed is applied to that, and
 * everything that listens for `harness` events hears the entry whole, as before. A server from before this says
 * every entry whole, which is each agent's first each time.
 */
import type { AgentInfo, RosterEvent, ServerEvent } from "./wire";

type Harness = ServerEvent & { type: "harness" };

/** Whether `v`, an event of a batch, says what changed of an agent's entry. */
export function isRosterEvent(v: unknown): v is RosterEvent {
  const e = v as Partial<RosterEvent> | null;
  return (
    typeof e === "object" && e !== null && e.type === "harness_patch" && typeof e.id === "string" && typeof e.set === "object" && e.set !== null && (e.unset === undefined || Array.isArray(e.unset))
  );
}

export class Roster {
  /** Each agent's entry as this socket last said it. */
  private last = new Map<string, AgentInfo>();

  /** The socket said an agent's entry whole: what's said to have changed from now on changed of this. */
  whole(info: AgentInfo) {
    this.last.set(info.id, info);
  }

  /**
   * The agent's entry with what `changed` says set and gone, as the `harness` event the socket would have said it
   * whole in; none when this socket never said that agent's entry (nothing to change: the server is told).
   */
  changed(changed: RosterEvent): Harness | undefined {
    const was = this.last.get(changed.id);
    if (!was) return undefined;
    const now: Record<string, unknown> = { ...was, ...changed.set };
    for (const key of changed.unset ?? []) delete now[key];
    const info = now as unknown as AgentInfo;
    this.last.set(changed.id, info);
    return { type: "harness", info };
  }

  /** The agent is gone: one that comes again under its id is said whole first. */
  closed(id: string) {
    this.last.delete(id);
  }

  /** Another socket, or events were missed: every agent is said whole again first. */
  clear() {
    this.last.clear();
  }
}
