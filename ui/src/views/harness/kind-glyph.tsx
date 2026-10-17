import "./kind-glyph.css";
import { cn } from "@/lib/utils";

/** What runs beside an agent, each drawn as its own small mark (the Optic means thinking). */
export type GlyphKind = "agent" | "shell" | "monitor" | "mcp" | "workflow";

const PATHS: Record<GlyphKind, React.ReactNode> = {
  // A subagent: a point that fans out.
  agent: (
    <>
      <circle cx="3" cy="8" r="1.5" fill="currentColor" stroke="none" />
      <path d="M4.6 8C8 8 9.8 4.6 13.5 3.6M4.6 8H13.5M4.6 8C8 8 9.8 11.4 13.5 12.4" />
    </>
  ),
  // A shell: a prompt and its cursor.
  shell: <path d="M3 4.5 6.5 8 3 11.5M8.5 11.5H13" />,
  // A monitor: a line that waits for one beat.
  monitor: <path d="M1.5 8.5H5l1.5-4L9 12l1.5-3.5h4" />,
  // An MCP call: a plug.
  mcp: <path d="M6 2.5v3M10 2.5v3M4.5 5.5h7V8a3.5 3.5 0 0 1-7 0zM8 11.5V14" />,
  // A workflow: one step leading to two.
  workflow: (
    <>
      <circle cx="3.2" cy="8" r="1.5" />
      <circle cx="12.8" cy="4" r="1.5" />
      <circle cx="12.8" cy="12" r="1.5" />
      <path d="M4.6 7.4 11.4 4.6M4.6 8.6 11.4 11.4" />
    </>
  ),
};

/** Maps a task's class (`classOf`) to its glyph; anything unknown is drawn as a subagent. */
export function glyphKind(cls: string): GlyphKind {
  return cls === "shell" || cls === "monitor" || cls === "mcp" || cls === "workflow" ? cls : "agent";
}

export function KindGlyph({ kind, live = false, pending = false, className }: { kind: GlyphKind; live?: boolean; pending?: boolean; className?: string }) {
  return (
    <svg className={cn("kg", className)} viewBox="0 0 16 16" data-kind={kind} data-live={live || undefined} data-pending={pending || undefined} aria-hidden="true">
      {PATHS[kind]}
    </svg>
  );
}
