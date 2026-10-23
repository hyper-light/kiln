/**
 * A workflow's flow view (docs/ci.md W21-R06): its jobs by stage (a job comes after what it needs),
 * each with where it runs, what it needs, its matrix and its steps, edited in place. The YAML is the
 * source: each edit is made to the text (`@/lib/workflow-doc`) with its comments and formatting kept,
 * and one that can't be made here says why and leaves the text as it was.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, X } from "lucide-react";
import { ci } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { SelectField, TextField } from "@/components/ui/misc";
import { useSuspenseResource } from "@/lib/resource";
import * as W from "@/lib/workflow-doc";

type Apply = (edit: (text: string) => string) => boolean;

/** The jobs in stages: a job's stage is one past the latest of those it needs. */
function stages(jobs: W.FlowJob[]): W.FlowJob[][] {
  const byId = new Map(jobs.map((j) => [j.id, j]));
  const depth = new Map<string, number>();
  const of = (j: W.FlowJob, seen: Set<string>): number => {
    const known = depth.get(j.id);
    if (known !== undefined) return known;
    if (seen.has(j.id)) return 0;
    seen.add(j.id);
    const before = j.needs.map((n) => byId.get(n)).filter((n): n is W.FlowJob => !!n);
    const d = before.length ? Math.max(...before.map((n) => of(n, seen))) + 1 : 0;
    depth.set(j.id, d);
    return d;
  };
  const out: W.FlowJob[][] = [];
  for (const j of jobs) (out[of(j, new Set())] ??= []).push(j);
  return out.filter(Boolean);
}

/** The jobs that need `id`, however far down: needing one of them would go round. */
function after(jobs: W.FlowJob[], id: string): Set<string> {
  const out = new Set<string>();
  let grew = true;
  while (grew) {
    grew = false;
    for (const j of jobs)
      if (!out.has(j.id) && j.needs.some((n) => n === id || out.has(n))) {
        out.add(j.id);
        grew = true;
      }
  }
  return out;
}

/** GitHub's hosted labels, offered beside this server's runners', pools' and rules'. */
const HOSTED = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-24.04-arm", "macos-latest", "windows-latest", "self-hosted"];

export function FlowView({ repo, text, onChange }: { repo: string; text: string; onChange: (text: string) => void }) {
  const flow = React.useMemo(() => W.readFlow(text), [text]);
  const known = useSuspenseResource(`ci-labels/${repo}`, (s) => ci.labels(repo, s)).data.labels;
  const labels = [...known.map((l) => l.label), ...HOSTED.filter((h) => !known.some((l) => l.label.toLowerCase() === h))];
  const [problem, setProblem] = React.useState<string | null>(null);
  const apply: Apply = (edit) => {
    try {
      onChange(edit(text));
      setProblem(null);
      return true;
    } catch (e) {
      setProblem(e instanceof Error ? e.message : String(e));
      return false;
    }
  };
  if (flow.error) return <p className="px-5 py-4 text-[12.5px] text-[#a1a4ad]">The YAML can&apos;t be read as it is ({flow.error}): fix it in the YAML view.</p>;
  return (
    <section aria-label="Its flow" className="grid gap-5 px-5 py-4">
      {problem && (
        <p role="status" className="text-[12.5px] leading-[1.55] text-[var(--color-fail)] [overflow-wrap:anywhere]">
          {problem}
        </p>
      )}
      {stages(flow.jobs).map((jobs, i) => (
        <section key={i} aria-label={`Stage ${i + 1}`} className="grid gap-2.5">
          <h3 className="label">Stage {i + 1}</h3>
          <div className="grid grid-cols-[repeat(auto-fit,minmax(min(100%,22rem),1fr))] gap-3">
            {jobs.map((j) => (
              <JobCard key={j.id} job={j} jobs={flow.jobs} labels={labels} apply={apply} />
            ))}
          </div>
        </section>
      ))}
      <AddJob labels={labels} apply={apply} />
    </section>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid gap-1.5">
      <span className="label">{label}</span>
      <div className="flex min-w-0 flex-wrap items-center gap-1.5">{children}</div>
    </div>
  );
}

function Chip({ text, remove, what }: { text: string; remove?: () => void; what: string }) {
  return (
    // Without its button, the chip's own space on the right.
    <span className="facet-chip font-mono" style={remove ? undefined : { paddingRight: 9 }}>
      <span>{text}</span>
      {remove && (
        <button type="button" aria-label={`Remove ${what} ${text}`} onClick={remove}>
          <X size={12} />
        </button>
      )}
    </span>
  );
}

/** A select whose choice is an action, back to its prompt after. */
function AddSelect({ label, options, onPick }: { label: string; options: string[]; onPick: (v: string) => void }) {
  if (!options.length) return null;
  return (
    <SelectField
      label={label}
      hideLabel
      value=""
      onChange={(v) => v && onPick(v)}
      options={[{ value: "", label: `${label}…` }, ...options.map((o) => ({ value: o, label: o }))]}
      className="min-w-0 max-w-full"
    />
  );
}

function JobCard({ job, jobs, labels, apply }: { job: W.FlowJob; jobs: W.FlowJob[]; labels: string[]; apply: Apply }) {
  const [removing, setRemoving] = React.useState(false);
  const later = after(jobs, job.id);
  const needable = jobs.filter((j) => j.id !== job.id && !job.needs.includes(j.id) && !later.has(j.id)).map((j) => j.id);
  return (
    <section aria-label={`Job ${job.id}`} className="grid min-w-0 content-start gap-3.5 rounded-[10px] border border-white/10 bg-white/[0.015] p-4">
      <header className="flex flex-wrap items-center justify-between gap-2">
        <h4 className="min-w-0 font-mono text-[13px] text-[#e4e5e7] [overflow-wrap:anywhere]">
          {job.id}
          {job.name && <span className="ml-2 font-sans text-[12px] text-[#8c8f98]">{job.name}</span>}
        </h4>
        {removing ? (
          <span className="flex gap-1.5">
            <Button size="xs" variant="secondary" onClick={() => apply((t) => W.removeJob(t, job.id))}>
              Remove {job.id}
            </Button>
            <Button size="xs" variant="ghost" onClick={() => setRemoving(false)}>
              Keep it
            </Button>
          </span>
        ) : (
          <Button size="xs" variant="ghost" onClick={() => setRemoving(true)}>
            Remove
          </Button>
        )}
      </header>
      <Row label="Runs on">
        {job.runsOn ? (
          <>
            {job.runsOn.map((l) => (
              <Chip
                key={l}
                text={l}
                what="label"
                remove={
                  job.runsOn && job.runsOn.length > 1
                    ? () =>
                        apply((t) =>
                          W.setRunsOn(
                            t,
                            job.id,
                            (job.runsOn ?? []).filter((x) => x !== l),
                          ),
                        )
                    : undefined
                }
              />
            ))}
            <AddSelect
              label="Add a label"
              options={labels.filter((l) => !job.runsOn?.some((x) => x.toLowerCase() === l.toLowerCase()))}
              onPick={(l) => apply((t) => W.setRunsOn(t, job.id, [...(job.runsOn ?? []), l]))}
            />
          </>
        ) : (
          <span className="text-[12px] text-[#8c8f98]">
            <code className="font-mono text-[#c9d0da]">{job.runsOnText}</code>, set in the YAML
          </span>
        )}
      </Row>
      <Row label="Needs">
        {job.needs.map((n) => (
          <Chip key={n} text={n} what="need" remove={() => apply((t) => W.removeNeed(t, job.id, n))} />
        ))}
        {!job.needs.length && !needable.length && <span className="text-[12px] text-[#8c8f98]">Nothing: it starts first.</span>}
        <AddSelect label="Add a need" options={needable} onPick={(n) => apply((t) => W.addNeed(t, job.id, n))} />
      </Row>
      <Matrix job={job} apply={apply} />
      {job.uses ? (
        <Row label="Calls">
          <code className="font-mono text-[12px] text-[#c9d0da] [overflow-wrap:anywhere]">{job.uses}</code>
        </Row>
      ) : (
        <Steps job={job} apply={apply} />
      )}
    </section>
  );
}

function Steps({ job, apply }: { job: W.FlowJob; apply: Apply }) {
  const [open, setOpen] = React.useState<number | null>(null);
  const [kind, setKind] = React.useState<"run" | "uses">("run");
  const [draft, setDraft] = React.useState("");
  const add = (e: React.FormEvent) => {
    e.preventDefault();
    const v = draft.trim();
    if (v && apply((t) => W.addStep(t, job.id, kind === "run" ? { run: v } : { uses: v }))) setDraft("");
  };
  return (
    <div className="grid gap-1.5">
      <span className="label">Steps</span>
      <ol aria-label={`${job.id}'s steps`} className="grid gap-1">
        {job.steps.map((s, i) => (
          <li key={i} className="grid gap-2 rounded-[6px] border border-white/[0.07] px-2.5 py-1.5">
            <div className="flex min-w-0 items-center gap-1">
              <button type="button" className="min-w-0 flex-1 truncate text-left text-[12.5px] text-[#c9d0da]" aria-expanded={open === i} onClick={() => setOpen(open === i ? null : i)}>
                <span className="mr-2 font-mono text-[11px] text-[#6d7078] tabular-nums">{i + 1}</span>
                {s.name ?? (s.uses ? `uses ${s.uses}` : (s.run?.split("\n")[0] ?? "a step"))}
              </button>
              <Button size="icon-sm" variant="ghost" aria-label={`Move step ${i + 1} up`} disabled={i === 0} onClick={() => apply((t) => W.moveStep(t, job.id, i, -1))}>
                <ArrowUp />
              </Button>
              <Button size="icon-sm" variant="ghost" aria-label={`Move step ${i + 1} down`} disabled={i === job.steps.length - 1} onClick={() => apply((t) => W.moveStep(t, job.id, i, 1))}>
                <ArrowDown />
              </Button>
              <Button size="icon-sm" variant="ghost" aria-label={`Remove step ${i + 1}`} disabled={job.steps.length === 1} onClick={() => apply((t) => W.removeStep(t, job.id, i))}>
                <X />
              </Button>
            </div>
            {open === i && <StepFields key={`${job.id}-${i}-${s.name}-${s.run}-${s.uses}`} job={job.id} index={i} step={s} apply={apply} />}
          </li>
        ))}
      </ol>
      <form className="flex min-w-0 flex-wrap items-end gap-2" onSubmit={add}>
        <SelectField
          label="Kind of step"
          hideLabel
          value={kind}
          onChange={(v) => setKind(v === "uses" ? "uses" : "run")}
          options={[
            { value: "run", label: "Run a command" },
            { value: "uses", label: "Use an action" },
          ]}
        />
        <TextField label="Add a step" className="min-w-0 flex-1 basis-48" value={draft} onChange={(e) => setDraft(e.target.value)} placeholder={kind === "run" ? "make test" : "actions/checkout@v4"} />
        <Button size="xs" type="submit" variant="secondary" disabled={!draft.trim()}>
          Add the step
        </Button>
      </form>
    </div>
  );
}

/** A step's name and its command or action, each written when its field is left. */
function StepFields({ job, index, step, apply }: { job: string; index: number; step: W.FlowStep; apply: Apply }) {
  const [name, setName] = React.useState(step.name ?? "");
  const [body, setBody] = React.useState(step.uses ?? step.run ?? "");
  const key = step.uses !== null ? "uses" : "run";
  const id = React.useId();
  return (
    <div className="grid gap-2 pb-1">
      <TextField label="Name" value={name} onChange={(e) => setName(e.target.value)} onBlur={() => name !== (step.name ?? "") && apply((t) => W.setStepField(t, job, index, "name", name.trim()))} />
      {key === "run" ? (
        <div>
          <label htmlFor={id} className="field-label">
            Run
          </label>
          <div className="field">
            <textarea
              id={id}
              rows={Math.min(8, Math.max(2, body.split("\n").length))}
              className="font-mono text-[12px]"
              value={body}
              onChange={(e) => setBody(e.target.value)}
              onBlur={() => body !== (step.run ?? "") && body.trim() && apply((t) => W.setStepField(t, job, index, "run", body))}
            />
          </div>
        </div>
      ) : (
        <TextField
          label="Uses"
          value={body}
          onChange={(e) => setBody(e.target.value)}
          onBlur={() => body !== (step.uses ?? "") && body.trim() && apply((t) => W.setStepField(t, job, index, "uses", body.trim()))}
        />
      )}
    </div>
  );
}

const split = (v: string) =>
  v
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean);

function Matrix({ job, apply }: { job: W.FlowJob; apply: Apply }) {
  const m = job.matrix;
  const [adding, setAdding] = React.useState(false);
  const [axis, setAxis] = React.useState({ name: "", values: "" });
  const [row, setRow] = React.useState({ kind: "exclude" as "include" | "exclude", text: "" });
  if (m?.expr)
    return (
      <Row label="Matrix">
        <span className="text-[12px] text-[#8c8f98]">
          <code className="font-mono text-[#c9d0da] [overflow-wrap:anywhere]">{m.expr}</code>, set in the YAML
        </span>
      </Row>
    );
  const addAxis = (e: React.FormEvent) => {
    e.preventDefault();
    if (axis.name.trim() && split(axis.values).length && apply((t) => W.setAxis(t, job.id, axis.name.trim(), split(axis.values)))) {
      setAxis({ name: "", values: "" });
      setAdding(false);
    }
  };
  const addRow = (e: React.FormEvent) => {
    e.preventDefault();
    if (row.text.trim() && apply((t) => W.addRow(t, job.id, row.kind, row.text.trim()))) setRow({ ...row, text: "" });
  };
  const axisForm = (
    <form className="flex min-w-0 flex-wrap items-end gap-2" onSubmit={addAxis}>
      <TextField label="Axis" className="w-28" value={axis.name} onChange={(e) => setAxis({ ...axis, name: e.target.value })} placeholder="os" />
      <TextField label="Its values" className="min-w-0 flex-1 basis-40" value={axis.values} onChange={(e) => setAxis({ ...axis, values: e.target.value })} placeholder="linux, mac" />
      <Button size="xs" type="submit" variant="secondary" disabled={!axis.name.trim() || !split(axis.values).length}>
        Add the axis
      </Button>
    </form>
  );
  if (!m)
    return adding ? (
      <div className="grid gap-1.5">
        <span className="label">Matrix</span>
        {axisForm}
      </div>
    ) : (
      <div>
        <Button size="xs" variant="ghost" onClick={() => setAdding(true)}>
          Add a matrix
        </Button>
      </div>
    );
  return (
    <div className="grid gap-2">
      <span className="label">Matrix</span>
      <table aria-label={`${job.id}'s matrix`} className="w-full table-fixed text-[12.5px]">
        <tbody>
          {m.axes.map((a) => (
            <AxisRow key={`${a.name}=${a.values.join()}`} job={job.id} axis={a} apply={apply} />
          ))}
        </tbody>
      </table>
      {(["include", "exclude"] as const).map((kind) =>
        m[kind].length ? (
          <Row key={kind} label={kind === "include" ? "Include" : "Exclude"}>
            {m[kind].map((r, i) => (
              <Chip key={`${i}-${r}`} text={r} what={`${kind} row`} remove={() => apply((t) => W.removeRow(t, job.id, kind, i))} />
            ))}
          </Row>
        ) : null,
      )}
      {m.where && (
        <Row label="Where">
          <code className="font-mono text-[12px] text-[#c9d0da] [overflow-wrap:anywhere]">{m.where}</code>
        </Row>
      )}
      {adding ? (
        axisForm
      ) : (
        <div className="flex flex-wrap gap-2">
          <Button size="xs" variant="ghost" onClick={() => setAdding(true)}>
            Add an axis
          </Button>
        </div>
      )}
      <form className="flex min-w-0 flex-wrap items-end gap-2" onSubmit={addRow}>
        <SelectField
          label="Kind of row"
          hideLabel
          value={row.kind}
          onChange={(v) => setRow({ ...row, kind: v === "include" ? "include" : "exclude" })}
          options={[
            { value: "exclude", label: "Exclude" },
            { value: "include", label: "Include" },
          ]}
        />
        <TextField label="Add a row" className="min-w-0 flex-1 basis-40" value={row.text} onChange={(e) => setRow({ ...row, text: e.target.value })} placeholder="os: mac, n: 1" />
        <Button size="xs" type="submit" variant="secondary" disabled={!row.text.trim()}>
          Add the row
        </Button>
      </form>
    </div>
  );
}

/** An axis: its values edited as a list, written when the field is left or Enter is pressed. */
function AxisRow({ job, axis, apply }: { job: string; axis: { name: string; values: string[] }; apply: Apply }) {
  const [values, setValues] = React.useState(axis.values.join(", "));
  const write = () => {
    const next = split(values);
    if (next.length && next.join() !== axis.values.join()) apply((t) => W.setAxis(t, job, axis.name, next));
  };
  return (
    <tr className="border-b border-white/[0.06] last:border-0">
      <th scope="row" className="w-24 py-1.5 pr-2 text-left align-middle font-mono text-[12px] font-normal text-[#a6c4ed] [overflow-wrap:anywhere]">
        {axis.name}
      </th>
      <td className="py-1.5">
        <form
          onSubmit={(e) => {
            e.preventDefault();
            write();
          }}
        >
          <TextField label={`Values of ${axis.name}`} className="[&>label]:sr-only" value={values} onChange={(e) => setValues(e.target.value)} onBlur={write} />
        </form>
      </td>
      <td className="w-9 py-1.5 text-right">
        <Button size="icon-sm" variant="ghost" aria-label={`Remove the axis ${axis.name}`} onClick={() => apply((t) => W.removeAxis(t, job, axis.name))}>
          <X />
        </Button>
      </td>
    </tr>
  );
}

function AddJob({ labels, apply }: { labels: string[]; apply: Apply }) {
  const [id, setId] = React.useState("");
  const [runsOn, setRunsOn] = React.useState(labels[0] ?? "ubuntu-latest");
  return (
    <form
      aria-label="Add a job"
      className="flex min-w-0 flex-wrap items-end gap-2 border-t border-white/[0.07] pt-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (id.trim() && apply((t) => W.addJob(t, id.trim(), runsOn))) setId("");
      }}
    >
      <TextField label="A new job" className="min-w-0 flex-1 basis-40" value={id} onChange={(e) => setId(e.target.value)} placeholder="lint" />
      <SelectField label="Runs on" value={runsOn} onChange={setRunsOn} options={labels.map((l) => ({ value: l, label: l }))} className="min-w-0" />
      <Button size="xs" type="submit" variant="secondary" disabled={!id.trim()}>
        Add the job
      </Button>
    </form>
  );
}
