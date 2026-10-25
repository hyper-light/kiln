/**
 * One plan: written or changed in place (each saved change a version), its steps as they will run
 * (as their graph: side by side where they don't wait for each other), its research (what its steps
 * rest on, each finding graded), run on an agent (one that runs, or a new one; as one message, step
 * by step, as goals, or an agent per step), shared by a link, exported, its versions and runs. A
 * plan made from a goal is planned first: its agent researches it and writes its steps, and it runs
 * once it's ready. Everything in the page's flow. A plan is kept with its agent session (written in
 * its Plan tab, or by the agent): the way back is to that session's plans. A plan that isn't there
 * (deleted, or a wrong link) says so.
 */
import * as React from "react";
import { Link, useNavigate, useParams, useSearchParams } from "@/lib/router";
import { ApiError } from "@/api/client";
import { harnessApi, type KindInfo } from "@/api/harness";
import { plansApi, runHref, shareUrl, type Plan, type PlanItem, type PlanMode, type PlanRan, type PlanStarted } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, CopyButton, EmptyState, SelectField, TextField } from "@/components/ui/misc";
import { useAuth } from "@/lib/auth";
import { harnessSessionsRes, useRes, warm, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { Prose } from "@/views/harness/parts";
import { Shimmer } from "@/views/harness/signals";
import { ResearchPanel } from "@/views/research/research";
import { SessionCrumbs } from "./crumbs";
import { effectiveNeeds, PlanGraph } from "./graph";
import { PlanItemsEditor } from "./items";
import { itemsMarkdown, planMarkdown, planSteps, withoutTitle } from "./steps";
import { PlanSteps, stepsWords } from "./steps-view";
import "./plans.css";

const tagList = (s: string) =>
  s
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

/** The harnesses each machine offers. */
const kindsRes: Res<Record<string, KindInfo[]>> = { key: "harness/kinds", fetch: () => harnessApi.kindsAll(), opts: { pollMs: 60_000 } };

/** "gone" for a plan that isn't there: trying again won't bring it. */
const unlessGone = (e: unknown): "gone" => {
  if (e instanceof ApiError && e.status === 404) return "gone";
  throw e;
};

/** A plan; "gone" for one that isn't there (deleted, or a wrong link). */
const planRes = (id: string): Res<Plan | "gone"> => ({ key: `plans/${id}`, fetch: (s) => plansApi.get(id, s).catch(unlessGone) });

export default function PlanPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page plan-page">
      <Loadable label="the plan" fallback={<PlanHold />}>
        <PlanOrNone key={id} id={id} />
      </Loadable>
    </div>
  );
}

/** The plan, or word that there's none here. */
function PlanOrNone({ id }: { id: string }) {
  if (useRes(planRes(id)).data === "gone") return <NoPlan />;
  return <PlanEditor id={id} />;
}

function NoPlan() {
  return (
    <>
      <title>No plan · kiln</title>
      <SessionCrumbs />
      <EmptyState
        title="No plan here"
        action={
          <Button asChild size="sm" variant="secondary">
            <Link to="/agents">Agents</Link>
          </Button>
        }
      >
        It was deleted, or the link is wrong.
      </EmptyState>
    </>
  );
}

/** The plan while it loads: the way back, its title's and text's space. */
function PlanHold() {
  return (
    <>
      <title>Plan · kiln</title>
      <SessionCrumbs />
      <Hold height={420} label="Loading the plan" />
    </>
  );
}

/** The saved plan the text here started from: changes are measured against it. */
interface Base {
  key: string;
  title: string;
  body: string;
  items: string;
  tags: string;
}

const baseOf = (p: Plan): Base => ({ key: keyOf(p), title: p.title, body: p.body, items: JSON.stringify(p.items ?? []), tags: p.tags.join(", ") });

function PlanEditor({ id }: { id: string }) {
  const res = useRes(planRes(id));
  const plan = res.data === "gone" ? null : res.data;
  if (plan) warm(harnessSessionsRes, kindsRes);

  const [base, setBase] = React.useState<Base | null>(() => (plan ? baseOf(plan) : null));
  const [title, setTitle] = React.useState(base?.title ?? "");
  const [body, setBody] = React.useState(base?.body ?? "");
  const [items, setItems] = React.useState<PlanItem[]>(plan?.items ?? []);
  const [tags, setTags] = React.useState(base?.tags ?? "");
  const [preview, setPreview] = React.useState(true);
  // A plan is read first; its words and steps are changed on asking.
  const [editing, setEditing] = React.useState(false);
  // Steps picked to run just those (`?pick=2,3`: picked elsewhere, an agent's Plan tab, to run here).
  const [search] = useSearchParams();
  const [picked, setPicked] = React.useState<number[]>(() =>
    (search.get("pick") ?? "")
      .split(",")
      .map(Number)
      .filter((n) => Number.isInteger(n) && n > 0),
  );
  const [cameToRun] = React.useState(() => search.has("pick"));
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const take = (p: Plan) => {
    const b = baseOf(p);
    setBase(b);
    setTitle(b.title);
    setBody(b.body);
    setItems(p.items ?? []);
    setTags(b.tags);
  };
  const dirty = base !== null && (title !== base.title || body !== base.body || JSON.stringify(items) !== base.items || tagList(tags).join(",") !== tagList(base.tags).join(","));
  // A newer version saved elsewhere comes in, unless there are unsaved changes here.
  if (plan && keyOf(plan) !== base?.key && !dirty) take(plan);

  const steps = items.length ? items.map((it) => it.title || "Untitled step") : planSteps(body);
  // What each step waits for, as it'll run: as they say, or each the one before.
  const waits = effectiveNeeds(items.map((x) => x.needs));

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      const p = await plansApi.update(id, { title, body, items, tags: tagList(tags) });
      res.setData(() => p);
      take(p);
      invalidate("plans");
      setEditing(false);
      setPicked([]);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const discard = () => {
    if (plan) take(plan);
  };

  return (
    <>
      <title>{`${plan?.title ?? "Plan"} · kiln`}</title>
      <SessionCrumbs session={plan?.source?.session} />
      {plan && (
        <p className="plan-dim plan-meta">
          {plan.phase === "planning" && (
            <>
              <Shimmer on>Being planned</Shimmer> ·{" "}
            </>
          )}
          v{plan.version} · changed {relativeTime(plan.updated_at)}
          {plan.runs ? ` · run ${plan.runs}×` : ""}
          {plan.source?.agent ? ` · from ${plan.source.agent}${plan.source.machine ? ` on ${plan.source.machine}` : ""}` : ""}
          {plan.source?.goal && (
            <>
              {" · made from the goal "}
              <Link to={`/agents/${encodeURIComponent(plan.source.goal.agent)}`}>
                &ldquo;{plan.source.goal.text.length > 80 ? `${plan.source.goal.text.slice(0, 80)}…` : plan.source.goal.text}&rdquo;
              </Link>
              {", met once every step is"}
            </>
          )}
        </p>
      )}
      {plan && !editing ? (
        <>
          <h1 className="run-title">{plan.title}</h1>
          <div className="plan-actions">
            <Button size="sm" variant="secondary" onClick={() => setEditing(true)}>
              Edit
            </Button>
          </div>
          {withoutTitle(plan.body, plan.title).trim() && (
            <div className="plan-overview">
              <Prose text={withoutTitle(plan.body, plan.title)} />
            </div>
          )}
          <PlanSteps plan={plan} picked={picked} onPick={setPicked} onRun={() => document.getElementById("plan-run")?.scrollIntoView({ behavior: "smooth", block: "start" })} />
        </>
      ) : (
        <>
          <TextField label="Title" className="plan-title-field" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="From its text, or its first step, when left empty" />
          <div className="plan-edit" data-preview={preview}>
            <div className="plan-text">
              <label htmlFor="plan-body" className="field-label">
                {items.length ? "Overview" : "Plan"}
              </label>
              <textarea
                id="plan-body"
                className="plan-body"
                data-short={items.length ? "" : undefined}
                value={body}
                onChange={(e) => setBody(e.target.value)}
                placeholder={
                  items.length
                    ? "Why, and anything every step should know (optional)"
                    : "Markdown. Each item of a top-level list is a step:\n\n1. Read the failing test\n2. Fix the parser\n3. Run the suite\n\nOr add steps below: each with its description, examples, acceptance criteria and testing."
                }
                spellCheck
              />
              <div className="field-label plan-items-label">Steps</div>
              <PlanItemsEditor items={items} onChange={setItems} idPrefix="plan-item" />
            </div>
            {preview && (
              <div className="plan-preview">
                <div className="field-label">Preview</div>
                {body.trim() || items.length ? <Prose text={[body.trim(), itemsMarkdown(items)].filter(Boolean).join("\n\n")} /> : <p className="plan-dim">Nothing yet.</p>}
                <div className="field-label plan-steps-label">
                  {!steps.length
                    ? "No steps: it runs as one message"
                    : items.some((it) => it.needs?.length)
                      ? `${steps.length} steps, side by side where they don't wait for each other`
                      : `${steps.length} step${steps.length === 1 ? "" : "s"}, as they run one by one`}
                </div>
                {items.length > 1 ? (
                  <PlanGraph label="Its steps, as they'll run" steps={items.map((it, i) => ({ n: i + 1, title: it.title, needs: waits[i] ?? [] }))} />
                ) : (
                  steps.length > 0 && (
                    <ol className="plan-steps">
                      {steps.map((s, i) => (
                        <li key={i}>{s}</li>
                      ))}
                    </ol>
                  )
                )}
              </div>
            )}
          </div>
          <label className="plan-check">
            <input type="checkbox" className="check" checked={preview} onChange={(e) => setPreview(e.target.checked)} /> Show the preview and steps
          </label>
          <TextField label="Tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="release, parser" hint="Separated by commas." />
          <div className="plan-actions">
            <Button size="sm" disabled={!dirty || busy || (!body.trim() && items.length === 0)} onClick={() => void save()}>
              {busy ? "Saving…" : "Save changes"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                discard();
                setEditing(false);
              }}
            >
              {dirty ? "Discard changes" : "Done"}
            </Button>
            {error && <span className="plan-error">{error}</span>}
          </div>
        </>
      )}
      {plan?.research && (
        <section className="plan-section" aria-label="Research">
          <h2 className="plan-h">Research</h2>
          <ResearchPanel id={plan.research} live={plan.phase === "planning"} />
        </section>
      )}
      {plan && (
        <>
          {plan.phase === "planning" ? (
            <section className="plan-section" aria-label="Run the plan">
              <h2 className="plan-h">Run it</h2>
              <p className="plan-dim">Being planned: its agent is researching it and writing its steps. It runs once the agent says it&rsquo;s ready.</p>
            </section>
          ) : (
            <Loadable label="where it can run" fallback={<RunHold />}>
              <RunPlan plan={plan} dirty={dirty} picked={picked} onPicked={setPicked} onRan={() => invalidate(`plans/${plan.id}`)} focus={cameToRun} />
            </Loadable>
          )}
          <SharePlan plan={plan} onChange={(p) => res.setData(() => p)} />
          <Loadable label="its versions and runs" fallback={<RowsHold rows={2} rowHeight={44} />}>
            <Versions
              plan={plan}
              onRestored={(p) => {
                res.setData(() => p);
                take(p);
                invalidate("plans");
              }}
            />
            <Runs plan={plan} />
          </Loadable>
          <DeletePlan plan={plan} />
        </>
      )}
    </>
  );
}

const hostOf = (url: string) => {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
};

const keyOf = (p: Plan) => `${p.id}:${p.version}:${p.tags.join(",")}`;

function machineOf(target: string) {
  return target === "local" ? "this server" : target.replace(/^agent:/, "");
}

function RunHold() {
  return (
    <section className="plan-section" aria-label="Run the plan">
      <h2 className="plan-h">Run it</h2>
      <Hold height={128} />
    </section>
  );
}

/** Run the plan on an agent. */
function RunPlan({ plan, dirty, picked, onPicked, onRan, focus }: { plan: Plan; dirty: boolean; picked: number[]; onPicked: (n: number[]) => void; onRan: () => void; focus?: boolean }) {
  warm(harnessSessionsRes, kindsRes);
  const agents = useRes(harnessSessionsRes).data;
  const kinds = useRes(kindsRes).data;
  // On the agent it came from, while that agent runs; else a new one.
  const [where, setWhere] = React.useState(() => {
    const from = plan.source?.session;
    return from && agents.some((a) => a.id === from && a.status !== "exited") ? from : "new";
  });
  const [target, setTarget] = React.useState("local");
  const [kindChoice, setKind] = React.useState("");
  const [cwd, setCwd] = React.useState("");
  const [modeChoice, setMode] = React.useState<PlanMode>("message");
  // As goals, each step a goal stack of its own: its acceptance criteria and tests, a goal each.
  const [split, setSplit] = React.useState(true);
  const [busy, setBusy] = React.useState(false);
  const [ran, setRan] = React.useState<PlanRan | null>(null);
  const [started, setStarted] = React.useState<PlanStarted | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  const live = agents.filter((a) => a.status !== "exited");
  const targets = Object.keys(kinds).sort((a, b) => (a === "local" ? -1 : b === "local" ? 1 : a.localeCompare(b)));
  const offered = (kinds[target] ?? []).filter((k) => k.available);
  // A harness the machine doesn't offer gives way to the first it does; a plan without steps runs as one message.
  const kind = offered.some((k) => k.id === kindChoice) ? kindChoice : (offered[0]?.id ?? "");
  // Steps picked: just those run, never as one message (an agent each, unless asked otherwise).
  const some = picked.length > 0;
  const mode: PlanMode = plan.steps.length === 0 ? "message" : some && modeChoice === "message" ? "agents" : modeChoice;
  const count = some ? picked.length : plan.steps.length;
  const checks = plan.items.some((it) => it.acceptance.length || it.testing.length);
  // Steps that say what they run after run as that says, side by side where they don't wait for each other.
  const graph = plan.items.some((it) => it.needs?.length);
  // One agent per step starts agents of its own.
  const fresh = where === "new" || mode === "agents";

  const run = async () => {
    setBusy(true);
    setError(null);
    setRan(null);
    setStarted(null);
    try {
      const to = { target, kind, cwd: cwd.trim() || undefined };
      const only = some ? picked : undefined;
      if (mode === "agents") setStarted(await plansApi.runAgents(plan.id, to, { only }));
      else setRan(await plansApi.run(plan.id, where === "new" ? { new: to } : { session: where }, mode, { only, split: mode === "goals" && checks && split }));
      onPicked([]);
      onRan();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="plan-section" id="plan-run" aria-label="Run the plan" ref={focus ? (el) => el?.scrollIntoView({ block: "start" }) : undefined}>
      <h2 className="plan-h">{some ? `Run ${stepsWords(picked)}` : "Run it"}</h2>
      {some && (
        <p className="plan-dim plan-meta-line">
          Just {picked.length === 1 ? "that step" : "those steps"}, each keeping its number; the steps {picked.length === 1 ? "it waits" : "they wait"} for outside{" "}
          {picked.length === 1 ? "it" : "them"} are taken as done.{" "}
          <button type="button" className="sv-link" onClick={() => onPicked([])}>
            Run the whole plan instead
          </button>
        </p>
      )}
      <div className="plan-run-fields">
        {mode !== "agents" && (
          <SelectField
            label="On"
            value={where}
            onChange={setWhere}
            options={[{ value: "new", label: "A new agent" }, ...live.map((a) => ({ value: a.id, label: `${a.name} · ${a.kind_name} · ${machineOf(a.target)}` }))]}
          />
        )}
        {fresh && (
          <>
            <SelectField label="Machine" value={target} onChange={setTarget} options={(targets.length ? targets : ["local"]).map((t) => ({ value: t, label: machineOf(t) }))} />
            <SelectField
              label="Harness"
              value={kind}
              onChange={setKind}
              options={offered.length ? offered.map((k) => ({ value: k.id, label: k.name })) : [{ value: "", label: "None on that machine" }]}
            />
            <TextField label="Folder" value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="Its home folder when empty" spellCheck={false} />
          </>
        )}
        <SelectField
          label="As"
          value={mode}
          onChange={(v) => setMode(v as PlanMode)}
          options={[
            ...(some ? [] : [{ value: "message", label: "One message" }]),
            ...(plan.steps.length
              ? [
                  { value: "steps", label: `Step by step (${count} message${count === 1 ? "" : "s"}, each after the last turn ends)` },
                  { value: "goals", label: `Goals on that agent's goal stack (${count}, each once the one before is met)` },
                  { value: "agents", label: `An agent each (${count}, each once ${graph ? "the steps it runs after are" : "the one before is"} met)` },
                ]
              : []),
          ]}
        />
      </div>
      {mode === "goals" && checks && (
        <label className="plan-check">
          <input type="checkbox" className="check" checked={split} onChange={(e) => setSplit(e.target.checked)} /> Each step as a goal stack of its own: its acceptance criteria and tests, a goal each,
          before it
        </label>
      )}
      {mode === "agents" && (
        <p className="plan-dim plan-meta-line">
          {graph
            ? "Each step gets an agent of its own, held to the step as a goal, and starts once the steps it runs after are met: steps that don’t wait for each other run side by side, in one folder, so each keeps to its own files."
            : "Each step gets an agent of its own, held to the step as a goal; the next starts once it’s met. To run steps side by side, say what each runs after."}
        </p>
      )}
      <div className="plan-actions">
        <Button size="sm" variant="secondary" disabled={busy || dirty || (fresh && !kind)} onClick={() => void run()}>
          {busy ? (mode === "agents" ? "Starting…" : "Sending…") : some ? `Run ${picked.length === 1 ? `step ${picked[0]}` : `${picked.length} steps`}` : "Run plan"}
        </Button>
        {dirty && <span className="plan-dim">Save your changes first: a run sends the saved version.</span>}
        {error && <span className="plan-error">{error}</span>}
        {ran && (
          <span className="plan-ok">
            {mode === "goals"
              ? `${ran.sent} goal${ran.sent === 1 ? "" : "s"} lined up${ran.split ? `, ${ran.split} as goal stacks of their criteria and tests` : ""}`
              : `Sent ${ran.sent === 1 ? "the plan" : `${ran.sent} messages`}`}{" "}
            · <Link to={`/agents/${encodeURIComponent(ran.session)}`}>open the agent</Link>
          </span>
        )}
        {started && (
          <span className="plan-ok">
            {started.steps} step{started.steps === 1 ? "" : "s"}, an agent each · <Link to={runHref(started.run)}>follow the run</Link>
            {started.note ? ` · ${started.note}` : ""}
          </span>
        )}
      </div>
    </section>
  );
}

/** Share by a link, stop sharing, or export the markdown. */
function SharePlan({ plan, onChange }: { plan: Plan; onChange: (p: Plan) => void }) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  // The address it is opened at, or another the server answers on (its front doors).
  const me = useAuth((s) => (s.kind === "authed" ? s.me : null));
  const [base, setBase] = React.useState(window.location.origin);
  const bases = [...new Set([window.location.origin, ...(me?.origins ?? []), ...(me?.public_url ? [me.public_url.replace(/\/+$/, "")] : [])])];
  const toggle = async (on: boolean) => {
    setBusy(true);
    setError(null);
    try {
      onChange(await plansApi.share(plan.id, on));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const exportMd = () => {
    const blob = new Blob([planMarkdown(plan.title, plan.body, plan.items ?? [])], { type: "text/markdown" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `${plan.title.replace(/[^\w.-]+/g, "-").replace(/^-+|-+$/g, "") || "plan"}.md`;
    a.click();
    URL.revokeObjectURL(a.href);
  };
  return (
    <section className="plan-section" aria-label="Share the plan">
      <h2 className="plan-h">Share it</h2>
      <label className="plan-check">
        <input type="checkbox" className="check" checked={Boolean(plan.share)} disabled={busy} onChange={(e) => void toggle(e.target.checked)} /> Anyone with the link can read it
      </label>
      {plan.share && (
        <>
          {bases.length > 1 && <SelectField className="plan-share-base" label="Link through" value={base} onChange={setBase} options={bases.map((b) => ({ value: b, label: b }))} />}
          <div className="plan-link">
            <code>{shareUrl(plan.share, base)}</code>
            <CopyButton text={shareUrl(plan.share, base)} label="Copy link" />
          </div>
          <p className="plan-dim plan-reach">
            It opens without signing in for anyone who can reach {hostOf(base)}: usually someone on your network or tailnet
            {base.startsWith("https://") && (me?.origins ?? []).includes(base) ? ", on a device that trusts kiln's certificate" : ""}, unless you give kiln a public address.
          </p>
        </>
      )}
      <div className="plan-actions">
        <Button size="xs" variant="ghost" onClick={exportMd}>
          Export as markdown
        </Button>
        {error && <span className="plan-error">{error}</span>}
      </div>
    </section>
  );
}

/** Every version, newest first; any one can be read and made current again. */
function Versions({ plan, onRestored }: { plan: Plan; onRestored: (p: Plan) => void }) {
  const versions = useSuspenseResource(`plans/${plan.id}/versions`, (s) => plansApi.versions(plan.id, s)).data;
  const [open, setOpen] = React.useState<number | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  if (versions.length <= 1) return null;
  return (
    <section className="plan-section" aria-label="Versions">
      <h2 className="plan-h">Versions</h2>
      <ul className="plan-rows">
        {versions.map((v) => (
          <li key={v.version}>
            <div className="plan-row-line">
              <span className="plan-row-main">
                v{v.version} · {v.title}
              </span>
              <span className="plan-dim">
                {relativeTime(v.at)} · {v.by}
              </span>
              <Button size="xs" variant="ghost" onClick={() => setOpen(open === v.version ? null : v.version)}>
                {open === v.version ? "Hide" : "Read"}
              </Button>
              {v.version !== plan.version && (
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={async () => {
                    setError(null);
                    try {
                      onRestored(await plansApi.restore(plan.id, v.version));
                    } catch (e) {
                      setError(errorMessage(e));
                    }
                  }}
                >
                  Make current
                </Button>
              )}
            </div>
            {open === v.version && <Prose text={v.body} className="plan-version-body" />}
          </li>
        ))}
      </ul>
      {error && <p className="plan-error">{error}</p>}
    </section>
  );
}

function Runs({ plan }: { plan: Plan }) {
  const runs = useSuspenseResource(`plans/${plan.id}/runs`, (s) => plansApi.runs(plan.id, s)).data;
  const agents = useRes(harnessSessionsRes).data;
  if (runs.length === 0) return null;
  const nameOf = (session: string) => agents.find((a) => a.id === session)?.name ?? `agent ${session.slice(-6)}`;
  return (
    <section className="plan-section" aria-label="Runs">
      <h2 className="plan-h">Runs</h2>
      <ul className="plan-rows">
        {runs.map((r) => (
          <li key={r.id} className="plan-row-line">
            {r.mode === "agents" ? (
              <Link to={runHref(r.id)} className="plan-row-main">
                One agent per step · v{r.version} · {r.state ?? "running"}
              </Link>
            ) : (
              <Link to={`/agents/${encodeURIComponent(r.session)}`} className="plan-row-main">
                {r.mode === "steps" ? "Step by step" : r.mode === "goals" ? "As goals" : "One message"} · v{r.version} · on {nameOf(r.session)}
              </Link>
            )}
            <span className="plan-dim">
              {relativeTime(r.at)} · {r.by}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}

function DeletePlan({ plan }: { plan: Plan }) {
  const navigate = useNavigate();
  const [asking, setAsking] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  return (
    <section className="plan-section" aria-label="Delete the plan">
      {!asking ? (
        <Button size="xs" variant="ghost" className="plan-delete" onClick={() => setAsking(true)}>
          Delete plan
        </Button>
      ) : (
        <Confirm
          label="Confirm deleting the plan"
          tone="danger"
          actions={
            <>
              <Button
                size="xs"
                variant="danger"
                onClick={async () => {
                  try {
                    await plansApi.remove(plan.id);
                    invalidate("plans");
                    // Back to the plans of the session it was kept with.
                    navigate(plan.source?.session ? `/agents/${encodeURIComponent(plan.source.session)}?tab=plan` : "/agents", { replace: true });
                  } catch (e) {
                    setError(errorMessage(e));
                  }
                }}
              >
                Delete
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>
                Keep it
              </Button>
            </>
          }
        >
          <p>
            Delete &ldquo;{plan.title}&rdquo;, its {plan.version} version{plan.version === 1 ? "" : "s"} and its runs? Agents keep what they were sent.
          </p>
          {error && <p className="plan-error">{error}</p>}
        </Confirm>
      )}
    </section>
  );
}
