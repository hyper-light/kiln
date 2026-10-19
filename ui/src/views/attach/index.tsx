/**
 * Agents attached to what they look after (docs/ci.md W20-R08 to R10, docs/agentic.md K2): a
 * repository, a workflow, an environment, a runner pool or the runners. kiln wakes the one
 * attached when something happens there (a failure, a flaky test, a runner offline, …) with what
 * happened, and it reports back on the incident. Attach one, pause or remove it, and follow its
 * incidents, all in the page's flow.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { Link } from "@/lib/router";
import { attachApi, attachmentsRes, CAN, incidentsRes, onWords, WAKES_CODE, WAKES_RUNNERS, type AttachmentInput, type AttachmentRow, type IncidentBrief } from "@/api/attach";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, EmptyState, PageIntro, SelectField, TextField } from "@/components/ui/misc";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import "./attach.css";

const WAKE_WORDS: Record<string, string> = {
  failure: "a run fails",
  flaky: "a test is flaky",
  ejected: "the land queue takes a change out",
  stuck: "a job waits for a runner",
  mention: "someone mentions it",
  schedule: "on a schedule",
  offline: "a runner goes offline",
  upgrade: "an upgrade fails",
  check: "kiln's checks find a problem",
};

const CAN_WORDS: Record<string, string> = {
  rerun: "Rerun",
  cancel: "Cancel runs",
  diagnose: "Diagnose (debug, terminals)",
  fix: "Open fixes as diffs",
  "edit-pipelines": "Change workflows",
  quarantine: "Quarantine flaky tests",
  comment: "Comment",
  experiment: "Run experiments",
  "manage-runners": "Manage runners",
  deploy: "Deploy",
  land: "Land",
};

const STATE_WORDS: Record<IncidentBrief["state"], string> = { open: "its agent is on it", reported: "reported", resolved: "resolved", waiting: "waiting for you", dismissed: "dismissed" };

export default function AttachedPage() {
  const [adding, setAdding] = React.useState(false);
  return (
    <div className="container-console page attach-page">
      <title>Attached agents · kiln</title>
      <PageIntro
        index={6}
        label="Agents / Attached"
        title="Attached agents."
        lead="Agents that look after a repository, a workflow, an environment or your runners. When something happens there, kiln wakes the one attached with what happened, and it reports back here."
        actions={
          !adding && (
            <Button size="sm" onClick={() => setAdding(true)}>
              <Plus /> Attach an agent
            </Button>
          )
        }
      />
      {adding && <AttachForm onDone={() => setAdding(false)} />}
      <Loadable label="the attachments" fallback={<RowsHold rows={3} rowHeight={84} label="Loading the attachments" />}>
        <Attached onAttach={() => setAdding(true)} adding={adding} />
      </Loadable>
      <h2 className="attach-heading">Incidents</h2>
      <Loadable label="the incidents" fallback={<RowsHold rows={3} rowHeight={56} label="Loading the incidents" />}>
        <Incidents />
      </Loadable>
    </div>
  );
}

function Attached({ onAttach, adding }: { onAttach: () => void; adding: boolean }) {
  const rows = useRes(attachmentsRes).data;
  if (rows.length === 0) {
    return (
      <EmptyState
        title="Nothing attached yet"
        action={
          !adding && (
            <Button size="sm" variant="secondary" onClick={onAttach}>
              Attach an agent
            </Button>
          )
        }
      >
        Attach an agent to a repository and it wakes when a run there fails; to your runners and it wakes when one goes offline or an upgrade fails.
      </EmptyState>
    );
  }
  return (
    <ul className="attach-list">
      {rows.map((r) => (
        <Row key={r.attachment.id} r={r} />
      ))}
    </ul>
  );
}

function Row({ r }: { r: AttachmentRow }) {
  const a = r.attachment;
  const [busy, setBusy] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await f();
      invalidate("attachments");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const agent = a.session ? `the live agent ${a.session.slice(0, 8)}` : a.harness ? `${a.harness.kind}${a.harness.on ? ` on ${a.harness.on}` : ""}` : "no agent";
  return (
    <li className="attach-row" data-paused={a.paused || undefined}>
      <div className="attach-row-head">
        <span className="attach-name">{a.name}</span>
        <span className="attach-on">{onWords(a.on)}</span>
        <span className="attach-state">{a.paused ? "paused" : r.open > 0 ? `${r.open} open incident${r.open === 1 ? "" : "s"}` : "watching"}</span>
      </div>
      <div className="attach-meta">
        Wakes when {a.wake.map((w) => WAKE_WORDS[w] ?? w).join(", ")}
        {a.workflows?.length ? ` (${a.workflows.join(", ")})` : ""} · {agent} · may read{a.can.filter((c) => c !== "read").length ? `, ${a.can.filter((c) => c !== "read").join(", ")}` : " only"} ·{" "}
        {r.today} of {a.limits.sessions_per_day} today{a.source ? ` · kept in ${a.source}` : ""}
      </div>
      {r.latest && (
        <div className="attach-latest">
          Latest: {r.latest.title} · {STATE_WORDS[r.latest.state]} · {relativeTime(r.latest.at)}
        </div>
      )}
      {removing ? (
        <Confirm
          label={`Remove ${a.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" disabled={busy} onClick={() => void act(() => attachApi.remove(a.id))}>
                Remove
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
                Keep it
              </Button>
            </>
          }
        >
          Nothing wakes {a.name} any more; its incidents stay.
        </Confirm>
      ) : (
        <div className="attach-actions">
          <Button size="xs" variant="secondary" disabled={busy} onClick={() => void act(() => attachApi.pause({ attachment: a.id, paused: !a.paused }))}>
            {a.paused ? "Resume" : "Pause"}
          </Button>
          {/* One kept in a repository is changed, or removed, there. */}
          {!a.source && (
            <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(true)}>
              Remove
            </Button>
          )}
          {error && <span className="attach-error">{error}</span>}
        </div>
      )}
    </li>
  );
}

function Incidents() {
  const list = useRes(incidentsRes).data;
  if (list.length === 0) return <p className="attach-dim">None yet: what wakes an attached agent shows here, with what it found.</p>;
  return (
    <ul className="attach-incidents">
      {list.map((i) => (
        <li key={i.id} className="attach-incident" data-state={i.state}>
          <div className="attach-incident-title">{i.title}</div>
          <div className="attach-meta">
            {i.name} · {STATE_WORDS[i.state]}
            {i.class ? ` · ${i.class}` : ""}
            {i.times > 1 ? ` · ${i.times} times` : ""} · {relativeTime(i.at)}
            {i.agent && (
              <>
                {" · "}
                <Link to={`/agents/${encodeURIComponent(i.agent)}`}>its agent</Link>
              </>
            )}
          </div>
          {i.why && i.state === "waiting" && <div className="attach-latest">{i.why}.</div>}
          {i.report && <div className="attach-report">{i.report}</div>}
        </li>
      ))}
    </ul>
  );
}

/** What it looks after, as the form asks it. */
type Kind = "repo" | "workflow" | "environment" | "runners" | "runner" | "pool";

function onOf(kind: Kind, repo: string, extra: string): string {
  switch (kind) {
    case "repo":
      return `repo:${repo.trim()}`;
    case "workflow":
      return `workflow:${repo.trim()}:${extra.trim()}`;
    case "environment":
      return `environment:${repo.trim()}:${extra.trim()}`;
    case "runners":
      return "runners";
    case "runner":
      return `runner:${extra.trim()}`;
    case "pool":
      return `pool:${extra.trim()}`;
  }
}

const KINDS = [
  { value: "repo", label: "A repository" },
  { value: "workflow", label: "A workflow" },
  { value: "environment", label: "An environment" },
  { value: "runners", label: "Every runner" },
  { value: "runner", label: "One runner" },
  { value: "pool", label: "A runner pool" },
];

const HARNESSES = [
  { value: "claude", label: "Claude Code" },
  { value: "codex", label: "Codex" },
  { value: "gemini", label: "Gemini CLI" },
  { value: "opencode", label: "opencode" },
  { value: "goose", label: "Goose" },
  { value: "amp", label: "Amp" },
  { value: "antigravity", label: "Antigravity CLI" },
];

function AttachForm({ onDone }: { onDone: () => void }) {
  const [name, setName] = React.useState("");
  const [kind, setKind] = React.useState<Kind>("repo");
  const [repo, setRepo] = React.useState("");
  const [extra, setExtra] = React.useState("");
  const [branches, setBranches] = React.useState("");
  const [harness, setHarness] = React.useState("claude");
  const [machine, setMachine] = React.useState("");
  const [folder, setFolder] = React.useState("");
  const runners = kind === "runners" || kind === "runner" || kind === "pool";
  const wakes: readonly string[] = runners ? WAKES_RUNNERS : WAKES_CODE;
  const [wake, setWake] = React.useState<string[]>(["failure"]);
  const [schedule, setSchedule] = React.useState("");
  const [can, setCan] = React.useState<string[]>([]);
  const [perDay, setPerDay] = React.useState("10");
  const [playbook, setPlaybook] = React.useState("");
  const [reviewers, setReviewers] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const toggle = (list: string[], set: (v: string[]) => void, v: string) => set(list.includes(v) ? list.filter((x) => x !== v) : [...list, v]);
  const pickKind = (k: string) => {
    const next = k as Kind;
    const toRunners = next === "runners" || next === "runner" || next === "pool";
    if (toRunners !== runners) setWake(toRunners ? ["offline", "upgrade"] : ["failure"]);
    setKind(next);
  };
  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const body: AttachmentInput = {
      name,
      on: onOf(kind, repo, extra),
      branches: runners
        ? []
        : branches
            .split(",")
            .map((b) => b.trim())
            .filter(Boolean),
      harness: { kind: harness, ...(machine.trim() && { on: machine.trim() }), ...(folder.trim() && { cwd: folder.trim() }) },
      wake: wake.filter((w) => wakes.includes(w)),
      ...(wake.includes("schedule") && { schedule: schedule.trim() }),
      can: ["read", ...can],
      limits: { sessions_per_day: Number(perDay) || 10, reruns_per_failure: 2, stuck_minutes: 15 },
      playbook,
      reviewers: reviewers
        .split(",")
        .map((r) => r.trim())
        .filter(Boolean),
    };
    try {
      await attachApi.save(body);
      invalidate("attachments");
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <form className="attach-form" onSubmit={(e) => void submit(e)}>
      <div className="attach-form-grid">
        <TextField label="Name" placeholder="ci-doctor" value={name} onChange={(e) => setName(e.target.value)} hint="People call it with @name in comments." required />
        <SelectField label="Looks after" value={kind} onChange={pickKind} options={KINDS} />
        {!runners && <TextField label="Repository" placeholder="acme/app" value={repo} onChange={(e) => setRepo(e.target.value)} required />}
        {kind === "workflow" && <TextField label="Workflow" placeholder=".github/workflows/ci.yml" value={extra} onChange={(e) => setExtra(e.target.value)} required />}
        {kind === "environment" && <TextField label="Environment" placeholder="production" value={extra} onChange={(e) => setExtra(e.target.value)} required />}
        {kind === "runner" && <TextField label="Runner" placeholder="metal-1" value={extra} onChange={(e) => setExtra(e.target.value)} required />}
        {kind === "pool" && <TextField label="Pool" placeholder="gpu" value={extra} onChange={(e) => setExtra(e.target.value)} required />}
        {!runners && <TextField label="Branches" placeholder="any; or main, release" value={branches} onChange={(e) => setBranches(e.target.value)} />}
        <SelectField label="Agent" value={harness} onChange={setHarness} options={HARNESSES} />
        <TextField label="Runs on" placeholder="the server; or a machine, linux/amd64, +gpu" value={machine} onChange={(e) => setMachine(e.target.value)} />
        <TextField label="In folder" placeholder="a checkout of the repository" value={folder} onChange={(e) => setFolder(e.target.value)} />
        <TextField label="Incidents a day" type="number" min={1} max={500} value={perDay} onChange={(e) => setPerDay(e.target.value)} />
      </div>
      <fieldset className="attach-checks">
        <legend className="field-label">Wakes when</legend>
        {wakes.map((w) => (
          <label key={w} className="attach-check">
            <input type="checkbox" className="check" checked={wake.includes(w)} onChange={() => toggle(wake, setWake, w)} />
            {WAKE_WORDS[w]}
          </label>
        ))}
      </fieldset>
      {wake.includes("schedule") && <TextField label="Schedule" placeholder="0 6 * * 1 (cron, UTC)" value={schedule} onChange={(e) => setSchedule(e.target.value)} required />}
      <fieldset className="attach-checks">
        <legend className="field-label">May, beyond reading what it looks after</legend>
        {CAN.filter((c) => (runners ? ["diagnose", "manage-runners", "comment"] : CAN.filter((x) => x !== "manage-runners")).includes(c)).map((c) => (
          <label key={c} className="attach-check">
            <input type="checkbox" className="check" checked={can.includes(c)} onChange={() => toggle(can, setCan, c)} />
            {CAN_WORDS[c]}
          </label>
        ))}
      </fieldset>
      <div>
        <label className="field-label" htmlFor="attach-playbook">
          Playbook
        </label>
        <div className="field">
          <textarea
            id="attach-playbook"
            rows={4}
            placeholder="What it does when it wakes: how to tell flaky from broken, when to rerun, what to open, whom to ask."
            value={playbook}
            onChange={(e) => setPlaybook(e.target.value)}
          />
        </div>
      </div>
      <TextField label="Reviewers" placeholder="who reviews what it opens: ada, grace" value={reviewers} onChange={(e) => setReviewers(e.target.value)} />
      <div className="attach-actions">
        <Button size="sm" type="submit" disabled={busy}>
          {busy ? "Attaching…" : "Attach"}
        </Button>
        <Button size="sm" type="button" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
        {error && <span className="attach-error">{error}</span>}
      </div>
    </form>
  );
}
