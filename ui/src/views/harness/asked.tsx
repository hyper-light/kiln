/**
 * The commands this page sent that wait for their answer, and those just refused, saying why
 * (docs/wire.md, "The UI's sockets"). A command answered at once never shows; one that waits a
 * moment (its machine is slow, or away and it waits for it) shows until its answer comes, sent again
 * on each socket meanwhile. It sits above the composer, with the controls it came from in view.
 */
import { CircleAlert, Hourglass } from "lucide-react";
import { useSession, type Command, type SessionStore } from "@/api/harness";
import { useClock } from "./dom";

/** How long a command waits before it shows as waiting: most are answered well within it. */
const SHOWN_AFTER_MS = 1500;

/** What a command asks, in a few words. */
export function askedWords(c: Command): string {
  switch (c.type) {
    case "send":
      return "Your message";
    case "skill_run":
      return `/${c.name}`;
    case "set_model":
      return `Model: ${c.model}`;
    case "set_mode":
      return `Permissions: ${c.mode}`;
    case "set_effort":
      return `Effort: ${c.effort}`;
    case "interrupt":
      return "Stop";
    case "force_stop":
      return "Force stop";
    case "approve":
      return "Your answer";
    case "queue_edit":
    case "queue_remove":
    case "queue_move":
    case "queue_steer":
    case "queue_send_now":
      return "The queue's change";
    default:
      return c.type.replaceAll("_", " ");
  }
}

export function AskedLine({ store }: { store: SessionStore }) {
  const asked = useSession(store, (s) => s.asked);
  const now = useClock(asked.length > 0 ? 500 : 30_000);
  const shown = asked.filter((a) => a.state === "refused" || now - a.at >= SHOWN_AFTER_MS);
  if (shown.length === 0) return null;
  return (
    <ul className="hx-asked" aria-label="What you asked of it">
      {shown.map((a) => (
        <li key={a.id} className="hx-asked-row" data-state={a.state} role={a.state === "refused" ? "alert" : "status"}>
          {a.state === "pending" ? <Hourglass size={13} aria-hidden="true" /> : <CircleAlert size={13} aria-hidden="true" />}
          <span className="hx-asked-what">{askedWords(a.command)}</span>
          <span className="hx-asked-how">{a.state === "pending" ? (a.why ? `waiting: ${a.why}` : "waiting for its machine to answer") : `refused: ${a.why ?? a.code ?? "no reason given"}`}</span>
        </li>
      ))}
    </ul>
  );
}
