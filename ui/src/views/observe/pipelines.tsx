/**
 * Log pipelines (docs/observe.md §8.2): how records are read as they are taken, Datadog's. Each
 * pipeline's filter picks records and its processors parse and remap them, the pipelines in order.
 * kiln's own read nginx's and Apache's logs: one is turned off, or copied to be changed.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Copy, Plus, Trash2 } from "lucide-react";
import { Link, useNavigate } from "@/lib/router";
import { observeApi, type Pipeline } from "@/api/observe";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { useActions } from "@/lib/action";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { swapped } from "@/lib/utils";
import { ObserveNav, pipelinesRes, useMoves } from "./parts";

export default function Pipelines() {
  return (
    <div className="container-console page">
      <title>Pipelines · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Pipelines"
        lead="How records are read as they are taken, on every machine: each pipeline's filter picks records, and its processors parse and remap them, the pipelines in order. kiln's own read nginx's and Apache's logs; turn one off, or copy it to change it."
        actions={
          <Button asChild>
            <Link to="/observe/pipelines/new">
              <Plus /> New pipeline
            </Link>
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the pipelines" fallback={<TableHold head={<Head />} rows={3} />}>
        <List />
      </Loadable>
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th className="num w-1">#</th>
      <th>Pipeline</th>
      <th className="num hidden sm:table-cell">Processors</th>
      <th className="hidden sm:table-cell">State</th>
      <th className="hidden sm:table-cell" />
    </tr>
  );
}

function State({ on }: { on: boolean }) {
  return (
    <span className="status">
      <i className="dot" data-status={on ? "succeeded" : "skipped"} aria-hidden="true" />
      <span className="status-text" data-status={on ? "succeeded" : "skipped"}>
        {on ? "On" : "Off"}
      </span>
    </span>
  );
}

function List() {
  const res = useRes(pipelinesRes);
  const all = res.data;
  const navigate = useNavigate();
  const [removing, setRemoving] = React.useState<Pipeline | null>(null);
  const mine = all.filter((p) => !p.builtin);
  const act = async (what: string, f: () => Promise<unknown>) => {
    try {
      await f();
      invalidate("observe/pipelines");
      return true;
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
      return false;
    }
  };
  const putBack = (ok: boolean) => {
    if (!ok) invalidate(pipelinesRes.key);
  };
  // Turned on or off, and moved, at once (W21-R15), and put back if the server refuses; a pipeline's
  // next turn waits for its answer, so an answer never undoes a later turn.
  const [turn, turning] = useActions((_id: string, p: Pipeline) => {
    const on = !p.is_enabled;
    res.change((l) => l.map((x) => (x.id === p.id ? { ...x, is_enabled: on } : x)));
    return act(on ? "turn it on" : "turn it off", () => observeApi.updatePipeline(p.id, { name: p.name, is_enabled: on, filter: p.filter, processors: p.processors })).then(putBack);
  });
  const moves = useMoves();
  const move = (p: Pipeline, by: -1 | 1) => {
    const ids = mine.map((m) => m.id);
    const i = ids.indexOf(p.id);
    const j = i + by;
    if (i < 0 || j < 0 || j >= ids.length) return;
    moves.move(p.id, by, () => {
      res.change((l) =>
        swapped(
          l,
          l.findIndex((x) => x.id === p.id),
          l.findIndex((x) => x.id === ids[j]),
        ),
      );
      return act("move it", () => observeApi.orderPipelines(swapped(ids, i, j))).then(putBack);
    });
  };
  const [copy, copying] = useActions(async (_id: string, p: Pipeline) => {
    const made = await observeApi.createPipeline({ name: `${p.name} (copy)`, is_enabled: true, filter: p.filter, processors: p.processors });
    invalidate("observe/pipelines");
    await navigate(`/observe/pipelines/${encodeURIComponent(made.id)}`);
  }, "Couldn't copy it");
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((p, i) => {
              const at = mine.findIndex((m) => m.id === p.id);
              // On a phone they go under its name, which keeps the row's width.
              const controls = (
                <>
                  <Button variant="ghost" size="xs" pending={turning(p.id)} onClick={() => turn(p.id, p)}>
                    {p.is_enabled ? `Turn off` : `Turn on`}
                    <span className="sr-only"> {p.name}</span>
                  </Button>
                  {p.builtin ? (
                    <Button variant="ghost" size="xs" aria-label={`Copy ${p.name} to change it`} pending={copying(p.id)} onClick={() => copy(p.id, p)}>
                      <Copy />
                    </Button>
                  ) : (
                    <>
                      <Button variant="ghost" size="xs" aria-label={`Move ${p.name} up`} {...moves.props(p.id, -1, at <= 0)} onClick={() => move(p, -1)}>
                        <ArrowUp />
                      </Button>
                      <Button variant="ghost" size="xs" aria-label={`Move ${p.name} down`} {...moves.props(p.id, 1, at < 0 || at >= mine.length - 1)} onClick={() => move(p, 1)}>
                        <ArrowDown />
                      </Button>
                      <Button variant="ghost-danger" size="xs" aria-label={`Remove ${p.name}`} onClick={() => setRemoving(p)}>
                        <Trash2 />
                      </Button>
                    </>
                  )}
                </>
              );
              return (
                <tr key={p.id}>
                  <td className="num cell-mono w-1 text-[#8c8f98]">{i + 1}</td>
                  <td className="truncate-cell">
                    <Link to={`/observe/pipelines/${encodeURIComponent(p.id)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                      {p.name}
                    </Link>
                    {p.builtin && <span className="ml-2 font-mono text-[10.5px] text-[#8c8f98]">kiln&apos;s own</span>}
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{p.filter.query.trim() || "every record"}</p>
                    <div className="mt-1 flex items-center justify-between gap-2 sm:hidden">
                      <State on={p.is_enabled} />
                      <span className="flex items-center">{controls}</span>
                    </div>
                  </td>
                  <td className="num cell-mono hidden sm:table-cell">{p.processors.length}</td>
                  <td className="hidden w-1 whitespace-nowrap sm:table-cell">
                    <State on={p.is_enabled} />
                  </td>
                  <td className="hidden w-1 whitespace-nowrap text-right sm:table-cell">{controls}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {mine.length === 0 && (
        <p className="empty-line py-10 text-center">
          None of yours yet. <Link to="/observe/pipelines/new">Make one</Link> to parse a log of yours, or copy one of kiln&apos;s.
        </p>
      )}
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Records taken from now are not read by it; those stored stay as they were read."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (removing) await act("remove it", () => observeApi.deletePipeline(removing.id));
        }}
      />
    </>
  );
}
