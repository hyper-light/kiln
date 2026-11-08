/**
 * Prose where markdown's renderer (lib/markdown.tsx, its largest library) may not be loaded yet:
 * Mission control's approvals and the palette's replies. Its words as they are until the renderer
 * is in, then drawn by prose.tsx; at once where that's loaded already (the conversation).
 */
import * as React from "react";
import { cn } from "@/lib/utils";

export interface ProseProps {
  text: string;
  className?: string;
  /** The event it says (for find). */
  ev?: string;
  /** A message's `seq`: the files it mentions show in it, as they were then (files.tsx). */
  seq?: number;
  /** It's still coming: only its last block is read again as each word comes (the blocks before it can't change). */
  streaming?: boolean;
}

/** prose.tsx's Prose, once it's loaded. */
export const proseHere: { Prose?: React.ComponentType<ProseProps> } = {};

const Loading = React.lazy(() => import("./prose").then((m) => ({ default: m.Prose })));

export function LaterProse(props: ProseProps) {
  const Here = proseHere.Prose;
  if (Here) return <Here {...props} />;
  return (
    <React.Suspense
      fallback={
        <div className={cn("hx-prose", props.className)} data-ev={props.ev}>
          <p className="whitespace-pre-wrap">{props.text}</p>
        </div>
      }
    >
      <Loading {...props} />
    </React.Suspense>
  );
}
