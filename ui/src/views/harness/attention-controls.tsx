/**
 * Park, Archive and Restore's lines, in the page's flow (attention plan 01a199e1b177752e84dad5dc990e6a15):
 * asked in a line under the row (its ⋯ menu's Park until… and Archive…, a swipe left on a phone, a
 * stale request's Park and Archive) or under the list's head (the bulk Actions menu), as the bulk
 * bar's Move and End are; and the one-line Undo that stands where the row was.
 */
import * as React from "react";
import type { AttentionSettings } from "@/api/attention";
import { Button } from "@/components/ui/button";
import { useNow } from "@/lib/hooks";
import { parkChoices, untilWords } from "./attention-state";

/** Until when: the park menu's times (your settings), or one you pick. `what`: who it parks, in words. */
export function ParkAsk({ what, settings, onPark, onCancel }: { what: string; settings: AttentionSettings; onPark: (until: number) => void; onCancel: () => void }) {
  const [picking, setPicking] = React.useState(false);
  const [custom, setCustom] = React.useState("");
  const now = useNow();
  const at = custom ? new Date(custom).getTime() : Number.NaN;
  const future = Number.isFinite(at) && at > now;
  return (
    <div className="ms-bulk-ask ms-ask" role="group" aria-label={`Park ${what}`}>
      <span className="ms-bulk-q">Park {what} until</span>
      {parkChoices(now, settings).map((c, i) => (
        <Button
          key={c.value}
          size="xs"
          variant="secondary"
          autoFocus={i === 0}
          title={untilWords(c.at, now)}
          // The time as of the press: the shown clock ticks once a second.
          onClick={() => onPark(parkChoices(Date.now(), settings).find((x) => x.value === c.value)?.at ?? c.at)}
        >
          {c.label}
        </Button>
      ))}
      {picking ? (
        <span className="ms-ask-pick">
          <input className="input" type="datetime-local" aria-label="Park until this time" value={custom} onChange={(e) => setCustom(e.target.value)} autoFocus />
          <Button size="xs" disabled={!future} onClick={() => at > Date.now() && onPark(at)}>
            Park
          </Button>
        </span>
      ) : (
        <Button size="xs" variant="ghost" onClick={() => setPicking(true)}>
          Pick a time…
        </Button>
      )}
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

/** Archiving stops it for everyone: asked first. `n`: how many. */
export function ArchiveAsk({ what, n, onArchive, onCancel }: { what: string; n: number; onArchive: () => void; onCancel: () => void }) {
  return (
    <div className="ms-bulk-ask ms-ask" role="group" aria-label={`Archive ${what}`}>
      <span className="ms-bulk-q">
        Archive {what}? {n === 1 ? "It stops" : "They stop"} for everyone; {n === 1 ? "its conversation is" : "their conversations are"} kept, and Restore, under the State filter&rsquo;s Archived,
        brings {n === 1 ? "it" : "them"} back.
      </span>
      <Button size="xs" autoFocus onClick={onArchive}>
        {n === 1 ? "Archive" : `Archive ${n}`}
      </Button>
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

/**
 * Restore didn't go (its harness's own conversation may be gone): it can go on, in a new session, from
 * kiln's saved copy of the conversation, once you say so.
 */
export function CopyAsk({ what, error, onContinue, onCancel }: { what: string; error: string; onContinue: () => void; onCancel: () => void }) {
  const [sure, setSure] = React.useState(false);
  return (
    <div className="ms-bulk-ask ms-ask" role="group" aria-label={`Restore ${what}`}>
      <span className="ms-bulk-q ms-ask-error" role="alert">
        {error}
      </span>
      {sure ? (
        <>
          <span className="ms-bulk-q">Continue {what} in a new session from kiln&rsquo;s saved copy of its conversation? The saved history stays.</span>
          <Button size="xs" autoFocus onClick={onContinue}>
            Continue from the saved copy
          </Button>
        </>
      ) : (
        <Button size="xs" variant="secondary" onClick={() => setSure(true)}>
          Continue from kiln&rsquo;s saved copy…
        </Button>
      )}
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
    </div>
  );
}

/**
 * Where a row was, after you pinned, parked, unparked, archived or restored it: what was done, in one
 * line, and Undo, in the row's own height so nothing under it moves. Until your next action, or you leave.
 */
export function UndoLine({ text, failed, height, pending, onUndo }: { text: string; failed?: string; height: number; pending: boolean; onUndo?: () => void }) {
  return (
    <li className="ms-undo" data-entry="undo" style={height > 0 ? { minHeight: height } : undefined}>
      <span className="ms-undo-text" role="status">
        {text}
        {failed && <span className="ms-undo-failed"> {failed}</span>}
      </span>
      {(onUndo || pending) && (
        <Button size="xs" variant="secondary" pending={pending} onClick={() => onUndo?.()}>
          Undo
        </Button>
      )}
    </li>
  );
}
