/**
 * A run's data flow (docs/ci.md W04-R23, W21-R04): each job output, from the step that made it to
 * the jobs that read it and where in each, with the value each leg gave it (never a secret's);
 * and each job's exported environment and the jobs that take it.
 */
import type { CiFlow } from "@/api/ci";
import { Panel } from "@/components/ui/misc";
import { Link } from "@/lib/router";

/** The most legs' values an output shows; the rest are counted. */
const SHOWN = 4;

function Values({ o }: { o: CiFlow["outputs"][number] }) {
  if (o.secret) return <span className="font-mono text-[10px] tracking-[0.06em] text-[#a9acb5] uppercase">secret</span>;
  const set = o.values.filter((v) => v.value !== null);
  if (!set.length) return <span className="text-[12px] text-[#8c8f98]">not set</span>;
  // One value, or every leg the same: said once.
  if (set.length === 1 || set.every((v) => v.value === set[0]?.value)) {
    return <code className="font-mono text-[11.5px] [overflow-wrap:anywhere] text-[#d4dde8]">{set[0]?.value}</code>;
  }
  return (
    <ul className="flex min-w-0 flex-col gap-0.5">
      {set.slice(0, SHOWN).map((v) => (
        <li key={v.leg} className="flex min-w-0 flex-wrap gap-x-2 font-mono text-[11px]">
          <span className="text-[#8c8f98]">{v.key}</span>
          <code className="[overflow-wrap:anywhere] text-[#d4dde8]">{v.value}</code>
        </li>
      ))}
      {set.length > SHOWN && <li className="text-[11px] text-[#8c8f98]">and {set.length - SHOWN} more</li>}
    </ul>
  );
}

/** A job that reads it or takes it: to its first leg, with where it reads it. */
function Reader({ run, job, legs, at }: { run: string; job: string; legs: { leg: string; key: string }[]; at?: string[] }) {
  const first = legs[0];
  return (
    <li className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
      {first ? (
        <Link to={`/runs/${run}/jobs/${first.leg}`} className="text-[12.5px] text-[#cfd6e2] hover:text-white">
          {job}
        </Link>
      ) : (
        <span className="text-[12.5px] text-[#a9acb5]" title="its legs aren't made yet">
          {job}
        </span>
      )}
      {legs.length > 1 && <span className="font-mono text-[10.5px] text-[#8c8f98]">{legs.length} legs</span>}
      {at && at.length > 0 && <span className="min-w-0 font-mono text-[10.5px] [overflow-wrap:anywhere] text-[#8c8f98]">{at.join(", ")}</span>}
    </li>
  );
}

export function DataFlow({ flow, run }: { flow: CiFlow; run: string }) {
  const n = flow.outputs.length + flow.exports.length;
  return (
    <Panel eyebrow={`Data flow · ${n}`} title="Outputs and exported environment" description="Where each job's outputs come from, and the jobs that read them, and where." bodyClassName="px-0 pb-1">
      <ul className="row-list" aria-label="The run's outputs and exports">
        {flow.outputs.map((o) => (
          <li key={`out:${o.job}.${o.name}`} className="grid gap-x-6 gap-y-2 px-5 py-3 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1.2fr)]">
            <div className="flex min-w-0 flex-col gap-0.5">
              <code className="font-mono text-[12px] [overflow-wrap:anywhere] text-[#e4e5e7]">
                {o.job}.outputs.{o.name}
              </code>
              <span className="text-[11.5px] text-[#8c8f98]">{o.step ? `from its step ${o.step.id}` : "set by the job"}</span>
            </div>
            <div className="min-w-0">
              <Values o={o} />
            </div>
            <div className="min-w-0">
              {o.readers.length ? (
                <ul className="flex flex-col gap-1" aria-label={`What reads ${o.job}.outputs.${o.name}`}>
                  {o.readers.map((r) => (
                    <Reader key={r.job} run={run} job={r.job} legs={r.legs} at={r.at} />
                  ))}
                </ul>
              ) : (
                <span className="text-[12px] text-[#8c8f98]">no job reads it</span>
              )}
            </div>
          </li>
        ))}
        {flow.exports.map((e) => (
          <li key={`env:${e.job}`} className="grid gap-x-6 gap-y-2 px-5 py-3 md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,1.2fr)]">
            <div className="flex min-w-0 flex-col gap-0.5">
              <code className="font-mono text-[12px] text-[#e4e5e7]">{e.job}.exports.env</code>
              <span className="text-[11.5px] text-[#8c8f98]">its environment, for the jobs that take it</span>
            </div>
            <code className="min-w-0 font-mono text-[11.5px] [overflow-wrap:anywhere] text-[#d4dde8]">{e.names.join(", ")}</code>
            <div className="min-w-0">
              {e.readers.length ? (
                <ul className="flex flex-col gap-1" aria-label={`What takes ${e.job}'s environment`}>
                  {e.readers.map((r) => (
                    <Reader key={r.job} run={run} job={r.job} legs={r.legs} at={["env-from"]} />
                  ))}
                </ul>
              ) : (
                <span className="text-[12px] text-[#8c8f98]">no job takes it</span>
              )}
            </div>
          </li>
        ))}
      </ul>
    </Panel>
  );
}
