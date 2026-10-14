import { events, useConnection } from "@/api/events";
import { useNow } from "@/lib/hooks";
import { Tip } from "@/components/ui/tooltip";

export function ConnectionIndicator({ compact }: { compact?: boolean }) {
  const c = useConnection();
  const now = useNow();
  const retryIn = c.retryAt ? Math.max(0, Math.ceil((c.retryAt - now) / 1000)) : null;
  const label =
    c.state === "open"
      ? "Live"
      : c.state === "connecting"
        ? "Connecting"
        : c.state === "closed"
          ? retryIn !== null
            ? `Retry ${retryIn}s`
            : "Offline"
          : "Idle";
  const tip =
    c.state === "open"
      ? "Event stream connected — views update live."
      : c.state === "closed"
        ? `Event stream disconnected (attempt ${c.attempts}). Click to reconnect now.`
        : "Connecting to the event stream…";
  return (
    <Tip content={tip}>
      <button
        type="button"
        className="conn"
        data-state={c.state}
        onClick={() => events.retryNow()}
        aria-label={`Event stream: ${label}`}
        aria-live="polite"
      >
        <i aria-hidden="true" />
        {!compact && <span>{label}</span>}
      </button>
    </Tip>
  );
}
