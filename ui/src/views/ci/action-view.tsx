/**
 * An action in the workflow editor (docs/ci.md W21-R07): its inputs and outputs edited in place (the
 * YAML stays the source, each edit made by `@/lib/workflow-doc`), and "Try it": the action as it is
 * here, saved or not, run once with chosen inputs on a chosen runner, its outputs and log shown.
 */
import * as React from "react";
import { X } from "lucide-react";
import { ci, ciRun } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Panel, SelectField, TextField } from "@/components/ui/misc";
import { Link } from "@/lib/router";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import * as W from "@/lib/workflow-doc";

const LogViewer = React.lazy(() => import("@/views/run-detail/log-viewer"));

type Apply = (edit: (text: string) => string) => boolean;
type Io = "inputs" | "outputs";

export function ActionView({ text, onChange }: { text: string; onChange: (text: string) => void }) {
  const shape = React.useMemo(() => W.readAction(text), [text]);
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
  if (shape.error) return <p className="px-5 py-4 text-[12.5px] text-[#a1a4ad]">The YAML can&apos;t be read as it is ({shape.error}): fix it in the YAML view.</p>;
  return (
    <section aria-label="Its inputs and outputs" className="grid gap-6 px-5 py-4">
      {problem && (
        <p role="status" className="text-[12.5px] leading-[1.55] text-[var(--color-fail)] [overflow-wrap:anywhere]">
          {problem}
        </p>
      )}
      <Fields kind="inputs" fields={shape.inputs} apply={apply} composite={shape.using === "composite"} />
      <Fields kind="outputs" fields={shape.outputs} apply={apply} composite={shape.using === "composite"} />
    </section>
  );
}

function Fields({ kind, fields, apply, composite }: { kind: Io; fields: W.ActionField[]; apply: Apply; composite: boolean }) {
  const one = kind === "inputs" ? "input" : "output";
  const [draft, setDraft] = React.useState({ name: "", description: "" });
  return (
    <section aria-label={kind === "inputs" ? "Inputs" : "Outputs"} className="grid gap-2.5">
      <h3 className="label">{kind === "inputs" ? "Inputs" : "Outputs"}</h3>
      {fields.length ? (
        <ul className="grid gap-2">
          {fields.map((f) => (
            <FieldCard key={`${f.name}\u0000${f.description}\u0000${f.default}\u0000${f.value}\u0000${f.required}`} kind={kind} field={f} apply={apply} composite={composite} />
          ))}
        </ul>
      ) : (
        <p className="text-[12.5px] text-[#8c8f98]">{kind === "inputs" ? "It takes no inputs." : "It gives no outputs."}</p>
      )}
      <form
        className="flex min-w-0 flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (draft.name.trim() && apply((t) => W.addField(t, kind, draft.name.trim(), draft.description.trim()))) setDraft({ name: "", description: "" });
        }}
      >
        <TextField label={`A new ${one}`} className="w-40" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} placeholder={kind === "inputs" ? "who" : "greeting"} />
        <TextField label="What it is" className="min-w-0 flex-1 basis-48" value={draft.description} onChange={(e) => setDraft({ ...draft, description: e.target.value })} />
        <Button size="xs" type="submit" variant="secondary" disabled={!draft.name.trim()}>
          Add the {one}
        </Button>
      </form>
    </section>
  );
}

/** One input or output, each field written when it's left. */
function FieldCard({ kind, field, apply, composite }: { kind: Io; field: W.ActionField; apply: Apply; composite: boolean }) {
  const [description, setDescription] = React.useState(field.description ?? "");
  const [other, setOther] = React.useState((kind === "inputs" ? field.default : field.value) ?? "");
  const key = kind === "inputs" ? "default" : "value";
  const was = (kind === "inputs" ? field.default : field.value) ?? "";
  return (
    <li aria-label={`${kind === "inputs" ? "Input" : "Output"} ${field.name}`} className="grid gap-2.5 rounded-[10px] border border-white/10 bg-white/[0.015] p-3.5">
      <div className="flex items-center justify-between gap-2">
        <span className="font-mono text-[13px] text-[#e4e5e7] [overflow-wrap:anywhere]">{field.name}</span>
        <Button size="icon-sm" variant="ghost" aria-label={`Remove the ${kind === "inputs" ? "input" : "output"} ${field.name}`} onClick={() => apply((t) => W.removeField(t, kind, field.name))}>
          <X />
        </Button>
      </div>
      <TextField
        label="What it is"
        value={description}
        onChange={(e) => setDescription(e.target.value)}
        onBlur={() => description !== (field.description ?? "") && apply((t) => W.setField(t, kind, field.name, "description", description))}
      />
      {(kind === "inputs" || composite) && (
        <TextField
          label={kind === "inputs" ? "Its default" : "Its value"}
          className="[&_input]:font-mono"
          value={other}
          onChange={(e) => setOther(e.target.value)}
          onBlur={() => other !== was && apply((t) => W.setField(t, kind, field.name, key, other))}
          placeholder={kind === "inputs" ? "none" : "${{ steps.<id>.outputs.<name> }}"}
        />
      )}
      {kind === "inputs" && (
        <label className="flex items-center gap-2 text-[12.5px] text-[#c9d0da]">
          <input type="checkbox" checked={field.required} onChange={(e) => apply((t) => W.setField(t, kind, field.name, "required", e.target.checked))} />
          Required
        </label>
      )}
    </li>
  );
}

/** "Try it": the action as the editor has it, with inputs given here, on a runner chosen here. */
export function TryPane({ repo, path, gitRef, text, blocked }: { repo: string; path: string; gitRef: string; text: string; blocked: boolean }) {
  const shape = React.useMemo(() => W.readAction(text), [text]);
  const labels = useSuspenseResource(`ci-labels/${repo}`, (s) => ci.labels(repo, s)).data.labels.map((l) => l.label);
  const [values, setValues] = React.useState<Record<string, string>>({});
  const [runsOn, setRunsOn] = React.useState(labels[0] ?? "");
  const [started, setStarted] = React.useState<{ run: string; trusted: boolean } | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const dir = path.slice(0, path.lastIndexOf("/"));
  const go = async (e: React.FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      // What's given; what's left empty takes its default.
      const inputs = Object.fromEntries(Object.entries(values).filter(([k, v]) => v !== "" && shape.inputs.some((i) => i.name === k)));
      const r = await ci.tryAction(repo, { path, content: text, ref: gitRef || undefined, inputs, runs_on: runsOn });
      setStarted({ run: r.run, trusted: r.trusted });
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Panel eyebrow="Try it" title={shape.name ? `${shape.name}, once` : "Run it once"}>
      <form className="grid gap-3" onSubmit={(e) => void go(e)}>
        {shape.inputs.map((i) => (
          <TextField
            key={i.name}
            label={i.required ? `${i.name} (required)` : i.name}
            hint={i.description ?? undefined}
            value={values[i.name] ?? ""}
            onChange={(e) => setValues({ ...values, [i.name]: e.target.value })}
            placeholder={i.default ?? ""}
          />
        ))}
        <div className="flex min-w-0 flex-wrap items-end gap-2">
          {labels.length ? (
            <SelectField label="On" value={runsOn} onChange={setRunsOn} options={labels.map((l) => ({ value: l, label: l }))} className="min-w-0 flex-1 basis-40" />
          ) : (
            <p className="flex-1 text-[12.5px] text-[#a1a4ad]">No runner here has a label yet.</p>
          )}
          <Button size="sm" type="submit" variant="secondary" disabled={busy || blocked || !runsOn}>
            Try it
          </Button>
        </div>
        <p className="text-[12px] leading-[1.55] text-[#8c8f98]">It runs as the editor has it, saved or not, on {gitRef || "the default branch"}.</p>
        {error && (
          <p role="status" className="text-[12.5px] leading-[1.55] text-[var(--color-fail)] [overflow-wrap:anywhere]">
            {error}
          </p>
        )}
      </form>
      {started && (
        <React.Suspense fallback={<Hold height={160} className="mt-4" />}>
          <RunFollow key={started.run} run={started.run} trusted={started.trusted} step={`./${dir}`} />
        </React.Suspense>
      )}
    </Panel>
  );
}

/** A run started here, followed: its state, and the log of its step that uses `step` (and the job's
 *  outputs) as it goes. */
export function RunFollow({ run, trusted, step }: { run: string; trusted: boolean; step: string }) {
  // Asked again while it runs; once it's done, not.
  const [done, setDone] = React.useState(false);
  const detail = useSuspenseResource(`ci-try/${run}`, (s) => ciRun(run, undefined, s), { pollMs: done ? undefined : 1500 }).data;
  const finished = !!detail?.run.finished_at;
  if (finished && !done) setDone(true);
  const leg = detail?.legs[0];
  const state = detail ? (detail.run.conclusion ?? detail.run.status) : "starting";
  return (
    <section aria-label="Its run" className="mt-5 grid gap-3 border-t border-white/[0.07] pt-4">
      <p role="status" className="text-[12.5px] text-[#c9d0da]">
        <Link to={`/runs/${run}`} className="text-link">
          Run #{detail?.run.number ?? "…"}
        </Link>{" "}
        {state.replace(/_/g, " ")}
        {!trusted && <span className="text-[#8c8f98]"> · as a fork&apos;s: no secrets, a read-only token</span>}
      </p>
      {leg && <LegResult leg={leg.id} uses={step} live={!finished} />}
    </section>
  );
}

function LegResult({ leg, uses, live }: { leg: string; uses: string; live: boolean }) {
  const d = useSuspenseResource(`ci-try-leg/${leg}`, (s) => ci.legDetail(leg, s), { pollMs: live ? 1500 : undefined }).data;
  const step = d.steps?.find((s) => s.action === uses);
  const outputs = Object.entries(d.outputs ?? {});
  return (
    <>
      {outputs.length > 0 && (
        <table aria-label="Its outputs" className="w-full table-fixed font-mono text-[12px]">
          <tbody>
            {outputs.map(([k, v]) => (
              <tr key={k} className="border-b border-white/[0.06] last:border-0">
                <th scope="row" className="w-32 py-1.5 pr-2 text-left align-top font-normal text-[#a6c4ed] [overflow-wrap:anywhere]">
                  {k}
                </th>
                <td className="py-1.5 text-[#d4dde8] [overflow-wrap:anywhere] whitespace-pre-wrap">{v === "" ? <span className="text-[#6d7078]">empty</span> : v}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {step && (
        <React.Suspense fallback={<Hold height={120} />}>
          <LogViewer jobId={leg} step={step.idx} live={live} />
        </React.Suspense>
      )}
    </>
  );
}
