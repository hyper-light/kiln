/**
 * The Runners page's pools and images (docs/ci.md W13-R09, W13-R11, W21-R08). Pools: executors
 * kiln starts for the legs that ask for them. Each pool's size now (starting, idle, busy) beside the size it's heading for, the
 * legs waiting for it, its executors (on which runner, doing what) and its last failure to start
 * one. A stored pool is edited in place, made or removed; the configuration's are shown as they
 * are.
 */
import * as React from "react";
import { ci, type CiImage, type CiPool, type CiPoolSpec } from "@/api/ci";
import { Link } from "@/lib/router";
import { Button } from "@/components/ui/button";
import { Confirm, Panel, SelectField } from "@/components/ui/misc";
import { useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";

export const poolsRes: Res<CiPool[]> = { key: "ci/pools", fetch: (s) => ci.pools(s), opts: { pollMs: 5000 } };
export const imagesRes: Res<CiImage[]> = { key: "ci/images", fetch: (s) => ci.images(s) };

/** The runner images kiln built (`kiln/build-image@v1`) into its registry, newest first: each
 * one's platforms and digest, and the run that built it. */
export function ImagesPanel() {
  const images = useRes(imagesRes).data;
  return (
    <Panel className="mt-8" title="Images" description="Runner images kiln built into its registry, for pools and routing rules (kiln/build-image@v1).">
      {images.length === 0 ? (
        <p className="empty-line px-5 py-4">None yet: a workflow&rsquo;s kiln/build-image@v1 step makes them.</p>
      ) : (
        <div className="table-wrap">
          <table className="data-table" aria-label="Images">
            <thead>
              <tr>
                <th>Image</th>
                <th>Platforms</th>
                <th className="hidden md:table-cell">Digest</th>
                <th className="num">Built</th>
              </tr>
            </thead>
            <tbody>
              {images.map((i) => (
                <tr key={i.id}>
                  <td>
                    <span className="font-mono text-[12.5px]">{i.name}</span>
                    {i.repo && <span className="ml-2 font-mono text-[10.5px] text-[#7f8a9a]">{i.repo}</span>}
                  </td>
                  <td className="cell-mono">{i.platforms.join(", ")}</td>
                  <td className="cell-mono hidden md:table-cell" title={i.image}>
                    {i.digest.slice(0, 19)}…
                  </td>
                  <td className="num cell-mono">{i.run_id ? <Link to={`/runs/${encodeURIComponent(i.run_id)}`}>{relativeTime(i.created_at)}</Link> : relativeTime(i.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

const KINDS = ["docker", "firecracker", "shards", "k8s"].map((k) => ({ value: k, label: k }));
/** A line of what a save or a start said, in this page's type (Settings' notes aren't loaded here). */
const NOTE = "text-[12px] leading-[1.55] [overflow-wrap:anywhere]";

/** The size a pool is heading for, as the scheduler scales it: enough spare executors for the
 * legs waiting and its minimum idle, within its maximum. */
function target(p: CiPool): number {
  const spare = p.starting + p.idle;
  const total = spare + p.busy;
  const need = Math.max(Math.max(p.waiting, p.min_idle) - spare, 0);
  return total + Math.min(need, Math.max(p.max - total, 0));
}

const now = (p: CiPool) => p.starting + p.idle + p.busy;

export function PoolsPanel({ manage }: { manage: boolean }) {
  const res = useRes(poolsRes);
  const pools = res.data;
  const [open, setOpen] = React.useState<string | null>(null);
  const [adding, setAdding] = React.useState(false);
  // What a pool just saved does now, as the server warns of it (W20-R03).
  const [warnings, setWarnings] = React.useState<string[]>([]);
  const saved = (w: string[]) => {
    setWarnings(w);
    res.reload();
  };
  return (
    <Panel
      className="mt-8"
      title="Pools"
      description="Executors kiln starts for the legs that ask for them (runs-on a pool's labels), each discarded when its leg ends."
      actions={
        manage && !adding ? (
          <Button size="xs" variant="ghost" onClick={() => setAdding(true)}>
            Add a pool
          </Button>
        ) : undefined
      }
    >
      {pools.length === 0 && !adding && <p className="empty-line px-5 py-4">No pools: legs run on the runners themselves.</p>}
      {pools.length > 0 && (
        <div className="table-wrap">
          <table className="data-table" aria-label="Pools">
            <thead>
              <tr>
                <th>Pool</th>
                <th className="hidden sm:table-cell">Executor</th>
                <th className="num">Now</th>
                <th className="num">Heading for</th>
                <th className="num">Waiting</th>
                <th className="hidden md:table-cell">Labels</th>
              </tr>
            </thead>
            <tbody>
              {pools.map((p) => (
                <React.Fragment key={p.name}>
                  <tr>
                    <td>
                      <button type="button" className="text-link font-mono text-[12.5px]" aria-expanded={open === p.name} onClick={() => setOpen(open === p.name ? null : p.name)}>
                        {p.name}
                      </button>
                      {p.source === "config" && <span className="ml-2 font-mono text-[10.5px] text-[#7f8a9a]">configured</span>}
                    </td>
                    <td className="cell-mono hidden sm:table-cell">{p.executor}</td>
                    <td className="num cell-mono" title={`${p.busy} busy · ${p.idle} idle · ${p.starting} starting`}>
                      {now(p)}
                    </td>
                    <td className="num cell-mono">{target(p)}</td>
                    <td className="num cell-mono">{p.waiting}</td>
                    <td className="cell-mono hidden md:table-cell">{p.labels.join(", ")}</td>
                  </tr>
                  {open === p.name && (
                    <tr>
                      <td colSpan={6} className="!p-0">
                        <PoolDetail pool={p} manage={manage && p.source === "stored"} onDone={() => setOpen(null)} reload={() => res.reload()} saved={saved} />
                      </td>
                    </tr>
                  )}
                </React.Fragment>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {adding && (
        <div className="border-t border-[#20232b] px-5 py-4">
          <PoolForm
            onSaved={(w) => {
              setAdding(false);
              saved(w);
            }}
            onCancel={() => setAdding(false)}
          />
        </div>
      )}
      {warnings.length > 0 && (
        <div className="border-t border-[#20232b] px-5 py-3" role="status">
          {warnings.map((w) => (
            <p key={w} className={`${NOTE} text-[var(--color-warn)]`}>
              {w}
            </p>
          ))}
        </div>
      )}
    </Panel>
  );
}

function PoolDetail({ pool, manage, onDone, reload, saved }: { pool: CiPool; manage: boolean; onDone: () => void; reload: () => void; saved: (warnings: string[]) => void }) {
  const [editing, setEditing] = React.useState(false);
  const [removing, setRemoving] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  return (
    <div className="border-t border-[#20232b] bg-[#0d0e12] px-5 py-4">
      <p className="font-mono text-[11.5px] text-[#a1a4ad]">
        {pool.busy} busy · {pool.idle} idle · {pool.starting} starting · at least {pool.min_idle} idle · at most {pool.max}
        {pool.image ? ` · ${pool.image}` : ""}
      </p>
      {pool.executors.length > 0 && (
        <ul className="mt-3 space-y-1.5" aria-label={`${pool.name}'s executors`}>
          {pool.executors.map((e) => (
            <li key={e.id} className="flex flex-wrap items-center gap-x-3 font-mono text-[11.5px] text-[#b0b4be]">
              <i className="dot" data-status={e.state === "busy" ? "running" : e.state === "idle" ? "succeeded" : "queued"} aria-hidden="true" />
              <span>{e.id}</span>
              <span className="text-[#7f8a9a]">on {e.runner}</span>
              <span>{e.state === "busy" && e.leg ? `running leg ${e.leg.slice(0, 8)}` : e.state}</span>
              <span className="text-[#7f8a9a]">since {relativeTime(e.since / 1000)}</span>
            </li>
          ))}
        </ul>
      )}
      {pool.last_error && (
        <p className={`mt-3 ${NOTE} text-[var(--color-fail)]`}>
          Its last start failed on {pool.last_error[0]}: {pool.last_error[1]}
        </p>
      )}
      {manage && !editing && (
        <div className="mt-3 flex gap-2">
          <Button size="xs" variant="secondary" onClick={() => setEditing(true)}>
            Edit
          </Button>
          <Button size="xs" variant="ghost" aria-expanded={removing} onClick={() => setRemoving(!removing)}>
            Remove
          </Button>
        </div>
      )}
      {editing && (
        <div className="mt-4">
          <PoolForm
            pool={pool}
            onSaved={(w) => {
              setEditing(false);
              saved(w);
            }}
            onCancel={() => setEditing(false)}
          />
        </div>
      )}
      {removing && (
        <Confirm
          label={`Remove ${pool.name}`}
          tone="danger"
          className="mt-3"
          actions={
            <>
              <Button
                size="xs"
                variant="primary"
                disabled={busy}
                onClick={async () => {
                  setBusy(true);
                  setProblem(null);
                  try {
                    await ci.removePool(pool.name);
                    reload();
                    onDone();
                  } catch (e) {
                    setProblem(errorMessage(e));
                  } finally {
                    setBusy(false);
                  }
                }}
              >
                Remove {pool.name}
              </Button>
              <Button size="xs" variant="ghost" disabled={busy} onClick={() => setRemoving(false)}>
                Keep it
              </Button>
            </>
          }
        >
          <p>Its idle executors stop now; busy ones end with their legs. Legs that ask for it wait for another.</p>
        </Confirm>
      )}
      {problem && (
        <p className={`mt-3 ${NOTE} text-[var(--color-fail)]`} role="status">
          {problem}
        </p>
      )}
    </div>
  );
}

function PoolForm({ pool, onSaved, onCancel }: { pool?: CiPool; onSaved: (warnings: string[]) => void; onCancel: () => void }) {
  const [name, setName] = React.useState(pool?.name ?? "");
  const [executor, setExecutor] = React.useState(pool?.executor ?? "docker");
  const [image, setImage] = React.useState(pool?.image ?? "");
  const [labels, setLabels] = React.useState((pool?.labels ?? []).join(", "));
  const [minIdle, setMinIdle] = React.useState(String(pool?.min_idle ?? 0));
  const [max, setMax] = React.useState(String(pool?.max ?? 4));
  const [busy, setBusy] = React.useState(false);
  const [problem, setProblem] = React.useState<string | null>(null);
  const field = (id: string, label: string, input: React.ReactNode) => (
    <div>
      <label htmlFor={id} className="field-label">
        {label}
      </label>
      <div className="field">{input}</div>
    </div>
  );
  return (
    <form
      noValidate
      aria-label={pool ? `Edit ${pool.name}` : "A new pool"}
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setProblem(null);
        const spec: CiPoolSpec = {
          name: name.trim(),
          executor,
          image: image.trim() || null,
          labels: labels
            .split(",")
            .map((l) => l.trim())
            .filter(Boolean),
          min_idle: Number(minIdle) || 0,
          max: Number(max) || null,
        };
        try {
          const r = await ci.putPool(spec);
          onSaved((r.diagnostics ?? []).map((d) => d.message));
        } catch (e2) {
          setProblem(errorMessage(e2));
        } finally {
          setBusy(false);
        }
      }}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        {field(
          "pool-name",
          "Name",
          <input id="pool-name" type="text" spellCheck={false} placeholder="linux-docker" value={name} disabled={busy || Boolean(pool)} onChange={(e) => setName(e.target.value)} />,
        )}
        <SelectField label="Executor" value={executor} onChange={setExecutor} options={KINDS} />
        {field(
          "pool-image",
          "Image",
          <input id="pool-image" type="text" spellCheck={false} placeholder="kiln/runner-ubuntu-24.04" value={image} disabled={busy} onChange={(e) => setImage(e.target.value)} />,
        )}
        {field(
          "pool-labels",
          "Labels (comma-separated)",
          <input id="pool-labels" type="text" spellCheck={false} placeholder="linux, docker" value={labels} disabled={busy} onChange={(e) => setLabels(e.target.value)} />,
        )}
        {field("pool-min", "At least idle", <input id="pool-min" type="number" min={0} value={minIdle} disabled={busy} onChange={(e) => setMinIdle(e.target.value)} />)}
        {field("pool-max", "At most", <input id="pool-max" type="number" min={1} value={max} disabled={busy} onChange={(e) => setMax(e.target.value)} />)}
      </div>
      <div className="mt-4 flex justify-end gap-2">
        <Button type="button" size="xs" variant="ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </Button>
        <Button type="submit" size="xs" variant="secondary" disabled={busy || !name.trim() || !image.trim()}>
          {pool ? "Save" : "Add the pool"}
        </Button>
      </div>
      {problem && (
        <p className={`mt-3 ${NOTE} text-[var(--color-fail)]`} role="status">
          {problem}
        </p>
      )}
    </form>
  );
}
