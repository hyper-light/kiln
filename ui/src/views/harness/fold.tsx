/**
 * A section of an agent's panel (its Plan and Tools tabs) that folds: its heading a toggle, its
 * chevron turning, what it holds shown while it's open, and beside the heading what belongs there
 * (how many, a chip, a button). Each section's open or folded is kept in this browser by its id,
 * the same for every agent.
 */
import * as React from "react";
import { ChevronRight } from "lucide-react";
import { useLocalStorage } from "@/lib/hooks";
import { cn } from "@/lib/utils";

/** Whether the section around is open: what fills it reads only once it is. */
export const FoldOpen = React.createContext(true);

/** Section `id`'s open or folded, as kept. */
export function useFold(id: string, defaultOpen = true): [boolean, (open: boolean) => void] {
  const [open, setOpen] = useLocalStorage(`kiln.panel.fold.${id}`, defaultOpen);
  return [open, setOpen];
}

export function Fold({ id, title, count, aside, defaultOpen = true, children }: { id: string; title: React.ReactNode; count?: React.ReactNode; aside?: React.ReactNode; defaultOpen?: boolean; children: React.ReactNode }) {
  const [open, setOpen] = useFold(id, defaultOpen);
  const bodyId = React.useId();
  return (
    <section className="in-fold" data-open={open || undefined}>
      <h3 className="in-sec-head">
        <button type="button" className="in-sec-toggle" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("hx-chev", open && "rotate-90")} aria-hidden="true" />
          {title}
          {count !== undefined && <span className="in-sec-count">{count}</span>}
        </button>
        {aside}
      </h3>
      <div id={bodyId} hidden={!open}>
        <FoldOpen.Provider value={open}>{children}</FoldOpen.Provider>
      </div>
    </section>
  );
}
