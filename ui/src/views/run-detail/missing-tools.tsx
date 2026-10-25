/**
 * What a job lacks to run (`GET /api/jobs/:id/tools`): the command its failed step's log says
 * wasn't there, or for a job waiting on its `requires`, what each runner its labels fit doesn't
 * meet. With an agent kiln starts on that runner to install it, the job run again once it has
 * (`POST /api/jobs/:id/tools/fix`). Nothing when it lacks nothing kiln can name.
 */
import * as React from "react";
import { Wrench } from "lucide-react";
import { api } from "@/api/client";
import type { Job, ToolFix } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { Link } from "@/lib/router";
import { Quiet } from "@/views/harness/change-bar";

/** The server's name for Xcode's command-line tools (toolfix.rs `XCODE_TOOLS`). */
const XCODE_TOOLS = "xcode-command-line-tools";

/** `a`, `a and b`, `a, b and c`. */
function and(tools: string[]): string {
  const n = tools.map((t) => (t === XCODE_TOOLS ? "Xcode's command-line tools" : t));
  return n.length < 2 ? (n[0] ?? "") : `${n.slice(0, -1).join(", ")} and ${n[n.length - 1]}`;
}

/** Where a fix stands, as the card's corner says it. */
const STATE: Record<ToolFix["state"], string> = {
  working: "its agent is on it",
  needs_you: "waiting for you",
  checking: "checking the runner",
  retried: "run again",
  waiting: "the runner meets it now",
  still_missing: "still missing",
  stopped: "no longer followed",
};

/** A fix still under way: no second agent for the job. */
const UNDER_WAY: ToolFix["state"][] = ["working", "needs_you", "checking"];

function said(fix: ToolFix, waiting: boolean): string {
  const what = and(fix.tools);
  switch (fix.state) {
    case "working":
      return fix.note
        ? `An agent is installing ${what} on ${fix.runner}: ${fix.note}.`
        : `An agent is installing ${what} on ${fix.runner}. When it's done, kiln checks the runner again and ${waiting ? "the job goes there once it meets it" : "runs the job again"}.`;
    case "needs_you":
      return `The agent installing ${what} on ${fix.runner} waits for you: open it to answer.`;
    case "checking":
      return `The agent is done: kiln is asking ${fix.runner} what it has now.`;
    case "retried":
      return `${fix.runner} has ${what} now: ${fix.note ?? "kiln ran the job again"}.`;
    default:
      return fix.note ? `${fix.note}.` : "";
  }
}

function Lacks({ job }: { job: Job }) {
  const res = useSuspenseResource(`job-tools/${job.id}`, (s) => api.jobTools(job.id, s).catch(() => null), {
    on: ["job"],
    // Another job's event changes nothing here; this one's loads it again.
    apply: (d, ev) => (ev.type === "job" && ev.job.id === job.id ? undefined : d),
  });
  const [asking, setAsking] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [chosen, setChosen] = React.useState<string | null>(null);
  if (!res.data) return null;
  const { missing, fix } = res.data;
  // A failed job: what its log says wasn't there, on the runner it failed on.
  const failed = missing.tools.length > 0 && missing.on ? { runner: missing.on, tools: missing.tools } : null;
  // A waiting one: the runners an install would let it run on, the one asked for first.
  const fixable = missing.runners.filter((r) => r.fixable);
  const pick = failed ? (missing.no_agent ? null : failed) : (fixable.find((r) => r.runner === chosen) ?? fixable[0] ?? null);
  if (!fix && !failed && missing.runners.length === 0) return null;
  const underWay = fix !== null && UNDER_WAY.includes(fix.state);
  const waiting = job.status === "queued" || job.status === "pending";
  // What it lacks, or once it lacks nothing, what its agent installed where.
  const title = failed
    ? `${and(failed.tools)} wasn't there on ${failed.runner}`
    : missing.runners.length > 0
      ? "No runner meets this job's requirements"
      : fix
        ? `${and(fix.tools)} on ${fix.runner}`
        : "";

  const start = async () => {
    if (!pick) return;
    setBusy(true);
    try {
      const r = await api.fixJobTools(job.id, { runner: pick.runner });
      res.setData((d) => (d ? { ...d, fix: r.fix } : d));
      toast({ title: `An agent is installing ${and(r.fix.tools)} on ${r.fix.runner}`, tone: "info" });
      setAsking(false);
    } catch (e) {
      toast.error("No agent started", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="lacks" aria-label="What this job lacks" data-state={fix?.state ?? "lacks"}>
      <div className="lacks-head">
        <span className="lacks-title">{title}</span>
        {fix && <span className="lacks-state">{STATE[fix.state]}</span>}
      </div>
      {failed && missing.why && <code className="lacks-why">{missing.why}</code>}
      {failed && missing.no_agent && !fix && <p className="lacks-line">{`${missing.no_agent}.`}</p>}
      {!failed && missing.runners.length > 0 && (
        <ul className="lacks-runners">
          {missing.runners.map((r) => (
            <li key={r.runner}>
              <span className="lacks-runner">{r.runner}</span> doesn't meet <code>{r.unmet.join(", ")}</code>
              {!r.fixable && `: ${r.no_agent ?? "more than an install can change"}`}
            </li>
          ))}
        </ul>
      )}
      {fix && (
        <p className="lacks-line">
          {said(fix, waiting)}
          {fix.agent && (
            <>
              {" "}
              <Link to={`/agents/${encodeURIComponent(fix.agent)}`}>Open its agent</Link>
            </>
          )}
        </p>
      )}
      {!underWay && pick && !asking && (
        <div>
          <Button size="xs" variant="secondary" onClick={() => setAsking(true)}>
            <Wrench aria-hidden="true" />
            {fix ? "Try again with an agent" : "Install with an agent"}
          </Button>
        </div>
      )}
      {!underWay && pick && asking && (
        <Confirm
          label={`Install ${and(pick.tools)} on ${pick.runner}?`}
          actions={
            <>
              <Button size="xs" disabled={busy} onClick={() => void start()}>
                Start the agent
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>
                Cancel
              </Button>
            </>
          }
        >
          <p className="font-medium text-[#e4e5e7]">{`Install ${and(pick.tools)} on ${pick.runner}?`}</p>
          {!failed && fixable.length > 1 && (
            <div className="select">
              <select aria-label="The runner to install them on" value={pick.runner} onChange={(e) => setChosen(e.target.value)}>
                {fixable.map((r) => (
                  <option key={r.runner} value={r.runner}>
                    {r.runner}
                  </option>
                ))}
              </select>
            </div>
          )}
          <p>
            kiln starts a Claude agent on {pick.runner}, as you, to install only what's missing. A step that needs a person there (a password, an installer's dialog) waits for you in the agent. When
            it's done, kiln checks the runner's tools and {failed ? "runs this run's failed jobs again" : "the job goes there once the runner meets it"}.
          </p>
        </Confirm>
      )}
    </section>
  );
}

/** What `job` lacks to run, and the agent installing it: nothing when it lacks nothing. */
export function MissingTools({ job }: { job: Job }) {
  return (
    <Quiet>
      <React.Suspense fallback={null}>
        <Lacks job={job} />
      </React.Suspense>
    </Quiet>
  );
}
