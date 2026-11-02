/**
 * A subagent that ended, removed, as kiln's Remove removes an agent that has (actions.tsx, bulk.tsx):
 * one kiln started, its session (`DELETE` it, as Mission control's Remove does); one of its harness's
 * own, from its agent's session (`Command::RemoveTask`). Wherever its row is (Mission control's tree,
 * the composer's): the trash where its pause was, a swipe by touch, or Remove for those picked.
 */
import * as React from "react";
import { Trash } from "lucide-react";
import { harnessApi } from "@/api/harness";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import type { NodeStand, SubagentNode } from "./subagent-tree";
import { slideShut } from "./swipe-away";

/** Where a subagent stands once it has ended: what can be removed. */
export const ENDED: ReadonlySet<NodeStand> = new Set(["done", "failed", "stopped"]);

type Removable = Pick<SubagentNode, "key" | "kind" | "session" | "task" | "name">;

function removeOne(n: Removable): Promise<unknown> {
  return n.kind === "kiln" ? harnessApi.stop(n.session) : harnessApi.command(n.session, { type: "remove_task", task: n.task ?? "" });
}

/**
 * The rows of a tree being removed: gone from it at once (the tree drawn without them), back with
 * why if their Remove doesn't go. `remove(rows, els)`: those rows (`els`) slide shut first.
 */
export function useRemoving() {
  const [going, setGoing] = React.useState<ReadonlySet<string>>(() => new Set());
  const remove = React.useCallback((rows: readonly Removable[], els: readonly (HTMLElement | null)[] = []) => {
    if (rows.length === 0) return;
    const go = () => {
      setGoing((g) => new Set([...g, ...rows.map((r) => r.key)]));
      for (const r of rows)
        removeOne(r).catch((e: unknown) => {
          setGoing((g) => new Set([...g].filter((k) => k !== r.key)));
          toast.error(`Couldn't remove ${r.name}`, errorMessage(e));
        });
    };
    const slid = els.filter((el): el is HTMLElement => el !== null);
    let left = slid.length;
    if (left === 0) return go();
    for (const el of slid)
      slideShut(el, () => {
        left -= 1;
        if (left === 0) go();
      });
  }, []);
  return { going, remove };
}

/** Its Remove, where its pause is while it goes (off where its harness's session can't take it). */
export function RemoveButton({
  n,
  className,
  size = 14,
  disabled,
  onRemove,
}: {
  n: Pick<SubagentNode, "name">;
  className?: string;
  size?: number;
  disabled?: boolean;
  onRemove: (el: HTMLElement | null) => void;
}) {
  return (
    <button type="button" className={className} disabled={disabled} aria-label={`Remove ${n.name}`} onClick={(e) => onRemove(e.currentTarget.closest("li"))}>
      <Trash size={size} aria-hidden="true" />
    </button>
  );
}
