/**
 * Stand-ins for what is loading, in its shape (Suspense fallbacks): the space it will take, its
 * frame where it has one (a table's head, a list's hairlines), and the loader in it, so nothing
 * moves when it lands. Static: the loader is the only thing that moves.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import { Loading } from "./loading";

/** A block as tall as what it stands in for, with the loader over its top. */
export function Hold({ height, label, className, children }: { height?: number | string; label?: string; className?: string; children?: React.ReactNode }) {
  return (
    <div className={cn("hold", className)} style={height !== undefined ? { minHeight: height } : undefined}>
      {children}
      <Loading label={label} className="hold-loader" />
    </div>
  );
}

/** A table loading: its own head (`head`, a `<thead>`), then empty rows as tall as its rows. */
export function TableHold({ head, rows = 8, rowHeight = 41, label }: { head: React.ReactNode; rows?: number; rowHeight?: number; label?: string }) {
  return (
    <div className="table-wrap hold">
      <table className="data-table">
        {head}
        <tbody aria-hidden="true">
          {Array.from({ length: rows }, (_, i) => (
            <tr key={i} className="hold-row">
              <td colSpan={99} style={{ height: rowHeight }} />
            </tr>
          ))}
        </tbody>
      </table>
      <Loading label={label} className="hold-loader" />
    </div>
  );
}

/** A hairline list loading (`.row-list`): empty rows as tall as its rows. */
export function RowsHold({ rows = 6, rowHeight = 48, label, className }: { rows?: number; rowHeight?: number; label?: string; className?: string }) {
  return (
    <div className={cn("row-list hold", className)}>
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} style={{ height: rowHeight }} aria-hidden="true" />
      ))}
      <Loading label={label} className="hold-loader" />
    </div>
  );
}
