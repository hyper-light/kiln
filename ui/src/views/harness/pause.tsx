/**
 * A pause for a usage limit (`State.pause`): what it waits for and until when, said once, with what
 * you can do about it where you'd act: resume now, let it go on by itself at the reset or not, or go
 * on now in another harness. It sits in the page's flow above the composer; nothing pops up.
 */
import * as React from "react";
import { ChevronDown } from "lucide-react";
import { harnessApi, useSession, type Command, type KindInfo, type Pause, type SessionStore } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { useSuspenseResource } from "@/lib/resource";
import { useClock } from "./dom";
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
  const moveOn = useSession(store, (s) => s.state?.move_on_limit ?? false);
  const queued = useSession(store, (s) => s.state?.queue?.length ?? 0);
  const target = useSession(store, (s) => s.info?.target);
  const kind = useSession(store, (s) => s.info?.kind);
  const canSwitch = useSession(store, (s) => (s.meta?.features ?? []).includes("switch_harness"));
  const now = useClock(30_000);
  if (!pause) return null;
  const waits = queued > 0 ? `${queued} queued message${queued === 1 ? " waits" : "s wait"}` : "Messages you send wait in the queue";
  return (
    <div className="hx-paused" role="status">
      <p className="hx-paused-what">
        <b>Paused.</b> {pauseLine(pause, now)}. {waits}
        {pause.cut ? "; the turn it cut off goes on first." : "."}
      </p>
      <div className="hx-paused-do">
        <Button size="xs" variant="secondary" onClick={() => send({ type: "resume" })}>
          Resume now
        </Button>
        <label className="hx-paused-auto">
          <input type="checkbox" className="check" checked={pause.auto} onChange={(e) => send({ type: "auto_resume", on: e.target.checked })} />
          Resume by itself at the reset
        </label>
        <label className="hx-paused-auto" title="It moves to another of your machines whose harness is signed in to an account with room; its conversation and repository go along">
          <input type="checkbox" className="check" checked={moveOn} onChange={(e) => send({ type: "move_on_limit", on: e.target.checked })} />
          Go on on another machine
        </label>
        {canSwitch && target && kind && (
          <React.Suspense fallback={null}>
            <GoOnElsewhere target={target} kind={kind} send={send} />
          </React.Suspense>
        )}
      </div>
      {pause.checkpoint && (
        <p className="hx-paused-kept">
          Its folder as it was at the pause is kept in <code>{pause.checkpoint.git_ref}</code> ({pause.checkpoint.commit.slice(0, 10)}).
        </p>
      )}
    </div>
  );
}

/** The other harnesses on its machine it can go on in now, the conversation carried over. */
function GoOnElsewhere({ target, kind, send }: { target: string; kind: string; send: (c: Command) => void }) {
  const kinds = useSuspenseResource(`harness-kinds/${target}`, () => harnessApi.kinds(target).catch((): KindInfo[] => []), { pollMs: 120_000 });
  const others = kinds.data.filter((k) => k.switch && k.available && k.id !== kind && (k.models?.length ?? 0) > 0);
  if (others.length === 0) return null;
  return (
    <div className="select hx-paused-switch">
      <select
        aria-label="Go on now in another harness"
        value=""
        onChange={(e) => {
          const [k, ...rest] = e.target.value.split(":");
          const model = rest.join(":");
          if (k && model) send({ type: "set_model", model, kind: k });
        }}
      >
        <option value="">Go on now in…</option>
        {others.map((k) => (
          <optgroup key={k.id} label={k.name}>
            {(k.models ?? []).map((m) => (
              <option key={m.id} value={`${k.id}:${m.id}`}>
                {m.label || m.id}
              </option>
            ))}
          </optgroup>
        ))}
      </select>
      <ChevronDown size={14} aria-hidden="true" />
    </div>
  );
}
