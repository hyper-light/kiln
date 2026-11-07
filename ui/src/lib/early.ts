/**
 * One read of a URL at a time, and the first reads asked before the code is in.
 *
 * A page's first reads used to wait for every file of its code, then for `/api/me`, and only then
 * go out: on a phone each wait is many round trips (Mission control's time to interactive). The
 * document now asks them itself as it's parsed (lib/early-script.ts, in routes/__root.tsx's head), and
 * the app's own fetches of the same URL take that answer: those of its first render, in the page's
 * first seconds, and not after the app has changed anything. And two parts of a page reading the same URL at once (the
 * agents for the composer and for the machines' names) share one request.
 */

/** An early answer is taken only this soon after the page opened (ms): later, it's read afresh. */
const EARLY_MS = 10_000;
/** Taken, it's there for the other reads of the same render this long (ms). */
const SHARED_MS = 1_000;

declare global {
  interface Window {
    __kilnEarly?: Record<string, Promise<Response>>;
  }
}

/** A plain read: GET of a same-origin `/api/…` path, with nothing but credentials and a signal. */
function plainRead(input: RequestInfo | URL, init?: RequestInit): string | null {
  if (typeof input !== "string" || !input.startsWith("/api/")) return null;
  if (!init) return input;
  for (const k of Object.keys(init) as (keyof RequestInit)[]) {
    const v = init[k];
    if (v === undefined) continue;
    if (k === "method" && String(v).toUpperCase() === "GET") continue;
    if (k === "credentials" && v === "same-origin") continue;
    if (k === "signal") continue;
    return null;
  }
  return input;
}

/** `answer` for one reader: its own copy of the body, given up when its signal is. */
function forReader(answer: Promise<Response>, signal: AbortSignal | null | undefined): Promise<Response> {
  const own = answer.then((r) => r.clone());
  if (!signal) return own;
  if (signal.aborted) return Promise.reject(signal.reason);
  return new Promise((resolve, reject) => {
    const abort = () => reject(signal.reason);
    signal.addEventListener("abort", abort, { once: true });
    own.then(
      (r) => {
        signal.removeEventListener("abort", abort);
        resolve(r);
      },
      (e: unknown) => {
        signal.removeEventListener("abort", abort);
        reject(e);
      },
    );
  });
}

/** Puts this module's fetch in front of the page's (after lib/peer-base.ts's, which it calls): client.tsx imports this second, before anything reads. */
function install() {
  const fetchHere = window.fetch.bind(window);
  let early = new Map(Object.entries(window.__kilnEarly ?? {}));
  delete window.__kilnEarly;
  const taken = new Set<string>();
  const reading = new Map<string, Promise<Response>>();
  window.fetch = (input: RequestInfo | URL, init?: RequestInit) => {
    const url = plainRead(input, init);
    if (url === null) {
      // A change: what was asked before it may be out of date now.
      if ((init?.method ?? "GET").toUpperCase() !== "GET") early = new Map();
      return fetchHere(input, init);
    }
    const asked = early.get(url);
    if (asked && performance.now() < EARLY_MS) {
      if (!taken.has(url)) {
        // The page's first render asks from several places at once (the composer's machines and
        // their names both read the agents): each of them takes it, then it's read afresh.
        taken.add(url);
        const from = early;
        window.setTimeout(() => from.delete(url), SHARED_MS);
      }
      return forReader(asked, init?.signal);
    }
    let answer = reading.get(url);
    if (!answer) {
      // Asked without the reader's signal: another reader may still want it.
      const a = fetchHere(url, { credentials: "same-origin" });
      answer = a;
      reading.set(url, a);
      const done = () => {
        if (reading.get(url) === a) reading.delete(url);
      };
      a.then(done, done);
    }
    return forReader(answer, init?.signal);
  };
}

if (typeof window !== "undefined") install();
