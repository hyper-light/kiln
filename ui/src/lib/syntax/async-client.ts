/** W26-R15: one lazy worker, bounded unfinished work, component-local derived results. */
import { MAX_SYNTAX_JOBS, MAX_SYNTAX_PENDING_CHARS, textSize, type HighlightLines, type HighlightRequest, type HighlightResponse } from "./worker-protocol";

export type SyntaxWorker = Pick<Worker, "postMessage" | "terminate" | "addEventListener" | "removeEventListener">;
type Answer = (lines: HighlightLines | null) => void;
interface Pending {
  size: number;
  count: number;
  answer: Answer;
}

export class HighlightClient {
  private worker: SyntaxWorker | null = null;
  private pending = new Map<number, Pending>();
  private size = 0;
  private next = 1;

  constructor(private createWorker: () => SyntaxWorker | null) {}

  request(language: string, texts: readonly string[], answer: Answer): () => void {
    const size = textSize(texts);
    if (size === null || this.pending.size >= MAX_SYNTAX_JOBS || this.size + size > MAX_SYNTAX_PENDING_CHARS) {
      answer(null);
      return () => {};
    }
    const worker = this.getWorker();
    if (!worker) {
      answer(null);
      return () => {};
    }
    const id = this.next++;
    this.pending.set(id, { size, count: texts.length, answer });
    this.size += size;
    try {
      worker.postMessage({ type: "highlight", id, language, texts } satisfies HighlightRequest);
    } catch {
      this.fail(worker);
    }
    return () => {
      if (!this.take(id)) return;
      if (this.worker !== worker) return;
      try {
        worker.postMessage({ type: "cancel", id } satisfies HighlightRequest);
      } catch {
        this.fail(worker);
      }
    };
  }

  private take(id: number): Pending | undefined {
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    this.size -= pending.size;
    return pending;
  }

  private getWorker(): SyntaxWorker | null {
    if (this.worker) return this.worker;
    let worker: SyntaxWorker | null;
    try {
      worker = this.createWorker();
    } catch {
      return null;
    }
    if (!worker) return null;
    this.worker = worker;
    worker.addEventListener("message", (event: MessageEvent<HighlightResponse>) => {
      if (this.worker !== worker) return;
      const response = event.data;
      if (!response || (response.type !== "highlight" && response.type !== "error")) return;
      const pending = this.take(response.id);
      if (!pending) return;
      const valid = response.type === "highlight" && Array.isArray(response.lines) && response.lines.length === pending.count;
      pending.answer(valid ? response.lines : null);
    });
    const failed = (event: Event) => {
      event.preventDefault();
      this.fail(worker);
    };
    worker.addEventListener("error", failed);
    worker.addEventListener("messageerror", failed);
    return worker;
  }

  private fail(worker: SyntaxWorker) {
    if (this.worker !== worker) return;
    this.worker = null;
    worker.terminate();
    const pending = [...this.pending.values()];
    this.pending.clear();
    this.size = 0;
    for (const job of pending) job.answer(null);
  }
}

/** A lease belongs to one immutable source. Completed colors stay while its viewport lease is off. */
export function highlightResource(client: HighlightClient, language: string | undefined, texts: readonly string[] | null) {
  let snapshot: HighlightLines | null = null;
  let finished = false;
  let cancel: (() => void) | null = null;
  let generation = 0;
  const listeners = new Set<() => void>();
  const source = texts && [...texts];
  return {
    read: () => snapshot,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      if (!finished && !cancel && language && source) {
        const mine = ++generation;
        const stop = client.request(language, source, (lines) => {
          if (mine !== generation || listeners.size === 0) return;
          snapshot = lines;
          finished = true;
          cancel = null;
          for (const notify of listeners) notify();
        });
        if (!finished) cancel = stop;
      }
      return () => {
        listeners.delete(listener);
        if (listeners.size !== 0 || !cancel) return;
        ++generation;
        const stop = cancel;
        cancel = null;
        stop();
      };
    },
  };
}
