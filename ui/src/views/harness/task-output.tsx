/** Native subagent output uses the task endpoint, which reads its transcript or harness output. */
import * as React from "react";
import { harnessApi, type Task, type TaskOutput } from "@/api/harness";
import { errorMessage } from "@/lib/hooks";
import { taskLive } from "./derive";
import { Output } from "./parts";

type Snapshot = { result: TaskOutput | null; error: string | null };
const EMPTY: Snapshot = { result: null, error: null };

export function SubagentOutput({ agent, task, visible }: { agent: string; task: Task; visible: boolean }) {
  const { id, status, ended, summary } = task;
  const live = taskLive(task);
  const snapshot = React.useMemo(() => ({ agent, id, value: EMPTY }), [agent, id]);
  const [retry, setRetry] = React.useState(0);
  const subscribe = React.useCallback((changed: () => void) => {
    // These changes request a final read, even if the child's summary arrives after completion.
    void status; void ended; void summary; void retry;
    if (!visible) return () => {};
    let stopped = false;
    let timer = 0;
    let request: AbortController | null = null;
    const poll = async () => {
      if (live && document.hidden) {
        timer = window.setTimeout(poll, 2000);
        return;
      }
      const controller = new AbortController();
      request = controller;
      const timeout = window.setTimeout(() => controller.abort(), 15000);
      try {
        // Native transcripts can replace streamed messages: each read is a full snapshot.
        const result = await harnessApi.taskOutput(snapshot.agent, snapshot.id, undefined, controller.signal);
        if (stopped) return;
        const previous = snapshot.value.result;
        snapshot.value = result.error
          ? { ...snapshot.value, error: result.error }
          : { result: result.gone && previous?.data ? { ...previous, live: result.live, gone: true } : result, error: null };
        changed();
      } catch (error) {
        if (stopped) return;
        snapshot.value = { ...snapshot.value, error: controller.signal.aborted ? "The output request timed out." : errorMessage(error) };
        changed();
      } finally {
        window.clearTimeout(timeout);
        request = null;
        if (!stopped && live) timer = window.setTimeout(poll, 2000);
      }
    };
    void poll();
    return () => {
      stopped = true;
      window.clearTimeout(timer);
      request?.abort();
    };
  }, [live, visible, status, ended, summary, retry, snapshot]);
  const { result, error } = React.useSyncExternalStore(subscribe, () => snapshot.value, () => EMPTY);
  // Claude may remove a completed child's output file; its saved result still belongs here.
  const text = result?.data || (!live ? summary ?? "" : "");
  const empty = !text && result !== null && !error;
  return (
    <div className="hx-stack">
      {text && <Output text={text} truncated={result?.truncated} />}
      {!text && !result && !error && <p className="in-none" role="status">Loading subagent output…</p>}
      {empty && <p className="in-none">{result.gone ? "This subagent's output is no longer available." : live || result.pending ? "No output yet." : "No output was recorded for this subagent."}</p>}
      {text && result?.gone && <p className="in-none">Showing the last available output.</p>}
      {error && <p className="in-none" role="alert">Couldn't load subagent output: {error}</p>}
      {(error || result?.gone || (empty && !live)) && (
        <button type="button" className="hx-link self-start" onClick={() => setRetry((n) => n + 1)}>Try again</button>
      )}
    </div>
  );
}
