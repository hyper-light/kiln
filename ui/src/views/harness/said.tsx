/**
 * What was said, found in the server's conversation store (docs/conversations.md, "Filtering and
 * search"): across Mission Control's standalone agents, and in one agent's history beyond
 * what its timeline holds from its find bar. Each match shows where its words are, marked.
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";
import { Link } from "@/lib/router";
import { harnessApi, type HEvent, type SaidHit } from "@/api/harness";
import { relativeTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { missionSaid } from "./mission-rows";

/** Words typed before conversations are searched. */
export const SAID_MIN = 3;
/** Matches shown at once. */
const SHOWN = 8;
/** Around the first match, the characters shown before and after it. */
const BEFORE = 60;
const AFTER = 120;

/** Markdown's marks, which a line of found words reads better without. */
const plain = (md: string) =>
  md
    .replace(/^\s{0,3}#{1,6}\s+/gm, "")
    .replace(/^\s*[-*+]\s+/gm, "")
    .replace(/(\*\*|__|`)/g, "");

/** An event's words as they are shown. */
export function wordsOf(e: HEvent): string {
  switch (e.kind) {
    case "user":
    case "notice":
      return e.text;
    case "assistant":
      return plain(e.text);
    case "thinking":
      return e.redacted ? "" : plain(e.text);
    case "tool":
      return [e.title, e.detail, e.output].filter(Boolean).join("\n");
  }
}

/** Who said it, in a word. */
const WHO: Record<HEvent["kind"], string> = { user: "You", assistant: "Agent", thinking: "Thought", tool: "Tool", notice: "Notice" };

/** `text` around where `q`'s first word first is (all of it: `whole`), every word of `q` in it marked. */
export function Snippet({ text, q, whole = false }: { text: string; q: string; whole?: boolean }) {
  const words = q
    .toLowerCase()
    .split(/\s+/)
    .filter((w) => w.length > 0);
  const lower = text.toLowerCase();
  const first = words.reduce((at, w) => {
    const i = lower.indexOf(w);
    return i >= 0 && (at < 0 || i < at) ? i : at;
  }, -1);
  const from = whole ? 0 : Math.max(0, first - BEFORE);
  const to = whole ? text.length : Math.min(text.length, Math.max(first, 0) + AFTER);
  const shown = whole ? text : text.slice(from, to).replace(/\s+/g, " ");
  if (words.length === 0) return <>{shown}</>;
  const re = new RegExp(`(${words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "gi");
  const parts = shown.split(re);
  return (
    <>
      {from > 0 && "…"}
      {parts.map((p, i) => (i % 2 === 1 ? <mark key={i}>{p}</mark> : <React.Fragment key={i}>{p}</React.Fragment>))}
      {to < text.length && "…"}
    </>
  );
}

const when = (at: number) => relativeTime(at / 1000);

/* ------------------------------------------------------------------ across every agent */

const searchAll = (q: string) => (): Promise<SaidHit[]> => harnessApi.said({ q, limit: 40 }).then((r) => r.hits);

function Hits({ q, agents }: { q: string; agents: ReadonlySet<string> }) {
  const found = useSuspenseResource(`conversations/said:${q}`, searchAll(q)).data;
  const hits = missionSaid(found, agents);
  const [all, setAll] = React.useState(false);
  if (hits.length === 0) return null;
  const shown = all ? hits : hits.slice(0, SHOWN);
  return (
    <section className="ms-said" aria-label="Found in conversations">
      <div className="ms-fold">
        <span className="ms-fold-btn">In conversations · {hits.length === 40 ? "40+" : hits.length}</span>
      </div>
      <ul className="ms-said-list">
        {shown.map((h) => {
          const body = (
            <>
              <span className="ms-said-head">
                <span className="ms-said-name">{h.name ?? "An agent that ended"}</span>
                {h.machine && <span className="ms-said-meta">{h.machine.replace(/^agent:/, "")}</span>}
                <span className="ms-said-meta">
                  {WHO[h.event.kind]} · {when(h.event.at)}
                </span>
              </span>
              <span className="ms-said-text">
                <Snippet text={wordsOf(h.event)} q={q} />
              </span>
            </>
          );
          return (
            <li key={`${h.agent}/${h.event.id}`}>
              {h.name ? (
                <Link to={`/agents/${encodeURIComponent(h.agent)}?find=${encodeURIComponent(q)}`} className="ms-said-hit">
                  {body}
                </Link>
              ) : (
                <div className="ms-said-hit ms-said-gone">{body}</div>
              )}
            </li>
          );
        })}
      </ul>
      {!all && hits.length > SHOWN && (
        <button type="button" className="ms-link ms-said-more" onClick={() => setAll(true)}>
          Show {hits.length - SHOWN} more
        </button>
      )}
    </section>
  );
}

/** Mission Control: matches in the standalone agents' conversations. */
export function SaidInConversations({ q, agents }: { q: string; agents: ReadonlySet<string> }) {
  const query = React.useDeferredValue(q.trim());
  if (query.length < SAID_MIN) return null;
  return (
    <React.Suspense fallback={null}>
      <Hits q={query} agents={agents} />
    </React.Suspense>
  );
}

/* ------------------------------------------------------------------ one agent's earlier history */

const searchEarlier = (agent: string, q: string, before: number) => (): Promise<SaidHit[]> => harnessApi.said({ q, agent, until: before, limit: 100 }).then((r) => r.hits);

function Earlier({ agent, q, before }: { agent: string; q: string; before: number }) {
  const hits = useSuspenseResource(`conversations/earlier:${agent}:${before}:${q}`, searchEarlier(agent, q, before)).data;
  const [open, setOpen] = React.useState(false);
  const [whole, setWhole] = React.useState<string | null>(null);
  if (hits.length === 0) return null;
  return (
    <div className="hx-earlier">
      <button type="button" className="hx-earlier-btn" aria-expanded={open} onClick={() => setOpen(!open)}>
        <ChevronRight size={12} className="hx-earlier-chev" aria-hidden="true" />
        {hits.length === 100 ? "100+" : hits.length} earlier, in its history
      </button>
      {open && (
        <ul className="hx-earlier-list">
          {hits.map((h) => {
            const words = wordsOf(h.event);
            const key = h.event.id;
            return (
              <li key={key}>
                <button type="button" className="hx-earlier-hit" aria-expanded={whole === key} onClick={() => setWhole(whole === key ? null : key)}>
                  <span className="hx-earlier-meta">
                    {WHO[h.event.kind]} · {when(h.event.at)}
                  </span>
                  <span className="hx-earlier-text">
                    <Snippet text={words} q={q} whole={whole === key} />
                  </span>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/**
 * An agent's find bar, when its timeline holds only its latest events: the matches before them,
 * from its history, to read in place.
 */
export function EarlierMatches({ agent, q, before }: { agent: string; q: string; before: number | undefined }) {
  const query = React.useDeferredValue(q.trim());
  if (!query || before === undefined) return null;
  return (
    <React.Suspense fallback={null}>
      <Earlier agent={agent} q={query} before={before} />
    </React.Suspense>
  );
}
