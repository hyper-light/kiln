/**
 * The reaper's policy at one scope (docs/ci.md W24-R31): what's in force there, each value with the
 * scope it came from, and the keys this scope sets, edited in place. The system page shows the
 * system's and a runner's, the repository's details its own, and an agent's settings its session's.
 */
import * as React from "react";
import { api } from "@/api/client";
import {
  REAP_HINT,
  REAP_KEYS,
  REAP_LABEL,
  fieldsOf,
  fromText,
  keysOf,
  policyOf,
  reapPolicies,
  scopeParam,
  setReapPolicy,
  valueText,
  type ReapAt,
  type ReapKey,
  type ReapScope,
  type ReapView,
} from "@/api/reap";
import { Button } from "@/components/ui/button";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Facts, FoldSection, SelectField, TextField } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";

const viewRes = (at: ReapAt): Res<ReapView> => ({ key: `reap/${at.runner ?? ""}/${at.repo ?? ""}/${at.session ?? ""}`, fetch: (s) => reapPolicies(at, s) });

/** What's in force at this scope, each value with where it came from. */
function Effective({ view }: { view: ReapView }) {
  return (
    <Facts
      items={REAP_KEYS.map((k) => ({
        label: REAP_LABEL[k],
        value: (
          <>
            {valueText(k, view.effective[k])} <span className="text-[#8b8f99]">· {fromText(view.effective[k]?.from ?? null)}</span>
          </>
        ),
      }))}
    />
  );
}

/** The policy at `scope` (`target`: the runner's, the repository's or the session's name), with what it inherits from `at`. */
export function ReapPolicy({ scope, target, at }: { scope: ReapScope; target: string; at: ReapAt }) {
  const res = React.useMemo(() => viewRes(at), [at]);
  const view = useRes(res).data;
  const mine = view.policies.find((p) => p.scope === scope && p.target === (scope === "system" ? "" : target));
  const [fields, setFields] = React.useState(() => fieldsOf(mine?.policy, scope));
  const [error, setError] = React.useState<string | null>(null);
  const [saving, setSaving] = React.useState(false);
  const save = async (next: Partial<Record<ReapKey, string>>) => {
    setSaving(true);
    try {
      await setReapPolicy(scopeParam(scope, target), policyOf(next, scope));
      setFields(next);
      setError(null);
      invalidate(res.key);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="reap-policy flex flex-col gap-4">
      <Effective view={view} />
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {keysOf(scope).map((k) => (
          <TextField
            key={k}
            label={REAP_LABEL[k]}
            hint={REAP_HINT[k]}
            placeholder={valueText(k, view.effective[k])}
            value={fields[k] ?? ""}
            onChange={(e) => setFields({ ...fields, [k]: e.target.value })}
          />
        ))}
      </div>
      {error && (
        <p role="alert" className="text-[13px] text-[#e2725b]">
          {error}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" disabled={saving} onClick={() => void save(fields)}>
          Save
        </Button>
        <Button size="sm" variant="ghost" disabled={saving || !mine} onClick={() => void save(fieldsOf(undefined, scope))}>
          Clear this scope's policy
        </Button>
        {mine && <span className="text-[12px] text-[#8b8f99]">Set by {mine.updated_by}</span>}
      </div>
    </div>
  );
}

const runnersRes: Res<{ runners: { name: string }[] }> = { key: "reach/runners", fetch: (s) => api.reachRunners(s) };

/** The system page's section: the system's policy, and a runner's. */
export function ReapSection({ index }: { index: string }) {
  return (
    <FoldSection id="system.reaper" index={index} label="Caches" title="Cache reaper">
      <p className="section-lead">
        What kiln's caches keep, and when they're reaped: on free space, a schedule, or a write past the ceiling. A runner's limits come before the system's when stricter; what to keep comes from the
        most specific scope.
      </p>
      <Loadable label="the cache reaper" fallback={<Hold height={1700} />}>
        <ReapBody />
      </Loadable>
    </FoldSection>
  );
}

function ReapBody() {
  const runners = useRes(runnersRes).data.runners.map((r) => r.name);
  const [runner, setRunner] = React.useState(runners[0] ?? "");
  return (
    <div className="flex flex-col gap-6">
      <section aria-label="The system's policy">
        <h4 className="field-label">The system</h4>
        <ReapPolicy scope="system" target="" at={{}} />
      </section>
      {runners.length > 0 && (
        <section aria-label="A runner's policy">
          <SelectField label="A runner" value={runner} onChange={setRunner} options={runners.map((n) => ({ value: n, label: n }))} />
          <Loadable label="the runner's cache policy" fallback={<Hold height={860} />}>
            <ReapPolicy key={runner} scope="runner" target={runner} at={{ runner }} />
          </Loadable>
        </section>
      )}
    </div>
  );
}
