/** A finished job run again, here or elsewhere (docs/ci.md W05-R28), from its own panel: on any
 * runner that can run it, on any but the one it ran on, or on one chosen from those able to. */
import * as React from "react";
import { RotateCcw } from "lucide-react";
import { ci } from "@/api/ci";
import { retriedWords, retryBody, retryChoices } from "@/api/retry";
import { Button } from "@/components/ui/button";
import { SelectField } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { useNavigate } from "@/lib/router";

/** Where it could run again comes back as runners come and go and the job changes. */
const RUNNERS_OPTS = { on: ["job", "agent"] } as const;

export function JobRetry({ jobId, runId }: { jobId: string; runId: string }) {
  const o = useSuspenseResource(`retry/${jobId}`, (s) => ci.legRunners(jobId, s), RUNNERS_OPTS).data;
  const navigate = useNavigate();
  const [choice, setChoice] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  if (!o.retry.can) {
    return o.retry.why ? <p className="px-5 py-3 font-mono text-[11px] text-[#7f8a9a]">Retry: {o.retry.why}</p> : null;
  }
  const choices = retryChoices(o);
  // A choice no longer offered (that runner went) is anywhere again.
  const chosen = choices.some((c) => c.value === choice) ? choice : "";
  const go = async () => {
    setBusy(true);
    try {
      const r = await ci.retryLeg(jobId, retryBody(chosen));
      toast({ title: retriedWords(r), tone: "info" });
      invalidate(`run/${runId}`);
      invalidate(`ci-run/${runId}`);
      invalidate("runs/");
      invalidate(`retry/${jobId}`);
      // A leg's retry is a new attempt of its run: its leg of the same key, there.
      if (r.kind === "ci" && r.key) navigate(`/runs/${encodeURIComponent(runId)}?key=${encodeURIComponent(r.key)}`, { replace: true });
    } catch (e) {
      toast.error("Retry failed", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flex flex-wrap items-center gap-2 px-5 py-3">
      <span className="mr-auto font-mono text-[11px] text-[#a6b2c6]">{o.ran_on ? `Ran on ${o.ran_on}` : "Run it again"}</span>
      {choices.length > 1 && <SelectField label="Where to run it again" hideLabel value={chosen} onChange={setChoice} options={choices} className="w-72 max-w-full" />}
      <Button size="xs" variant="primary" disabled={busy} onClick={go}>
        <RotateCcw /> Retry
      </Button>
    </div>
  );
}
