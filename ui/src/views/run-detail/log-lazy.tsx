/**
 * The run page's LogViewer with its code (xterm's) fetched as a log first shows, not with the page
 * that holds it (goal 8, plan 01a1a832ebfd step 6): its box is held at its height until it's here.
 */
import * as React from "react";
import { Hold } from "@/components/ui/hold";
import type LogViewerOf from "./log-viewer";

const LogViewer = React.lazy(() => import("./log-viewer"));

/** A log box's height for `rows` lines (log-viewer.tsx: 12px type at 1.18, 20px of padding). */
const heightOf = (rows: number) => rows * Math.ceil(12 * 1.18) + 20;

export function LazyLogViewer(props: React.ComponentProps<typeof LogViewerOf>) {
  return (
    <React.Suspense fallback={<Hold height={props.rows !== undefined ? heightOf(props.rows) : 125} label="Loading the log" />}>
      <LogViewer {...props} />
    </React.Suspense>
  );
}
