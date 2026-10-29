/**
 * The opened commit's Output (plan 01a17ba155a17737a39348c6c5362210, step 3): the picked job's
 * output in the run page's LogViewer, its step by stepToShow: stdout, stderr and the terminal's
 * colours, streamed live (backlog, then live frames). A fixed height that scrolls inside itself
 * and follows the end unless you've scrolled up, the lanes card's width (ship.css).
 */
import * as React from "react";
import { Check, Copy } from "lucide-react";
import { api } from "@/api/client";
import { shipApi, type ShipCommit } from "@/api/ship";
import { STAGE_WORD, type Stage } from "@/components/ui/status-glyph-spec";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import LogViewer from "@/views/run-detail/log-viewer";
import { stepToShow } from "@/views/updates/logs";
import { pickCard } from "./model";

/** The card's height in terminal rows, whatever the log holds. */
const OUTPUT_ROWS = 14;

export function ShipOutput({ commit, stage }: { commit: ShipCommit; stage: string | null }) {
  const detail = useSuspenseResource(`ship/commit/${commit.repo}/${commit.sha}`, (s) => shipApi.commit(commit.repo, commit.sha, s), { pollMs: 15_000, on: ["run", "job"] });
  // Checks' jobs are its legs, the shown one picked by the same rule as the cards.
  const legs = stage === "checks" ? (detail.data.checks ?? []).flatMap((c) => c.legs) : [];
  // As legs come and go the job shown changes: the one shown stays until the next has loaded, so
  // the card isn't torn down meanwhile and a focus in it survives (opened commit's step 3).
  const job = React.useDeferredValue(legs[pickCard(legs)]?.job);
  if (!job)
    return (
      <div className="ship-output ship-output--none">
        <p>{stage ? `${STAGE_WORD[stage as Stage] ?? stage} has no job output yet.` : "No job output yet."}</p>
      </div>
    );
  return <JobOutput job={job} />;
}

function JobOutput({ job }: { job: string }) {
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
        <span className="ship-output__name">{j.data.display || j.data.key}</span>
        <button type="button" className="icon-btn" data-variant="bare" data-size="sm" aria-label={copied ? "Copied the output" : "Copy the output"} onClick={() => void copy()}>
          {copied ? <Check size={14} /> : <Copy size={14} />}
        </button>
      </div>
      <LogViewer key={`${job}/${step}`} jobId={job} step={step} live={j.data.status === "running"} rows={OUTPUT_ROWS} />
    </div>
  );
}
