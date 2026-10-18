"use client";
/**
 * A small Copy button for a block of code or output. A client component: markdown rendered on the
 * server (lib/markdown.tsx in a server component) keeps it working in the browser.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import { copyText } from "@/views/terminal/clipboard";

/** A small Copy button for a block of code or output. */
export function CopyBlock({ text, className }: { text: string; className?: string }) {
  const [done, setDone] = React.useState(false);
  return (
    <button
      type="button"
      className={cn("hx-copy", className)}
      onClick={() =>
        void copyText(text).then((ok) => {
          setDone(ok);
          if (ok) window.setTimeout(() => setDone(false), 1500);
        })
      }
    >
      {done ? "Copied" : "Copy"}
    </button>
  );
}
