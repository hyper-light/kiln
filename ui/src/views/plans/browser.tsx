/**
 * An agent's Plans, in its Plan tab: the plan under way up top on its slate, as it moves (its to-do
 * list, each step where it stands, the one at work in the prism's light; else the plan it proposed),
 * and under it every plan kiln keeps, in brief, found by their words and facets (whose, where they
 * stand, their tags), a row each with a segment a step: opened in place, read in full, to run it
 * here, change it, or open its page. A plan is written here too. Everything stays in the page's flow.
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";
import { ApiError } from "@/api/client";
import { clientId } from "@/api/harness";
import { planHref, plansApi, type Plan, type PlanItem, type PlanMode, type PlanRan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen } from "@/components/ui/facet-box";
import { RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { Link } from "@/lib/router";
import { Tries } from "@/lib/tries";
import { cn } from "@/lib/utils";
import { Fold } from "@/views/harness/fold";
import { ApproveInstead, useWaitsOn, type Approved } from "./approve";
import { facetsOf, hasChosen, planDimensions, planMeta, planRow, useAsked } from "./browse";
import { StepBar } from "./graph";
import { PlanItemsEditor } from "./items";
import type { Stand } from "./status";
import "./plans.css";

/** How a plan runs on this agent (one agent per step starts agents of its own: from the plan's page). */
type HereMode = Exclude<PlanMode, "agents">;

const MODES: { value: HereMode; label: string }[] = [
  { value: "message", label: "One message: the whole plan" },
  { value: "steps", label: "Step by step: a step a turn" },
  { value: "goals", label: "Goals: each step a /goal, the next once it's met" },
];

/** Rows shown before "Show more", and how many more each time. */
const PAGE = 6;
const MORE = 20;

const unlessGone = (e: unknown): "gone" => {
  if (e instanceof ApiError && (e.status === 404 || e.status === 400)) return "gone";
  throw e;
};

/** The Plans section: `active`, the plan under way (its own words and graph), then every plan kept (with an agent `session`). */
export function PlansSection({ session, active }: { session?: string; active: React.ReactNode }) {
  return (
    <Fold id="plans" title="Plans">
      <div className="pb">
        <div className="pb-active">
          <p className="pb-eyebrow">Under way</p>
          {active}
        </div>
        {session && <Browser session={session} />}
      </div>
    </Fold>
  );
}

interface Draft {
  /** The plan it changes; none for a new one. */
  id?: string;
  title: string;
  body: string;
  items: PlanItem[];
}

function Browser({ session }: { session: string }) {
  const [typed, asked, type] = useAsked();
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [draft, setDraft] = React.useState<Draft | null>(null);
  // While the words asked for are read, what was found before stays.
  const words = React.useDeferredValue(asked);
  return (
    <div className="pb-browse">
      <p className="pb-eyebrow">Saved plans</p>
      <Loadable label="your plans" fallback={<RowsHold rows={3} rowHeight={64} />}>
        <Found session={session} words={words} typed={typed} type={type} chosen={chosen} onChosen={setChosen} draft={draft} onDraft={setDraft} />
      </Loadable>
    </div>
  );
}

function Found({
  session,
  words,
  typed,
  type,
  chosen,
  onChosen,
  draft,
  onDraft,
}: {
  session: string;
  words: string;
  typed: string;
  type: (w: string) => void;
  chosen: Chosen;
  onChosen: (c: Chosen) => void;
  draft: Draft | null;
  onDraft: (d: Draft | null) => void;
}) {
  const res = useSuspenseResource(`plans/briefs/${words}`, (s) => plansApi.briefs(words, s), { pollMs: 20_000 });
  const [open, setOpen] = React.useState<string | null>(null);
  const [more, setMore] = React.useState(0);
  const rows = React.useMemo(() => res.data.map((p) => planRow(p, session)), [res.data, session]);
  const dims = React.useMemo(() => planDimensions(rows), [rows]);
  const facets = React.useMemo(() => facetsOf(dims, rows, chosen), [dims, rows, chosen]);
  const shown = rows.filter((r) => hasChosen(dims, chosen, r));
  const limit = PAGE + more;
  const rest = shown.length - limit;
  const saved = (p: Plan) => {
    invalidate("plans/");
    onDraft(null);
    setOpen(p.id);
  };
  return (
    <>
      <FilterBar>
        <FacetBox label="Search plans" placeholder="Search plans by title, step or tag…" query={typed} onQuery={type} facets={facets} chosen={chosen} onChosen={onChosen} />
        <Button size="bar" variant="secondary" onClick={() => onDraft({ title: "", body: "", items: [] })}>
          New plan
        </Button>
      </FilterBar>
      {draft && !draft.id && <DraftForm session={session} draft={draft} onDraft={onDraft} onSaved={saved} />}
      {shown.length === 0 ? (
        <p className="in-none">{words || Object.values(chosen).some((c) => c.length) ? "No plan matches." : "No plans kept yet: write one with New plan, or keep the one it proposes."}</p>
      ) : (
        <ul className="pb-list">
          {shown.slice(0, limit).map((r) => {
            const n = r.p.step_count;
            // A segment a step, the plan being planned in the prism's light.
            const stands: Stand[] = Array.from({ length: Math.max(n, 1) }, () => (r.stand === "planning" ? "working" : "waiting"));
            const isOpen = open === r.p.id;
            return (
              <li key={r.p.id} className="pb-row" data-open={isOpen || undefined}>
                <button type="button" className="pb-head" aria-expanded={isOpen} onClick={() => setOpen(isOpen ? null : r.p.id)}>
                  <span className="pb-title">{r.p.title}</span>
                  <span className="pb-line">
                    <span className="pb-bar">
                      <StepBar stands={stands} />
                    </span>
                    <span className="pb-mono">{planMeta(r).join(" · ")}</span>
                  </span>
                  <ChevronRight size={14} className={cn("pb-chev hx-chev", isOpen && "rotate-90")} aria-hidden="true" />
                </button>
                {isOpen && (
                  <div className="pb-body">
                    <Loadable label="the plan" fallback={<RowsHold rows={2} rowHeight={28} />}>
                      <Recall id={r.p.id} session={session} draft={draft?.id === r.p.id ? draft : null} onDraft={onDraft} onSaved={saved} />
                    </Loadable>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
      {rest > 0 && (
        <div className="plan-actions">
          <Button size="xs" variant="ghost" onClick={() => setMore(more + MORE)}>
            {rest > MORE ? `Show ${MORE} more of ${rest}` : `Show ${rest} more`}
          </Button>
        </div>
      )}
    </>
  );
}

/**
 * A kept plan, opened in its row and read in full: what it does, and running it here, changing it, or
 * its page. One its agent's goals wait on your review of is approved instead (approve.tsx).
 */
function Recall({ id, session, draft, onDraft, onSaved }: { id: string; session: string; draft: Draft | null; onDraft: (d: Draft | null) => void; onSaved: (p: Plan) => void }) {
  const p = useSuspenseResource(`plans/${id}`, (s) => plansApi.get(id, s).catch(unlessGone)).data;
  const waits = useWaitsOn(id);
  const [approved, setApproved] = React.useState<Approved | null>(null);
  const [mode, setMode] = React.useState<HereMode>("message");
  const [busy, setBusy] = React.useState(false);
  const [ran, setRan] = React.useState<PlanRan | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  /** The run asked and its key, until it's sent (CP-T3.2). */
  const [tries] = React.useState(() => new Tries(clientId));
  if (p === "gone") return <p className="in-none">This plan is gone.</p>;
  if (draft) return <DraftForm session={session} draft={draft} onDraft={onDraft} onSaved={onSaved} />;
  const titles = p.items.length ? p.items.map((it) => it.title || "Untitled step") : p.steps;
  const n = titles.length;
  const run = async () => {
    setBusy(true);
    setError(null);
    setRan(null);
    try {
      const how = n ? mode : "message";
      // Asked again (its answer lost, or it failed), the same run under its key, never a second.
      const key = tries.keyFor(JSON.stringify({ plan: p.id, version: p.version, session, mode: how }));
      setRan(await plansApi.run(p.id, { session }, how, {}, key));
      tries.done();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  // Its overview: the words beside its written steps, or all of them when it has no steps.
  const overview = (p.items.length || !n ? p.body : "").trim();
  return (
    <div className="pb-recall">
      {overview && <p className="pb-overview">{overview}</p>}
      {n > 0 && (
        <ol className="pb-steps">
          {titles.slice(0, PAGE).map((t, i) => (
            <li key={i}>
              <span className="pb-step-n">{i + 1}</span>
              <span>{t}</span>
            </li>
          ))}
          {n > PAGE && <li className="pb-steps-more">and {n - PAGE} more</li>}
        </ol>
      )}
      {p.phase === "planning" ? (
        <p className="ph-meta">Being planned: its agent is researching it and writing its steps; it runs once it says it&rsquo;s ready.</p>
      ) : waits || approved ? (
        // Its review is at the top of this tab when it's this agent's own.
        <ApproveInstead plan={p} a={waits} approved={approved} onApproved={setApproved} here={waits?.id === session} />
      ) : (
        n > 0 && <SelectField label="Run as" value={mode} onChange={(v) => setMode(v as HereMode)} options={MODES} />
      )}
      <div className="plan-actions">
        {p.phase !== "planning" && !waits && !approved && (
          <Button size="sm" variant="secondary" disabled={busy} onClick={() => void run()}>
            {busy ? "Sending…" : "Run here"}
          </Button>
        )}
        <Button size="sm" variant="ghost" onClick={() => onDraft({ id: p.id, title: p.title, body: p.body, items: p.items })}>
          Edit
        </Button>
        <Button size="sm" variant="ghost" asChild>
          <Link to={planHref(p.id)}>Open</Link>
        </Button>
        <span className="ph-meta">v{p.version}</span>
      </div>
      {ran && (
        <p className="plan-ok">
          {ran.approved
            ? `Approved instead (its goals waited on your review of it): its ${ran.sent} step${ran.sent === 1 ? "" : "s"} go to it as its goal's parts.`
            : mode === "goals" && n
              ? `${ran.sent} goals lined up in its goal stack, below.`
              : `Sent ${ran.sent === 1 ? "the plan" : `${ran.sent} messages`}.`}
        </p>
      )}
      {error && <p className="plan-error">{error}</p>}
    </div>
  );
}

/** A plan written or changed here: its title, overview and steps (each with its description, examples, acceptance criteria and testing). */
function DraftForm({ session, draft, onDraft, onSaved }: { session: string; draft: Draft; onDraft: (d: Draft | null) => void; onSaved: (p: Plan) => void }) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const id = React.useId();
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      // Emptied, the title is taken from the plan (its overview, else its first step).
      const body = { title: draft.title.trim(), body: draft.body, items: draft.items };
      onSaved(draft.id ? await plansApi.update(draft.id, body) : await plansApi.create({ ...body, source: { session } }));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="ph">
      <div>
        <label className="field-label" htmlFor={`${id}-title`}>
          Title
        </label>
        <div className="field">
          <input id={`${id}-title`} value={draft.title} placeholder="From the overview, or the first step, when left empty" onChange={(e) => onDraft({ ...draft, title: e.target.value })} />
        </div>
      </div>
      <div>
        <label className="field-label" htmlFor={`${id}-body`}>
          Overview (optional)
        </label>
        <div className="field">
          <textarea id={`${id}-body`} rows={2} value={draft.body} placeholder="Why, and anything every step should know" onChange={(e) => onDraft({ ...draft, body: e.target.value })} />
        </div>
      </div>
      <div>
        <div className="field-label">Steps</div>
        <PlanItemsEditor items={draft.items} onChange={(items) => onDraft({ ...draft, items })} idPrefix={`pb-${session}`} />
      </div>
      <div className="plan-actions">
        <Button size="sm" disabled={busy || (!draft.items.length && !draft.body.trim())} onClick={() => void save()}>
          {busy ? "Saving…" : draft.id ? "Save changes" : "Save plan"}
        </Button>
        <Button size="sm" variant="ghost" onClick={() => onDraft(null)}>
          Cancel
        </Button>
        {error && <span className="plan-error">{error}</span>}
      </div>
    </div>
  );
}
