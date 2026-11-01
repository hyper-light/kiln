import * as React from "react";
import { ArrowDownToLine, ChevronDown, ChevronUp, Download, Search, X } from "lucide-react";
import { api, ApiError, wsUrl } from "@/api/client";
import { cdApi } from "@/api/cd";
import { readFrame, readLogFrame } from "@/api/frames";
import type { CdLogLine, CdResourceKey, Status } from "@/api/types";
import type { CdLogDown, UnreadFrame } from "@/api/wire";
import { createTerminal, ensureFont, safeFit, SEARCH_DECORATIONS, type KilnTerminal } from "@/components/terminal/xterm-setup";
import { Tip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { formatDuration } from "@/lib/format";

/** A deployment's log socket's frames, by tag. */
const CD_LOG_FRAMES = { lines: true, error: true, end: true, refused: true } satisfies Record<CdLogDown["type"], true>;
/** How many of a deployment object's last lines are read, and followed from. */
const CD_TAIL = 500;

/** A deployment object's line, as a job's reads: its time (hours, minutes, seconds) dim before it. */
function cdLine(l: CdLogLine): string {
  const t = l.ts ? new Date(l.ts) : null;
  const hms = t && !Number.isNaN(t.getTime()) ? t.toISOString().slice(11, 19) : "        ";
  return `\x1b[38;2;109;112;120m${hms}\x1b[39m  ${l.text}\n`;
}

/** What a log is of: a job's step, or one of a deployment's objects (Ship's Deploys, docs/ci.md W26-R24). */
type LogSource = { jobId: string; step: number; cd?: undefined } | { cd: { app: string; key: CdResourceKey }; jobId?: undefined; step?: undefined };

const MAX_ROWS = 38;
const MIN_ROWS = 4;

/**
 * Read-only xterm.js log for one step. Streams /api/jobs/:id/logs/ws?step=N
 * (backlog then live binary frames); falls back to the REST endpoint. Of a deployment's object
 * (`cd`), its last lines and then what it writes (/api/cd/apps/:app/resources/:key/logs/ws), each
 * with its time; falls back to reading the last lines.
 */
export default function LogViewer({
  jobId,
  step,
  cd,
  live,
  onStepStatus,
  rows: fixedRows,
  timed,
  bare,
}: LogSource & {
  live: boolean;
  onStepStatus?: (idx: number, status: Status) => void;
  /** A fixed height of this many rows (Ship's Output card): the box holds still as lines arrive. */
  rows?: number;
  /** Ship's Output (plan 01a1856fe81375b1bd1d1d86f270a45b, step 9): each line opens with its time
   * since the job began (`begun`, unix seconds), from when kiln noted its piece arrived; a line newer
   * than kiln's note, from when it arrives. */
  timed?: { begun: number };
  /** Without its own bar: Ship's Output draws its header. */
  bare?: boolean;
}) {
  const kRef = React.useRef<KilnTerminal | null>(null);
  // A job's step (a deployment object's log has none).
  const stepN = step ?? 0;
  const begun = timed?.begun;
  const [follow, setFollow] = React.useState(true);
  const followRef = React.useRef(true);
  followRef.current = follow;
  const [ended, setEnded] = React.useState(false);
  const [outputState, setOutputState] = React.useState<string | null>(null);
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
      // Timed output can be received before its pending timing metadata renders it.
      let received = false;
      let canonical = false;
      let complete = false;
      let unavailable = false;
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
        setOutputState(null);
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

        const put = (data: Uint8Array | string) => {
          total += typeof data === "string" ? data.length : data.byteLength;
          gotAny = true;
          k.term.write(data, () => grow(k));
          setBytes(total);
        };
        // Timed: each line opens with its time, dim, in a gutter of one width. A piece kiln noted
        // has its time; one it hadn't yet (newer than its note) has now's. Until kiln's note is
        // read, what comes waits, so every line has its time.
        let noted: { times: [number, number][]; size: number } | null = null;
        const waiting: Uint8Array[] = [];
        let at = 0;
        let fresh = true;
        const gutter = (offset: number) => {
          if (begun === undefined) return "";
          let ms = Date.now();
          if (noted && offset < noted.size) {
            let found: number | null = null;
            for (const [o, t] of noted.times) if (o <= offset) found = t;
            if (found === null) return " ".repeat(9);
            ms = found;
          }
          return `\x1b[38;2;109;112;120m${formatDuration(ms / 1000 - begun).padStart(7)}\x1b[39m  `;
        };
        const lines = (data: Uint8Array) => {
          let i = 0;
          while (i < data.length) {
            // The gutter isn't the log's: drawn, not counted.
            if (fresh) {
              k.term.write(gutter(at + i));
              fresh = false;
            }
            // A line ends at a newline; a carriage return alone (a progress line drawn again)
            // starts it over, so it takes its time again rather than drawing over it.
            let end = data.length;
            for (let j = i; j < data.length; j++) {
              const b = data[j];
              if (b === 0x0a) {
                end = j + 1;
                break;
              }
              if (b === 0x0d) {
                end = data[j + 1] === 0x0a ? j + 2 : j + 1;
                break;
              }
            }
            put(data.subarray(i, end));
            if (end < data.length || data[end - 1] === 0x0a || data[end - 1] === 0x0d) fresh = true;
            i = end;
          }
          at += data.length;
        };
        const write = (data: Uint8Array | string) => {
          if (begun === undefined) return put(data);
          const bytes = typeof data === "string" ? new TextEncoder().encode(data) : data;
          if (!noted) waiting.push(bytes);
          else lines(bytes);
        };
        if (begun !== undefined && jobId !== undefined)
          void api
            .jobLogTimes(jobId, stepN)
            // Unread, no line is given a time it may not have: every gutter blank.
            .catch(() => ({ times: [] as [number, number][], size: Number.MAX_SAFE_INTEGER }))
            .then((n) => {
              if (disposed) return;
              noted = n.size === null ? { times: [], size: Number.MAX_SAFE_INTEGER } : { times: n.times, size: n.size };
              for (const b of waiting.splice(0)) lines(b);
            });

        if (cd) {
          const read = () =>
            cdApi
              .logs(cd.app, cd.key, { tail_lines: CD_TAIL })
              .then((r) => {
                if (disposed || gotAny) return;
                for (const l of r.lines) put(cdLine(l));
                for (const e of r.errors) put(`\x1b[38;2;226;185;189m${e}\x1b[0m\n`);
                if (!gotAny) k.term.write("\x1b[38;2;151;153;159m(no output)\x1b[0m");
                setEnded(true);
              })
              .catch(() => {
                if (!disposed && !gotAny) k.term.write("\x1b[38;2;151;153;159m(no output)\x1b[0m");
              });
          try {
            ws = new WebSocket(wsUrl(cdApi.logsFollowPath(cd.app, cd.key, { tail_lines: CD_TAIL })));
            ws.onmessage = (ev) => {
              if (typeof ev.data !== "string") return;
              const frame = readFrame<CdLogDown>(ev.data, "type", CD_LOG_FRAMES);
              if ("unread" in frame) {
                if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(frame.unread satisfies UnreadFrame));
                return;
              }
              const msg = frame.frame;
              if (msg.type === "lines") for (const l of msg.lines) put(cdLine(l));
              else if (msg.type === "error") put(`\x1b[38;2;226;185;189m${msg.pod ? `${msg.pod}: ` : ""}${msg.message}\x1b[0m\n`);
              else if (msg.type === "end") setEnded(true);
            };
            ws.onclose = () => {
              if (disposed) return;
              if (!gotAny) void read();
              setEnded(true);
            };
          } catch {
            void read();
          }
          return;
        }
        if (jobId === undefined) return;
        const fallback = () => {
          api
            // A strict fallback may finish only with this capture's own complete result.
            // Jobs without a producer contract retain their legacy response semantics.
            .jobLogs(jobId, stepN, true)
            .then((text) => {
              if (disposed || gotAny || (canonical && received)) return;
              write(text);
              complete = true;
              setOutputState(null);
              setEnded(true);
            })
            .catch((error: unknown) => {
              if (disposed) return;
              if (error instanceof ApiError && (error.message === "log_pending" || error.message === "log_unavailable")) {
                canonical = true;
                setEnded(false);
                setOutputState(error.message === "log_pending" ? "Output is not complete yet. Reload to try again." : "Output is unavailable. Reload to try again.");
              } else if (canonical) {
                setEnded(false);
                setOutputState("Output is unavailable. Reload to try again.");
              } else if (!gotAny) {
                k.term.write("\x1b[38;2;151;153;159m(no output)\x1b[0m");
              }
            });
        };

        try {
          ws = new WebSocket(wsUrl(`/api/jobs/${encodeURIComponent(jobId)}/logs/ws?step=${stepN}`));
          ws.binaryType = "arraybuffer";
          ws.onmessage = (ev) => {
            if (typeof ev.data === "string") {
              // Applied only when it reads as one of the socket's frames; else the server, which sent it, is told.
              const read = readLogFrame(ev.data);
              if ("unread" in read) {
                if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(read.unread satisfies UnreadFrame));
                return;
              }
              const msg = read.frame;
              if (msg.type === "end") {
                complete = true;
                setOutputState(null);
                setEnded(true);
              } else if (msg.type === "pending" || msg.type === "unavailable") {
                canonical = true;
                unavailable = msg.type === "unavailable";
                setOutputState(msg.message);
                setEnded(false);
              } else if (msg.type === "step") cb.current?.(msg.idx, msg.status);
              return;
            }
            received = true;
            if (canonical && !unavailable) setOutputState(null);
            write(new Uint8Array(ev.data as ArrayBuffer));
          };
          ws.onclose = () => {
            if (disposed) return;
            if (canonical) {
              if (!complete && !unavailable) {
                setOutputState("Output is not complete yet. Reload to try again.");
                if (!received) fallback();
              }
              return;
            }
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
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the object's key, by its parts: a new object, a new stream
    [jobId, step, begun, cd?.app, cd?.key.group, cd?.key.kind, cd?.key.namespace, cd?.key.name],
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
    <div className={cn(!bare && "border-t border-[#1d2128]")}>
      <div className={cn("flex flex-wrap items-center gap-1 px-3 py-1.5", bare && "hidden")}>
        <span className="label mr-auto flex items-center gap-2">
          {live && !ended && !outputState ? (
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
            <span className="shrink-0 pr-2 font-mono text-[10px] text-[#7f838d]">{matches && query ? (matches.count ? `${matches.idx + 1}/${matches.count}` : "0") : ""}</span>
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
        {jobId !== undefined && (
          <Tip content="Download raw log">
            <a className="icon-btn" data-variant="bare" data-size="sm" aria-label="Download raw log" href={api.jobLogsUrl(jobId, stepN)} download={`${jobId}-step-${stepN}.log`}>
              <Download size={14} />
            </a>
          </Tip>
        )}
      </div>
      {outputState && (
        <div role="status" className="px-3 py-1.5 text-xs text-[#969aa3]">
          {outputState}
        </div>
      )}
      <div className="relative bg-[#08090a]" style={{ height }}>
        <div ref={hostRef} className="absolute inset-0 py-2 pr-1 pl-3" data-terminal />
      </div>
    </div>
  );
}
