/**
 * The opened commit's Output (plan 01a17ba155a17737a39348c6c5362210, step 3; as f-ship draws it,
 * plan 01a1856fe81375b1bd1d1d86f270a45b, step 9): the picked job's card, its head the job's glyph
 * and "Stage · job", stdout · stderr and copy; under it the job's output in the run page's
 * LogViewer, its step by stepToShow: stdout, stderr and the terminal's colours, streamed live
 * (backlog, then live frames), each line opening with its time since the job began. A fixed height
 * that scrolls inside itself and follows the end unless you've scrolled up, the lanes card's width
 * (ship.css).
 */
import * as React from "react";
import { Check, Copy } from "lucide-react";
import { api } from "@/api/client";
import { shipApi, type ShipCommit } from "@/api/ship";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import LogViewer from "@/views/run-detail/log-viewer";
import { stepToShow } from "@/views/updates/logs";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { glyphOf } from "./model";
import { partOf, pickPart, stationsOf } from "./jobs";

/** The card's height in terminal rows, whatever the log holds. */
const OUTPUT_ROWS = 14;

export function ShipOutput({ commit, picked }: { commit: ShipCommit; picked: string | null }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipApi.commit(commit.repo, commit.sha, s), { pollMs: 15_000, on: ["run", "job"] });
  // The job picked in Jobs, else the one Jobs shows picked by its rule (plan
  // 01a1856fe81375b1bd1d1d86f270a45b, step 8): one that ran has its job's output.
  const stations = stationsOf(detail.data);
  const part = partOf(stations, picked ?? pickPart(stations));
  // As jobs come and go the job shown changes: the one shown stays until the next has loaded, so
  // the card isn't torn down meanwhile and a focus in it survives (opened commit's step 3).
  const job = React.useDeferredValue(part?.lane.job);
  const title = part ? `${part.group.label} · ${part.lane.name}` : "No job yet";
  const state = part?.lane.state ?? "none";
  if (!job)
    return (
      <div className="ship-output ship-output--none">
        <div className="ship-output__bar">
          <StatusGlyph state={glyphOf(state)} size={12} />
          <span className="ship-output__name">{title}</span>
        </div>
        <p>No output yet.</p>
      </div>
    );
  return <JobOutput job={job} title={title} state={state} />;
}

function JobOutput({ job, title, state }: { job: string; title: string; state: string }) {
  const j = useSuspenseResource(`ship/job/${job}`, (s) => api.job(job, s), { on: ["job"] });
  const step = stepToShow(j.data.steps) ?? 0;
  const [copied, setCopied] = React.useState(false);
  // Copy (step 4): the step's whole output as the REST log has it, through the clipboard as
  // kiln's CopyButton writes it; a refusal says so.
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(await api.jobLogs(job, step));
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1500);
    } catch (e) {
      toast({ title: "Couldn't copy the output", description: errorMessage(e), tone: "failed" });
    }
  };
  return (
    <div className="ship-output" data-job={job} data-step={step}>
      <div className="ship-output__bar">
        <StatusGlyph state={glyphOf(state)} size={12} />
        <span className="ship-output__name">{title}</span>
        <span className="ship-output__streams">stdout · stderr</span>
        <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label={copied ? "Copied the output" : "Copy the output"} onClick={() => void copy()}>
          {copied ? <Check size={14} /> : <Copy size={14} />}
        </button>
      </div>
      <LogViewer
        key={`${job}/${step}`}
        jobId={job}
        step={step}
        live={j.data.status === "running"}
        rows={OUTPUT_ROWS}
        bare
        {...(j.data.started_at != null ? { timed: { begun: j.data.started_at } } : {})}
      />
    </div>
  );
}
