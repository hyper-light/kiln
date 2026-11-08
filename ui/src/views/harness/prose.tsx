/**
 * The agent's prose, with markdown's renderer (lib/markdown.tsx): the conversation and the plans
 * import it as they render. Where a page shows prose only now and then (Mission control's
 * approvals, the palette's replies), prose-later.tsx loads this when it's first shown.
 */
import * as React from "react";
import "./harness.css";
import { MarkdownBody, type LocalFile } from "@/lib/markdown";
import { streamBlocks } from "@/lib/markdown-blocks";
import { cn } from "@/lib/utils";
import { MessageFile, MessageSeq } from "./files";
import { proseHere, type ProseProps } from "./prose-later";

export type { ProseProps };

/** The agent's prose: GitHub-flavored markdown (tables, lists, task lists, quotes, code), drawn the
 *  console's way. Text only: HTML in it is never injected. */
export const Prose = React.memo(function Prose({ text, className, ev, seq, streaming }: ProseProps) {
  const files = seq == null ? undefined : MessageFile;
  // Once it came in here a block at a time it stays so: done, nothing is drawn again.
  const [came] = React.useState({ streaming: false });
  if (streaming) came.streaming = true;
  const body = came.streaming ? <StreamingBody text={text} files={files} /> : <MarkdownBody text={text} files={files} />;
  return (
    <div className={cn("hx-prose", className)} data-ev={ev}>
      {seq == null ? body : <MessageSeq.Provider value={seq}>{body}</MessageSeq.Provider>}
    </div>
  );
});

/** A block of a message still coming: drawn once, however long the rest takes to come. */
const Block = React.memo(MarkdownBody);

/** A message still coming, a block at a time (markdown-blocks.ts): what it renders is what it would whole. */
function StreamingBody({ text, files }: { text: string; files?: LocalFile }) {
  return streamBlocks(text).map((block, i) => <Block key={i} text={block} files={files} />);
}

// Loaded: prose-later.tsx draws with it at once from now on.
proseHere.Prose = Prose;
