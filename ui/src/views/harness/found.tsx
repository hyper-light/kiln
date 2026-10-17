/**
 * Coding sessions running on your machines that kiln didn't start: Claude Code, Codex or Gemini in
 * a terminal. They sit in Mission control's list with the same rows, in the order they started, and
 * nothing in a row moves as it updates. Each offers the same two things: take it over (it ends
 * where it runs and continues here) or fork it (a copy continues here; it keeps running); and its
 * transcript, to read in place.
 */
import * as React from "react";
import { Link, Navigate, useLocation, useNavigate, useSearchParams } from "react-router";
import { ChevronRight, SquareTerminal } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi } from "@/api/harness";
import type { AiSession, AiTranscriptEntry } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { tildify } from "./derive";
import { Ago, HarnessMark } from "./parts";

const TOOL: Record<string, string> = { claude: "Claude Code", codex: "Codex", gemini: "Gemini CLI" };
const toolLabel = (tool: string) => TOOL[tool] ?? tool;
export const sessionKey = (s: AiSession) => `${s.machine}:${s.pid}`;
const terminalLink = (s: AiSession) => `/terminal?session=${encodeURIComponent(s.terminal ?? "")}&target=${encodeURIComponent(`agent:${s.machine}`)}`;

/** Newest first, by when each started: a row keeps its place for as long as it runs. */
export const byStart = (a: AiSession, b: AiSession) => b.started_at - a.started_at || a.pid - b.pid;

/** Mission control's stands, as far as a session seen from outside can tell. */
type Stand = "working" | "finished" | "ready";
const standOf = (s: AiSession): Stand => (s.state === "working" ? "working" : s.state === "waiting" ? "finished" : "ready");
const WORD: Record<Stand, string> = { working: "Working", finished: "Your turn", ready: "Running" };

function lineOf(s: AiSession, stand: Stand): string {
  if (stand === "working") return s.last_prompt ? `› ${s.last_prompt}` : "Working";
  if (stand === "finished") return s.last_reply || s.last_prompt || "Done";
  return s.last_prompt ? `› ${s.last_prompt}` : tildify(s.cwd);
}

/** Whether it can be taken over and forked from here, and if not, why (one line). */
function abilities(s: AiSession): { take: boolean; fork: boolean; why: string; update: boolean } {
  const t = s.takeover;
  const take = t === "ready" || t === "stop";
  const fork = s.fork === true;
  if (take && fork) return { take, fork, why: "", update: false };
  if (t === "no_id") return { take, fork, why: "Starting: nothing to continue yet", update: false };
  if (t === "upgrade") return { take, fork, why: `Update ${s.machine} to take over${fork ? "" : " or fork"}`, update: true };
  if (t === "outside") return { take, fork, why: "Quit it there to take it over", update: false };
  if (t === "unsupported") return { take, fork, why: "kiln can't continue this tool's sessions", update: false };
  if (take && !fork && s.fork === false) return { take, fork, why: `Update ${s.machine} to fork`, update: true };
  return { take, fork, why: "", update: false };
}

/** Take over, confirmed in the row: what happens to it where it runs now, then go. */
function Confirm({ s, onCancel }: { s: AiSession; onCancel: () => void }) {
  const navigate = useNavigate();
  const [busy, setBusy] = React.useState(false);
  const go = async () => {
    setBusy(true);
    try {
      const [a] = await harnessApi.takeover(s.machine, s.pid, { stop: s.takeover === "stop" });
      if (a) navigate(`/agents/${a.id}`);
    } catch (e) {
      toast.error("Couldn't take it over", errorMessage(e));
      setBusy(false);
    }
  };
  return (
    <div className="ms-confirm" role="group" aria-label="Take over">
      <p>
        {s.takeover === "stop" ? `Stops it on ${s.machine}` : `Ends its terminal on ${s.machine}`} and continues here as an agent: same folder, conversation
        and model.
      </p>
      <span className="ms-confirm-actions">
        <Button size="xs" disabled={busy} onClick={go}>
          Take over
        </Button>
        <Button size="xs" variant="ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </Button>
      </span>
    </div>
  );
}

/** Its transcript, read in the row (read again whenever the machine reports activity). */
function Transcript({ s }: { s: AiSession }) {
  const facts = [toolLabel(s.tool), s.model, s.branch && `on ${s.branch}`, tildify(s.cwd), `pid ${s.pid}`].filter(Boolean).join(" · ");
  return (
    <div className="ms-watch">
      <p className="ms-watch-facts">{facts}</p>
      {!s.transcript ? (
        <p className="ms-watch-note">Its transcript can't be read from here.</p>
      ) : (
        <Loadable label="Loading its transcript" fallback={<p className="ms-watch-note">Reading its transcript…</p>}>
          <Log machine={s.machine} pid={s.pid} />
        </Loadable>
      )}
    </div>
  );
}

/** The transcript's messages, or why they can't be read. */
function Log({ machine, pid }: { machine: string; pid: number }) {
  const atBottom = React.useRef(true);
  const { entries, err } = useSuspenseResource(
    `ai/transcript/${machine}/${pid}`,
    (signal) =>
      api.aiTranscript(machine, pid, signal).then(
        (entries): { entries: AiTranscriptEntry[]; err: string | null } => ({ entries, err: null }),
        (e: unknown) => ({ entries: [], err: errorMessage(e) }),
      ),
    // A machine reports its sessions when one of them changes (its last activity among them).
    { on: ["ai_sessions"] },
  ).data;
  // Follow new messages unless the reader scrolled up: at once, and whenever the log changes.
  const follow = React.useCallback((log: HTMLDivElement | null) => {
    if (!log) return;
    const end = () => {
      if (atBottom.current) log.scrollTop = log.scrollHeight;
    };
    end();
    const changes = new MutationObserver(end);
    changes.observe(log, { subtree: true, childList: true, characterData: true });
    return () => changes.disconnect();
  }, []);
  return err ? (
    <p className="ms-watch-note">{err}</p>
  ) : entries.length === 0 ? (
    <p className="ms-watch-note">Nothing said yet.</p>
  ) : (
    <div
      ref={follow}
      className="ms-watch-log"
      onScroll={(e) => {
        const el = e.currentTarget;
        atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
      }}
    >
      {entries.map((e, i) => (
        <p key={i} className="ms-entry" data-role={e.role}>
          {e.text}
        </p>
      ))}
    </div>
  );
}

/** Re-renders only when its session changes; its clock counts by itself. */
const Row = React.memo(function Row({ s, focused }: { s: AiSession; focused: boolean }) {
  const navigate = useNavigate();
  const [reading, setReading] = React.useState(focused);
  const [confirming, setConfirming] = React.useState(false);
  const [forking, setForking] = React.useState(false);
  // The session a link points at comes into view (again if the link changes to it).
  const reveal = React.useCallback(
    (li: HTMLLIElement | null) => {
      if (li && focused) li.scrollIntoView({ block: "center" });
    },
    [focused],
  );
  const stand = standOf(s);
  const title = s.title || s.name || toolLabel(s.tool);
  const can = abilities(s);
  const fork = async () => {
    setForking(true);
    try {
      const [a] = await harnessApi.fork(s.machine, s.pid);
      if (a) navigate(`/agents/${a.id}`);
    } catch (e) {
      toast.error("Couldn't fork it", errorMessage(e));
    } finally {
      setForking(false);
    }
  };
  return (
    <li ref={reveal} className="ms-row ms-found-row" data-stand={stand}>
      <span className="ms-mark" title={toolLabel(s.tool)}>
        <HarnessMark kind={s.tool} size={16} />
      </span>
      <div className="ms-main">
        <span className="ms-name" title={title}>
          {title}
        </span>
        <span className="ms-line">
          <span className="ms-word">
            <i aria-hidden="true" />
            {WORD[stand]}
          </span>
          <span className="ms-text">{lineOf(s, stand)}</span>
        </span>
      </div>
      <span className="ms-meta">
        <span>
          <span className="ms-tool">{toolLabel(s.tool)} · </span>
          {s.machine} · <Ago className="ms-time" ts={s.last_activity} />
        </span>
        <span title={s.cwd}>{s.terminal ? tildify(s.cwd) : "outside kiln"}</span>
      </span>
      <span className="ms-actions">
        <span className="ms-buttons">
          <Button size="xs" disabled={!can.take || confirming} title={can.take ? undefined : can.why} onClick={() => setConfirming(true)}>
            Take over
          </Button>
          <Button size="xs" variant="secondary" disabled={!can.fork || forking} title={can.fork ? "Continue a copy here; it keeps running" : can.why} onClick={fork}>
            Fork
          </Button>
          {s.terminal && (
            <Button size="xs" variant="ghost" asChild>
              <Link to={terminalLink(s)} aria-label="Open its terminal" title="Open its terminal">
                <SquareTerminal />
              </Link>
            </Button>
          )}
        </span>
        <button type="button" className="ms-read" aria-expanded={reading} onClick={() => setReading(!reading)}>
          Transcript
          <ChevronRight size={13} className={cn("ms-chev", reading && "rotate-90")} aria-hidden="true" />
        </button>
        {can.update ? (
          <Link to="/settings" className="ms-why">
            {can.why}
          </Link>
        ) : (
          <span className="ms-why">{can.why}</span>
        )}
      </span>
      {confirming && <Confirm s={s} onCancel={() => setConfirming(false)} />}
      {reading && <Transcript s={s} />}
    </li>
  );
});

/** Mission control's section of found sessions; folds like Ended. */
export function FoundSessions({ sessions, quiet }: { sessions: AiSession[]; quiet: string[] }) {
  const [params] = useSearchParams();
  const { hash } = useLocation();
  const focus = params.get("session");
  const [open, setOpen] = useLocalStorage<boolean>("kiln.agents.showFound", true);
  const shown = open || Boolean(focus);
  const there = sessions.length > 0 || quiet.length > 0;
  // At #found (where the old page's links go) the section scrolls into view, once, if it's here as
  // the page opens or the address changes; not when it turns up later.
  const [at, setAt] = React.useState({ hash, focus, there });
  if (at.hash !== hash || at.focus !== focus) setAt({ hash, focus, there });
  const reveal = React.useMemo(() => {
    let due = at.there && at.hash === "#found" && !at.focus;
    return (section: HTMLElement | null) => {
      if (!section || !due) return;
      due = false;
      section.scrollIntoView({ block: "start" });
    };
  }, [at]);
  if (!there) return null;
  return (
    <section ref={reveal} id="found" className="ms-found">
      <div className="ms-fold">
        <button type="button" className="ms-fold-btn" aria-expanded={shown} onClick={() => setOpen(!shown)}>
          <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
          Also on your machines · {sessions.length}
        </button>
      </div>
      {shown && (
        <>
          {sessions.length > 0 && (
            <ul className="ms-list">
              {sessions.map((s) => (
                <Row key={sessionKey(s)} s={s} focused={focus === sessionKey(s)} />
              ))}
            </ul>
          )}
          {quiet.length > 0 && (
            <p className="ms-found-quiet">
              {quiet.join(", ")} {quiet.length === 1 ? "runs" : "run"} an older kiln and {quiet.length === 1 ? "doesn't" : "don't"} report sessions yet.
            </p>
          )}
        </>
      )}
    </section>
  );
}

/** The old page of found sessions: they live in Mission control now. */
export default function FoundRedirect() {
  const [params] = useSearchParams();
  // Links from before CI machines were called runners.
  if (params.has("agent") || params.has("join")) return <Navigate to={`/runners?${params.toString()}`} replace />;
  const session = params.get("session");
  return <Navigate to={session ? `/agents?session=${encodeURIComponent(session)}` : "/agents#found"} replace />;
}
