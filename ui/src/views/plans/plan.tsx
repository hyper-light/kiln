/**
 * One plan: written or changed in place (each saved change a version), its steps as they will run,
 * run on an agent (one that runs, or a new one; as one message or step by step), shared by a link,
 * exported, its versions and runs. Everything in the page's flow.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { api } from "@/api/client";
import { harnessApi } from "@/api/harness";
import { planHref, plansApi, shareUrl, type Plan, type PlanRan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { CopyButton, ErrorState, SelectField, SkeletonRows, TextField } from "@/components/ui/misc";
import { relativeTime } from "@/lib/format";
import { errorMessage, useDocumentTitle, useResource } from "@/lib/hooks";
import { useAgents } from "@/views/harness/use-agents";
import { Prose } from "@/views/harness/parts";
import { planMarkdown, planSteps } from "./steps";
import "./plans.css";

const tagList = (s: string) =>
  s
    .split(",")
    .map((t) => t.trim())
    .filter(Boolean);

export default function PlanPage() {
  const { id = "new" } = useParams();
  const isNew = id === "new";
  const navigate = useNavigate();
  const res = useResource((s) => (isNew ? Promise.resolve(null) : plansApi.get(id, s)), [id]);
  const plan = res.data ?? null;
  useDocumentTitle(plan ? plan.title : "New plan");

  const [title, setTitle] = React.useState("");
  const [body, setBody] = React.useState("");
  const [tags, setTags] = React.useState("");
  // The saved plan the text here started from: changes are measured against it.
  const [base, setBase] = React.useState<{ key: string; title: string; body: string; tags: string } | null>(null);
  const [preview, setPreview] = React.useState(true);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  const take = React.useCallback((p: Plan) => {
    const b = { key: keyOf(p), title: p.title, body: p.body, tags: p.tags.join(", ") };
    setBase(b);
    setTitle(b.title);
    setBody(b.body);
    setTags(b.tags);
  }, []);
  const dirty = isNew ? body.trim() !== "" || title.trim() !== "" : base !== null && (title !== base.title || body !== base.body || tagList(tags).join(",") !== tagList(base.tags).join(","));
  // Take the plan in when it arrives or changes, unless there are unsaved changes here.
  React.useEffect(() => {
    if (plan && keyOf(plan) !== base?.key && !dirty) take(plan);
  }, [plan, base, dirty, take]);

  const steps = planSteps(body);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      if (isNew) {
        const p = await plansApi.create({ title, body, tags: tagList(tags) });
        navigate(planHref(p.id), { replace: true });
      } else {
        const p = await plansApi.update(id, { title, body, tags: tagList(tags) });
        res.setData(() => p);
        take(p);
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  const discard = () => {
    if (plan) take(plan);
  };

  if (!isNew && res.error && !plan) return <ErrorState error={res.error} onRetry={res.reload} />;
  if (!isNew && !plan) return <SkeletonRows rows={4} cols={2} />;

  return (
    <div className="container-console page plan-page">
      <div className="plan-crumbs">
        <Link to="/agents/plans">Plans</Link>
        {plan && (
          <span>
            {" "}
            · v{plan.version} · changed {relativeTime(plan.updated_at)}
            {plan.runs ? ` · run ${plan.runs}×` : ""}
            {plan.source?.agent ? ` · from ${plan.source.agent}${plan.source.machine ? ` on ${plan.source.machine}` : ""}` : ""}
          </span>
        )}
      </div>
      <TextField label="Title" className="plan-title-field" value={title} onChange={(e) => setTitle(e.target.value)} placeholder={isNew ? "From the first heading when left empty" : ""} />
      <div className="plan-edit" data-preview={preview}>
        <div className="plan-text">
          <label htmlFor="plan-body" className="field-label">
            Plan
          </label>
          <textarea
            id="plan-body"
            className="plan-body"
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder={"Markdown. Each item of a top-level list is a step:\n\n1. Read the failing test\n2. Fix the parser\n3. Run the suite"}
            spellCheck
          />
        </div>
        {preview && (
          <div className="plan-preview">
            <div className="field-label">Preview</div>
            {body.trim() ? <Prose text={body} /> : <p className="plan-dim">Nothing yet.</p>}
            <div className="field-label plan-steps-label">{steps.length ? `${steps.length} step${steps.length === 1 ? "" : "s"}, as they run one by one` : "No steps: it runs as one message"}</div>
            {steps.length > 0 && (
              <ol className="plan-steps">
                {steps.map((s, i) => (
                  <li key={i}>{s}</li>
                ))}
              </ol>
            )}
          </div>
        )}
      </div>
      <label className="plan-check">
        <input type="checkbox" checked={preview} onChange={(e) => setPreview(e.target.checked)} /> Show the preview and steps
      </label>
      <TextField label="Tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="release, parser" hint="Separated by commas." />
      <div className="plan-actions">
        <Button size="sm" disabled={!dirty || busy || !body.trim()} onClick={() => void save()}>
          {busy ? "Saving…" : isNew ? "Save plan" : "Save changes"}
        </Button>
        {!isNew && dirty && (
          <Button size="sm" variant="ghost" onClick={discard}>
            Discard changes
          </Button>
        )}
        {error && <span className="plan-error">{error}</span>}
      </div>
      {plan && (
        <>
          <RunPlan plan={plan} dirty={dirty} onRan={res.reload} />
          <SharePlan plan={plan} onChange={(p) => res.setData(() => p)} />
          <Versions
            plan={plan}
            onRestored={(p) => {
              res.setData(() => p);
              take(p);
            }}
          />
          <Runs plan={plan} />
          <DeletePlan plan={plan} />
        </>
      )}
    </div>
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

/** Run the plan on an agent. */
function RunPlan({ plan, dirty, onRan }: { plan: Plan; dirty: boolean; onRan: () => void }) {
  const agents = useAgents();
  const kinds = useResource(() => harnessApi.kindsAll(), []);
  const [where, setWhere] = React.useState("new");
  const [target, setTarget] = React.useState("local");
  const [kind, setKind] = React.useState("");
  const [cwd, setCwd] = React.useState("");
  const [mode, setMode] = React.useState<"message" | "steps">("message");
  const [busy, setBusy] = React.useState(false);
  const [ran, setRan] = React.useState<PlanRan | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  const live = (agents.list ?? []).filter((a) => a.status !== "exited");
  const targets = Object.keys(kinds.data ?? {}).sort((a, b) => (a === "local" ? -1 : b === "local" ? 1 : a.localeCompare(b)));
  const offered = (kinds.data?.[target] ?? []).filter((k) => k.available);
  React.useEffect(() => {
    if (offered.length > 0 && !offered.some((k) => k.id === kind)) setKind(offered[0]?.id ?? "");
  }, [offered, kind]);
  React.useEffect(() => {
    if (plan.steps.length === 0) setMode("message");
  }, [plan.steps.length]);

  const run = async () => {
    setBusy(true);
    setError(null);
    setRan(null);
    try {
      const to = where === "new" ? { new: { target, kind, cwd: cwd.trim() || undefined } } : { session: where };
      setRan(await plansApi.run(plan.id, to, mode));
      onRan();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="plan-section" aria-label="Run the plan">
      <h2 className="plan-h">Run it</h2>
      <div className="plan-run-fields">
        <SelectField
          label="On"
          value={where}
          onChange={setWhere}
          options={[{ value: "new", label: "A new agent" }, ...live.map((a) => ({ value: a.id, label: `${a.name} · ${a.kind_name} · ${machineOf(a.target)}` }))]}
        />
        {where === "new" && (
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
          onChange={(v) => setMode(v as "message" | "steps")}
          options={[
            { value: "message", label: "One message" },
            ...(plan.steps.length ? [{ value: "steps", label: `Step by step (${plan.steps.length} messages, each after the last turn ends)` }] : []),
          ]}
        />
      </div>
      <div className="plan-actions">
        <Button size="sm" variant="secondary" disabled={busy || dirty || (where === "new" && !kind)} onClick={() => void run()}>
          {busy ? "Sending…" : "Run plan"}
        </Button>
        {dirty && <span className="plan-dim">Save your changes first: a run sends the saved version.</span>}
        {error && <span className="plan-error">{error}</span>}
        {ran && (
          <span className="plan-ok">
            Sent {ran.sent === 1 ? "the plan" : `${ran.sent} messages`} · <Link to={`/agents/${encodeURIComponent(ran.session)}`}>open the agent</Link>
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
  const me = useResource((s) => api.me(s), []);
  const [base, setBase] = React.useState(window.location.origin);
  const bases = [...new Set([window.location.origin, ...(me.data?.origins ?? []), ...(me.data?.public_url ? [me.data.public_url.replace(/\/+$/, "")] : [])])];
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
    const blob = new Blob([planMarkdown(plan.title, plan.body)], { type: "text/markdown" });
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
        <input type="checkbox" checked={Boolean(plan.share)} disabled={busy} onChange={(e) => void toggle(e.target.checked)} /> Anyone with the link can read it
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
            {base.startsWith("https://") && (me.data?.origins ?? []).includes(base) ? ", on a device that trusts kiln's certificate" : ""}, unless you give kiln a public address.
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
  const res = useResource((s) => plansApi.versions(plan.id, s), [plan.id, plan.version]);
  const [open, setOpen] = React.useState<number | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const versions = res.data ?? [];
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
  const res = useResource((s) => plansApi.runs(plan.id, s), [plan.id, plan.runs]);
  const agents = useAgents();
  const runs = res.data ?? [];
  if (runs.length === 0) return null;
  const nameOf = (session: string) => agents.list?.find((a) => a.id === session)?.name ?? `agent ${session.slice(-6)}`;
  return (
    <section className="plan-section" aria-label="Runs">
      <h2 className="plan-h">Runs</h2>
      <ul className="plan-rows">
        {runs.map((r) => (
          <li key={r.id} className="plan-row-line">
            <Link to={`/agents/${encodeURIComponent(r.session)}`} className="plan-row-main">
              {r.mode === "steps" ? "Step by step" : "One message"} · v{r.version} · on {nameOf(r.session)}
            </Link>
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
        <div className="plan-confirm" role="group" aria-label="Confirm deleting the plan">
          <p>
            Delete &ldquo;{plan.title}&rdquo;, its {plan.version} version{plan.version === 1 ? "" : "s"} and its runs? Agents keep what they were sent.
          </p>
          <div className="plan-actions">
            <Button
              size="xs"
              variant="danger"
              onClick={async () => {
                try {
                  await plansApi.remove(plan.id);
                  navigate("/agents/plans", { replace: true });
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
          </div>
          {error && <p className="plan-error">{error}</p>}
        </div>
      )}
    </section>
  );
}
