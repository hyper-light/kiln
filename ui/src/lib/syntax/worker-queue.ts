/** W26-R15: serialize the mutable parsers, yielding between pieces so cancellation can be read. */
import type { Highlight } from "./engine";
import { languageById } from "./languages";
import {
  MAX_SYNTAX_JOBS,
  MAX_SYNTAX_PENDING_CHARS,
  MAX_SYNTAX_RESULT_LINES,
  MAX_SYNTAX_RESULT_TOKENS,
  textSize,
  type HighlightLines,
  type HighlightRequest,
  type HighlightResponse,
} from "./worker-protocol";

export interface WorkerHighlighter {
  load(language: string): Promise<boolean>;
  highlight(language: string, text: string): Highlight | null;
}

interface HighlightJob {
  id: number;
  language: string;
  texts: readonly string[];
  size: number;
  cancelled: boolean;
}

export class HighlightQueue {
  private jobs = new Map<number, HighlightJob>();
  private queue: HighlightJob[] = [];
  private running = false;
  private size = 0;

  constructor(
    private highlighter: WorkerHighlighter,
    private send: (response: HighlightResponse) => void,
    private yieldTask: () => Promise<void> = () => new Promise((done) => setTimeout(done, 0)),
  ) {}

  receive(message: HighlightRequest) {
    if (!message || !Number.isSafeInteger(message.id) || message.id < 1) return;
    if (message.type === "cancel") {
      const job = this.jobs.get(message.id);
      if (!job) return;
      job.cancelled = true;
      job.texts = [];
      this.queue = this.queue.filter((queued) => queued !== job);
      this.release(job);
      return;
    }
    if (message.type !== "highlight") return;
    const size = textSize(message.texts);
    if (!languageById(message.language) || size === null || this.jobs.has(message.id) || this.jobs.size >= MAX_SYNTAX_JOBS || this.size + size > MAX_SYNTAX_PENDING_CHARS) {
      this.send({ type: "error", id: message.id });
      return;
    }
    const job: HighlightJob = { ...message, size, cancelled: false };
    this.jobs.set(job.id, job);
    this.size += size;
    this.queue.push(job);
    void this.drain();
  }

  private release(job: HighlightJob) {
    if (this.jobs.get(job.id) !== job) return;
    this.jobs.delete(job.id);
    this.size -= job.size;
  }

  private async drain() {
    if (this.running) return;
    this.running = true;
    try {
      let job: HighlightJob | undefined;
      while ((job = this.queue.shift())) {
        if (job.cancelled) continue;
        try {
          const lines = await this.run(job);
          if (!job.cancelled) this.send(lines ? { type: "highlight", id: job.id, lines } : { type: "error", id: job.id });
        } catch {
          if (!job.cancelled) this.send({ type: "error", id: job.id });
        } finally {
          job.texts = [];
          this.release(job);
        }
        await this.yieldTask();
      }
    } finally {
      this.running = false;
    }
  }

  private async run(job: HighlightJob): Promise<HighlightLines | null> {
    if (!(await this.highlighter.load(job.language)) || job.cancelled) return null;
    const lines: (Highlight["lines"] | null)[] = [];
    let lineCount = 0;
    let tokenCount = 0;
    for (let i = 0; i < job.texts.length; i++) {
      await this.yieldTask();
      if (job.cancelled) return null;
      const text = job.texts[i]!;
      const tried = new Set<string>();
      for (;;) {
        const result = this.highlighter.highlight(job.language, text);
        if (!result || result.wants.length === 0) {
          lineCount += result?.lines.length ?? 0;
          for (const row of result?.lines ?? []) tokenCount += row.length;
          if (lineCount > MAX_SYNTAX_RESULT_LINES || tokenCount > MAX_SYNTAX_RESULT_TOKENS) return null;
          lines.push(result?.lines ?? null);
          break;
        }
        // Failed dependencies stop being wanted in Highlighter; this guard also bounds a
        // malformed dependency cycle without publishing incomplete or misassociated tokens.
        const wanted = result.wants.filter((id) => !tried.has(id));
        if (wanted.length === 0) return null;
        for (const id of wanted) tried.add(id);
        await Promise.all(wanted.map((id) => this.highlighter.load(id)));
        await this.yieldTask();
        if (job.cancelled) return null;
      }
    }
    return lines;
  }
}
