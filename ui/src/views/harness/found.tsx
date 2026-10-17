/**
 * Coding sessions running on your machines that kiln didn't start: Claude Code, Codex or Gemini in
 * a terminal. They sit in Mission control's list with the same rows: open one to watch its
 * transcript right there, open its terminal, or take it over as an agent kiln runs.
 */
import * as React from "react";
import { Link, Navigate, useLocation, useNavigate, useSearchParams } from "react-router";
import { ChevronRight, SquareTerminal } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi } from "@/api/harness";
import type { AiSession, AiTranscriptEntry } from "@/api/types";
import { Button } from "@/components/ui/button";
import { toast } from "@/components/ui/toast";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { tildify } from "./derive";
import { HarnessMark } from "./parts";

const TOOL: Record<string, string> = { claude: "Claude Code", codex: "Codex", gemini: "Gemini CLI" };
const toolLabel = (tool: string) => TOOL[tool] ?? tool;
export const sessionKey = (s: AiSession) => `${s.machine}:${s.pid}`;
const terminalLink = (s: AiSession) => `/terminal?session=${encodeURIComponent(s.terminal ?? "")}&target=${encodeURIComponent(`agent:${s.machine}`)}`;

/** Mission control's stands, as far as a session seen from outside can tell. */
type Stand = "working" | "finished" | "ready";
const standOf = (s: AiSession): Stand => (s.state === "working" ? "working" : s.state === "waiting" ? "finished" : "ready");
const WORD: Record<Stand, string> = { working: "Working", finished: "Your turn", ready: "Running" };

function lineOf(s: AiSession, stand: Stand): string {
  if (stand === "working") return s.last_prompt ? `› ${s.last_prompt}` : "Working";
  if (stand === "finished") return s.last_reply || s.last_prompt || "Done";
  return s.last_prompt ? `› ${s.last_prompt}` : tildify(s.cwd);
}

/** Take over, confirmed in the row: what happens to the session where it runs now, then go. */
function TakeOver({ s, confirming, onConfirming }: { s: AiSession; confirming: boolean; onConfirming: (on: boolean) => void }) {
  const t = s.takeover;
  if (t === "ready" || t === "stop")
    return confirming ? null : (
      <Button size="xs" onClick={() => onConfirming(true)}>
        Take over
      </Button>
    );
  if (t === "upgrade")
    return (
      <Link to="/settings" className="ms-reason">
        Update {s.machine} to take over
      </Link>
    );
  if (t === "outside") return <span className="ms-reason">Quit it there to take over</span>;
  if (t === "no_id") return <span className="ms-reason">Starting…</span>;
  return null;
}

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
          {busy ? "Taking over…" : "Take over"}
        </Button>
        <Button size="xs" variant="ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </Button>
      </span>
    </div>
  );
}

/** Its transcript, read live in the row (refetched whenever the machine reports activity). */
function Watch({ s }: { s: AiSession }) {
  const [entries, setEntries] = React.useState<AiTranscriptEntry[] | null>(null);
  const [err, setErr] = React.useState<string | null>(null);
  const log = React.useRef<HTMLDivElement>(null);
  const atBottom = React.useRef(true);
  const { machine, pid, last_activity: activity, transcript } = s;
  React.useEffect(() => {
    if (!transcript) return;
    const ctl = new AbortController();
    api
      .aiTranscript(machine, pid, ctl.signal)
      .then((e) => {
        setEntries(e);
        setErr(null);
      })
      .catch((e: unknown) => !ctl.signal.aborted && setErr(errorMessage(e)));
    return () => ctl.abort();
  }, [machine, pid, activity, transcript]);
  // Follow new messages unless the reader scrolled up.
  React.useLayoutEffect(() => {
    const el = log.current;
    if (el && atBottom.current) el.scrollTop = el.scrollHeight;
  }, [entries]);
  const facts = [toolLabel(s.tool), s.model, s.branch && `on ${s.branch}`, tildify(s.cwd), `pid ${s.pid}`].filter(Boolean).join(" · ");
  return (
    <div className="ms-watch">
      <p className="ms-watch-facts">{facts}</p>
      {!transcript ? (
        <p className="ms-watch-note">Its transcript can't be read from here.</p>
      ) : err ? (
        <p className="ms-watch-note">{err}</p>
      ) : !entries ? (
        <p className="ms-watch-note">Reading its transcript…</p>
      ) : entries.length === 0 ? (
        <p className="ms-watch-note">Nothing said yet.</p>
      ) : (
        <div
          ref={log}
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
      )}
    </div>
  );
}

function Row({ s, now, focused }: { s: AiSession; now: number; focused: boolean }) {
  const [watching, setWatching] = React.useState(focused);
  const [confirming, setConfirming] = React.useState(false);
  const ref = React.useRef<HTMLLIElement>(null);
  React.useEffect(() => {
    if (focused) ref.current?.scrollIntoView({ block: "center" });
  }, [focused]);
  const stand = standOf(s);
  const title = s.title || s.name || toolLabel(s.tool);
  return (
    <li ref={ref} className="ms-row ms-found-row" data-stand={stand}>
      <span className="ms-mark" title={toolLabel(s.tool)}>
        <HarnessMark kind={s.tool} size={16} />
      </span>
      <button type="button" className="ms-main" aria-expanded={watching} onClick={() => setWatching(!watching)}>
        <span className="ms-name">{title}</span>
        <span className="ms-line">
          <span className="ms-word">
            <i aria-hidden="true" />
            {WORD[stand]}
          </span>
          <span className="ms-text">{lineOf(s, stand)}</span>
        </span>
      </button>
      <span className="ms-meta">
        <span>
          <span className="ms-tool">{toolLabel(s.tool)} · </span>
          {s.machine} · {relativeTime(s.last_activity, now)}
        </span>
        <span title={s.cwd}>{s.terminal ? tildify(s.cwd) : "outside kiln"}</span>
      </span>
      <span className="ms-actions">
        <TakeOver s={s} confirming={confirming} onConfirming={setConfirming} />
        {s.terminal && (
          <Button size="xs" variant="secondary" asChild>
            <Link to={terminalLink(s)}>
              <SquareTerminal /> Terminal
            </Link>
          </Button>
        )}
        <button type="button" className="ms-go" aria-expanded={watching} aria-label={watching ? `Close ${title}` : `Watch ${title}`} onClick={() => setWatching(!watching)}>
          <ChevronRight size={16} className={cn("ms-chev", watching && "rotate-90")} />
        </button>
      </span>
      {confirming && <Confirm s={s} onCancel={() => setConfirming(false)} />}
      {watching && <Watch s={s} />}
    </li>
  );
}

/** Mission control's section of found sessions; folds like Ended. */
export function FoundSessions({ sessions, quiet, now }: { sessions: AiSession[]; quiet: string[]; now: number }) {
  const [params] = useSearchParams();
  const { hash } = useLocation();
  const focus = params.get("session");
  const [open, setOpen] = useLocalStorage<boolean>("kiln.agents.showFound", true);
  const ref = React.useRef<HTMLElement>(null);
  const shown = open || Boolean(focus);
  React.useEffect(() => {
    if (hash === "#found" && !focus) ref.current?.scrollIntoView({ block: "start" });
  }, [hash, focus]);
  if (sessions.length === 0 && quiet.length === 0) return null;
  const working = sessions.filter((s) => s.state === "working").length;
  return (
    <section ref={ref} id="found" className="ms-found">
      <div className="ms-fold">
        <button type="button" className="ms-fold-btn" aria-expanded={shown} onClick={() => setOpen(!shown)}>
          <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
          Also on your machines · {sessions.length}
        </button>
        {working > 0 && <span className="ms-fold-note">{working} working</span>}
      </div>
      {shown && (
        <>
          {sessions.length > 0 && (
            <ul className="ms-list">
              {sessions.map((s) => (
                <Row key={sessionKey(s)} s={s} now={now} focused={focus === sessionKey(s)} />
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
