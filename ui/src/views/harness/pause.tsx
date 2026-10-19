/**
 * A pause for a usage limit (`State.pause`): what it waits for and until when, said once, with what
 * you can do about it where you'd act: resume now, let it go on by itself at the reset or not, or go
 * on now, here in another harness or on another machine. It sits in the page's flow above the
 * composer; nothing pops up.
 */
import * as React from "react";
import { useSession, type Command, type Pause, type SessionStore } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Link } from "@/lib/router";
import { useClock } from "./dom";
import { GoOnNow } from "./elsewhere";
import { resetWhen } from "./limits";

/** The window, in words: "the Session limit", "a usage limit". */
const limitWords = (p: Pause) => (p.label ? `the ${p.label} limit` : "a usage limit");

/** When it goes on: "resumes 10:30 PM PDT", "checks again tomorrow 9:00 AM PDT", "resumes when you resume it". */
export function whenWords(p: Pause, now: number): string {
  if (!p.auto) return "resumes when you resume it";
  if (p.checking) return "checking whether it reset";
  if (p.next_check) return `checks again ${resetWhen(p.next_check, now)}`;
  if (p.until) return p.until > now ? `resumes ${resetWhen(p.until, now)}` : "resuming";
  return "checks back now and then";
}

/** One line for a pause: what it waits for, and when it goes on. */
export function pauseLine(p: Pause, now: number): string {
  const what = p.reason === "threshold" ? `Stopped at its stop point for ${limitWords(p)}` : p.by === "account" ? `Another agent on this account reached ${limitWords(p)}` : `Reached ${limitWords(p)}`;
  return `${what} · ${whenWords(p, now)}`;
}

/** Above the composer while the agent is paused. */
export function PausedLine({ store, send }: { store: SessionStore; send: (c: Command) => void }) {
  const pause = useSession(store, (s) => s.state?.pause);
  const queued = useSession(store, (s) => s.state?.queue?.length ?? 0);
  const info = useSession(store, (s) => s.info);
  const canSwitch = useSession(store, (s) => (s.meta?.features ?? []).includes("switch_harness"));
  // What was just asked here: when, and of which pause (to tell whether anything came of it).
  const [tried, setTried] = React.useState<{ at: number; since: number } | null>(null);
  const now = useClock(tried ? 1000 : 30_000);
  if (!pause) return null;
  const act = (...cs: Command[]) => {
    setTried({ at: Date.now(), since: pause.since });
    for (const c of cs) send(c);
  };
  // A few seconds on, still this pause, or paused again since: its runner didn't do it. One from
  // before a stop point could be resumed past stops again at once; one that predates a command
  // leaves it be.
  const since = tried ? now - tried.at : 0;
  const stuck = tried !== null && since > 4000 && since < 180_000 && (pause.since === tried.since || pause.since >= tried.at - 1000);
  const machine = info?.target.startsWith("agent:") ? info.target.slice("agent:".length) : null;
  const waits = queued > 0 ? `${queued} queued message${queued === 1 ? " waits" : "s wait"}` : "What you send waits in the queue";
  return (
    <div className="hx-paused" role="status">
      <p className="hx-paused-what">
        <b>Paused.</b> {pauseLine(pause, now)}. {waits}
        {pause.cut ? "; the turn it cut off goes on first." : "."}
      </p>
      <div className="hx-paused-do">
        <Button size="xs" variant="secondary" onClick={() => act({ type: "resume" })}>
          Resume now
        </Button>
        <label className="hx-paused-auto">
          <input type="checkbox" className="check" checked={pause.auto} onChange={(e) => send({ type: "auto_resume", on: e.target.checked })} />
          Resume at the reset
        </label>
      </div>
      {stuck && (
        <p className="hx-paused-stuck">
          {pause.since === tried.since ? "It hasn't gone on" : "It stopped again at its stop point"}: its runner{machine ? ` on ${machine}` : ""} runs an older kiln.{" "}
          <Link to="/settings#updates.runners">Upgrade it</Link>
          {pause.reason === "threshold" && (
            <>
              , or{" "}
              <button type="button" className="hx-paused-link" onClick={() => act({ type: "pause_at", percent: null }, { type: "resume" })}>
                resume with its stop point off
              </button>{" "}
              (it then pauses at the limit itself)
            </>
          )}
          .
        </p>
      )}
      {info && (
        <React.Suspense fallback={null}>
          <GoOnNow agent={info} canSwitch={canSwitch} send={(c) => act(c)} />
        </React.Suspense>
      )}
      {pause.checkpoint && (
        <p className="hx-paused-kept">
          Its folder at the pause is kept: <code title={pause.checkpoint.git_ref}>{pause.checkpoint.commit.slice(0, 10)}</code>
        </p>
      )}
    </div>
  );
}
