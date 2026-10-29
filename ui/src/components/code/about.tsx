"use client";
/**
 * About this code: in an agent's conversation (and its Changes panel, and the review page's diffs
 * an agent made), tap a name or a line of a diff or a code block and a card opens under the line.
 * Tap another line's number, or another line, to take in the lines between. **About** asks a
 * helper of the agent (reading only) what it is, what it does there, where else it's used and
 * what's worth knowing, and its answer streams into the card. Closing the card ends the helper:
 * an answer isn't kept (and kiln ends a helper no card reads: `crates/kiln/src/harness/about.rs`).
 * With subagents off the agent answers itself, and its answer streams into the card from its
 * conversation. The card is a band across the code, ruled above and below; it slides open and
 * shut, and nothing under it jumps while it works (its answer's room is kept from the start).
 */
import * as React from "react";
import { flushSync } from "react-dom";
import { harnessApi, useSession, type AgentResult, type HEvent, type SessionStore } from "@/api/harness";
import "./about.css";

/** The agent whose code it is: who asking goes to. */
export interface AboutAgent {
  id: string;
  name?: string;
  /** It runs subagents (none: it answers itself). */
  subagents: boolean;
  /** A turn of its is under way. */
  busy: boolean;
  /** Its conversation, where it answers itself (none: the card says it went there). */
  store?: SessionStore;
}

/** Given only where code is an agent's: code elsewhere can't be asked about. */
export const AboutContext = React.createContext<AboutAgent | null>(null);

/** What was tapped: a name on a line, a line or lines, or the change on a line, and the code around it. */
export interface Asked {
  path: string;
  line?: number;
  /** The last of the lines, when they're several. */
  end?: number;
  selection?: string;
  change?: boolean;
  code: string;
  /** How the card names it. */
  label: string;
  where: string;
}

const W = /[\p{L}\p{N}_$]/u;

/** The name under offset `at` of a line (a word of letters, digits, `_` and `$`), or null: whitespace or punctuation. */
export function wordAt(text: string, at: number): { start: number; end: number } | null {
  if (at < 0 || (!W.test(text.charAt(at)) && !W.test(text.charAt(at - 1)))) return null;
  let start = at;
  let end = at;
  while (start > 0 && W.test(text.charAt(start - 1))) start--;
  while (end < text.length && W.test(text.charAt(end))) end++;
  return end > start ? { start, end } : null;
}

/** Where in an element's text a click at (x, y) landed, in code units; null outside its text. */
export function offsetAt(el: HTMLElement, x: number, y: number): number | null {
  const doc = el.ownerDocument as Document & { caretPositionFromPoint?: (x: number, y: number) => { offsetNode: Node; offset: number } | null };
  let node: Node | null = null;
  let off = 0;
  const pos = doc.caretPositionFromPoint?.(x, y);
  if (pos) [node, off] = [pos.offsetNode, pos.offset];
  else {
    const r = doc.caretRangeFromPoint?.(x, y);
    if (r) [node, off] = [r.startContainer, r.startOffset];
  }
  if (!node || !el.contains(node)) return null;
  const walk = doc.createTreeWalker(el, NodeFilter.SHOW_TEXT);
  let total = 0;
  for (let n = walk.nextNode(); n; n = walk.nextNode()) {
    if (n === node) return total + off;
    total += n.textContent?.length ?? 0;
  }
  return null;
}

/** A tap that's really the end of selecting text asks nothing. */
export const selecting = () => (globalThis.getSelection?.()?.toString() ?? "") !== "";

/** How a card names lines `a` to `b`. */
export const linesLabel = (a: number | undefined, b: number | undefined) => (a !== undefined && b !== undefined && b !== a ? `lines ${Math.min(a, b)}–${Math.max(a, b)}` : `line ${a ?? b ?? ""}`);

/** The parts an answer comes in (`ABOUT_PARTS` in crates/kiln/src/harness/ops.rs). */
export const PARTS = ["What it is", "What it does here", "Used elsewhere", "Worth knowing"] as const;

/** An answer's parts, by their headings (null: not come yet); none found, the whole text as the first. */
export function partsOf(text: string): (string | null)[] {
  const out: (string | null)[] = PARTS.map(() => null);
  const marks = [...text.matchAll(/^#{1,4}\s*\**\s*(What it is|What it does here|Used elsewhere|Worth knowing)\s*\**\s*:?\s*$/gim)];
  if (!marks.length) {
    out[0] = text.trim() || null;
    return out;
  }
  marks.forEach((m, i) => {
    const at = PARTS.findIndex((p) => p.toLowerCase() === (m[1] ?? "").toLowerCase());
    const from = (m.index ?? 0) + m[0].length;
    if (at >= 0) out[at] = text.slice(from, marks[i + 1]?.index ?? text.length).trim();
  });
  return out;
}

/**
 * What an agent said after message `id` in its conversation (its own words, not its subagents',
 * until the next message), or null before the message shows. The message is found from the end,
 * where a question just asked is.
 */
export function replyTo(events: readonly HEvent[], id: string): string | null {
  let at = -1;
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e?.kind === "user" && (e.id === id || e.client_id === id)) {
      at = i;
      break;
    }
  }
  if (at < 0) return null;
  const said: string[] = [];
  for (const e of events.slice(at + 1)) {
    if (e.parent) continue;
    if (e.kind === "user") break;
    if (e.kind === "assistant" && e.text) said.push(e.text);
  }
  return said.join("\n\n");
}

/** A part's text as the card shows it: paragraphs and lists, `code` and **bold** (no HTML in it is ever drawn). */
function Text({ text }: { text: string }) {
  const inline = (s: string) =>
    s
      .split(/(`[^`]+`|\*\*[^*]+\*\*)/g)
      .map((p, i) =>
        p.startsWith("`") && p.endsWith("`") && p.length > 1 ? <code key={i}>{p.slice(1, -1)}</code> : p.startsWith("**") && p.endsWith("**") && p.length > 3 ? <b key={i}>{p.slice(2, -2)}</b> : p,
      );
  return text.split(/\n{2,}/).map((b, i) => {
    const lines = b.split("\n").filter((l) => l.trim());
    if (lines.length && lines.every((l) => /^\s*([-*•]|\d+\.)\s+/.test(l)))
      return (
        <ul key={i}>
          {lines.map((l, j) => (
            <li key={j}>{inline(l.replace(/^\s*([-*•]|\d+\.)\s+/, ""))}</li>
          ))}
        </ul>
      );
    return <p key={i}>{inline(lines.join(" "))}</p>;
  });
}

/**
 * The cards a surface holds: the one open, and those closing, each sliding shut before it goes.
 * `update` changes the open card (to none: closes it), and the card it replaces slides shut; a
 * card's `key` is its own (`cardKey`), so a card opened again where one is closing is a new one.
 */
export function useCards<T extends { key: string }>() {
  const [s, setS] = React.useState<{ open: T | null; closing: T[] }>({ open: null, closing: [] });
  const update = React.useCallback(
    (f: (open: T | null) => T | null) =>
      setS((s) => {
        const open = f(s.open);
        if (open === s.open) return s;
        const closing = s.open && s.open.key !== open?.key ? [...s.closing, s.open] : s.closing;
        return { open, closing: closing.filter((c) => c.key !== open?.key) };
      }),
    [],
  );
  const gone = React.useCallback((key: string) => setS((s) => ({ ...s, closing: s.closing.filter((c) => c.key !== key) })), []);
  return { open: s.open, closing: s.closing, update, gone };
}

let cards = 0;
/** A new card's key: `base` says where it is, the count makes it this card alone. */
export const cardKey = (base: string) => `${base}#${++cards}`;

/** A part of the card that slides open and shut (its height from nothing and back), so what's under it moves smoothly. Shut, it's out of reach. */
function Fold({ open, children }: { open: boolean; children: React.ReactNode }) {
  return (
    <div className="hx-about-fold" data-open={open || undefined} inert={!open}>
      <div className="hx-about-clip">{children}</div>
    </div>
  );
}

/**
 * Where an answer goes, its height kept from the moment it's asked for (its loading view: what's
 * reading, then each part's shape until the part comes), so nothing under the card moves while it
 * comes: a longer answer scrolls in it. `lead` comes first in it (the question, what it's reading).
 */
function Parts({ text, waiting, lead }: { text: string; waiting: boolean; lead?: React.ReactNode }) {
  const parts = partsOf(text);
  const headed = parts.some((x, j) => j > 0 && x);
  return (
    <div className="hx-about-answer" aria-live="polite" aria-busy={waiting}>
      {lead}
      {parts.map((p, i) =>
        p ? (
          <section key={i} className="hx-about-part">
            {(headed || i > 0) && <h4>{PARTS[i]}</h4>}
            <Text text={p} />
          </section>
        ) : waiting ? (
          <div key={i} className="hx-about-sk" data-refs={i === 2 || undefined} aria-hidden="true">
            <i />
            <i />
            <i />
          </div>
        ) : null,
      )}
    </div>
  );
}

/** Under the answer: how it stands, and what can be done (stop it, ask a follow-up), in a row that keeps its height. */
function Foot({ meta, children }: { meta: React.ReactNode; children?: React.ReactNode }) {
  return (
    <div className="hx-about-foot">
      <span className="hx-about-meta">{meta}</span>
      {children}
    </div>
  );
}

const Question = ({ q }: { q?: string }) => (q ? <p className="hx-about-q">“{q}”</p> : null);

const errorOf = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The helper's answer, read as it comes: what it's reading, then each part in its place. */
function Answer({ child, question, onDone, onFollowUp }: { child: string; question?: string; onDone: () => void; onFollowUp: () => void }) {
  const [r, setR] = React.useState<AgentResult | null>(null);
  const [trail, setTrail] = React.useState<string[]>([]);
  const [failed, setFailed] = React.useState<string | null>(null);
  const [stopped, setStopped] = React.useState(false);
  const started = React.useRef(Date.now());
  const over = (res: AgentResult) => res.agent.status === "exited" || (res.last_turn != null && res.agent.stand !== "working" && !!res.reply?.text);
  const done = !!r && over(r);
  // Following it starts as the answer shows and ends, with the helper, as it goes (the card closed).
  const follow = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      const abort = new AbortController();
      let timer = 0;
      const tick = async () => {
        try {
          const res = await harnessApi.result(child, abort.signal);
          setR(res);
          const doing = res.agent.doing?.trim();
          if (doing) setTrail((t) => (t[t.length - 1] === doing ? t : [...t, doing].slice(-4)));
          if (over(res)) onDone();
          else timer = window.setTimeout(() => void tick(), 1000);
        } catch (e) {
          if (!abort.signal.aborted) setFailed(errorOf(e));
        }
      };
      void tick();
      return () => {
        abort.abort();
        window.clearTimeout(timer);
        void harnessApi.end(child, true).catch(() => undefined);
      };
    },
    [child, onDone],
  );
  const stop = () => {
    setStopped(true);
    void harnessApi.end(child, true).catch(() => undefined);
  };
  const waiting = !done && !stopped && !failed;
  const secs = Math.max(0, Math.round((Date.now() - started.current) / 1000));
  return (
    <div ref={follow}>
      <Parts
        text={r?.reply?.text ?? ""}
        waiting={waiting}
        lead={
          <>
            <Question q={question} />
            {waiting && trail.length > 0 && (
              <ul className="hx-about-trail">
                {trail.map((t, i) => (
                  <li key={i}>{t}</li>
                ))}
              </ul>
            )}
            {failed && <p className="hx-about-note">Couldn&apos;t read its answer: {failed}</p>}
          </>
        }
      />
      <Foot
        meta={
          failed
            ? ""
            : stopped
              ? "Stopped. What it found so far stays."
              : done
                ? `Answered in ${secs}s${r?.agent.model ? ` · ${r.agent.model}` : ""}${r?.cost_usd ? ` · $${r.cost_usd.toFixed(2)}` : ""}`
                : r?.reply?.text
                  ? "Answering…"
                  : "Reading…"
        }
      >
        {waiting ? (
          <button type="button" className="hx-about-btn" onClick={stop}>
            Stop
          </button>
        ) : (
          <button type="button" className="hx-about-btn" onClick={onFollowUp}>
            Ask a follow-up
          </button>
        )}
      </Foot>
    </div>
  );
}

/** The agent's own answer, read from its conversation as it says it: after its turn, or between its steps. */
function ItselfAnswer({ store, message, now, question, onDone, onFollowUp }: { store: SessionStore; message: string; now: boolean; question?: string; onDone: () => void; onFollowUp: () => void }) {
  const text = useSession(store, (s) => replyTo(s.events, message));
  const idle = useSession(store, (s) => s.state?.turn_started == null);
  const done = !!text && idle;
  // Told once, as the answer ends (a ref callback runs as its element comes, as the card's do).
  const told = React.useCallback((el: HTMLSpanElement | null) => void (el && onDone()), [onDone]);
  return (
    <div>
      <Parts
        text={text ?? ""}
        waiting={!done}
        lead={
          <>
            <Question q={question} />
            {text === null && <p className="hx-about-note">{now ? "It answers between its next steps." : "It answers when this turn ends."}</p>}
          </>
        }
      />
      <Foot meta={done ? <span ref={told}>Answered in its conversation</span> : text ? "Answering…" : "Waiting for it…"}>
        {done && (
          <button type="button" className="hx-about-btn" onClick={onFollowUp}>
            Ask a follow-up
          </button>
        )}
      </Foot>
    </div>
  );
}

type Step =
  | { at: "menu" }
  | { at: "choose" }
  | { at: "starting"; question?: string }
  | { at: "answer"; child: string; name: string; question?: string; done?: boolean }
  | { at: "itself"; now: boolean; message?: string; question?: string; done?: boolean }
  | { at: "error"; why: string; question?: string };

/**
 * The card under a tapped line: what was tapped, and asking about it. It slides open as it comes
 * and, `closing`, slides shut, then says it's `onGone`; each step of it (asking, choosing when,
 * the answer) slides in as the one before slides out, and the answer's room is kept from the
 * moment it's asked for, so nothing under the card jumps.
 */
export function AboutCard({ ask, onClose, closing = false, onGone }: { ask: Asked; onClose: () => void; closing?: boolean; onGone?: () => void }) {
  const agent = React.useContext(AboutContext);
  const [step, setStep] = React.useState<Step>({ at: "menu" });
  const [question, setQuestion] = React.useState<string | null>(null);
  const [pending, setPending] = React.useState<string | undefined>(undefined);
  const [copied, setCopied] = React.useState(false);
  const input = React.useRef<HTMLInputElement | null>(null);
  const onDone = React.useCallback(() => setStep((s) => (s.at === "answer" || s.at === "itself" ? { ...s, done: true } : s)), []);
  // Shut, it goes when its slide ends; or soon after, when nothing slides (motion reduced, or out of sight).
  const gone = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || !onGone) return;
      const t = window.setTimeout(onGone, 450);
      return () => window.clearTimeout(t);
    },
    [onGone],
  );
  if (!agent) return null;
  const name = agent.name || "The agent";
  const go = async (o: { question?: string; now?: boolean }) => {
    setQuestion(null);
    setStep({ at: "starting", question: o.question });
    try {
      const r = await harnessApi.about(agent.id, { path: ask.path, code: ask.code, line: ask.line, end_line: ask.end, selection: ask.selection, change: ask.change, question: o.question, now: o.now });
      const child = r.agents[0];
      setStep(
        r.itself
          ? { at: "itself", now: !!o.now, message: r.sent?.message, question: o.question }
          : child
            ? { at: "answer", child: child.id, name: child.name, question: o.question }
            : { at: "error", why: "No helper started.", question: o.question },
      );
    } catch (e) {
      setStep({ at: "error", why: errorOf(e), question: o.question });
    }
  };
  const about = (q?: string) => {
    if (!agent.subagents && agent.busy) {
      setPending(q);
      setQuestion(null);
      setStep({ at: "choose" });
    } else void go({ question: q });
  };
  // The question field slides open under the card and takes the keys at once.
  const askOpen = () => {
    flushSync(() => setQuestion(""));
    input.current?.focus({ preventScroll: true });
  };
  const answering = step.at === "starting" || step.at === "answer" || step.at === "itself" || step.at === "error";
  const done = (step.at === "answer" || step.at === "itself") && !!step.done;
  const lines = !ask.selection && !ask.change;
  return (
    <div
      className="hx-about-card"
      data-closing={closing || undefined}
      inert={closing}
      ref={closing ? gone : undefined}
      onTransitionEnd={(e) => {
        if (closing && e.target === e.currentTarget && e.propertyName === "grid-template-rows") onGone?.();
      }}
    >
      <div className="hx-about-clip">
        <div className="hx-about" role="region" aria-label={`About ${ask.label}`} onKeyDown={(e) => e.key === "Escape" && onClose()} onClick={(e) => e.stopPropagation()}>
          <div className="hx-about-head">
            <code className="hx-about-subject">{ask.label}</code>
            <span className="hx-about-where">{ask.where}</span>
            <button type="button" className="hx-about-x" aria-label="Close" onClick={onClose}>
              ×
            </button>
          </div>
          <Fold open={step.at === "menu"}>
            <div className="hx-about-acts">
              <button type="button" className="hx-about-btn" data-primary autoFocus onClick={() => about()}>
                About
              </button>
              <button type="button" className="hx-about-btn" onClick={askOpen}>
                Ask…
              </button>
              <button
                type="button"
                className="hx-about-btn"
                onClick={() => {
                  void navigator.clipboard?.writeText(ask.selection ?? ask.code).then(() => setCopied(true));
                }}
              >
                {copied ? "Copied" : "Copy"}
              </button>
            </div>
            <p className="hx-about-note">
              {agent.subagents ? `A helper of ${name}'s reads the repository to answer, changing nothing.` : `Subagents are off for ${name}: it answers itself.`}
              {lines && " Tap another line to take in the lines between."}
            </p>
          </Fold>
          <Fold open={step.at === "choose"}>
            <div className="hx-about-choose">
              <p>
                <b>{name}</b> is in the middle of a turn.
              </p>
              <div className="hx-about-acts">
                <button type="button" className="hx-about-btn" data-primary onClick={() => void go({ question: pending, now: true })}>
                  Ask now
                </button>
                <button type="button" className="hx-about-btn" onClick={() => void go({ question: pending })}>
                  After this turn
                </button>
              </div>
              <p className="hx-about-note">Now: it answers between its next steps, then carries on. After: when this turn ends.</p>
            </div>
          </Fold>
          <Fold open={answering}>
            {answering && (
              <>
                <div className="hx-about-who">
                  <i className="hx-about-dot" data-done={done || undefined} data-failed={step.at === "error" || undefined} />
                  <span>
                    {step.at === "answer" ? (
                      <>
                        <b>{step.name}</b> · a helper of {name}&apos;s
                      </>
                    ) : step.at === "itself" ? (
                      <>
                        <b>{name}</b> · answering itself
                      </>
                    ) : step.at === "error" ? (
                      "Couldn't ask"
                    ) : (
                      "Asking…"
                    )}
                  </span>
                </div>
                {step.at === "answer" ? (
                  <Answer key={step.child} child={step.child} question={step.question} onDone={onDone} onFollowUp={askOpen} />
                ) : step.at === "itself" && step.message && agent.store ? (
                  <ItselfAnswer key={step.message} store={agent.store} message={step.message} now={step.now} question={step.question} onDone={onDone} onFollowUp={askOpen} />
                ) : step.at === "itself" ? (
                  <>
                    <Parts
                      text=""
                      waiting={false}
                      lead={
                        <p className="hx-about-said">
                          Asked {name} itself: its answer comes in its conversation, {step.now ? "between its next steps" : "when this turn ends"}.
                        </p>
                      }
                    />
                    <Foot meta="" />
                  </>
                ) : step.at === "error" ? (
                  <>
                    <Parts text="" waiting={false} lead={<p className="hx-about-note">Couldn&apos;t ask: {step.why}</p>} />
                    <Foot meta="">
                      <button type="button" className="hx-about-btn" onClick={() => about(step.question)}>
                        Try again
                      </button>
                    </Foot>
                  </>
                ) : (
                  <>
                    <Parts text="" waiting lead={<Question q={step.question} />} />
                    <Foot meta="Asking…" />
                  </>
                )}
              </>
            )}
          </Fold>
          <Fold open={question !== null && (step.at === "menu" || step.at === "answer" || step.at === "itself")}>
            <form
              className="hx-about-ask"
              onSubmit={(e) => {
                e.preventDefault();
                const q = String(new FormData(e.currentTarget).get("q") ?? "").trim();
                if (!q) return;
                e.currentTarget.reset();
                about(q);
              }}
            >
              <input ref={input} name="q" aria-label="Your question" placeholder={step.at === "menu" ? `Ask about ${ask.label}` : "Ask a follow-up"} />
              <button type="submit" className="hx-about-btn" data-primary>
                Ask
              </button>
            </form>
          </Fold>
        </div>
      </div>
    </div>
  );
}
