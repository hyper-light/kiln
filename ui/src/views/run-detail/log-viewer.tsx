import * as React from "react";
import { ArrowDownToLine, ChevronDown, ChevronUp, Download, Search, X } from "lucide-react";
import { api, wsUrl } from "@/api/client";
import type { LogControlFrame, Status } from "@/api/types";
import { createTerminal, ensureFont, safeFit, SEARCH_DECORATIONS, type KilnTerminal } from "@/components/terminal/xterm-setup";
import { Tip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

const MAX_ROWS = 38;
const MIN_ROWS = 4;

/**
 * Read-only xterm.js log for one step. Streams /api/jobs/:id/logs/ws?step=N
 * (backlog then live binary frames); falls back to the REST endpoint.
 */
export default function LogViewer({
  jobId,
  step,
  live,
  onStepStatus,
  rows: fixedRows,
}: {
  jobId: string;
  step: number;
  live: boolean;
  onStepStatus?: (idx: number, status: Status) => void;
  /** A fixed height of this many rows (Ship's Output card): the box holds still as lines arrive. */
  rows?: number;
}) {
  const kRef = React.useRef<KilnTerminal | null>(null);
  const [follow, setFollow] = React.useState(true);
  const followRef = React.useRef(true);
  followRef.current = follow;
  const [ended, setEnded] = React.useState(false);
  const [bytes, setBytes] = React.useState(0);
  const [searchOpen, setSearchOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const [matches, setMatches] = React.useState<{ idx: number; count: number } | null>(null);
  const [grown, setRows] = React.useState(MIN_ROWS);
  const rows = fixedRows ?? grown;
  const cb = React.useRef(onStepStatus);
  cb.current = onStepStatus;

  // The terminal and its stream live as long as the element they draw into (and the step).
  const hostRef = React.useCallback(
    (host: HTMLDivElement | null) => {
      if (!host) return;
      let disposed = false;
      let ws: WebSocket | null = null;
      let total = 0;
      let gotAny = false;
      let growRaf = 0;
      const subs: { dispose(): void }[] = [];

      // The box grows with the log up to MAX_ROWS (the observer below fits the terminal to it).
      const grow = (k: KilnTerminal) => {
        cancelAnimationFrame(growRaf);
        growRaf = requestAnimationFrame(() => {
          const buf = k.term.buffer.active;
          const want = Math.max(MIN_ROWS, Math.min(MAX_ROWS, buf.length + 1));
          setRows((r) => (want > r ? want : r));
          if (followRef.current) k.term.scrollToBottom();
        });
      };

      void ensureFont().then(() => {
        if (disposed) return;
        const k = createTerminal(
          host,
          {
            disableStdin: true,
            convertEol: true,
            cursorBlink: false,
            cursorStyle: "bar",
            cursorInactiveStyle: "none",
            scrollback: 100_000,
            fontSize: 12,
            lineHeight: 1.18,
          },
          false,
        );
        kRef.current = k;
        // Hide the cursor entirely: this is a log, not a prompt.
        k.term.write("\x1b[?25l");
        safeFit(k, host);

        subs.push(
          k.term.onScroll(() => {
            const b = k.term.buffer.active;
            const atBottom = b.viewportY >= b.baseY;
            if (atBottom !== followRef.current) setFollow(atBottom);
          }),
          k.search.onDidChangeResults((r) => setMatches(r ? { idx: r.resultIndex, count: r.resultCount } : null)),
        );

        const write = (data: Uint8Array | string) => {
          total += typeof data === "string" ? data.length : data.byteLength;
          gotAny = true;
          k.term.write(data, () => grow(k));
          setBytes(total);
        };

        const fallback = () => {
          api
            .jobLogs(jobId, step)
            .then((text) => {
              if (disposed || gotAny) return;
              write(text);
              setEnded(true);
            })
            .catch(() => {
              if (!disposed && !gotAny) k.term.write("\x1b[38;2;151;153;159m(no output)\x1b[0m");
            });
        };

        try {
          ws = new WebSocket(wsUrl(`/api/jobs/${encodeURIComponent(jobId)}/logs/ws?step=${step}`));
          ws.binaryType = "arraybuffer";
          ws.onmessage = (ev) => {
            if (typeof ev.data === "string") {
              try {
                const msg = JSON.parse(ev.data) as LogControlFrame;
                if (msg.type === "end") setEnded(true);
                else if (msg.type === "step") cb.current?.(msg.idx, msg.status);
              } catch {
                write(ev.data);
              }
              return;
            }
            write(new Uint8Array(ev.data as ArrayBuffer));
          };
          ws.onclose = () => {
            if (disposed) return;
            if (!gotAny) fallback();
            setEnded(true);
          };
        } catch {
          fallback();
        }
      });

      // The box's height changes as it grows: the terminal fits it once a frame.
      let fitRaf = 0;
      const ro = new ResizeObserver(() => {
        cancelAnimationFrame(fitRaf);
        fitRaf = requestAnimationFrame(() => {
          if (kRef.current) safeFit(kRef.current, host);
        });
      });
      ro.observe(host);

      return () => {
        disposed = true;
        cancelAnimationFrame(growRaf);
        cancelAnimationFrame(fitRaf);
        ro.disconnect();
        subs.forEach((s) => s.dispose());
        ws?.close();
        kRef.current?.dispose();
        kRef.current = null;
      };
    },
    [jobId, step],
  );

  const runSearch = (dir: "next" | "prev", q = query) => {
    const k = kRef.current;
    if (!k || !q) {
      k?.search.clearDecorations();
      setMatches(null);
      return;
    }
    const opts = { caseSensitive: false, regex: false, decorations: SEARCH_DECORATIONS };
    if (dir === "next") k.search.findNext(q, opts);
    else k.search.findPrevious(q, opts);
  };

  const lineH = Math.ceil(12 * 1.18);
  const height = rows * lineH + 20;

  return (
    <div className="border-t border-[#1d2128]">
      <div className="flex flex-wrap items-center gap-1 px-3 py-1.5">
        <span className="label mr-auto flex items-center gap-2">
          {live && !ended ? (
            <>
              <i className="dot" data-status="running" aria-hidden="true" /> streaming
            </>
          ) : (
            <>log</>
          )}
          <span className="text-[#6d7078]">· {bytes >= 1024 ? `${(bytes / 1024).toFixed(1)} KiB` : `${bytes} B`}</span>
        </span>
        {searchOpen && (
          <div className="field h-8 min-h-8 w-full max-w-[260px] sm:w-56">
            <Search size={13} aria-hidden="true" />
            <input
              autoFocus
              className="!min-h-7 !text-[12px]"
              placeholder="Find in log…"
              aria-label="Find in log"
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                runSearch("next", e.target.value);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter") runSearch(e.shiftKey ? "prev" : "next");
                if (e.key === "Escape") {
                  setSearchOpen(false);
                  kRef.current?.search.clearDecorations();
                }
              }}
            />
            <span className="shrink-0 pr-2 font-mono text-[10px] text-[#7f838d]">
              {matches && query ? (matches.count ? `${matches.idx + 1}/${matches.count}` : "0") : ""}
            </span>
          </div>
        )}
        {searchOpen && (
          <>
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Previous match" onClick={() => runSearch("prev")}>
              <ChevronUp size={14} />
            </button>
            <button className="icon-btn" data-variant="bare" data-size="sm" aria-label="Next match" onClick={() => runSearch("next")}>
              <ChevronDown size={14} />
            </button>
          </>
        )}
        <Tip content={searchOpen ? "Close search" : "Search log"}>
          <button
            className="icon-btn"
            data-variant="bare"
            data-size="sm"
            aria-label={searchOpen ? "Close search" : "Search log"}
            onClick={() => {
              if (searchOpen) kRef.current?.search.clearDecorations();
              setSearchOpen((o) => !o);
            }}
          >
            {searchOpen ? <X size={14} /> : <Search size={14} />}
          </button>
        </Tip>
        <Tip content={follow ? "Following output" : "Jump to end and follow"}>
          <button
            className={cn("icon-btn", follow && "!text-[#d8d0f2]")}
            data-variant="bare"
            data-size="sm"
            aria-pressed={follow}
            aria-label="Auto-follow"
            onClick={() => {
              kRef.current?.term.scrollToBottom();
              setFollow(true);
            }}
          >
            <ArrowDownToLine size={14} />
          </button>
        </Tip>
        <Tip content="Download raw log">
          <a className="icon-btn" data-variant="bare" data-size="sm" aria-label="Download raw log" href={api.jobLogsUrl(jobId, step)} download={`${jobId}-step-${step}.log`}>
            <Download size={14} />
          </a>
        </Tip>
      </div>
      <div className="relative bg-[#08090a]" style={{ height }}>
        <div ref={hostRef} className="absolute inset-0 py-2 pr-1 pl-3" data-terminal />
      </div>
    </div>
  );
}
