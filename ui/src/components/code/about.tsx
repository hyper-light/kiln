"use client";
/**
 * About this code: in an agent's conversation, tap a name or a line of a diff or a code block and a
 * card opens under the line. **About** asks a child of the agent (reading only) what it is, what it
 * does there, where else it's used and what's worth knowing, and its answer streams into the card.
 * Closing the card ends the child: an answer isn't kept. With subagents off the agent answers
 * itself, in its conversation (after its turn, or between its steps when asked to now).
 */
import * as React from "react";
import { harnessApi, type AgentResult } from "@/api/harness";
import "./about.css";

/** The agent whose conversation the code is in: who asking goes to. */
export interface AboutAgent {
  id: string;
  name?: string;
  /** It runs subagents (none: it answers itself). */
  subagents: boolean;
  /** A turn of its is under way. */
  busy: boolean;
}

/** Given only in an agent's conversation: code elsewhere can't be asked about. */
export const AboutContext = React.createContext<AboutAgent | null>(null);

/** What was tapped: a name on a line, a whole line, or the change on a line, and the code around it. */
export interface Asked {
  path: string;
  line?: number;
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

/** A part's text as the card shows it: paragraphs and lists, `code` and **bold** (no HTML in it is ever drawn). */
function Text({ text }: { text: string }) {
  const inline = (s: string) =>
    s
      .split(/(`[^`]+`|\*\*[^*]+\*\*)/g)
      .map((p, i) =>
        p.startsWith("`") && p.endsWith("`") && p.length > 1 ? <code key={i}>{p.slice(1, -1)}</code> : p.startsWith("**") && p.endsWith("**") && p.length > 3 ? <b key={i}>{p.slice(2, -2)}</b> : p,
      );
  const blocks = text.split(/\n{2,}/);
  return blocks.map((b, i) => {
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

const errorOf = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The answer, read from the child as it comes: what it's reading, then each part in its place. */
function Answer({ child, onDone }: { child: string; onDone: (r: AgentResult) => void }) {
  const [r, setR] = React.useState<AgentResult | null>(null);
  const [trail, setTrail] = React.useState<string[]>([]);
  const [failed, setFailed] = React.useState<string | null>(null);
  const [stopped, setStopped] = React.useState(false);
  const started = React.useRef(Date.now());
  const done = !!r && (r.agent.status === "exited" || (r.last_turn != null && r.agent.stand !== "working" && !!r.reply?.text));
  // Following it starts as the answer shows and ends, with the child, as it goes (the card closed).
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
          const over = res.agent.status === "exited" || (res.last_turn != null && res.agent.stand !== "working" && !!res.reply?.text);
          if (over) onDone(res);
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
  const parts = partsOf(r?.reply?.text ?? "");
  const waiting = !done && !stopped && !failed;
  const secs = Math.max(0, Math.round((Date.now() - started.current) / 1000));
  return (
    <div ref={follow}>
      {waiting && trail.length > 0 && (
        <ul className="hx-about-trail">
          {trail.map((t, i) => (
            <li key={i}>{t}</li>
          ))}
        </ul>
      )}
      <div className="hx-about-answer" aria-live="polite" aria-busy={waiting}>
        {parts.map((p, i) =>
          p ? (
            <section key={i} className="hx-about-part">
              {(parts.some((x, j) => j > 0 && x) || i > 0) && <h4>{PARTS[i]}</h4>}
              <Text text={p} />
            </section>
          ) : waiting ? (
            // Its loading view: the part's shape, held in its place until it comes.
            <div key={i} className="hx-about-sk" data-refs={i === 2 || undefined} aria-hidden="true">
              <i />
              <i />
              <i />
            </div>
          ) : null,
        )}
      </div>
      {failed && <p className="hx-about-note">Couldn't read its answer: {failed}</p>}
      <div className="hx-about-foot">
        <span className="hx-about-meta">
          {failed
            ? ""
            : stopped
              ? "Stopped. What it found so far stays."
              : done
                ? `Answered in ${secs}s${r?.agent.model ? ` · ${r.agent.model}` : ""}${r?.cost_usd ? ` · $${r.cost_usd.toFixed(2)}` : ""}`
                : r?.reply?.text
                  ? "Answering…"
                  : "Reading…"}
        </span>
        {waiting && (
          <button type="button" className="hx-about-btn" onClick={stop}>
            Stop
          </button>
        )}
      </div>
    </div>
  );
}

type Step =
  | { at: "menu" }
  | { at: "choose" }
  | { at: "starting" }
  | { at: "answer"; child: string; name: string; question?: string; done?: boolean }
  | { at: "itself"; now: boolean }
  | { at: "error"; why: string };

/** The card under a tapped line: what was tapped, and asking about it. */
export function AboutCard({ ask, onClose }: { ask: Asked; onClose: () => void }) {
  const agent = React.useContext(AboutContext);
  const [step, setStep] = React.useState<Step>({ at: "menu" });
  const [question, setQuestion] = React.useState<string | null>(null);
  const [copied, setCopied] = React.useState(false);
  const onDone = React.useCallback(() => setStep((s) => (s.at === "answer" ? { ...s, done: true } : s)), []);
  if (!agent) return null;
  const name = agent.name || "The agent";
  const go = async (o: { question?: string; now?: boolean }) => {
    setQuestion(null);
    setStep({ at: "starting" });
    try {
      const r = await harnessApi.about(agent.id, { path: ask.path, code: ask.code, line: ask.line, selection: ask.selection, change: ask.change, question: o.question, now: o.now });
      const child = r.agents[0];
      setStep(r.itself ? { at: "itself", now: !!o.now } : child ? { at: "answer", child: child.id, name: child.name, question: o.question } : { at: "error", why: "No helper started." });
    } catch (e) {
      setStep({ at: "error", why: errorOf(e) });
    }
  };
  const about = (q?: string) => (!agent.subagents && agent.busy ? setStep({ at: "choose" }) : void go({ question: q }));
  const asking = (
    <form
      className="hx-about-ask"
      onSubmit={(e) => {
        e.preventDefault();
        const q = String(new FormData(e.currentTarget).get("q") ?? "").trim();
        if (q) about(q);
      }}
    >
      <input name="q" autoFocus aria-label="Your question" placeholder={step.at === "answer" ? "Ask a follow-up" : `Ask about ${ask.label}`} />
      <button type="submit" className="hx-about-btn" data-primary>
        Ask
      </button>
    </form>
  );
  return (
    <div className="hx-about" role="region" aria-label={`About ${ask.label}`} onKeyDown={(e) => e.key === "Escape" && onClose()} onClick={(e) => e.stopPropagation()}>
      <div className="hx-about-head">
        <code className="hx-about-subject">{ask.label}</code>
        <span className="hx-about-where">{ask.where}</span>
        <button type="button" className="hx-about-x" aria-label="Close" onClick={onClose}>
          ×
        </button>
      </div>
      {step.at === "menu" && (
        <>
          <div className="hx-about-acts">
            <button type="button" className="hx-about-btn" data-primary autoFocus onClick={() => about()}>
              About
            </button>
            <button type="button" className="hx-about-btn" onClick={() => setQuestion("")}>
              Ask…
            </button>
            <button
              type="button"
              className="hx-about-btn"
              onClick={() => {
                void navigator.clipboard?.writeText(ask.selection ?? ask.code.split("\n")[0] ?? "").then(() => setCopied(true));
              }}
            >
              {copied ? "Copied" : "Copy"}
            </button>
          </div>
          {question !== null && asking}
          <p className="hx-about-note">
            {agent.subagents ? `A helper of ${name}'s reads the repository to answer, changing nothing.` : `Subagents are off for ${name}: it answers itself, in its conversation.`}
          </p>
        </>
      )}
      {step.at === "choose" && (
        <div className="hx-about-choose">
          <p>
            <b>{name}</b> is in the middle of a turn.
          </p>
          <div className="hx-about-acts">
            <button type="button" className="hx-about-btn" data-primary onClick={() => void go({ now: true })}>
              Ask now
            </button>
            <button type="button" className="hx-about-btn" onClick={() => void go({})}>
              After this turn
            </button>
          </div>
          <p className="hx-about-note">Now: it answers between its next steps, then carries on. After: when this turn ends.</p>
        </div>
      )}
      {(step.at === "starting" || step.at === "answer") && (
        <>
          <div className="hx-about-who">
            <i className="hx-about-dot" data-done={(step.at === "answer" && step.done) || undefined} />
            <span>
              {step.at === "answer" ? (
                <>
                  <b>{step.name}</b> · a helper of {name}&apos;s
                </>
              ) : (
                "Starting a helper…"
              )}
            </span>
          </div>
          {step.at === "answer" && step.question && <p className="hx-about-q">“{step.question}”</p>}
          {step.at === "answer" ? (
            <Answer key={step.child} child={step.child} onDone={onDone} />
          ) : (
            <div className="hx-about-answer" aria-busy="true">
              {PARTS.map((p, i) => (
                <div key={p} className="hx-about-sk" data-refs={i === 2 || undefined} aria-hidden="true">
                  <i />
                  <i />
                  <i />
                </div>
              ))}
            </div>
          )}
          {step.at === "answer" && step.done && (
            <>
              {question !== null ? (
                asking
              ) : (
                <div className="hx-about-acts">
                  <button type="button" className="hx-about-btn" onClick={() => setQuestion("")}>
                    Ask a follow-up
                  </button>
                </div>
              )}
            </>
          )}
        </>
      )}
      {step.at === "itself" && (
        <p className="hx-about-note hx-about-said">
          Asked {name} itself: its answer comes in the conversation, {step.now ? "between its next steps" : "when this turn ends"}.
        </p>
      )}
      {step.at === "error" && (
        <div className="hx-about-choose">
          <p className="hx-about-note">Couldn&apos;t ask: {step.why}</p>
          <div className="hx-about-acts">
            <button type="button" className="hx-about-btn" onClick={() => about()}>
              Try again
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
