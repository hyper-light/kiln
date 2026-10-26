/**
 * Deploy plans: what a branch, a commit or a pull request would deploy before it lands
 * (docs/cd.md, "Stacks, diffs and pull requests"). Choose the repository and what to plan; each
 * commit, in the order they land, lists the apps it changes. One select chooses what every app
 * shows: its own change (against the commit below), the stack's change (against trunk), or the
 * steps a sync would take against what runs. What is planned lives in the URL, so a plan can be
 * passed on as a link.
 */
import * as React from "react";
import { Link, useSearchParams } from "@/lib/router";
import { GitBranch } from "lucide-react";

import { appHref, cdApi } from "@/api/cd";
import type { CdPlan, CdPlanApp, CdPlanAsk, CdPlanChanges, CdPlanDiff, CdPlanObject, CdPlanSync } from "@/api/types";
import { Button } from "@/components/ui/button";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, SelectField, TextField } from "@/components/ui/misc";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { FileDiffView } from "@/views/harness/diff";
import { RelTime } from "@/views/shared";
import { StatePair, objectKey, objectLabel } from "./parts";
import "./deployments.css";

type Mode = "head" | "commit" | "pr";
type Show = "own" | "stack" | "sync";

const MODES: { value: Mode; label: string }[] = [
  { value: "head", label: "A branch, commit by commit" },
  { value: "commit", label: "One commit" },
  { value: "pr", label: "A pull request" },
];

const SHOWS: { value: Show; label: string }[] = [
  { value: "own", label: "Each commit's own change" },
  { value: "stack", label: "The stack's change, against trunk" },
  { value: "sync", label: "What a sync would do" },
];

const FIELD: Record<Mode, { label: string; placeholder: string }> = {
  head: { label: "Branch", placeholder: "feature/checkout" },
  commit: { label: "Commit", placeholder: "a hash, branch or tag" },
  pr: { label: "Pull request", placeholder: "128" },
};

const short = (sha: string | null | undefined) => (sha ? sha.slice(0, 10) : "");
const plural = (n: number, one: string, many = `${one}s`) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

/** "+1 ~2 −0": what one render adds, changes and removes against another. */
const changes = (c: CdPlanChanges | null) => (c ? `+${c.added} ~${c.changed} −${c.removed}` : "unknown");

/** "create 1, update 2", or that a sync changes nothing. */
function syncWords(s: CdPlanSync | null) {
  if (!s) return "unknown";
  const parts = (
    [
      ["create", s.create],
      ["update", s.update],
      ["prune", s.prune],
      [s.hooks === 1 ? "hook" : "hooks", s.hooks],
    ] as const
  ).flatMap(([w, n]) => (n ? [`${w} ${n}`] : []));
  return parts.length ? parts.join(", ") : "nothing to do";
}

/** How the app takes the change once it lands. */
function landing(a: CdPlanApp) {
  if (a.shadow) return "shadowed: Argo CD deploys it";
  if (a.auto) return a.auto_prune ? "syncs when it lands, and prunes" : "syncs when it lands";
  return "synced by hand";
}

/** What happens to an object: "update · .spec.replicas differs", "hook · PreSync". A line of its
 * own, since a diff's header shortens its label from the left on a phone. */
function verb(o: CdPlanObject) {
  const where = o.step && (o.phase !== "Sync" || (o.wave ?? 0) !== 0) ? `${o.phase}${o.wave ? `, wave ${o.wave}` : ""}` : "";
  return [o.change ?? o.step ?? "", where, o.why ?? ""].filter(Boolean).join(" · ");
}

function Objects({ list, held }: { list: CdPlanObject[]; held?: CdPlanSync["held"] }) {
  if (list.length === 0 && !held?.length) return null;
  return (
    <div className="cd-diffs cd-plan-objects">
      {list.map((o) => (
        <div key={`${o.step ?? o.change}-${o.hook}-${objectKey(o.key)}`} className="cd-diff">
          <p className="cd-why cd-plan-verb" data-what={o.step ?? o.change}>
            {verb(o)}
          </p>
          {o.hunks ? (
            <FileDiffView diff={{ path: objectLabel(o.key), hunks: o.hunks }} label={objectLabel(o.key)} defaultOpen={list.length <= 3} language="yaml" />
          ) : (
            <p className="cd-plan-line">
              <span>{objectLabel(o.key)}</span>
              <span className="cd-mono">
                +{o.added_lines} −{o.removed_lines} lines, too long to show here
              </span>
            </p>
          )}
        </div>
      ))}
      {held?.map((h) => (
        <p key={`held-${objectKey(h.key)}`} className="cd-plan-line">
          <span>stays {objectLabel(h.key)}</span>
          <span className="cd-dim">{h.prune === "false" ? "kept by Prune=false" : "pruned once a person confirms (Prune=confirm)"}</span>
        </p>
      ))}
    </div>
  );
}

function AppPlan({ a, show }: { a: CdPlanApp; show: Show }) {
  const list = show === "own" ? (a.own?.objects ?? []) : show === "stack" ? (a.stack?.objects ?? []) : (a.sync?.steps ?? []);
  const nothing =
    show === "sync" ? (a.sync && a.sync.steps.length === 0 && a.sync.held.length === 0 ? "A sync changes nothing in the cluster." : null) : list.length === 0 && (show === "own" ? a.own : a.stack) ? "Its manifests are the same." : null;
  return (
    <div className="cd-plan-app" data-tone={a.ok ? undefined : "fail"}>
      <div className="cd-plan-app-head">
        <Link to={appHref(a.app)} className="cd-plan-app-name">
          {a.app}
        </Link>
        {!a.ok && <StatePair tone="failed" word="Fails" />}
        <span className="cd-dim">
          {a.cluster}/{a.namespace} · {landing(a)}
        </span>
      </div>
      {!a.error && (
        <p className="cd-mono cd-plan-counts">
          own {changes(a.own)} · stack {changes(a.stack)} · sync: {syncWords(a.sync)}
        </p>
      )}
      {a.problems.map((p) => (
        <p key={p} className="cd-notice" data-tone="fail">
          {p}
        </p>
      ))}
      {a.notes.map((n) => (
        <p key={n} className="cd-plan-note">
          {n}
        </p>
      ))}
      {nothing ? <p className="cd-dim cd-plan-nothing">{nothing}</p> : <Objects list={list} held={show === "sync" ? a.sync?.held : undefined} />}
    </div>
  );
}

function DiffPlan({ d, show }: { d: CdPlanDiff; show: Show }) {
  // Its own change is about the apps it touches; the stack and a sync, about every app it differs in.
  const shown = show === "own" ? d.apps.filter((a) => a.touched || !a.ok) : d.apps;
  const left = show === "own" ? d.apps.filter((a) => !a.touched && a.ok) : [];
  return (
    <li className="cd-plan-diff" data-tone={d.ok ? undefined : "fail"}>
      <div className="cd-plan-commit">
        <span className="cd-mono">{short(d.commit.sha)}</span>
        <span className="cd-plan-subject">{d.commit.subject || "(no message)"}</span>
        <StatePair tone={d.ok ? "succeeded" : "failed"} word={d.ok ? "Passes" : "Fails"} />
        <span className="cd-dim">
          {d.commit.author}
          {d.commit.at_ms ? (
            <>
              {" · "}
              <RelTime ts={d.commit.at_ms / 1000} />
            </>
          ) : null}
        </span>
      </div>
      {shown.length === 0 && left.length === 0 ? (
        <p className="cd-dim cd-plan-nothing">Changes no app.</p>
      ) : (
        shown.map((a) => <AppPlan key={a.app} a={a} show={show} />)
      )}
      {left.length > 0 && (
        <p className="cd-dim cd-plan-nothing">
          Leaves {left.map((a) => a.app).join(", ")} as the commit below has {left.length === 1 ? "it" : "them"}.
        </p>
      )}
    </li>
  );
}

function PlanView({ plan, show, onShow, onAgain, busy }: { plan: CdPlan; show: Show; onShow: (s: Show) => void; onAgain: () => void; busy: boolean }) {
  const n = plan.diffs.length;
  const line = [
    `${plural(n, "commit")} onto ${plan.trunk.branch} (${short(plan.trunk.commit)})`,
    `${plural(plan.apps.affected, "app")} touched`,
    plan.apps.unaffected ? `${plan.apps.unaffected.toLocaleString()} more left alone` : "",
  ]
    .filter(Boolean)
    .join(" · ");
  return (
    <section className="cd-plan" aria-label="Plan">
      <div className="cd-plan-bar">
        <span className="cd-state">
          <StatePair tone={plan.ok ? "succeeded" : "failed"} word={plan.ok ? "Passes" : "Fails"} />
          <span className="cd-meta">{line}</span>
        </span>
        <div className="cd-plan-bar-end">
          <SelectField label="Show" hideLabel value={show} onChange={(v) => onShow(v as Show)} options={SHOWS} className="cd-plan-show" />
          <Button size="xs" variant="secondary" onClick={onAgain} disabled={busy}>
            {busy ? "Planning…" : "Plan again"}
          </Button>
        </div>
      </div>
      {plan.apps.unplanned.length > 0 && (
        <p className="cd-notice" data-tone="fail">
          {plural(plan.apps.unplanned.length, "more app")} touched but not planned (a plan renders at most {plan.apps.planned}).
        </p>
      )}
      {plan.notes.map((x) => (
        <p key={x} className="cd-plan-note">
          {x}
        </p>
      ))}
      {n > 1 && <p className="cd-count cd-plan-order">Bottom first, in the order they land</p>}
      <ol className="cd-plan-diffs">
        {plan.diffs.map((d) => (
          <DiffPlan key={d.commit.sha} d={d} show={show} />
        ))}
      </ol>
    </section>
  );
}

function PlansHead() {
  return (
    <header className="cd-head">
      <Crumbs index={3} items={[{ label: "Deployments", to: "/deployments" }, { label: "Deploy plans" }]} />
      <h1 className="cd-title">Deploy plans</h1>
      <p className="cd-note">
        What a branch, a commit or a pull request would deploy before it lands: each commit's own change, the whole stack's against trunk, and what a sync would do against what runs.
      </p>
    </header>
  );
}

export default function DeployPlans() {
  return (
    <div className="container-console page cd-page">
      <title>Deploy plans · kiln</title>
      <PlansHead />
      <Loadable label="the repositories" fallback={<FormHold />}>
        <PlanForm />
      </Loadable>
    </div>
  );
}

/** What is planned, as the URL says it. */
function useAsk() {
  const [params, setParams] = useSearchParams();
  const mode: Mode = params.has("commit") ? "commit" : params.has("pr") ? "pr" : "head";
  const target = (params.get(mode) ?? "").trim();
  const repo = params.get("repo") ?? "";
  const show = (SHOWS.find((x) => x.value === params.get("show"))?.value ?? "own") as Show;
  return { params, setParams, mode, target, repo, show };
}

function PlanForm() {
  const { params, setParams, mode, target, repo, show } = useAsk();
  const list = useSuspenseResource("cd/plans/repos", (s) => cdApi.planRepos(s)).data.repos;
  const [form, setForm] = React.useState({ repo, mode, target });
  // The page's own URL changes (back, a link) carry into the form.
  const [seen, setSeen] = React.useState({ repo, mode, target });
  if (seen.repo !== repo || seen.mode !== mode || seen.target !== target) {
    setSeen({ repo, mode, target });
    setForm({ repo, mode, target });
  }
  const [planning, startPlanning] = React.useTransition();
  const chosen = list.find((r) => r.url === form.repo) ?? (list.length === 1 ? list[0] : undefined);
  const branches = (chosen?.branches ?? []).filter((b) => b.name !== chosen?.default_branch);

  const ask: CdPlanAsk | null = target ? { repo: repo || undefined, ...(mode === "pr" ? { pr: Number(target) } : { [mode]: target }) } : null;
  const askKey = JSON.stringify(ask);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    const t = form.target.trim();
    if (!t) return;
    const next = new URLSearchParams();
    const r = form.repo || (list.length === 1 ? (list[0]?.url ?? "") : "");
    if (r) next.set("repo", r);
    next.set(form.mode, form.mode === "pr" ? t.replace(/^#/, "") : t);
    if (show !== "own") next.set("show", show);
    // The same plan asked for again is planned again.
    if (next.toString() === params.toString()) invalidate(`cd/plan/${askKey}`);
    else startPlanning(() => setParams(next));
  };
  const setShow = (s: Show) => {
    const next = new URLSearchParams(params);
    if (s === "own") next.delete("show");
    else next.set("show", s);
    setParams(next, { replace: true });
  };
  const listId = React.useId();
  const field = FIELD[form.mode];

  return (
    <>
      <form className="cd-plan-form" onSubmit={submit} aria-label="What to plan">
        {list.length > 1 && (
          <SelectField
            label="Repository"
            value={form.repo}
            onChange={(v) => setForm({ ...form, repo: v })}
            options={[{ value: "", label: "Whichever has it" }, ...list.map((r) => ({ value: r.url, label: `${r.url} · ${plural(r.apps, "app")}` }))]}
            className="cd-plan-repo"
          />
        )}
        <SelectField label="Plan" value={form.mode} onChange={(v) => setForm({ ...form, mode: v as Mode })} options={MODES} className="cd-plan-mode" />
        <TextField
          label={field.label}
          value={form.target}
          onChange={(e) => setForm({ ...form, target: e.target.value })}
          placeholder={field.placeholder}
          list={form.mode === "head" && branches.length ? listId : undefined}
          inputMode={form.mode === "pr" ? "numeric" : undefined}
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          className="cd-plan-target"
        />
        {form.mode === "head" && branches.length > 0 && (
          <datalist id={listId}>
            {branches.map((b) => (
              <option key={b.name} value={b.name} />
            ))}
          </datalist>
        )}
        <Button type="submit" size="sm" disabled={!form.target.trim() || planning} className="cd-plan-go">
          Plan
        </Button>
      </form>
      {list.length === 1 && list[0] && <p className="cd-meta cd-plan-from">From {list[0].url}</p>}

      {!ask ? (
        <EmptyState icon={<GitBranch size={20} />} title="Nothing planned yet.">
          Name a branch, a commit or a pull request to see what each of its commits would change in every app it touches, before it lands. From a checkout: <code>kiln cd plan --stack</code>.
        </EmptyState>
      ) : (
        <Loadable label="the plan" fallback={<RowsHold rows={6} rowHeight={56} label="Planning" />}>
          <AskedPlan ask={ask} askKey={askKey} show={show} onShow={setShow} planning={planning} />
        </Loadable>
      )}
    </>
  );
}

/** The plan asked for: planned once per ask, and again when asked again. */
function AskedPlan({ ask, askKey, show, onShow, planning }: { ask: CdPlanAsk; askKey: string; show: Show; onShow: (s: Show) => void; planning: boolean }) {
  const plan = useSuspenseResource(`cd/plan/${askKey}`, (s) => cdApi.plan(ask, s));
  return <PlanView plan={plan.data} show={show} onShow={onShow} onAgain={plan.reload} busy={plan.refreshing || planning} />;
}

/** The form while the repositories load: its fields' space. */
function FormHold() {
  const { mode, target } = useAsk();
  const field = FIELD[mode];
  const noop = () => undefined;
  return (
    <form className="cd-plan-form" aria-label="What to plan" aria-busy="true">
      <SelectField label="Plan" value={mode} onChange={noop} options={MODES} className="cd-plan-mode" />
      <TextField label={field.label} value={target} readOnly placeholder={field.placeholder} className="cd-plan-target" />
      <Button type="submit" size="sm" disabled className="cd-plan-go">
        Plan
      </Button>
    </form>
  );
}
