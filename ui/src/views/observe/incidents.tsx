/**
 * Incidents (docs/observe.md §9.5), Datadog's: each one declared by a person or by a monitor whose
 * message names @incident; its severity, state and commander, how long it has run; found by state,
 * by severity and by Datadog's search (`services:checkout`, a word of its title). One is declared
 * from here, the integrations it names told.
 */
import * as React from "react";
import { Plus } from "lucide-react";
import { observeApi, type Incident, type IncidentSearch } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, warm, type Res } from "@/lib/data";
import { formatDuration, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate } from "@/lib/router";
import { ObserveNav, integrationsRes, useParam, useParams } from "./parts";

const PAGE = 50;

export const SEVERITIES = ["SEV-0", "SEV-1", "SEV-2", "SEV-3", "SEV-4", "SEV-5", "UNKNOWN"] as const;
const SEVERITY_TONE: Record<string, string> = { "SEV-0": "failed", "SEV-1": "failed", "SEV-2": "timed_out", "SEV-3": "pending" };
const STATE_TONE: Record<string, string> = { active: "failed", stable: "timed_out", resolved: "succeeded" };
export const STATE_WORDS: Record<string, string> = { active: "Active", stable: "Stable", resolved: "Resolved" };

export const searchRes = (query: string, offset: number, size: number): Res<IncidentSearch> => ({
  key: `observe/incidents/search?${offset}/${size}/${query}`,
  fetch: (s) => observeApi.incidentSearch(query, offset, size, s),
  opts: { pollMs: 15_000 },
});

/** An incident's severity: its tone's dot and its word. */
export function Severity({ s }: { s: string }) {
  const tone = SEVERITY_TONE[s] ?? "skipped";
  return (
    <span className="status whitespace-nowrap">
      <i className="dot" data-status={tone} aria-hidden="true" />
      <span className="status-text font-mono" data-status={tone}>
        {s === "UNKNOWN" ? "Unknown" : s}
      </span>
    </span>
  );
}

/** An incident's state: its tone's dot and its word. */
export function IncidentState({ s }: { s: string }) {
  const tone = STATE_TONE[s] ?? "skipped";
  return (
    <span className="status whitespace-nowrap">
      <i className="dot" data-status={tone} aria-hidden="true" />
      <span className="status-text" data-status={tone}>
        {STATE_WORDS[s] ?? s}
      </span>
    </span>
  );
}

const seconds = (rfc3339: string | null) => (rfc3339 ? Date.parse(rfc3339) / 1000 : null);

/** How long it has run: to its resolving, or until now. */
export function ranFor(i: Incident, now = Date.now()) {
  const a = i.attributes;
  return a.time_to_resolve ?? Math.max(0, (now - Date.parse(a.created)) / 1000);
}

type Show = "active" | "stable" | "resolved" | "all";
const SHOWS: Show[] = ["active", "stable", "resolved", "all"];

/** A value as the search syntax takes it: quoted where it is more than a plain word. */
const term = (v: string) => (/^[\w.\-/:@]+$/.test(v) ? v : `"${v.replace(/"/g, "")}"`);

export default function Incidents() {
  const [show] = useParam("state", "active");
  const [severity] = useParam("severity", "");
  const [q] = useParam("q", "");
  const [offset, setOffset] = useParam("offset", "0");
  const set = useParams();
  const [declaring, setDeclaring] = React.useState(false);
  const shown: Show = SHOWS.includes(show as Show) ? (show as Show) : "active";
  const base = [severity ? `severity:${term(severity)}` : "", q.trim()].filter(Boolean).join(" AND ");
  const query = [shown === "all" ? "" : `state:${shown}`, base].filter(Boolean).join(" AND ");
  const from = Math.max(0, Number(offset) || 0);
  const onShow = (v: Show) => set({ state: v === "active" ? null : v, offset: null });
  warm(searchRes(base, 0, 1), searchRes(query, from, PAGE));
  return (
    <div className="container-console page">
      <title>Incidents · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Incidents"
        lead="Declared by people, or by a monitor whose message names @incident: each one's severity, state and commander, its timeline, todos and postmortem. The integrations an incident names are told when it is declared and as its state or severity changes."
        actions={
          <Button onClick={() => setDeclaring(true)}>
            <Plus /> Declare incident
          </Button>
        }
      />
      <ObserveNav />
      <div className="mb-4 flex flex-wrap items-end gap-3">
        <Loadable key={base} label="the incidents' counts" fallback={<StateTabs show={shown} onShow={onShow} />}>
          <Counted base={base} show={shown} onShow={onShow} />
        </Loadable>
        <SelectField
          label="Severity"
          hideLabel
          value={severity}
          onChange={(v) => set({ severity: v || null, offset: null })}
          options={[{ value: "", label: "Any severity" }, ...SEVERITIES.map((s) => ({ value: s, label: s === "UNKNOWN" ? "Unknown" : s }))]}
          className="min-w-[9rem]"
        />
        <Search key={q} q={q} onQ={(v) => set({ q: v || null, offset: null })} />
      </div>
      <Loadable key={`${query}/${from}`} label="the incidents" fallback={<TableHold head={<Head />} rows={4} />}>
        <List query={query} from={from} onFrom={(n) => setOffset(String(n))} filtered={Boolean(base)} show={shown} onDeclare={() => setDeclaring(true)} />
      </Loadable>
      {declaring && <DeclareDialog onClose={() => setDeclaring(false)} />}
    </div>
  );
}

/** The states to show, each with how many incidents of it match when counted. */
function StateTabs({ show, onShow, count }: { show: Show; onShow: (s: Show) => void; count?: (s: Show) => number }) {
  return <Segmented label="State" value={show} onChange={onShow} options={SHOWS.map((s) => ({ value: s, label: s === "all" ? "All" : STATE_WORDS[s], count: count?.(s) }))} />;
}

/** [`StateTabs`] counted: what `base` matches, by state. */
function Counted({ base, show, onShow }: { base: string; show: Show; onShow: (s: Show) => void }) {
  const { facets, total } = useRes(searchRes(base, 0, 1)).data;
  return <StateTabs show={show} onShow={onShow} count={(s) => (s === "all" ? total : (facets.state.find((f) => f.name === s)?.count ?? 0))} />;
}

function Search({ q, onQ }: { q: string; onQ: (q: string) => void }) {
  const [text, setText] = React.useState(q);
  return (
    <form
      className="field w-full sm:ml-auto sm:w-80"
      onSubmit={(e) => {
        e.preventDefault();
        onQ(text.trim());
      }}
    >
      <input aria-label="Search incidents" type="search" value={text} onChange={(e) => setText(e.target.value)} placeholder="services:checkout, or a word of the title" spellCheck={false} />
    </form>
  );
}

function Head() {
  return (
    <tr>
      <th>Incident</th>
      <th className="hidden sm:table-cell">Severity</th>
      <th className="hidden sm:table-cell">State</th>
      <th className="hidden md:table-cell">Commander</th>
      <th className="hidden text-right lg:table-cell">Open todos</th>
      <th className="hidden text-right md:table-cell">Ran</th>
    </tr>
  );
}

function List({ query, from, onFrom, filtered, show, onDeclare }: { query: string; from: number; onFrom: (n: number) => void; filtered: boolean; show: Show; onDeclare: () => void }) {
  const { incidents, total } = useRes(searchRes(query, from, PAGE)).data;
  if (total === 0)
    return (
      <p className="empty-line py-14 text-center">
        {filtered ? (
          "No incident matches."
        ) : show === "all" ? (
          <>
            No incidents yet.{" "}
            <button type="button" className="text-link" onClick={onDeclare}>
              Declare one
            </button>
            , or name <span className="font-mono">@incident</span> in a monitor's message to have it declare one when it alerts.
          </>
        ) : (
          `No ${STATE_WORDS[show]?.toLowerCase()} incident.`
        )}
      </p>
    );
  const now = Date.now();
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {incidents.map((i) => (
              <Row key={i.id} i={i} now={now} />
            ))}
          </tbody>
        </table>
      </div>
      {total > PAGE && (
        <div className="mt-3 flex flex-wrap items-center gap-2 text-[12.5px] text-[#8c8f98]">
          <span>
            {from + 1}–{Math.min(total, from + incidents.length)} of {total}
          </span>
          <Button variant="ghost" size="xs" disabled={from === 0} onClick={() => onFrom(Math.max(0, from - PAGE))}>
            Newer
          </Button>
          <Button variant="ghost" size="xs" disabled={from + PAGE >= total} onClick={() => onFrom(from + PAGE)}>
            Older
          </Button>
        </div>
      )}
    </>
  );
}

function Row({ i, now }: { i: Incident; now: number }) {
  const a = i.attributes;
  const commander = i.relationships.commander_user.data?.id ?? null;
  const by = a.kiln.monitor ? `by its monitor ${a.kiln.monitor.name}` : `by ${a.declared_by.name}`;
  return (
    <tr>
      <td className="truncate-cell">
        <Link to={`/observe/incidents/${a.public_id}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
          <span className="mr-1.5 font-mono text-[#8c8f98]">#{a.public_id}</span>
          {a.title}
        </Link>
        <p className="truncate text-[11px] text-[#8c8f98]">
          Declared {relativeTime(seconds(a.declared))} {by}
          {a.customer_impacted ? " · customers impacted" : ""}
          {a.is_test ? " · a test" : ""}
        </p>
        <p className="mt-0.5 flex gap-3 text-[11px] sm:hidden">
          <Severity s={a.severity} />
          <IncidentState s={a.state} />
        </p>
      </td>
      <td className="hidden text-[12px] sm:table-cell">
        <Severity s={a.severity} />
      </td>
      <td className="hidden text-[12px] sm:table-cell">
        <IncidentState s={a.state} />
      </td>
      <td className="hidden text-[12px] md:table-cell">{commander ?? <span className="text-[#8c8f98]">—</span>}</td>
      <td className="hidden whitespace-nowrap text-right font-mono text-[12px] lg:table-cell">
        {a.kiln.todos ? `${a.kiln.todos_open} of ${a.kiln.todos}` : <span className="text-[#8c8f98]">—</span>}
      </td>
      <td className="hidden text-right font-mono text-[12px] md:table-cell">{formatDuration(ranFor(i, now))}</td>
    </tr>
  );
}

/** An incident declared: its title, severity, commander, whether customers are impacted, the
 * integrations told, a first note; then its page. */
function DeclareDialog({ onClose }: { onClose: () => void }) {
  const navigate = useNavigate();
  const [title, setTitle] = React.useState("");
  const [severity, setSeverity] = React.useState("UNKNOWN");
  const [commander, setCommander] = React.useState("");
  const [impacted, setImpacted] = React.useState(false);
  const [scope, setScope] = React.useState("");
  const [handles, setHandles] = React.useState<string[]>([]);
  const [note, setNote] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const ready = title.trim() !== "" && (!impacted || scope.trim() !== "");
  const save = async () => {
    setBusy(true);
    try {
      const i = await observeApi.declareIncident(
        {
          title: title.trim(),
          severity,
          customer_impacted: impacted,
          ...(impacted ? { customer_impact_scope: scope.trim() } : {}),
          notification_handles: handles.map((h) => ({ display_name: h, handle: `@${h}` })),
          ...(note.trim() ? { initial_cells: [{ cell_type: "markdown" as const, content: { content: note.trim() } }] } : {}),
        },
        commander.trim() || undefined,
      );
      invalidate("observe/incidents");
      navigate(`/observe/incidents/${i.attributes.public_id}`);
    } catch (e) {
      toast({ title: "Couldn't declare it", description: errorMessage(e), tone: "failed" });
      setBusy(false);
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title="Declare an incident" description="It starts active. The integrations you pick are told now, and again as its state or severity changes.">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ready && !busy) void save();
          }}
        >
          <div>
            <label htmlFor="incident-title" className="field-label">
              What is happening
            </label>
            <div className="field">
              <input id="incident-title" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Checkout requests failing" maxLength={512} autoComplete="off" />
            </div>
          </div>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <SelectField label="Severity" value={severity} onChange={setSeverity} options={SEVERITIES.map((s) => ({ value: s, label: s === "UNKNOWN" ? "Unknown" : s }))} />
            <div>
              <label htmlFor="incident-commander" className="field-label">
                Commander (optional)
              </label>
              <div className="field">
                <input id="incident-commander" value={commander} onChange={(e) => setCommander(e.target.value)} placeholder="Who leads the response" autoComplete="off" />
              </div>
            </div>
          </div>
          <div>
            <label className="flex items-center gap-2 text-[13px]">
              <input type="checkbox" className="check" checked={impacted} onChange={(e) => setImpacted(e.target.checked)} /> Customers are impacted
            </label>
            {impacted && (
              <div className="field mt-2">
                <textarea aria-label="How customers are impacted" rows={2} value={scope} onChange={(e) => setScope(e.target.value)} placeholder="Every checkout fails with a 502" />
              </div>
            )}
          </div>
          <Loadable label="the integrations" fallback={null}>
            <Handles value={handles} onChange={setHandles} />
          </Loadable>
          <div>
            <label htmlFor="incident-note" className="field-label">
              A first note (optional, Markdown)
            </label>
            <div className="field">
              <textarea id="incident-note" rows={3} value={note} onChange={(e) => setNote(e.target.value)} placeholder="What is known so far" />
            </div>
          </div>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ready || busy}>
              Declare
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** The integrations an incident tells, picked from those set up (`@slack-ops`, …). */
export function Handles({ value, onChange }: { value: string[]; onChange: (v: string[]) => void }) {
  const all = useRes(integrationsRes).data;
  const names = all.map((i) => `${i.kind}-${i.name}`);
  const extra = value.filter((v) => !names.includes(v));
  if (names.length === 0 && extra.length === 0)
    return (
      <p className="text-[12.5px] text-[#8c8f98]">
        No integration to tell: set one up in{" "}
        <Link to="/observe/integrations" className="text-link">
          Integrations
        </Link>{" "}
        (Slack, PagerDuty, …).
      </p>
    );
  return (
    <fieldset>
      <legend className="field-label">Tell</legend>
      <div className="flex flex-wrap gap-x-4 gap-y-2">
        {[...names, ...extra].map((h) => (
          <label key={h} className="flex items-center gap-2 font-mono text-[12.5px]">
            <input type="checkbox" className="check" checked={value.includes(h)} onChange={(e) => onChange(e.target.checked ? [...value, h] : value.filter((v) => v !== h))} />@{h}
          </label>
        ))}
      </div>
    </fieldset>
  );
}
