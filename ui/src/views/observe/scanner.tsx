/**
 * The Sensitive Data Scanner (docs/observe.md §8.9), Datadog's: scanning groups in order, each a
 * filter picking records and rules finding card numbers, emails, keys and tokens (or a pattern of
 * your own) in them, replacing or hashing what they find before anything is kept, on every machine.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";
import { Link } from "@/lib/router";
import { observeApi, type ScanningGroup } from "@/api/observe";
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
import { ObserveNav, scanningGroupsRes, useMoves } from "./parts";

export default function Scanner() {
  return (
    <div className="container-console page">
      <title>Sensitive data · Observe · kiln</title>
      <PageIntro
        index={8}
        label="Observe"
        title="Sensitive data"
        lead="What records hold that should not be kept: card numbers, emails, keys and tokens. Each group's filter picks records, and its rules find what they look for in them and replace it, hide part of it or hash it, on every machine, before anything is stored."
        actions={
          <Button asChild>
            <Link to="/observe/scanner/new">
              <Plus /> New group
            </Link>
          </Button>
        }
      />
      <ObserveNav />
      <Loadable label="the scanning groups" fallback={<TableHold head={<Head />} rows={2} />}>
        <List />
      </Loadable>
    </div>
  );
}

function Head() {
  return (
    <tr>
      <th className="num w-1">#</th>
      <th>Group</th>
      <th className="hidden md:table-cell">Rules</th>
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
  const res = useRes(scanningGroupsRes);
  const all = res.data;
  const [removing, setRemoving] = React.useState<ScanningGroup | null>(null);
  const act = async (what: string, f: () => Promise<unknown>) => {
    try {
      await f();
      invalidate("observe/scanner/groups");
      return true;
    } catch (e) {
      toast({ title: `Couldn't ${what}`, description: errorMessage(e), tone: "failed" });
      return false;
    }
  };
  const putBack = (ok: boolean) => {
    if (!ok) invalidate(scanningGroupsRes.key);
  };
  // Turned on or off, and moved, at once (W21-R15), and put back if the server refuses; a group's
  // next turn waits for its answer, so an answer never undoes a later turn.
  const [turn, turning] = useActions((_id: string, g: ScanningGroup) => {
    const on = !g.is_enabled;
    res.change((l) => l.map((x) => (x.id === g.id ? { ...x, is_enabled: on } : x)));
    return act(on ? "turn it on" : "turn it off", () => observeApi.updateScanningGroup(g.id, { ...g, is_enabled: on })).then(putBack);
  });
  const moves = useMoves();
  const move = (i: number, by: -1 | 1) => {
    const j = i + by;
    const g = all[i];
    if (!g || j < 0 || j >= all.length) return;
    moves.move(g.id, by, () => {
      res.change((l) => swapped(l, i, j));
      return act("move it", () =>
        observeApi.orderScanningGroups(
          swapped(
            all.map((x) => x.id),
            i,
            j,
          ),
        ),
      ).then(putBack);
    });
  };
  if (all.length === 0)
    return (
      <p className="empty-line py-12 text-center">
        None yet. <Link to="/observe/scanner/new">Make a group</Link> to keep card numbers, keys and personal data out of what is stored.
      </p>
    );
  return (
    <>
      <div className="table-wrap">
        <table className="data-table">
          <thead>
            <Head />
          </thead>
          <tbody>
            {all.map((g, i) => {
              const on = g.rules.filter((r) => r.is_enabled);
              // On a phone they go under its name, which keeps the row's width.
              const controls = (
                <>
                  <Button variant="ghost" size="xs" pending={turning(g.id)} onClick={() => turn(g.id, g)}>
                    {g.is_enabled ? "Turn off" : "Turn on"}
                    <span className="sr-only"> {g.name}</span>
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Move ${g.name} up`} {...moves.props(g.id, -1, i === 0)} onClick={() => move(i, -1)}>
                    <ArrowUp />
                  </Button>
                  <Button variant="ghost" size="xs" aria-label={`Move ${g.name} down`} {...moves.props(g.id, 1, i === all.length - 1)} onClick={() => move(i, 1)}>
                    <ArrowDown />
                  </Button>
                  <Button variant="ghost-danger" size="xs" aria-label={`Remove ${g.name}`} onClick={() => setRemoving(g)}>
                    <Trash2 />
                  </Button>
                </>
              );
              return (
                <tr key={g.id}>
                  <td className="num cell-mono w-1 text-[#8c8f98]">{i + 1}</td>
                  <td className="truncate-cell">
                    <Link to={`/observe/scanner/${encodeURIComponent(g.id)}`} className="text-[13px] text-[#e4e5e7] hover:text-white">
                      {g.name}
                    </Link>
                    <p className="truncate font-mono text-[11px] text-[#8c8f98]">{g.filter.query.trim() || "every record"}</p>
                    <p className="truncate text-[11px] text-[#8c8f98] md:hidden">
                      {on.length} {on.length === 1 ? "rule" : "rules"} on
                    </p>
                    <div className="mt-1 flex items-center justify-between gap-2 sm:hidden">
                      <State on={g.is_enabled} />
                      <span className="flex items-center">{controls}</span>
                    </div>
                  </td>
                  <td className="hidden max-w-[24rem] md:table-cell">
                    <div className="truncate text-[12.5px]" title={on.map((r) => r.name).join(", ")}>
                      {on.length === 0 ? <span className="text-[#8c8f98]">none on</span> : on.map((r) => r.name).join(", ")}
                    </div>
                  </td>
                  <td className="hidden w-1 whitespace-nowrap sm:table-cell">
                    <State on={g.is_enabled} />
                  </td>
                  <td className="hidden w-1 whitespace-nowrap text-right sm:table-cell">{controls}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <p className="mt-3 text-[12.5px] text-[#8c8f98]">In this order, each group whose filter picks a record scans it, after the pipelines; a change applies to the records taken after it.</p>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(o) => !o && setRemoving(null)}
        title={`Remove ${removing?.name ?? ""}?`}
        description="Records taken from now are not scanned by it; those stored stay as they were kept."
        confirmLabel="Remove"
        danger
        onConfirm={async () => {
          if (removing) await act("remove it", () => observeApi.deleteScanningGroup(removing.id));
        }}
      />
    </>
  );
}
