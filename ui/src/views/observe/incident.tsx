/**
 * An incident (docs/observe.md §9.5): its state and severity, changed in place; its commander, its
 * customer impact and Datadog's figures of it (time to detect, respond, repair and resolve); its
 * timeline (notes, each change, its monitor's alerts and recoveries), a note put on it; its todos,
 * its postmortem and links, its properties and the integrations it tells, edited.
 */
import * as React from "react";
import { X } from "lucide-react";
import { observeApi, type Incident, type IncidentAttachment, type IncidentChange, type IncidentField, type IncidentState as State, type IncidentTodo, type TimelineCell } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { Hold, RowsHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel, SelectField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { useRes, warm, type Res } from "@/lib/data";
import { formatDuration, relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link, useNavigate, useParams } from "@/lib/router";
import { Handles, IncidentState, SEVERITIES, STATE_WORDS, Severity, ranFor } from "./incidents";
import { StateBadge } from "./parts";

// Markdown's renderer is big: only a page with a note loads it.
const MarkdownBody = React.lazy(() => import("@/lib/markdown").then((m) => ({ default: m.MarkdownBody })));

const incidentRes = (id: string): Res<Incident> => ({ key: `observe/incidents/one/${id}`, fetch: (s) => observeApi.incident(id, s), opts: { pollMs: 15_000 } });
const timelineRes = (id: string): Res<TimelineCell[]> => ({ key: `observe/incidents/one/${id}/timeline`, fetch: (s) => observeApi.incidentTimeline(id, s), opts: { pollMs: 15_000 } });
const todosRes = (id: string): Res<IncidentTodo[]> => ({ key: `observe/incidents/one/${id}/todos`, fetch: (s) => observeApi.incidentTodos(id, s), opts: { pollMs: 30_000 } });
const attachmentsRes = (id: string): Res<IncidentAttachment[]> => ({ key: `observe/incidents/one/${id}/attachments`, fetch: (s) => observeApi.incidentAttachments(id, s), opts: { pollMs: 30_000 } });

const seconds = (rfc3339: string) => Date.parse(rfc3339) / 1000;
const at = (rfc3339: string) => new Date(rfc3339).toLocaleString();

/** Something done to the incident: what it now is read again, or why it was refused. */
async function act(what: string, f: () => Promise<unknown>): Promise<boolean> {
  try {
    await f();
    invalidate("observe/incidents");
    return true;
  } catch (e) {
    toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
    return false;
  }
}

export default function IncidentPage() {
  const { id = "" } = useParams();
  warm(incidentRes(id), timelineRes(id), todosRes(id), attachmentsRes(id));
  return (
    <div className="container-console page">
      <Loadable key={id} label="the incident" fallback={<Hold height={480} />}>
        <IncidentView id={id} />
      </Loadable>
    </div>
  );
}

function IncidentView({ id }: { id: string }) {
  const i = useRes(incidentRes(id)).data;
  const navigate = useNavigate();
  const [editing, setEditing] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const a = i.attributes;
  const commander = i.relationships.commander_user.data?.id ?? null;
  const m = a.kiln.monitor;
  return (
    <>
      <title>{`#${a.public_id} ${a.title} · Incidents · Observe · kiln`}</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={8} items={[{ label: "Observe", to: "/observe" }, { label: "Incidents", to: "/observe/incidents" }, { label: `#${a.public_id}` }]} />
          <h1 className="[overflow-wrap:anywhere]">{a.title}</h1>
          <p className="lead max-w-3xl">
            <Severity s={a.severity} /> <IncidentState s={a.state} />
            <span className="text-[#8c8f98]">
              {" "}
              — declared {relativeTime(seconds(a.declared))} by {m ? "its monitor" : a.declared_by.name}
              {a.state === "resolved" && a.resolved ? `, resolved ${relativeTime(seconds(a.resolved))}` : ""}
              {a.is_test ? " · a test" : ""}
            </span>
            {m && (
              <span className="block text-[13px] text-[#8c8f98]">
                From{" "}
                <Link to={`/observe/monitors/${encodeURIComponent(m.id)}`} className="text-link">
                  {m.name}
                </Link>
                {m.group.length > 0 ? ` on ${m.group.map(([k, v]) => `${k}:${v}`).join(", ")}` : ""}
              </span>
            )}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button variant="secondary" onClick={() => setEditing(true)}>
            Edit…
          </Button>
          <Button variant="ghost-danger" onClick={() => setRemoving(true)}>
            Delete
          </Button>
        </div>
      </header>
      <StateAndSeverity key={`${a.state}/${a.severity}`} i={i} />
      <Facts
        className="mb-8"
        items={[
          { label: "Commander", value: commander ?? "None" },
          { label: "Customers", value: a.customer_impacted ? "Impacted" : "Not impacted", tone: a.customer_impacted && a.customer_impact_end === null ? "fail" : undefined },
          { label: a.state === "resolved" ? "Ran for" : "Running for", value: formatDuration(ranFor(i)) },
          { label: "Time to detect", value: formatDuration(a.time_to_detect) },
          { label: "Time to respond", value: formatDuration(a.time_to_internal_response) },
          { label: "Time to repair", value: formatDuration(a.time_to_repair) },
          { label: "Time to resolve", value: formatDuration(a.time_to_resolve) },
          { label: "Impact lasted", value: a.customer_impacted ? (a.customer_impact_end ? formatDuration(a.customer_impact_duration) : "Ongoing") : "—" },
        ]}
      />
      {a.customer_impacted && a.customer_impact_scope && (
        <p className="mb-8 max-w-3xl text-[13px] text-[#c8cad0]">
          <span className="text-[#8c8f98]">How customers are impacted: </span>
          {a.customer_impact_scope}
          {a.customer_impact_start ? <span className="text-[#8c8f98]"> — from {at(a.customer_impact_start)}</span> : ""}
          {a.customer_impact_end ? <span className="text-[#8c8f98]"> to {at(a.customer_impact_end)}</span> : ""}
        </p>
      )}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,1fr)_24rem]">
        <Panel title="Timeline" description="Notes, each change, and its monitor's alerts and recoveries; the newest first.">
          <Note id={i.id} />
          <Loadable label="its timeline" fallback={<RowsHold rows={4} rowHeight={44} />}>
            <Timeline id={id} />
          </Loadable>
        </Panel>
        <div className="flex min-w-0 flex-col gap-4">
          <Panel title="Todos" description={a.kiln.todos ? `${a.kiln.todos_open} of ${a.kiln.todos} open` : "What remains to do."}>
            <Loadable label="its todos" fallback={<RowsHold rows={2} rowHeight={32} />}>
              <Todos id={id} incident={i.id} />
            </Loadable>
          </Panel>
          <Panel title="Postmortem and links">
            <Loadable label="its attachments" fallback={<RowsHold rows={2} rowHeight={32} />}>
              <Attachments id={id} incident={i.id} />
            </Loadable>
          </Panel>
          <Panel
            title="Properties"
            actions={
              <Button variant="ghost" size="xs" onClick={() => setEditing(true)}>
                Edit
              </Button>
            }
          >
            <Properties i={i} />
          </Panel>
        </div>
      </div>
      {editing && <EditDialog i={i} onClose={() => setEditing(false)} />}
      <ConfirmDialog
        open={removing}
        onOpenChange={setRemoving}
        title={`Delete incident #${a.public_id}?`}
        description="Its timeline, todos and attachments go with it. Nothing is told."
        confirmLabel="Delete"
        danger
        onConfirm={async () => {
          if (await act("delete it", () => observeApi.removeIncident(i.id))) navigate("/observe/incidents");
        }}
      />
    </>
  );
}

/** Its state and severity, changed in place: shown as asked at once, put back if the change is
 * refused; made again (by its key) as the server's change of them comes. */
function StateAndSeverity({ i }: { i: Incident }) {
  const [state, setState] = React.useState(i.attributes.state);
  const [severity, setSeverity] = React.useState(i.attributes.severity);
  const change = async (what: string, c: IncidentChange, undo: () => void) => {
    if (!(await act(what, () => observeApi.updateIncident(i.id, c)))) undo();
  };
  return (
    <div className="mb-6 flex flex-wrap items-end gap-3">
      <Segmented<State>
        label="State"
        value={state}
        onChange={(s) => {
          if (s === state) return;
          const was = state;
          setState(s);
          void change("change its state", { state: s }, () => setState(was));
        }}
        options={(["active", "stable", "resolved"] as State[]).map((s) => ({ value: s, label: STATE_WORDS[s] }))}
      />
      <SelectField
        label="Severity"
        hideLabel
        value={severity}
        onChange={(s) => {
          const was = severity;
          setSeverity(s);
          void change("change its severity", { severity: s }, () => setSeverity(was));
        }}
        options={SEVERITIES.map((s) => ({ value: s, label: s === "UNKNOWN" ? "Severity unknown" : s }))}
        className="min-w-[10rem]"
      />
    </div>
  );
}

/** A note put on its timeline (Markdown), important or not. */
function Note({ id }: { id: string }) {
  const [text, setText] = React.useState("");
  const [important, setImportant] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const add = async () => {
    setBusy(true);
    if (await act("add the note", () => observeApi.noteIncident(id, text.trim(), important))) {
      setText("");
      setImportant(false);
    }
    setBusy(false);
  };
  return (
    <form
      className="mb-5"
      onSubmit={(e) => {
        e.preventDefault();
        if (text.trim() && !busy) void add();
      }}
    >
      <div className="field">
        <textarea
          aria-label="A note"
          rows={2}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey) && text.trim() && !busy) {
              e.preventDefault();
              void add();
            }
          }}
          placeholder="What was found, what was done (Markdown)"
        />
      </div>
      <div className="mt-2 flex items-center justify-between gap-3">
        <label className="flex items-center gap-2 text-[12.5px] text-[#b4b5ba]">
          <input type="checkbox" className="check" checked={important} onChange={(e) => setImportant(e.target.checked)} /> Important
        </label>
        <Button type="submit" size="sm" disabled={!text.trim() || busy}>
          Add note
        </Button>
      </div>
    </form>
  );
}

const CHANGED: Record<string, string> = { severity: "Severity", state: "State", commander: "Commander", title: "Title" };

function Timeline({ id }: { id: string }) {
  const cells = useRes(timelineRes(id)).data;
  return (
    <ol className="space-y-4">
      {[...cells].reverse().map((c) => (
        <li key={c.id} className={`min-w-0 border-l-2 pl-3 ${c.cell_type === "markdown" && c.important ? "border-[#d8cbb0]" : "border-white/10"}`}>
          <p className="mb-1 text-[11px] text-[#8c8f98]">
            <time dateTime={c.created} title={at(c.created)}>
              {relativeTime(seconds(c.created))}
            </time>{" "}
            · {c.author}
            {c.cell_type === "markdown" && c.important && <span className="chip ml-2">important</span>}
          </p>
          <Cell c={c} />
        </li>
      ))}
    </ol>
  );
}

function Cell({ c }: { c: TimelineCell }) {
  switch (c.cell_type) {
    case "markdown":
      return (
        <div className="obs-note min-w-0 text-[13px] [overflow-wrap:anywhere]">
          <React.Suspense fallback={<p className="whitespace-pre-wrap">{c.content}</p>}>
            <MarkdownBody text={c.content} />
          </React.Suspense>
        </div>
      );
    case "changed":
      if (c.what === "declared") return <p className="text-[13px]">Declared: {c.to}</p>;
      if (c.what === "postmortem") return <p className="text-[13px]">Postmortem attached: {c.to}</p>;
      return (
        <p className="text-[13px] [overflow-wrap:anywhere]">
          {CHANGED[c.what] ?? c.what}: <span className="text-[#8c8f98]">{c.from || "none"}</span> → {c.to || "none"}
        </p>
      );
    case "monitor":
      return (
        <div className="min-w-0 text-[13px]">
          <p>
            <StateBadge state={c.state} />{" "}
            <Link to={`/observe/monitors/${encodeURIComponent(c.monitor_id)}`} className="text-link">
              {c.monitor}
            </Link>
            {c.value !== null && <span className="font-mono text-[#8c8f98]"> at {c.value}</span>}
          </p>
          {c.message && <p className="mt-1 line-clamp-3 whitespace-pre-wrap text-[12px] text-[#8c8f98]">{c.message}</p>}
        </div>
      );
  }
}

/** Its todos: done (or not), removed, and one added (`@name` its assignees). */
function Todos({ id, incident }: { id: string; incident: string }) {
  const todos = useRes(todosRes(id)).data;
  const [text, setText] = React.useState("");
  const add = async () => {
    const assignees = [...text.matchAll(/(?:^|\s)@([\w.\-]+)/g)].flatMap((x) => (x[1] ? [x[1]] : []));
    if (await act("add the todo", () => observeApi.addIncidentTodo(incident, text.trim(), assignees))) setText("");
  };
  return (
    <>
      {todos.length > 0 && (
        <ul className="mb-3 space-y-2">
          {todos.map((t) => (
            <TodoRow key={`${t.id}:${t.attributes.completed !== null}`} t={t} incident={incident} />
          ))}
        </ul>
      )}
      <form
        className="flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (text.trim()) void add();
        }}
      >
        <div className="field flex-1">
          <input aria-label="A todo" value={text} onChange={(e) => setText(e.target.value)} placeholder="Roll back search-api @ada" autoComplete="off" />
        </div>
        <Button type="submit" variant="secondary" size="sm" disabled={!text.trim()}>
          Add
        </Button>
      </form>
    </>
  );
}

/** A todo: done (or not) at once, put back if the change is refused; made again (by its key) as
 * the server's change of it comes. */
function TodoRow({ t, incident }: { t: IncidentTodo; incident: string }) {
  const [done, setDone] = React.useState(t.attributes.completed !== null);
  const set = async (now: boolean) => {
    setDone(now);
    if (!(await act("change the todo", () => observeApi.setIncidentTodo(incident, t.id, { completed: now ? new Date().toISOString() : null })))) setDone(!now);
  };
  return (
    <li className="flex min-w-0 items-start gap-2 text-[13px]">
      <input type="checkbox" className="check mt-0.5" aria-label={`Done: ${t.attributes.content}`} checked={done} onChange={(e) => void set(e.target.checked)} />
      <span className={`min-w-0 flex-1 [overflow-wrap:anywhere] ${done ? "text-[#8c8f98] line-through" : ""}`}>
        {t.attributes.content}
        {t.attributes.due_date && <span className="ml-1.5 text-[11px] text-[#8c8f98] no-underline">due {t.attributes.due_date}</span>}
      </span>
      <button
        type="button"
        className="icon-btn -my-1"
        data-variant="bare"
        aria-label={`Remove the todo: ${t.attributes.content}`}
        onClick={() => void act("remove the todo", () => observeApi.removeIncidentTodo(incident, t.id))}
      >
        <X size={14} />
      </button>
    </li>
  );
}

/** Its postmortem (one) and links: opened, removed, one attached. */
function Attachments({ id, incident }: { id: string; incident: string }) {
  const all = useRes(attachmentsRes(id)).data;
  const [url, setUrl] = React.useState("");
  const [title, setTitle] = React.useState("");
  const [postmortem, setPostmortem] = React.useState(false);
  const has = all.some((x) => x.attributes.attachment_type === "postmortem");
  const add = async () => {
    if (await act("attach it", () => observeApi.attachToIncident(incident, postmortem && !has ? "postmortem" : "link", url.trim(), title.trim() || url.trim()))) {
      setUrl("");
      setTitle("");
      setPostmortem(false);
    }
  };
  const sorted = [...all].sort((x, y) => Number(y.attributes.attachment_type === "postmortem") - Number(x.attributes.attachment_type === "postmortem"));
  return (
    <>
      {sorted.length > 0 && (
        <ul className="mb-3 space-y-2">
          {sorted.map((x) => (
            <li key={x.id} className="flex min-w-0 items-center gap-2 text-[13px]">
              {x.attributes.attachment_type === "postmortem" && <span className="chip">postmortem</span>}
              <a href={x.attributes.attachment.documentUrl} target="_blank" rel="noopener noreferrer" className="text-link min-w-0 flex-1 truncate">
                {x.attributes.attachment.title}
              </a>
              <button
                type="button"
                className="icon-btn -my-1"
                data-variant="bare"
                aria-label={`Remove ${x.attributes.attachment.title}`}
                onClick={() => void act("remove it", () => observeApi.removeIncidentAttachment(incident, x.id))}
              >
                <X size={14} />
              </button>
            </li>
          ))}
        </ul>
      )}
      <form
        className="space-y-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (/^https?:\/\//.test(url.trim())) void add();
        }}
      >
        <div className="field">
          <input aria-label="Its URL" type="url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://…" autoComplete="off" />
        </div>
        <div className="flex gap-2">
          <div className="field flex-1">
            <input aria-label="Its title" value={title} onChange={(e) => setTitle(e.target.value)} placeholder="Title" autoComplete="off" />
          </div>
          <Button type="submit" variant="secondary" size="sm" disabled={!/^https?:\/\//.test(url.trim())}>
            Attach
          </Button>
        </div>
        {!has && (
          <label className="flex items-center gap-2 text-[12.5px] text-[#b4b5ba]">
            <input type="checkbox" className="check" checked={postmortem} onChange={(e) => setPostmortem(e.target.checked)} /> It is the postmortem
          </label>
        )}
      </form>
    </>
  );
}

/** A field's name as read: `root_cause` → Root cause. */
const fieldName = (k: string) => (k.charAt(0).toUpperCase() + k.slice(1)).replace(/_/g, " ");
const fieldText = (f: IncidentField) => (Array.isArray(f.value) ? f.value.join(", ") : (f.value ?? ""));

function Properties({ i }: { i: Incident }) {
  const a = i.attributes;
  const fields = Object.entries(a.fields).filter(([k, f]) => k !== "severity" && k !== "state" && fieldText(f) !== "");
  const rows: [string, React.ReactNode][] = [
    ...fields.map(([k, f]): [string, React.ReactNode] => [fieldName(k), fieldText(f)]),
    ["Tells", a.notification_handles.length > 0 ? <span className="font-mono">{a.notification_handles.map((h) => h.handle).join(" ")}</span> : "No integration"],
  ];
  return (
    <dl className="space-y-1.5 text-[12.5px]">
      {rows.map(([k, v]) => (
        <div key={k} className="flex min-w-0 gap-3">
          <dt className="w-28 shrink-0 text-[#8c8f98]">{k}</dt>
          <dd className="min-w-0 [overflow-wrap:anywhere]">{v}</dd>
        </div>
      ))}
    </dl>
  );
}

const DETECTION = [
  { value: "", label: "Not said" },
  { value: "monitor", label: "A monitor" },
  { value: "customer", label: "A customer" },
  { value: "employee", label: "An employee" },
  { value: "other", label: "Otherwise" },
];

/** A time as a local `datetime-local` input takes it, and back. */
const toLocal = (rfc3339: string | null) => {
  if (!rfc3339) return "";
  const d = new Date(rfc3339);
  return new Date(d.getTime() - d.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
};
const fromLocal = (v: string) => (v ? new Date(v).toISOString() : null);
const list = (v: string) =>
  v
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

/** Its title, commander, customer impact, properties, the integrations it tells; whether a test. */
function EditDialog({ i, onClose }: { i: Incident; onClose: () => void }) {
  const a = i.attributes;
  const text = (k: string) => (a.fields[k] ? fieldText(a.fields[k]) : "");
  const [title, setTitle] = React.useState(a.title);
  const [commander, setCommander] = React.useState(i.relationships.commander_user.data?.id ?? "");
  const [impacted, setImpacted] = React.useState(a.customer_impacted);
  const [scope, setScope] = React.useState(a.customer_impact_scope ?? "");
  const [start, setStart] = React.useState(toLocal(a.customer_impact_start));
  const [end, setEnd] = React.useState(toLocal(a.customer_impact_end));
  const [summary, setSummary] = React.useState(text("summary"));
  const [cause, setCause] = React.useState(text("root_cause"));
  const [services, setServices] = React.useState(text("services"));
  const [teams, setTeams] = React.useState(text("teams"));
  const [detection, setDetection] = React.useState(text("detection_method"));
  const [handles, setHandles] = React.useState(a.notification_handles.map((h) => h.handle.replace(/^@/, "")));
  const [test, setTest] = React.useState(a.is_test);
  const [busy, setBusy] = React.useState(false);
  const ready = title.trim() !== "" && (!impacted || scope.trim() !== "");
  const save = async () => {
    setBusy(true);
    const textbox = (v: string): IncidentField => ({ type: "textbox", value: v.trim() || null });
    const many = (v: string): IncidentField => ({ type: "multiselect", value: list(v).length ? list(v) : null });
    const change: IncidentChange = {
      title: title.trim(),
      customer_impacted: impacted,
      customer_impact_scope: impacted ? scope.trim() : null,
      customer_impact_start: impacted ? fromLocal(start) : null,
      customer_impact_end: impacted ? fromLocal(end) : null,
      fields: { summary: textbox(summary), root_cause: textbox(cause), services: many(services), teams: many(teams), detection_method: { type: "dropdown", value: detection || null } },
      notification_handles: handles.map((h) => ({ display_name: h, handle: `@${h}` })),
      is_test: test,
    };
    if (await act("save it", () => observeApi.updateIncident(i.id, change, commander.trim() || null))) onClose();
    else setBusy(false);
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()}>
      <DialogContent title={`Edit incident #${a.public_id}`} description="A change of its commander or title is put on its timeline.">
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault();
            if (ready && !busy) void save();
          }}
        >
          <Text id="edit-title" label="Title" value={title} onChange={setTitle} />
          <Text id="edit-commander" label="Commander" value={commander} onChange={setCommander} placeholder="Who leads the response" />
          <div>
            <label className="flex items-center gap-2 text-[13px]">
              <input type="checkbox" className="check" checked={impacted} onChange={(e) => setImpacted(e.target.checked)} /> Customers are impacted
            </label>
            {impacted && (
              <div className="mt-2 space-y-2">
                <div className="field">
                  <textarea aria-label="How customers are impacted" rows={2} value={scope} onChange={(e) => setScope(e.target.value)} placeholder="Every checkout fails with a 502" />
                </div>
                <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                  <Text id="edit-start" label="Impact began" type="datetime-local" value={start} onChange={setStart} />
                  <Text id="edit-end" label="Impact ended" type="datetime-local" value={end} onChange={setEnd} />
                </div>
              </div>
            )}
          </div>
          <div>
            <label htmlFor="edit-summary" className="field-label">
              Summary
            </label>
            <div className="field">
              <textarea id="edit-summary" rows={2} value={summary} onChange={(e) => setSummary(e.target.value)} />
            </div>
          </div>
          <div>
            <label htmlFor="edit-cause" className="field-label">
              Root cause
            </label>
            <div className="field">
              <textarea id="edit-cause" rows={2} value={cause} onChange={(e) => setCause(e.target.value)} />
            </div>
          </div>
          <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
            <Text id="edit-services" label="Services" value={services} onChange={setServices} placeholder="checkout, payments" />
            <Text id="edit-teams" label="Teams" value={teams} onChange={setTeams} placeholder="payments" />
          </div>
          <SelectField
            label="Found by"
            value={detection}
            onChange={setDetection}
            options={DETECTION.some((d) => d.value === detection) ? DETECTION : [...DETECTION, { value: detection, label: detection }]}
          />
          <Loadable label="the integrations" fallback={null}>
            <Handles value={handles} onChange={setHandles} />
          </Loadable>
          <label className="flex items-center gap-2 text-[13px]">
            <input type="checkbox" className="check" checked={test} onChange={(e) => setTest(e.target.checked)} /> A test incident
          </label>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ready || busy}>
              Save
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function Text({ id, label, value, onChange, placeholder, type }: { id: string; label: string; value: string; onChange: (v: string) => void; placeholder?: string; type?: string }) {
  return (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">
        <input id={id} type={type} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} autoComplete="off" />
      </div>
    </div>
  );
}
