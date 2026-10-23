/**
 * The image builder (docs/ci.md W21-R08, W13-R11): a runner image's definition in the code editor,
 * a Dockerfile or an Agentfile, checked as it's typed (a Dockerfile by kiln's lints; an Agentfile is
 * shards' to check, where it builds), built once as it is here (a run, followed here), pushed to
 * kiln's registry with it (the server owner's), or saved to its repository as a diff.
 */
import * as React from "react";
import { api } from "@/api/client";
import { ci } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { SelectField, TextField } from "@/components/ui/misc";
import { Segmented } from "@/components/ui/tabs";
import { Link } from "@/lib/router";
import { placed } from "@/lib/diagnostics";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import type { Diagnostic } from "@/components/code/editor";
import { RunFollow } from "@/views/ci/action-view";

const CodeEditor = React.lazy(() => import("@/components/code/editor"));

type Kind = "dockerfile" | "agentfile";

const STARTER: Record<Kind, string> = {
  dockerfile: "FROM ubuntu:24.04\nRUN apt-get update \\\n    && apt-get install -y --no-install-recommends ca-certificates curl git \\\n    && rm -rf /var/lib/apt/lists/*\n",
  agentfile: "# A Dockerfile's vocabulary, and shards' own: AGENT, HARNESS, SKILL, MCP, NETWORK, CONNECT, ATTACH.\nFROM ubuntu:24.04\n",
};

const PLATFORMS = ["linux/amd64", "linux/arm64"];

/** An image's name, as kiln's registry takes one. */
const named = (n: string) => /^[a-z0-9]+(?:[._-][a-z0-9]+)*$/.test(n);

export function ImageBuilder({ onClose }: { onClose: () => void }) {
  const repos = useSuspenseResource("repos/", (s) => api.repos(s)).data;
  const [repo, setRepo] = React.useState(repos[0]?.name ?? "");
  const [name, setName] = React.useState("");
  const [kind, setKind] = React.useState<Kind>("dockerfile");
  const [text, setText] = React.useState(STARTER.dockerfile);
  const [counts, setCounts] = React.useState<{ errors: number; warnings: number; note?: string } | null>(null);
  const [platforms, setPlatforms] = React.useState<string[]>([PLATFORMS[0] ?? "linux/amd64"]);
  const [busy, setBusy] = React.useState(false);
  const [said, setSaid] = React.useState<{ tone?: "error"; text: string; diff?: string } | null>(null);
  const [started, setStarted] = React.useState<{ run: string; trusted: boolean } | null>(null);
  const ok = named(name) && !!repo && (counts?.errors ?? 0) === 0;

  const lint = async (content: string): Promise<Diagnostic[]> => {
    const r = await ci.validateImage(kind, content);
    const errors = r.diagnostics.filter((d) => d.severity === "error").length;
    setCounts({ errors, warnings: r.diagnostics.length - errors, note: r.checked ? undefined : r.note });
    return placed(content, r.diagnostics);
  };
  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setSaid(null);
    try {
      await f();
    } catch (e) {
      setSaid({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };
  const caption =
    counts?.note ??
    (counts ? (counts.errors || counts.warnings ? `${counts.errors} error${counts.errors === 1 ? "" : "s"}, ${counts.warnings} warning${counts.warnings === 1 ? "" : "s"}` : "No problems.") : null);

  return (
    <section aria-label="A new image" className="grid gap-4 border-t border-white/[0.07] px-5 py-5">
      <div className="flex min-w-0 flex-wrap items-end gap-3">
        <TextField label="Its name" className="w-48" value={name} onChange={(e) => setName(e.target.value.trim())} placeholder="tools" />
        <SelectField label="Repository" value={repo} onChange={setRepo} options={repos.map((r) => ({ value: r.name, label: r.name }))} className="min-w-0 w-56" />
        <Segmented
          label="Kind"
          value={kind}
          onChange={(k) => {
            // A definition of the other kind starts from its own example.
            setKind(k);
            if (text === STARTER[kind]) setText(STARTER[k]);
            setCounts(null);
          }}
          options={[
            { value: "dockerfile", label: "Dockerfile" },
            { value: "agentfile", label: "Agentfile" },
          ]}
        />
      </div>
      {name && !named(name) && <p className="text-[12.5px] text-[var(--color-fail)]">An image&apos;s name is lowercase letters and digits, with `.`, `_` or `-` between them.</p>}
      <div className="rounded-[10px] border border-white/10">
        <React.Suspense fallback={<Hold height={260} />}>
          <CodeEditor key={kind} language={kind} value={text} onChange={setText} lint={lint} label={`The image's ${kind === "dockerfile" ? "Dockerfile" : "Agentfile"}`} />
        </React.Suspense>
        {caption && <p className="border-t border-white/[0.07] px-4 py-2 text-[12px] text-[#a1a4ad]">{caption}</p>}
      </div>
      <div className="flex min-w-0 flex-wrap items-end gap-x-4 gap-y-3">
        <fieldset className="grid gap-1.5">
          <legend className="field-label">Platforms</legend>
          <div className="flex gap-3">
            {PLATFORMS.map((p) => (
              <label key={p} className="flex items-center gap-1.5 font-mono text-[12px] text-[#c9d0da]">
                <input type="checkbox" checked={platforms.includes(p)} onChange={(e) => setPlatforms(e.target.checked ? [...platforms, p] : platforms.filter((x) => x !== p))} />
                {p}
              </label>
            ))}
          </div>
        </fieldset>
        {repo && (
          <React.Suspense fallback={<Hold height={40} className="w-48" />}>
            <BuildButtons
              key={repo}
              repo={repo}
              disabled={busy || !ok}
              build={(runsOn, push) =>
                act(async () => {
                  const r = await ci.buildImage(repo, { name, kind, content: text, platforms, runs_on: runsOn, push });
                  setStarted({ run: r.run, trusted: r.trusted });
                })
              }
            />
          </React.Suspense>
        )}
        <Button
          size="xs"
          variant="ghost"
          disabled={busy || !ok}
          onClick={() =>
            void act(async () => {
              const d = (await ci.proposeImage(repo, { name, kind, content: text })).diffs[0];
              setSaid(d ? { text: "opened: it lands through review.", diff: d.name } : { text: "Proposed." });
            })
          }
        >
          Save to repo
        </Button>
        <Button size="xs" variant="ghost" onClick={onClose}>
          Close
        </Button>
      </div>
      {said && (
        <p role="status" className={`text-[12.5px] leading-[1.55] [overflow-wrap:anywhere] ${said.tone === "error" ? "text-[var(--color-fail)]" : "text-[#a1a4ad]"}`}>
          {said.diff && (
            <>
              <Link to={`/review/${encodeURIComponent(said.diff)}`} className="text-link">
                {said.diff}
              </Link>{" "}
            </>
          )}
          {said.text}
        </p>
      )}
      {started && (
        <React.Suspense fallback={<Hold height={160} />}>
          <RunFollow key={started.run} run={started.run} trusted={started.trusted} step="kiln/build-image@v1" />
        </React.Suspense>
      )}
    </section>
  );
}

/** Where it's built (this server's runners' labels), and the two ways: built only, or pushed too. */
function BuildButtons({ repo, disabled, build }: { repo: string; disabled: boolean; build: (runsOn: string, push: boolean) => void }) {
  const labels = useSuspenseResource(`ci-labels/${repo}`, (s) => ci.labels(repo, s)).data.labels.map((l) => l.label);
  const [runsOn, setRunsOn] = React.useState(labels[0] ?? "");
  return (
    <div className="flex min-w-0 flex-wrap items-end gap-2">
      <SelectField label="On" value={runsOn} onChange={setRunsOn} options={labels.map((l) => ({ value: l, label: l }))} className="min-w-0 w-44" />
      <Button size="xs" variant="secondary" disabled={disabled || !runsOn} onClick={() => build(runsOn, false)}>
        Build
      </Button>
      <Button size="xs" variant="secondary" disabled={disabled || !runsOn} onClick={() => build(runsOn, true)}>
        Build and push
      </Button>
    </div>
  );
}
