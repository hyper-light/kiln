/**
 * Comparing two runs, or two legs (docs/ci.md W19-R03): what differs from the base to this run
 * (actions, variables, environment, images, machine, runner, cache), the files changed between
 * their commits, and each leg's steps side by side. `?with=` names the base (a run's id, or
 * `last-green`, the default: the last run of this workflow and branch that succeeded before it);
 * `?leg=` compares that leg alone.
 */
import { ci, ciState, type CiCompare, type CiDifference } from "@/api/ci";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, EmptyState, Panel } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { formatDuration, shortSha } from "@/lib/format";
import { Link, useParams, useSearchParams } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";

/** A link inside a sentence, as the metrics page's. */
const INLINE = "text-[#d1c6e6] underline decoration-[#635b73] underline-offset-4 hover:text-[#f1e7ff]";

/** What each kind of difference is called. */
const KINDS: Record<string, string> = {
  action: "Action",
  var: "Variable",
  leg: "Leg",
  conclusion: "Conclusion",
  runner: "Runner",
  executor: "Executor",
  machine: "Machine",
  version: "Runner version",
  image: "Image",
  env: "Environment",
  cache: "Cache",
};

export default function Compare() {
  const { id = "" } = useParams();
  const [search] = useSearchParams();
  const other = search.get("with") || "last-green";
  const leg = search.get("leg") ?? "";
  return (
    <div className="container-console page">
      <title>Compare · kiln</title>
      <Loadable label="the comparison" fallback={<Hold height={420} className="mt-6" />}>
        <CompareBody run={id} other={other} leg={leg} />
      </Loadable>
    </div>
  );
}

function CompareBody({ run, other, leg }: { run: string; other: string; leg: string }) {
  const d = useSuspenseResource(`ci-compare/${run}/${other}/${leg}`, (s) => (leg ? ci.compareLegs(leg, other, s) : ci.compare(run, other, s))).data;
  const { base, head } = d;
  const legName = leg ? d.legs[0]?.key : null;
  return (
    <>
      <Crumbs index={2} items={[{ label: "Runs", to: "/runs" }, { label: `#${head.number}`, to: `/runs/${encodeURIComponent(head.id)}` }, { label: "Compare" }]} />
      <h1 className="mt-5 text-[clamp(26px,3vw,40px)] leading-[1.1] font-[450] tracking-[-0.04em] break-words">
        {legName ? `${legName}: ` : ""}#{head.number} against #{base.number}
      </h1>
      <p className="mt-3 max-w-2xl text-[14px] leading-[1.7] text-[#b0b4be]">
        {other === "last-green" ? "What changed since the last run of this workflow and branch that succeeded" : "What changed from one run to the other"}:{" "}
        <Link to={`/runs/${encodeURIComponent(base.id)}`} className={INLINE}>
          #{base.number}
        </Link>{" "}
        at <span className="font-mono text-[12px]">{shortSha(base.sha)}</span>, then{" "}
        <Link to={`/runs/${encodeURIComponent(head.id)}`} className={INLINE}>
          #{head.number}
        </Link>{" "}
        at <span className="font-mono text-[12px]">{shortSha(head.sha)}</span>.
      </p>
      <div className="mt-6 grid gap-6">
        <Panel title="What differs">
          {d.differences.length === 0 ? (
            <p className="text-[13px] text-[#7b7e88]">Nothing but the commits: the same actions, variables, environment, machines and cache.</p>
          ) : (
            <Differences items={d.differences} />
          )}
        </Panel>
        <Files files={d.files} />
        <Panel title={leg ? "Steps" : "Legs"} description="Each step's time in the base run, then in this one.">
          {d.legs.length === 0 ? (
            <EmptyState title="Neither run has legs." />
          ) : (
            <div className="grid gap-2">
              {d.legs.map((l) => (
                <LegSteps key={l.key} leg={l} open={!!leg || d.legs.length === 1 || l.base?.conclusion !== l.head?.conclusion} />
              ))}
            </div>
          )}
        </Panel>
      </div>
    </>
  );
}

/** A value of a difference: an action's `ref sha` as ref and short SHA, the rest as they are. */
function value(kind: string, v: string | null) {
  if (v === null) return <span className="text-[#6d7078]">none</span>;
  if (kind === "action") {
    const [ref, sha] = v.split(" ");
    return (
      <>
        {ref} <span className="text-[#7b7e88]">{sha ? shortSha(sha) : ""}</span>
      </>
    );
  }
  if (kind === "image") {
    const [image, digest] = v.split("@");
    return (
      <>
        {image} <span className="text-[#7b7e88]">{digest?.replace(/^sha256:/, "").slice(0, 12)}</span>
      </>
    );
  }
  return v;
}

function Differences({ items }: { items: CiDifference[] }) {
  return (
    <div className="table-wrap">
      <table className="data-table">
        <thead>
          <tr>
            <th>What</th>
            <th className="hidden sm:table-cell">Where</th>
            <th>Base</th>
            <th>This run</th>
          </tr>
        </thead>
        <tbody>
          {items.map((x, i) => (
            <tr key={i}>
              <td className="max-w-[240px]">
                <div className="font-mono text-[10px] tracking-[0.06em] text-[#8c8f98] uppercase">{KINDS[x.kind] ?? x.kind}</div>
                {x.what !== x.kind && <div className="truncate font-mono text-[12px] text-[#e4e5e7]">{x.what}</div>}
                {x.leg && <div className="truncate font-mono text-[10px] text-[#7b7e88] sm:hidden">{x.leg}</div>}
              </td>
              <td className="cell-mono hidden max-w-[180px] truncate sm:table-cell">{x.leg ?? "the run"}</td>
              <td className="cell-mono max-w-[260px] [overflow-wrap:anywhere]">{value(x.kind, x.a)}</td>
              <td className="cell-mono max-w-[260px] [overflow-wrap:anywhere]">{value(x.kind, x.b)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Files({ files }: { files: CiCompare["files"] }) {
  const n = files.count;
  return (
    <Panel title="Changed files" description={files.error ?? (n === 0 ? "The same commit." : n === null ? undefined : `${n} file${n === 1 ? "" : "s"} changed between the commits.`)}>
      {files.files.length > 0 && (
        <details open={files.files.length <= 12}>
          <summary className="cursor-pointer font-mono text-[11px] text-[#9499a5]">{files.files.length <= 12 ? "Files" : `All ${n} files`}</summary>
          <ul className="mt-2 grid gap-1">
            {files.files.map((f) => (
              <li key={f} className="font-mono text-[12px] [overflow-wrap:anywhere] text-[#c8cad0]">
                {f}
              </li>
            ))}
          </ul>
          {n !== null && n > files.files.length && <p className="mt-2 font-mono text-[11px] text-[#7b7e88]">and {n - files.files.length} more</p>}
        </details>
      )}
    </Panel>
  );
}

/** A signed change in seconds: "+12s", "−3s", "same". */
function delta(a: number | null, b: number | null) {
  if (a === null || b === null) return "";
  const d = b - a;
  if (Math.abs(d) < 0.5) return "same";
  return `${d > 0 ? "+" : "−"}${formatDuration(Math.abs(d))}`;
}

function LegSteps({ leg, open }: { leg: CiCompare["legs"][number]; open: boolean }) {
  const state = (c: string | undefined) => (c ? ciState("", c) : "skipped");
  return (
    <details open={open} className="rounded-[10px] border border-[#1d1f23]">
      <summary className="flex cursor-pointer flex-wrap items-center gap-x-3 gap-y-1 px-4 py-3">
        <span className="font-mono text-[12.5px] text-[#e4e5e7]">{leg.key}</span>
        <span className="flex items-center gap-1.5 font-mono text-[11px] text-[#a7aab3] tabular-nums">
          {leg.base ? <StatusDot status={state(leg.base.conclusion)} /> : <span className="text-[#6d7078]">not run</span>}
          {leg.base && formatDuration(leg.base.duration)}
          <span className="text-[#6d7078]">→</span>
          {leg.head ? <StatusDot status={state(leg.head.conclusion)} /> : <span className="text-[#6d7078]">not run</span>}
          {leg.head && formatDuration(leg.head.duration)}
          {leg.base && leg.head && <span className="text-[#7b7e88]">{delta(leg.base.duration, leg.head.duration)}</span>}
        </span>
      </summary>
      <div className="table-wrap px-2 pb-2">
        <table className="data-table">
          <thead>
            <tr>
              <th>Step</th>
              <th className="num">Base</th>
              <th className="num">This run</th>
              <th className="num">Change</th>
            </tr>
          </thead>
          <tbody>
            {leg.steps.map((s, i) => (
              <tr key={i}>
                <td className="max-w-[420px] truncate max-sm:max-w-[150px]">
                  {(s.a_conclusion ?? s.b_conclusion) && <StatusDot status={state(s.b_conclusion ?? s.a_conclusion ?? undefined)} className="mr-2 align-middle" />}
                  {s.name}
                </td>
                <td className="num cell-mono">{s.a === null ? "—" : formatDuration(s.a)}</td>
                <td className="num cell-mono">{s.b === null ? "—" : formatDuration(s.b)}</td>
                <td className="num cell-mono">{delta(s.a, s.b)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </details>
  );
}
