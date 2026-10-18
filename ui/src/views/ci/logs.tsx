/**
 * Log search (docs/ci.md W19-R04): a string in the step logs of every run, newest first, with
 * the filters in the URL (repository, branch, conclusion, window). Each hit opens its leg at the
 * step it's in.
 */
import * as React from "react";
import { Search } from "lucide-react";
import { api } from "@/api/client";
import { ci, ciState, type CiLogHit } from "@/api/ci";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, SelectField, TextField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { relativeTime } from "@/lib/format";
import { Link, useSearchParams } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";

type Window = "7" | "30" | "90";
const WINDOWS: { value: Window; label: string }[] = [
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
];

const CONCLUSIONS = [
  { value: "", label: "Any conclusion" },
  { value: "failure", label: "Failed" },
  { value: "success", label: "Succeeded" },
  { value: "cancelled", label: "Cancelled" },
];

export default function Logs() {
  const [search, setSearch] = useSearchParams();
  const q = search.get("q") ?? "";
  const repo = search.get("repo") ?? "";
  const branch = search.get("branch") ?? "";
  const conclusion = search.get("conclusion") ?? "";
  const win = (WINDOWS.find((w) => w.value === search.get("days"))?.value ?? "30") as Window;
  const set = (next: Record<string, string>) =>
    setSearch(Object.fromEntries(Object.entries({ q, repo, branch, conclusion, days: win, ...next }).filter(([k, v]) => v !== "" && !(k === "days" && v === "30"))), { replace: true });
  const [text, setText] = React.useState(q);
  const [branchText, setBranchText] = React.useState(branch);
  return (
    <div className="container-console page">
      <title>{q ? `${q} · Search logs · kiln` : "Search logs · kiln"}</title>
      <Crumbs index={2} items={[{ label: "Runs", to: "/runs" }, { label: "Search logs" }]} />
      <h1 className="mt-5 text-[clamp(26px,3vw,40px)] leading-[1.1] font-[450] tracking-[-0.04em]">Search logs</h1>
      <p className="mt-3 max-w-2xl text-[14px] leading-[1.7] text-[#b0b4be]">Find a line in the step logs of every run: when an error started, and how often it comes.</p>
      <form
        className="mt-6 flex flex-wrap items-end gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          set({ q: text.trim(), branch: branchText.trim() });
        }}
      >
        <TextField label="Text" value={text} onChange={(e) => setText(e.target.value)} placeholder="error[E0308], ECONNRESET, panicked at…" className="w-80 max-sm:w-full" />
        <Loadable label="the repositories" fallback={<Hold height={44} className="w-56" />}>
          <RepoSelect value={repo} onChange={(v) => set({ repo: v })} />
        </Loadable>
        <TextField
          label="Branch"
          value={branchText}
          onChange={(e) => setBranchText(e.target.value)}
          onBlur={() => branchText.trim() !== branch && set({ branch: branchText.trim() })}
          placeholder="any"
          className="w-40"
        />
        <SelectField label="Conclusion" value={conclusion} onChange={(v) => set({ conclusion: v })} options={CONCLUSIONS} className="w-44" />
        <Button type="submit" variant="secondary">
          <Search /> Search
        </Button>
      </form>
      <div className="mt-4">
        <Segmented label="Window" value={win} onChange={(w) => set({ days: w })} options={WINDOWS} />
      </div>
      {q.length >= 3 ? (
        <Loadable label="the search" fallback={<Hold height={320} className="mt-6" />}>
          <Hits q={q} repo={repo} branch={branch} conclusion={conclusion} days={Number(win)} />
        </Loadable>
      ) : (
        <div className="mt-8">
          <EmptyState title={q ? "Search for three characters or more." : "Search the logs."}>Every finished leg's steps are indexed, newest first.</EmptyState>
        </div>
      )}
    </div>
  );
}

function RepoSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const repos = useSuspenseResource("repos/", (s) => api.repos(s)).data;
  return (
    <SelectField label="Repository" value={value} onChange={onChange} options={[{ value: "", label: "Every repository" }, ...repos.map((r) => ({ value: r.name, label: r.name }))]} className="w-56" />
  );
}

/** The line, with what matched marked. */
function Line({ hit, q }: { hit: CiLogHit; q: string }) {
  const end = hit.at + new TextEncoder().encode(q).length;
  const bytes = new TextEncoder().encode(hit.text);
  const dec = new TextDecoder();
  const [pre, mid, post] = [dec.decode(bytes.slice(0, hit.at)), dec.decode(bytes.slice(hit.at, end)), dec.decode(bytes.slice(end))];
  return (
    <code className="block font-mono text-[12px] leading-[1.6] [overflow-wrap:anywhere] whitespace-pre-wrap text-[#c8cad0]">
      {pre}
      <mark className="rounded-[3px] bg-[#b8a6d5]/25 px-0.5 text-[#f1e7ff]">{mid}</mark>
      {post}
    </code>
  );
}

function Hits({ q, repo, branch, conclusion, days }: { q: string; repo: string; branch: string; conclusion: string; days: number }) {
  const d = useSuspenseResource(`logs/search/${q}/${repo}/${branch}/${conclusion}/${days}`, (s) => ci.searchLogs({ q, repo, branch, conclusion, days }, s)).data;
  // Hits by leg and step, in the order they came (newest first).
  const groups: { key: string; first: CiLogHit; hits: CiLogHit[] }[] = [];
  for (const h of d.hits) {
    const key = `${h.leg}\u0000${h.step}`;
    const g = groups.find((x) => x.key === key);
    if (g) g.hits.push(h);
    else groups.push({ key, first: h, hits: [h] });
  }
  return (
    <div className="mt-6">
      <p className="font-mono text-[11px] text-[#7b7e88]">
        {d.hits.length === 0 ? "No line matches" : `${d.hits.length}${d.hits.length >= 100 ? "+" : ""} line${d.hits.length === 1 ? "" : "s"}`} · {d.took_ms} ms
        {d.unindexed_legs > 0 && ` · ${d.unindexed_legs} finished leg${d.unindexed_legs === 1 ? "" : "s"} not indexed yet`}
      </p>
      {groups.length > 0 && (
        <ul className="mt-3 grid gap-3">
          {groups.map(({ key, first: h, hits }) => (
            <li key={key} className="rounded-[10px] border border-[#1d1f23] px-4 py-3">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <StatusDot status={ciState("", h.conclusion)} />
                <Link to={`/runs/${encodeURIComponent(h.run)}/jobs/${encodeURIComponent(h.leg)}?step=${h.step}`} className="font-mono text-[12.5px] text-[#e4e5e7] hover:text-white">
                  {h.job} › {h.step_name ?? `step ${h.step + 1}`}
                </Link>
                <span className="font-mono text-[11px] text-[#7b7e88]">
                  #{h.number} · {h.repo_name ?? h.repo} · {h.workflow.split("/").pop()} · {h.branch} · {relativeTime(h.created_at)}
                </span>
              </div>
              <ul className="mt-2 grid gap-1">
                {hits.map((x) => (
                  <li key={x.line} className="grid grid-cols-[3.5rem_1fr] gap-3">
                    <span className="text-right font-mono text-[11px] leading-[1.6] text-[#6d7078] tabular-nums">{x.line}</span>
                    <Line hit={x} q={q} />
                  </li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
