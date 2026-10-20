/**
 * The workflow editor (docs/ci.md W21-R01, W21-R06): a repository's workflow in the code editor,
 * checked as you type, with the plan it would make for an event beside it (W04-R29). The YAML is
 * the source. An edit is saved as its repository takes it (`ci.workflows.edit`): a diff on the
 * trunk of a repository kiln hosts, a pull or merge request on GitHub, GitLab or Bitbucket, or a
 * commit to a branch; for any repository the patch, Copy and Download take it away. Below, the
 * workflow's own secrets and variables (W09-R11).
 */
import * as React from "react";
import { Download } from "lucide-react";
import { api } from "@/api/client";
import { ci, type CiMatrix, type CiOutline, type CiPreview, type CiSave } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { CopyButton, Crumbs, EmptyState, Panel, SelectField, TextField } from "@/components/ui/misc";
import { Link, useSearchParams } from "@/lib/router";
import { unifiedPatch } from "@/lib/patch";
import { describe, keysAt, yamlSpot, type Schema } from "@/lib/yaml-spot";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import type { CompletionContext, CompletionResult, Diagnostic } from "@/components/code/editor";
import { ScopeSecrets } from "@/views/settings-ci";

const CodeEditor = React.lazy(() => import("@/components/code/editor"));

const EVENTS = ["push", "pull_request", "workflow_dispatch", "schedule"];

/** GitHub's hosted labels, offered after `runs-on:` beside the runners' own. */
const HOSTED = ["ubuntu-latest", "ubuntu-24.04", "ubuntu-22.04", "ubuntu-24.04-arm", "macos-latest", "macos-15", "windows-latest", "windows-2025", "self-hosted"];

export default function Workflows() {
  const [search, setSearch] = useSearchParams();
  const repo = search.get("repo") ?? "";
  const path = search.get("path") ?? "";
  const ref = search.get("ref") ?? "";
  const set = (next: Record<string, string>) => setSearch(Object.fromEntries(Object.entries({ repo, path, ref, ...next }).filter(([, v]) => v !== "")), { replace: true });
  return (
    <div className="container-console page">
      <title>{path ? `${path.split("/").pop()} · Workflows · kiln` : "Workflows · kiln"}</title>
      <Crumbs index={2} items={[{ label: "Runs", to: "/runs" }, { label: "Workflows" }]} />
      <h1 className="mt-5 text-[clamp(26px,3vw,40px)] leading-[1.1] font-[450] tracking-[-0.04em]">Workflows</h1>
      <p className="mt-3 max-w-2xl text-[14px] leading-[1.7] text-[#b0b4be]">Edit a workflow and see, as you type, what it would run and where.</p>
      <Loadable label="the repositories" fallback={<Hold height={64} className="mt-6" />}>
        <Picker repo={repo} path={path} gitRef={ref} onChange={set} />
      </Loadable>
      {repo && path ? (
        <>
          <Loadable label="the workflow" fallback={<Hold height={420} className="mt-6" />}>
            <WorkflowFile key={`${repo}\u0000${path}\u0000${ref}`} repo={repo} path={path} gitRef={ref} />
          </Loadable>
          <Loadable label="its secrets and variables" fallback={<Hold height={160} className="mt-8" />}>
            <section className="mt-8 max-w-3xl" aria-label="This workflow's secrets and variables">
              <ScopeSecrets key={`${repo}\u0000${path}`} scope={`workflow:${repo}:${path}`} of="This workflow's" />
            </section>
          </Loadable>
        </>
      ) : (
        <div className="mt-8">
          <EmptyState title="Choose a workflow.">Pick a repository and one of its workflows to edit it.</EmptyState>
        </div>
      )}
    </div>
  );
}

function Picker({ repo, path, gitRef, onChange }: { repo: string; path: string; gitRef: string; onChange: (next: Record<string, string>) => void }) {
  const repos = useSuspenseResource("repos/", (s) => api.repos(s)).data;
  const [branch, setBranch] = React.useState(gitRef);
  return (
    <div className="mt-6 flex flex-wrap items-end gap-3">
      <SelectField
        label="Repository"
        value={repo}
        onChange={(v) => onChange({ repo: v, path: "" })}
        options={[{ value: "", label: "Choose a repository" }, ...repos.map((r) => ({ value: r.name, label: r.name }))]}
        className="w-64"
      />
      <form
        onSubmit={(e) => {
          e.preventDefault();
          onChange({ ref: branch.trim() });
        }}
      >
        <TextField
          label="Branch, tag or commit"
          value={branch}
          onChange={(e) => setBranch(e.target.value)}
          onBlur={() => branch.trim() !== gitRef && onChange({ ref: branch.trim() })}
          placeholder="default branch"
          className="w-52"
        />
      </form>
      {repo && (
        <Loadable label="the workflows" fallback={<Hold height={40} className="w-72" />}>
          <WorkflowSelect repo={repo} gitRef={gitRef} path={path} onChange={(p) => onChange({ path: p })} />
        </Loadable>
      )}
    </div>
  );
}

function WorkflowSelect({ repo, gitRef, path, onChange }: { repo: string; gitRef: string; path: string; onChange: (p: string) => void }) {
  const list = useSuspenseResource(`ci-workflows/${repo}/${gitRef}`, (s) => ci.workflows(repo, gitRef, s)).data.workflows;
  return (
    <SelectField
      label="Workflow"
      value={path}
      onChange={onChange}
      options={[
        { value: "", label: list.length ? "Choose a workflow" : "No workflows here" },
        ...list.map((w) => ({ value: w.path, label: `${w.path}${w.errors ? ` · ${w.errors} error${w.errors === 1 ? "" : "s"}` : ""}` })),
      ]}
      className="w-80"
    />
  );
}

/** The job a position is in: the last `  <id>:` under `jobs:` before it. */
function jobAt(text: string): string | null {
  const lines = text.split("\n");
  let inJobs = false;
  let job: string | null = null;
  for (const l of lines) {
    if (/^jobs:\s*(#.*)?$/.test(l)) {
      inJobs = true;
      continue;
    }
    if (/^\S/.test(l)) inJobs = false;
    const m = inJobs ? /^ {2}([A-Za-z_][\w-]*):\s*(#.*)?$/.exec(l) : null;
    if (m?.[1]) job = m[1];
  }
  return job;
}

function WorkflowFile({ repo, path, gitRef }: { repo: string; path: string; gitRef: string }) {
  const file = useSuspenseResource(`ci-workflow/${repo}/${path}/${gitRef}`, (s) => ci.workflow(repo, path, gitRef, s)).data;
  // Saving (W21-R06): what the repository takes, from where it is kept; the patch for any.
  const saves = file.saves ?? ["patch"];
  const change = file.host === "gitlab" ? { one: "a merge request", title: "Merge request" } : { one: "a pull request", title: "Pull request" };
  const [text, setText] = React.useState(file.source);
  const [saving, setSaving] = React.useState(false);
  const [saved, setSaved] = React.useState<{ tone?: "error"; text: string; diff?: string; url?: string } | null>(null);
  // The branch a commit goes to, while it's being named.
  const [branch, setBranch] = React.useState<string | null>(null);
  const [counts, setCounts] = React.useState<{ errors: number; warnings: number } | null>(null);
  const [event, setEvent] = React.useState(EVENTS[0] ?? "push");
  const [preview, setPreview] = React.useState<CiPreview | null>(null);
  const [previewError, setPreviewError] = React.useState<string | null>(null);
  const known = React.useRef<{
    outline: CiOutline | null;
    names?: { secrets: string[]; variables: string[] };
    catalog?: { label: string; detail: string }[];
    schema?: Schema;
    schemaAsked?: boolean;
    timer?: number;
    asked?: number;
  }>({
    outline: file.outline,
  });
  // The file's schema (W02-R09): the keys a mapping may have, and what each is (W21-R01).
  const kind = path.startsWith(".github/workflows/") ? "gha" : /(^|\/)action\.ya?ml$/.test(path) ? "action" : "kiln";
  const schema = (): Schema | undefined => {
    if (!known.current.schemaAsked) {
      known.current.schemaAsked = true;
      ci.schema(kind).then(
        (s) => (known.current.schema = s),
        () => (known.current.schemaAsked = false),
      );
    }
    return known.current.schema;
  };
  const hover = (content: string, pos: number) => {
    const s = schema();
    const spot = s ? yamlSpot(content, pos) : null;
    if (!s || !spot?.key) return null;
    const text = describe(s, [...spot.path, spot.key.name]);
    return text ? { from: spot.key.from, to: spot.key.to, text } : null;
  };

  const ask = (content: string, ev: string) => {
    window.clearTimeout(known.current.timer);
    known.current.timer = window.setTimeout(async () => {
      const n = (known.current.asked ?? 0) + 1;
      known.current.asked = n;
      try {
        const p = await ci.preview({ repo, ref: gitRef, sha: file.sha, path, content, event: ev });
        if (known.current.asked === n) {
          setPreview(p);
          setPreviewError(null);
        }
      } catch (e) {
        if (known.current.asked === n) setPreviewError(e instanceof Error ? e.message : String(e));
      }
    }, 400);
  };

  const lint = async (content: string): Promise<Diagnostic[]> => {
    schema();
    const r = await ci.validate(path, content, repo);
    known.current.outline = r.outline ?? known.current.outline;
    const errors = r.diagnostics.filter((d) => d.severity === "error").length;
    setCounts({ errors, warnings: r.diagnostics.length - errors });
    if (errors === 0) ask(content, event);
    const end = content.length;
    const starts = [0];
    for (let i = 0; i < content.length; i++) if (content[i] === "\n") starts.push(i + 1);
    // Positions by line and column, which every diagnostic has.
    const at = (p: { line: number; col: number }) => {
      const line = starts[Math.max(0, p.line - 1)] ?? end;
      const next = starts[p.line] ?? end + 1;
      return Math.min(line + Math.max(0, p.col - 1), next - 1, end);
    };
    return r.diagnostics.map((d) => {
      const from = at(d.range.start);
      return {
        from,
        to: Math.min(Math.max(at(d.range.end), from + 1), end),
        severity: d.severity === "error" ? "error" : d.severity === "warning" ? "warning" : "info",
        message: `${d.code}: ${d.message}`,
      };
    });
  };

  const complete = async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const outline = known.current.outline;
    const before = ctx.state.doc.sliceString(0, ctx.pos);
    const job = outline?.jobs.find((j) => j.id === jobAt(before));
    // `needs.<job>.outputs.<name>`, then `needs.<job>.` and `steps.<id>.`, then the namespaces.
    const out = ctx.matchBefore(/\bneeds\.([\w-]+)\.outputs\.[\w-]*$/);
    if (out) {
      const id = /needs\.([\w-]+)\./.exec(out.text)?.[1];
      const names = outline?.jobs.find((j) => j.id === id)?.outputs ?? [];
      return { from: out.to - (/[\w-]*$/.exec(out.text)?.[0].length ?? 0), options: names.map((label) => ({ label, detail: "output" })) };
    }
    const member = ctx.matchBefore(/\b(needs|steps)\.[\w-]+\.[\w-]*$/);
    if (member) {
      const fields = member.text.startsWith("needs") ? ["outputs", "result"] : ["outputs", "outcome", "conclusion"];
      return { from: member.to - (/[\w-]*$/.exec(member.text)?.[0].length ?? 0), options: fields.map((label) => ({ label })) };
    }
    const ns = ctx.matchBefore(/\b(needs|steps|matrix|inputs|env|secrets|vars|jobs)\.[\w-]*$/);
    if (ns) {
      const space = ns.text.slice(0, ns.text.indexOf("."));
      if ((space === "secrets" || space === "vars") && !known.current.names) known.current.names = await ci.names(repo, path).catch(() => ({ secrets: [], variables: [] }));
      const names: Record<string, string[]> = {
        needs: job?.needs.length ? job.needs : (outline?.jobs.map((j) => j.id).filter((id) => id !== job?.id) ?? []),
        steps: job?.step_ids ?? [],
        matrix: job?.matrix_axes ?? [],
        inputs: outline?.inputs ?? [],
        env: outline?.env ?? [],
        secrets: ["GITHUB_TOKEN", ...(known.current.names?.secrets ?? [])],
        vars: known.current.names?.variables ?? [],
        jobs: outline?.jobs.map((j) => j.id) ?? [],
      };
      return { from: ns.from + space.length + 1, options: (names[space] ?? []).map((label) => ({ label, detail: space })) };
    }
    const uses = ctx.matchBefore(/\buses:\s*[\w./@-]*$/);
    if (uses) {
      if (!known.current.catalog) {
        const list = await ci.catalog("").catch(() => ({ actions: [] }));
        known.current.catalog = list.actions.flatMap((a) => a.versions.slice(0, 3).map((v) => ({ label: `${a.uses}@${v.ref}`, detail: a.action?.description?.split("\n")[0] ?? v.sha.slice(0, 12) })));
      }
      return { from: uses.from + (/^uses:\s*/.exec(uses.text)?.[0].length ?? 0), options: known.current.catalog };
    }
    const runsOn = ctx.matchBefore(/\bruns-on:\s*[\w.-]*$/);
    if (runsOn) return { from: runsOn.from + (/^runs-on:\s*/.exec(runsOn.text)?.[0].length ?? 0), options: HOSTED.map((label) => ({ label, detail: "label" })) };
    // A key being typed at a line's start (or asked for there): the keys its mapping may have.
    const typing = ctx.matchBefore(/^\s*(?:-\s+)*[\w.-]*$/);
    const word = /[\w.-]*$/.exec(typing?.text ?? "")?.[0] ?? "";
    if (typing && (word || ctx.explicit)) {
      const s = schema() ?? (known.current.schema = await ci.schema(kind).catch(() => undefined));
      const spot = s ? yamlSpot(ctx.state.doc.toString(), ctx.pos) : null;
      if (s && spot) return { from: ctx.pos - word.length, options: keysAt(s, spot.path).map((k) => ({ label: k.key, info: k.description, apply: `${k.key}: ` })) };
    }
    return null;
  };

  const changed = text !== file.source;
  const blocked = saving || (counts?.errors ?? 0) > 0;
  const save = async (to: CiSave, onto?: string) => {
    setSaving(true);
    setSaved(null);
    try {
      const r = await ci.saveWorkflow(repo, path, { content: text, to, branch: onto });
      const d = r.diffs?.[0];
      if (d) setSaved({ text: `${d.name} opened: it lands through review.`, diff: d.name });
      else if (r.change) setSaved({ text: ` #${r.change.number} opened on ${r.branch ?? "a new branch"}.`, url: r.change.url });
      else if (r.branch) {
        setSaved({ text: `Committed to ${r.branch} (${(r.sha ?? "").slice(0, 7)}).` });
        setBranch(null);
      } else setSaved({ text: "Saved." });
    } catch (e) {
      setSaved({ tone: "error", text: errorMessage(e) });
    } finally {
      setSaving(false);
    }
  };
  const patch = () => {
    const url = URL.createObjectURL(new Blob([unifiedPatch(path, file.source, text)], { type: "text/x-diff" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `${path.split("/").pop() ?? "workflow"}.patch`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: "text/yaml" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = path.split("/").pop() ?? "workflow.yml";
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="mt-6 grid gap-4 xl:grid-cols-[minmax(0,1.35fr)_minmax(0,1fr)]">
      <Panel
        eyebrow={`${file.ref || "default branch"} · ${file.sha.slice(0, 7)}`}
        title={path}
        actions={
          <>
            <CopyButton text={text} label="Copy" />
            <Button size="xs" variant="ghost" onClick={download}>
              <Download /> Download
            </Button>
          </>
        }
        bodyClassName="px-0 pb-2"
        caption={
          counts && (counts.errors || counts.warnings ? `${counts.errors} error${counts.errors === 1 ? "" : "s"}, ${counts.warnings} warning${counts.warnings === 1 ? "" : "s"}` : "No problems.")
        }
      >
        <React.Suspense fallback={<Hold height={420} />}>
          <CodeEditor value={file.source} onChange={setText} lint={lint} complete={complete} hover={hover} label={`Editing ${path}`} />
        </React.Suspense>
        {/* Under the edit, so a phone's header keeps to Copy and Download. */}
        {(changed || saved) && (
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-5 pt-3">
            <p className={`min-w-0 flex-1 basis-64 text-[12.5px] leading-[1.55] [overflow-wrap:anywhere] ${saved?.tone === "error" ? "text-[var(--color-fail)]" : "text-[#a1a4ad]"}`} role="status">
              {saved?.diff ? (
                <>
                  <Link to={`/review/${encodeURIComponent(saved.diff)}`} className="text-link">
                    {saved.diff}
                  </Link>{" "}
                  opened: it lands through review.
                </>
              ) : saved?.url ? (
                <>
                  <a href={saved.url} target="_blank" rel="noreferrer" className="text-link">
                    {change.title}
                  </a>
                  {saved.text}
                </>
              ) : (
                saved?.text
              )}
            </p>
            {changed && (
              <div className="flex flex-wrap gap-2">
                <Button size="xs" variant="ghost" onClick={patch}>
                  The patch
                </Button>
                {saves.includes("branch") && branch === null && (
                  <Button size="xs" variant="ghost" disabled={blocked} onClick={() => setBranch("")}>
                    To a branch
                  </Button>
                )}
                {saves.includes("change") && (
                  <Button size="xs" variant="secondary" disabled={blocked} onClick={() => void save("change")}>
                    Open {change.one}
                  </Button>
                )}
                {saves.includes("diff") && (
                  <Button size="xs" variant="secondary" disabled={blocked} onClick={() => void save("diff")}>
                    Open a diff
                  </Button>
                )}
              </div>
            )}
            {changed && branch !== null && (
              <form
                className="flex w-full flex-wrap items-end gap-2"
                onSubmit={(e) => {
                  e.preventDefault();
                  if (branch.trim()) void save("branch", branch.trim());
                }}
              >
                <TextField id="wf-save-branch" label="Branch" className="min-w-0 flex-1" value={branch} onChange={(e) => setBranch(e.target.value)} placeholder="ci/faster-tests" autoFocus />
                <Button size="xs" type="submit" variant="secondary" disabled={blocked || !branch.trim()}>
                  Commit
                </Button>
                <Button size="xs" type="button" variant="ghost" onClick={() => setBranch(null)}>
                  Cancel
                </Button>
              </form>
            )}
          </div>
        )}
      </Panel>
      <PreviewPane
        preview={preview}
        error={previewError}
        event={event}
        onEvent={(ev) => {
          setEvent(ev);
          ask(text, ev);
        }}
      />
    </div>
  );
}

/** A job's matrix as written (W21-R06): its axes as a table, then `include`, `exclude` and `where` as rows. */
function MatrixView({ m }: { m: CiMatrix }) {
  const value = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));
  const each = (v: unknown): unknown[] => (Array.isArray(v) ? v : v == null ? [] : [v]);
  const pairs = (o: unknown) =>
    o && typeof o === "object" && !Array.isArray(o)
      ? Object.entries(o)
          .map(([k, v]) => `${k}: ${value(v)}`)
          .join(", ")
      : value(o);
  const row = (key: string, label: string, text: string) => (
    <tr key={key}>
      <th scope="row" className="w-0 pr-3 text-left align-top font-normal whitespace-nowrap text-[#93a1b4]">
        {label}
      </th>
      <td className="[overflow-wrap:anywhere] text-[#d4dde8]">{text}</td>
    </tr>
  );
  if (m.expr) return <p className="mb-2 font-mono text-[11.5px] leading-[1.6] [overflow-wrap:anywhere] text-[#a6b2c6]">matrix: {m.expr}</p>;
  return (
    <table className="mb-2 w-full font-mono text-[11.5px] leading-[1.6]" aria-label="Its matrix">
      <tbody>
        {m.axes?.map((a) => row(`a-${a.name}`, a.name, Array.isArray(a.values) ? a.values.map(value).join(" · ") : a.values))}
        {m.platforms != null && row("platforms", "platforms", Array.isArray(m.platforms) ? m.platforms.map(value).join(" · ") : value(m.platforms))}
        {each(m.include).map((r, i) => row(`i${i}`, "include", pairs(r)))}
        {each(m.exclude).map((r, i) => row(`e${i}`, "exclude", pairs(r)))}
        {m.where && row("where", "where", m.where)}
      </tbody>
    </table>
  );
}

function PreviewPane({ preview, error, event, onEvent }: { preview: CiPreview | null; error: string | null; event: string; onEvent: (e: string) => void }) {
  const plan = preview?.plan;
  const byJob = new Map<string, NonNullable<typeof plan>["legs"]>();
  for (const l of plan?.legs ?? []) byJob.set(l.job, [...(byJob.get(l.job) ?? []), l]);
  const title = !preview ? "Waiting for a clean check…" : !preview.runs ? "It wouldn't run" : `${plan?.legs.length ?? 0} leg${plan?.legs.length === 1 ? "" : "s"} would run`;
  return (
    <Panel
      eyebrow="Plan preview"
      title={title}
      actions={<SelectField label="Event" hideLabel value={event} onChange={onEvent} options={EVENTS.map((e) => ({ value: e, label: e.replace(/_/g, " ") }))} className="w-44" />}
    >
      {error && <p className="font-mono text-[11.5px] text-[#d1aaad]">{error}</p>}
      {preview && !preview.runs && (preview.why || preview.policy?.length) ? (
        <ul className="mb-3 flex flex-col gap-1.5">
          {preview.why && <li className="text-[13px] text-[#d4dde8]">{preview.why}</li>}
          {preview.policy?.map((p) => (
            <li key={p} className="font-mono text-[11.5px] text-[#d1aaad]">
              {p}
            </li>
          ))}
        </ul>
      ) : null}
      {plan?.error && <p className="font-mono text-[11.5px] text-[#d1aaad]">{plan.error}</p>}
      {plan && (
        <div className="flex flex-col gap-4">
          {[...byJob].map(([job, legs]) => (
            <section key={job} aria-label={`Job ${job}`}>
              {(legs.length > 1 || legs[0]?.key !== job) && (
                <div className="label mb-1.5">
                  {job}
                  {legs.length > 1 ? ` · ${legs.length} legs` : ""}
                  {plan.removed[job]?.length ? ` · ${plan.removed[job]?.length} removed` : ""}
                </div>
              )}
              {preview?.matrices?.[job] && <MatrixView m={preview.matrices[job]} />}
              <ul className="flex flex-col gap-1">
                {legs.map((l) => {
                  const fits = l.routing?.runners.filter((r) => r.eligible) ?? [];
                  const where = fits.length
                    ? `runs on ${fits.map((r) => r.name).join(", ")}`
                    : l.routing
                      ? `no runner can take it now${l.routing.labels.length ? ` (needs ${l.routing.labels.join(", ")})` : ""}`
                      : "";
                  return (
                    <li key={l.key} className="flex flex-col font-mono text-[11.5px] leading-[1.6]">
                      <span className="text-[#d4dde8]">{l.key}</span>
                      {where && <span className={fits.length ? "text-[#93a1b4]" : "text-[#d1aaad]"}>{where}</span>}
                    </li>
                  );
                })}
              </ul>
            </section>
          ))}
          {plan.decided.length > 0 && (
            <section aria-label="Decided now">
              <div className="label mb-1.5">Decided now</div>
              <ul className="flex flex-col gap-1">
                {plan.decided.map((d) => (
                  <li key={`${d.job}-${d.leg ?? ""}`} className="font-mono text-[11.5px] leading-[1.6] text-[#a6b2c6]">
                    {d.leg ?? d.job}: {d.reason}
                  </li>
                ))}
              </ul>
            </section>
          )}
          {Object.keys(plan.removed).length > 0 && (
            <section aria-label="Removed combinations">
              <div className="label mb-1.5">Removed combinations</div>
              <ul className="flex flex-col gap-1">
                {Object.entries(plan.removed).flatMap(([job, list]) =>
                  list.map(([combo, why], i) => (
                    <li key={`${job}-${i}`} className="font-mono text-[11.5px] leading-[1.6] [overflow-wrap:anywhere] text-[#a6b2c6]">
                      {job} {JSON.stringify(combo)}: {why}
                    </li>
                  )),
                )}
              </ul>
            </section>
          )}
          {plan.later.length > 0 && <p className="font-mono text-[11.5px] leading-[1.6] text-[#93a1b4]">Decided when their needs finish: {plan.later.join(", ")}.</p>}
        </div>
      )}
    </Panel>
  );
}
