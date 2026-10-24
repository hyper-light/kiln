/**
 * Agents attached to what they look after (docs/ci.md W20-R08 to R10), where what they watch is
 * shown: a repository's panel (its workflows' and environments' too) and a runner's details (every
 * runner's too). "Watched by" on its own row, each attached agent's chip saying how it stands in a
 * word (watching, on it, waiting for you, paused): a chip opens its own line (what wakes it, its
 * agent, its latest incident; pause, remove), Attach a row of three (its name, its agent, what wakes
 * it). The rest is as kiln has it by default, or as trunk's `.kiln/agents.yaml` says.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { attachApi, attachmentsRes, onWords, WAKES_CODE, WAKES_RUNNERS, type AttachmentRow } from "@/api/attach";
import { Button } from "@/components/ui/button";
import { FacetBox } from "@/components/ui/facet-box";
import { SelectField, TextField } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { Link } from "@/lib/router";
import { invalidate } from "@/lib/resource";
import "./attach.css";

const WAKE_WORDS: Record<string, string> = {
  failure: "a run fails",
  flaky: "a test is flaky",
  ejected: "the land queue takes a change out",
  stuck: "a job waits for a runner",
  slow: "a run is slower than usual",
  unroutable: "a job can't run anywhere",
  degraded: "an app it deploys degrades",
  mention: "someone mentions it",
  schedule: "on a schedule",
  offline: "a runner goes offline",
  upgrade: "an upgrade fails",
  check: "kiln's checks find a problem",
};

const HARNESSES = [
  { value: "claude", label: "Claude Code" },
  { value: "codex", label: "Codex" },
  { value: "gemini", label: "Gemini CLI" },
  { value: "opencode", label: "opencode" },
  { value: "goose", label: "Goose" },
  { value: "amp", label: "Amp" },
  { value: "antigravity", label: "Antigravity CLI" },
];

/** How an attached agent stands, in a word, and its dot. */
type Stand = "watching" | "on it" | "waiting for you" | "paused";
const DOT: Record<Stand, string> = { watching: "succeeded", "on it": "running", "waiting for you": "pending", paused: "cancelled" };

function standOf(r: AttachmentRow): Stand {
  if (r.attachment.paused) return "paused";
  if (r.latest?.state === "waiting") return "waiting for you";
  return r.open > 0 ? "on it" : "watching";
}

const forRunners = (on: string) => on === "runners" || on.startsWith("runner:") || on.startsWith("pool:");

/** What can wake one that looks after `on`: an environment's are its deployments' and its apps', a workflow's its runs'. */
function wakesOf(on: string): readonly string[] {
  if (forRunners(on)) return WAKES_RUNNERS;
  if (on.startsWith("environment:")) return ["failure", "degraded", "mention", "schedule"];
  return on.startsWith("workflow:") ? WAKES_CODE.filter((w) => w !== "degraded") : WAKES_CODE;
}

/** What wakes it, in words; its agent. */
const wakeWords = (wake: string[]) => wake.map((w) => WAKE_WORDS[w] ?? w).join(", ");
const agentWords = (r: AttachmentRow) =>
  r.attachment.session ? "a live agent" : (HARNESSES.find((h) => h.value === r.attachment.harness?.kind)?.label ?? r.attachment.harness?.kind ?? "no agent");

/**
 * "Watched by", where what they watch is: its own row, then each attached agent's chip (what of it
 * it watches when that's narrower: a workflow, an environment, a pool), and Attach. A chip opens
 * its own line; Attach, a row of three. `watches` says which attachments are this thing's; `on` is
 * what one attached here watches.
 */
export function WatchedBy({ on, watches }: { on: string; watches: (on: string) => boolean }) {
  return (
    <section className="watch" aria-label="Watched by">
      <h4 className="field-label watch-label">Watched by</h4>
      <React.Suspense fallback={<div className="watch-chips" aria-hidden="true" />}>
        <Watchers on={on} watches={watches} />
      </React.Suspense>
    </section>
  );
}

/** What of the thing an attachment watches, said on its chip when it's narrower than the thing. */
function narrower(on: string, main: string): string | null {
  if (on === main) return null;
  if (on.startsWith("workflow:")) return on.split("/").pop() ?? on;
  if (on.startsWith("environment:")) return on.split(":").pop() ?? on;
  if (on.startsWith("pool:")) return `pool ${on.slice(5)}`;
  if (on.startsWith("runner:")) return on.slice(7);
  if (on === "runners") return "all runners";
  return onWords(on);
}

function Watchers({ on, watches }: { on: string; watches: (on: string) => boolean }) {
  const rows = useRes(attachmentsRes).data.filter((r) => watches(r.attachment.on));
  const [open, setOpen] = React.useState<string | null>(null);
  const [adding, setAdding] = React.useState(false);
  const shown = rows.find((r) => r.attachment.id === open);
  return (
    <>
      <div className="watch-chips">
        {rows.length === 0 && !adding && <span className="watch-none">No agent watches it.</span>}
        {rows.map((r) => {
          const stand = standOf(r);
          const id = r.attachment.id;
          const of = narrower(r.attachment.on, on);
          return (
            <button
              key={id}
              type="button"
              className="watch-chip"
              data-stand={stand}
              aria-expanded={open === id}
              onClick={() => {
                setOpen(open === id ? null : id);
                setAdding(false);
              }}
            >
              <i className="dot" data-status={DOT[stand]} aria-hidden="true" />
              <span className="watch-name">{r.attachment.name}</span>
              {of && <span className="watch-of">{of}</span>}
              <span className="watch-stand">{stand}</span>
            </button>
          );
        })}
        {!adding && (
          <Button
            size="xs"
            variant="secondary"
            className="watch-add"
            onClick={() => {
              setAdding(true);
              setOpen(null);
            }}
          >
            <Plus aria-hidden="true" /> Attach
          </Button>
        )}
      </div>
      {shown && <Watcher key={shown.attachment.id} r={shown} onGone={() => setOpen(null)} />}
      {adding && <QuickAttach on={on} onDone={() => setAdding(false)} />}
    </>
  );
}

/** Pause or resume one, or remove it (asked once more): the buttons, and what went wrong. */
function WatcherActions({ r, onGone }: { r: AttachmentRow; onGone?: () => void }) {
  const a = r.attachment;
  const [busy, setBusy] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const act = async (f: () => Promise<unknown>, after?: () => void) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      invalidate("attachments");
      after?.();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <span className="watch-acts">
      {removing ? (
        <>
          <Button size="xs" variant="danger" disabled={busy} onClick={() => void act(() => attachApi.remove(a.id), onGone)}>
            Remove {a.name}
          </Button>
          <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
            Keep it
          </Button>
        </>
      ) : (
        <>
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void act(() => attachApi.pause({ attachment: a.id, paused: !a.paused }))}>
            {a.paused ? "Resume" : "Pause"}
          </Button>
          {/* One kept in a repository is changed, or removed, there. */}
          {a.source ? (
            <span className="watch-kept">in {a.source.split(":").pop()}</span>
          ) : (
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(true)}>
              Remove
            </Button>
          )}
        </>
      )}
      {error && <span className="attach-error">{error}</span>}
    </span>
  );
}

/** An attached agent's own line: what wakes it, its agent, its latest incident; pause, remove. */
function Watcher({ r, onGone }: { r: AttachmentRow; onGone: () => void }) {
  return (
    <div className="watch-detail">
      <p className="watch-what">
        Wakes when {wakeWords(r.attachment.wake)} · {agentWords(r)}
        {r.latest && (
          <>
            {" · "}
            {r.latest.agent ? <Link to={`/agents/${encodeURIComponent(r.latest.agent)}`}>{r.latest.title}</Link> : r.latest.title}
          </>
        )}
      </p>
      <WatcherActions r={r} onGone={onGone} />
    </div>
  );
}

/** Attaching one: its name, its agent and what wakes it; the rest as kiln has it by default. */
function QuickAttach({ on, onDone }: { on: string; onDone: () => void }) {
  const runners = forRunners(on);
  const wakes = wakesOf(on);
  const [name, setName] = React.useState(runners ? "runner-doctor" : "ci-doctor");
  const [harness, setHarness] = React.useState("claude");
  const [wake, setWake] = React.useState<string[]>(runners ? ["offline", "upgrade"] : ["failure"]);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await attachApi.save(defaultsFor({ name, on, harness, wake, playbook: "" }));
      invalidate("attachments");
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="watch-form" onSubmit={(e) => void submit(e)}>
      <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} required />
      <SelectField label="Agent" value={harness} onChange={setHarness} options={HARNESSES} />
      <div className="watch-wakes">
        <span className="field-label">Wakes when</span>
        <FacetBox
          label="Wakes when"
          placeholder="what wakes it"
          facets={[{ id: "wake", label: "Wakes when", options: wakes.map((w) => ({ id: w, label: WAKE_WORDS[w] ?? w })) }]}
          chosen={{ wake }}
          onChosen={(c) => setWake(c.wake ?? [])}
        />
      </div>
      <div className="watch-form-acts">
        <Button size="xs" type="submit" disabled={busy || wake.length === 0 || !name.trim()}>
          {busy ? "Attaching…" : "Attach"}
        </Button>
        <Button size="xs" type="button" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        {error && <span className="attach-error">{error}</span>}
      </div>
    </form>
  );
}

/** An attachment as kiln keeps it, from what's asked of a person: the rest, kiln's defaults. */
function defaultsFor({ name, on, harness, wake, playbook, machine, cwd }: { name: string; on: string; harness: string; wake: string[]; playbook: string; machine?: string; cwd?: string }) {
  return {
    name: name.trim(),
    on,
    branches: [],
    harness: { kind: harness, ...(machine && { on: machine }), ...(cwd && { cwd }) },
    wake,
    can: ["read"],
    limits: { sessions_per_day: 10, reruns_per_failure: 2, stuck_minutes: 15, slow_percent: 25, tokens_per_day: 5_000_000, runner_minutes_per_day: 300 },
    playbook,
    reviewers: [],
  };
}
