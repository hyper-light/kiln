/**
 * The agent's page before its conversation is in (W21-R15): nothing on it waits. An agent started
 * from Mission control a moment ago (starting.ts) shows what it was asked, as sent, while it starts,
 * or why it couldn't, with Try again and Back. And the message box takes what's written meanwhile:
 * kept as its draft, or, sent, kept to go once the agent can take it (the composer sends what's
 * kept, composer.tsx), the box taking the focus back once the page is whole.
 */
import * as React from "react";
import "./early.css";
import { CornerDownLeft } from "lucide-react";
import { clientId } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { useNavigate } from "@/lib/router";
import { LoadingMark, PrismLabel } from "./signals";
import { putBack, startAgain, type Start } from "./starting";
import { useDrafts, useHeld, type Held } from "./state";

const SAID_AT = new Intl.DateTimeFormat([], { hour: "2-digit", minute: "2-digit" });
const NO_HELD: Held[] = [];

/** Enter sends with a keyboard and pointer; on a touch screen it's a new line (as in the composer). */
const fine = () => window.matchMedia?.("(pointer: fine)").matches;

/** The box was being written in as the page came: the page's own box takes the focus (`takeTyping`). */
let typing = false;

/** On the page once its conversation is in: its message box takes the focus the early one had, the caret at the end. */
export function takeTyping(el: HTMLElement | null) {
  if (!el || !typing) return;
  typing = false;
  const box = el.querySelector<HTMLTextAreaElement>("textarea.hx-compose-input");
  box?.focus();
  box?.setSelectionRange(box.value.length, box.value.length);
}

/** A message of yours the conversation doesn't show yet, drawn as it will be (timeline.tsx's `Sending`). */
function Mine({ text, at, files, word, failed }: { text: string; at: number; files?: string[]; word: string; failed?: boolean }) {
  return (
    <section className="hx-turn hx-sending" data-state={failed ? "failed" : "sending"}>
      <div className="hx-prompt-wrap">
        <div className="hx-prompt" data-origin="human">
          <div className="hx-prompt-meta">
            <span className="hx-eyebrow">You</span>
            <span className="hx-dim">{SAID_AT.format(at)}</span>
            <span className="hx-dim">
              ·{" "}
              <span className="hx-delivery" data-delivery={failed ? "failed" : "sending"}>
                {word}
              </span>
            </span>
            {files && files.length > 0 && (
              <span className="hx-dim hx-trunc" title={files.join(", ")}>
                · {files.join(", ")}
              </span>
            )}
          </div>
          <div className="hx-prompt-text">{text}</div>
        </div>
      </div>
    </section>
  );
}

/** Where the conversation goes, before it's in: what was asked and kept (or the loader). */
export function EarlyTurns({ id, start }: { id: string; start?: Start }) {
  const held = useHeld((s) => s.held[id] ?? NO_HELD);
  const failed = start?.state === "failed";
  if (!start)
    return (
      <div className="hx-timeline ag-loading" role="status">
        <LoadingMark />
        <PrismLabel live>Loading conversation</PrismLabel>
        {held.length > 0 && (
          <div className="ag-early-kept">
            {held.map((m) => (
              <Mine key={m.id} text={m.text} at={m.at} word="sent once it's open" />
            ))}
          </div>
        )}
      </div>
    );
  return (
    <div className="hx-timeline ag-early">
      {start.text && <Mine text={start.text} at={start.at} files={start.files} word={failed ? "not sent" : "sending…"} failed={failed} />}
      {held
        .filter((m) => m.id !== start.heldFirst)
        .map((m) => (
          <Mine key={m.id} text={m.text} at={m.at} word={failed ? "not sent" : "sent once it has started"} failed={failed} />
        ))}
    </div>
  );
}

/**
 * How its start goes, over the message box, where the page says what the agent does and that it
 * couldn't start (agent.tsx's dock): never under another agent's card, which sits above the box.
 */
export function EarlyLine({ id, start }: { id: string; start: Start }) {
  const navigate = useNavigate();
  // What it asked goes back to Mission control's box (starting.ts `putBack`).
  const back = () => {
    putBack(start);
    void navigate("/agents");
  };
  if (start.state === "failed")
    return (
      <div className="hx-startfail" role="alert">
        <b>{start.spec.resume ? "It couldn't be continued." : "This agent couldn't start."}</b> {start.why}
        <div className="mt-2 flex flex-wrap gap-2">
          <Button size="sm" variant="secondary" onClick={() => startAgain(id)}>
            Try again
          </Button>
          <Button size="sm" variant="ghost" onClick={back}>
            Back to Mission control
          </Button>
        </div>
      </div>
    );
  return (
    <p className="ag-early-line" role="status">
      <LoadingMark />
      <PrismLabel live>{start.spec.resume ? `Continuing it on ${start.machine}` : `Starting ${start.kindName} on ${start.machine}`}</PrismLabel>
    </p>
  );
}

/** The message box before the conversation is in: written in at once, what's sent kept to go once the agent can take it. */
export function EarlyComposer({ id, start }: { id: string; start?: Start }) {
  const draft = useDrafts((s) => s.drafts[id] ?? "");
  const setDraft = useDrafts((s) => s.setDraft);
  const failed = start?.state === "failed";
  const sizeDraft = React.useCallback(
    (el: HTMLTextAreaElement | null) => {
      void draft;
      if (!el) return;
      el.style.height = "auto";
      el.style.height = `${el.scrollHeight}px`;
    },
    [draft],
  );
  const keep = async () => {
    const text = draft.trim();
    if (!text || failed) return;
    if (await useHeld.getState().hold(id, { id: clientId(), text, at: Date.now(), mode: "queue" })) setDraft(id, "");
  };
  return (
    <div className="hx-composer cp">
      <div className="hx-compose-box">
        <div className="cp-field">
          <div className="hx-compose-input cp-mirror" aria-hidden="true">
            {draft}
            {"​"}
          </div>
          <textarea
            ref={sizeDraft}
            className="hx-compose-input cp-input"
            rows={1}
            value={draft}
            placeholder={start ? "The next message: it goes once the agent has started" : "Message the agent: it goes once the page is open"}
            aria-label="Message to the agent"
            enterKeyHint="send"
            onChange={(e) => setDraft(id, e.target.value)}
            onFocus={() => (typing = true)}
            onBlur={() => (typing = false)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !e.nativeEvent.isComposing && (e.metaKey || e.ctrlKey || (!e.shiftKey && fine()))) {
                e.preventDefault();
                keep();
              }
            }}
          />
        </div>
        <div className="cp-bar">
          <span className="cp-gap" />
          <button type="button" className="cp-send" data-mode="send" disabled={!draft.trim() || failed} aria-label="Queue" onClick={keep}>
            <CornerDownLeft size={15} />
            <span className="cp-label">Queue</span>
          </button>
        </div>
      </div>
    </div>
  );
}
