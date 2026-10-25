import * as React from "react";
import { AlertCircle, ArrowDown, Check, ChevronDown, Copy, Download, Maximize2, Minimize2, Terminal } from "lucide-react";
import { events, useConnection } from "@/api/events";
import type { UpdateJob } from "@/api/types";
import { toast } from "@/components/ui/toast";
import { formatDuration, shortSha } from "@/lib/format";
import { errorMessage, useNow } from "@/lib/hooks";
import { copyText } from "@/views/terminal/clipboard";
import "./upgrade-log.css";

interface UpgradeLogProps {
  job: UpdateJob;
  onChangeWaits?: () => void;
  /** Its output shows from the start, finished or not (else only while it runs). */
  defaultOpen?: boolean;
}

/** A new upgrade gets its own disclosure and scroll state, even in the same runner row. */
export function UpgradeLog(props: UpgradeLogProps) {
  return <UpgradeLogView key={props.job.id} {...props} />;
}

function UpgradeLogView({ job, onChangeWaits, defaultOpen = false }: UpgradeLogProps) {
  const running = job.state === "running";
  const [open, setOpen] = React.useState(running || defaultOpen);
  const [expanded, setExpanded] = React.useState(false);
  const [following, setFollowing] = React.useState(true);
  const [copiedAt, setCopiedAt] = React.useState(0);
  const [feedback, setFeedback] = React.useState("");
  const [runningSeen, setRunningSeen] = React.useState(running);
  if (running !== runningSeen) {
    setRunningSeen(running);
    if (running) setOpen(true);
  }

  const now = useNow();
  const connection = useConnection();
  const reconnecting = running && connection.state !== "open";
  const viewport = React.useRef<HTMLDivElement | null>(null);
  const followRef = React.useRef(true);
  const scrollTop = React.useRef(0);
  const outputId = React.useId();
  const text = job.log.join("\n");
  const copied = copiedAt > 0 && now - copiedAt < 2_000;
  const state = running ? "Running" : job.state === "done" ? "Complete" : job.timed_out ? "Timed out" : "Failed";
  const title = job.method === "build" ? "Build output" : job.method === "restart" ? "Restart output" : "Upgrade output";
  const end = job.ended_at ?? (running ? now / 1000 : null);
  const duration = formatDuration(end === null ? null : end - job.started_at);
  const target = job.commit ? shortSha(job.commit) : job.to_build ? `build ${job.to_build}` : null;

  const setFollow = (next: boolean) => {
    followRef.current = next;
    setFollowing(next);
  };

  // Observe the content, not its length: replacement snapshots, wrapping and height changes
  // all follow correctly. Reading earlier output always takes precedence over incoming lines.
  const attachViewport = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    viewport.current = el;
    el.scrollTop = followRef.current ? el.scrollHeight : scrollTop.current;
    const observer = new ResizeObserver(() => {
      if (followRef.current) el.scrollTop = el.scrollHeight;
    });
    observer.observe(el);
    if (el.firstElementChild) observer.observe(el.firstElementChild);
    return () => {
      scrollTop.current = el.scrollTop;
      observer.disconnect();
      if (viewport.current === el) viewport.current = null;
    };
  }, []);

  const jumpToLatest = () => {
    setFollow(true);
    if (viewport.current) viewport.current.scrollTop = viewport.current.scrollHeight;
  };

  const copy = async () => {
    try {
      if (!(await copyText(text))) throw new Error("Select the output and copy it with your browser.");
      setCopiedAt(Date.now());
      setFeedback("Upgrade output copied.");
    } catch (error) {
      setFeedback("Could not copy the output.");
      toast.error("Couldn't copy the output", errorMessage(error));
    }
  };

  const download = () => {
    let url: string | null = null;
    let link: HTMLAnchorElement | null = null;
    try {
      url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
      link = document.createElement("a");
      link.href = url;
      link.download = `kiln-upgrade-${job.target}-${job.id}`.replace(/[^a-zA-Z0-9._-]/g, "-") + ".log";
      document.body.appendChild(link);
      link.click();
      setFeedback("Log download requested.");
    } catch (error) {
      setFeedback("Could not download the output.");
      toast.error("Couldn't download the output", errorMessage(error));
    } finally {
      link?.remove();
      // Give the browser time to start reading the object URL before releasing it.
      const objectUrl = url;
      if (objectUrl) window.setTimeout(() => URL.revokeObjectURL(objectUrl), 1_000);
    }
  };

  return (
    <section className="upgrade-log" data-state={job.state} data-expanded={expanded || undefined} aria-label={`${title} for ${job.target}`}>
      <div className="upgrade-log-head">
        <button type="button" className="upgrade-log-toggle" aria-expanded={open} aria-controls={outputId} onClick={() => setOpen((value) => !value)}>
          <Terminal size={15} aria-hidden="true" className="upgrade-log-icon" />
          <span className="upgrade-log-heading">
            <span className="upgrade-log-title">{title}</span>
            <span className="upgrade-log-meta">
              <span className="upgrade-log-state"><i aria-hidden="true" />{state}</span>
              <span aria-hidden="true">·</span>
              <span className="upgrade-log-duration" title="Elapsed time">{duration}</span>
              {target && <span className="upgrade-log-target" title={job.commit ?? job.to_build ?? undefined}>{target}</span>}
            </span>
          </span>
          <ChevronDown size={14} aria-hidden="true" className="upgrade-log-chevron" data-open={open || undefined} />
        </button>
        {open && (
          <div className="upgrade-log-actions" aria-label="Output controls">
            <button type="button" onClick={() => void copy()} disabled={!text} aria-label={copied ? "Output copied" : "Copy output"} title={copied ? "Copied" : "Copy output"}>
              {copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />}
            </button>
            <button type="button" onClick={download} disabled={!text} aria-label="Download output" title="Download output"><Download size={14} aria-hidden="true" /></button>
            <button type="button" onClick={() => setExpanded((value) => !value)} aria-label={expanded ? "Reduce output height" : "Expand output height"} title={expanded ? "Reduce height" : "Expand height"} aria-pressed={expanded}>
              {expanded ? <Minimize2 size={14} aria-hidden="true" /> : <Maximize2 size={14} aria-hidden="true" />}
            </button>
          </div>
        )}
      </div>
      <span className="sr-only" role="status">{`${title}: ${state}${reconnecting ? ". Reconnecting to live output." : "."}`}</span>
      <span className="sr-only" role="status">{feedback}</span>
      <div id={outputId} hidden={!open}>
        {open && (
          <>
            <div
              ref={attachViewport}
              className="upgrade-log-viewport"
              role="region"
              aria-label="Upgrade log output"
              tabIndex={0}
              onScroll={(event) => {
                const el = event.currentTarget;
                scrollTop.current = el.scrollTop;
                // Only the explicit jump resumes following. A replacement snapshot can shrink
                // the output and clamp scrollTop to the bottom; that isn't a request to follow.
                if (el.scrollHeight - el.scrollTop - el.clientHeight > 4) setFollow(false);
              }}
              onWheel={(event) => {
                if (event.deltaY < 0 && event.currentTarget.scrollHeight > event.currentTarget.clientHeight) setFollow(false);
              }}
              onKeyDown={(event) => {
                if (["ArrowUp", "PageUp", "Home"].includes(event.key) && event.currentTarget.scrollHeight > event.currentTarget.clientHeight) setFollow(false);
              }}
            >
              <pre className="upgrade-log-output" data-empty={!text || undefined}>{text || (running ? "Waiting for the first output…" : "No output was recorded.")}</pre>
            </div>
            <div className="upgrade-log-foot">
              <span>{following ? running ? "Following output" : `${job.log.length} ${job.log.length === 1 ? "line" : "lines"}` : "Scroll paused"}</span>
              {!following && <button type="button" onClick={jumpToLatest}><ArrowDown size={12} aria-hidden="true" />Jump to latest</button>}
            </div>
          </>
        )}
      </div>
      {reconnecting && (
        <div className="upgrade-log-notice" data-tone="reconnecting">
          <span>Reconnecting to live output…</span>
          <button type="button" onClick={() => events.retryNow()} disabled={connection.state === "connecting"}>Reconnect</button>
        </div>
      )}
      {job.error && <p className="upgrade-log-error" role="alert"><AlertCircle size={14} aria-hidden="true" /><span>{job.error}</span></p>}
      {job.state === "failed" && job.timed_out && (
        <p className="upgrade-log-timeout">
          Kiln stopped waiting. The runner may still finish; its status will update when it reports back.
          {onChangeWaits && <button type="button" onClick={onChangeWaits}>Change wait time</button>}
        </p>
      )}
    </section>
  );
}
