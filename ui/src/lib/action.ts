/**
 * A press's request, and whether it's on its way (docs/ci.md W21-R15). Nothing waits on the server
 * to show a press was taken: what it changes shows at once where that's known (and goes back if the
 * request fails); where it isn't, the control pressed says its request has gone (Button's
 * `pending`, or `sendingProps` on any other control) while the rest of the page goes on working.
 *
 * Not a transition: while an async transition is under way, React holds every transition started
 * after it until that one settles (it entangles them), so a request run in one would freeze each
 * tab, filter and list the page changes in a transition until the server answered.
 */
import * as React from "react";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "./hooks";

/** What a request that failed says: a toast's title (its reason under it), or what's done instead. */
export type Failed = string | ((e: unknown) => void);

function failed(e: unknown, how: Failed | undefined) {
  if (typeof how === "function") how(e);
  else toast.error(how ?? "That didn't go through", errorMessage(e));
}

const settles = (v: unknown): v is PromiseLike<unknown> => typeof (v as PromiseLike<unknown> | null)?.then === "function";

/**
 * `[run, pending]`: `run` does `act` at once, in the press itself (what needs the press, a copy or a
 * window opened, still has it), and `pending` holds from then until what it returned settles; a
 * press meanwhile does nothing. A failure goes to `how` (a toast, unless it says otherwise).
 */
export function useAction<A extends unknown[]>(act: (...args: A) => unknown, how?: Failed): [run: (...args: A) => void, pending: boolean] {
  const [pending, setPending] = React.useState(false);
  const going = React.useRef(false);
  const run = (...args: A) => {
    if (going.current) return;
    let r: unknown;
    try {
      r = act(...args);
    } catch (e) {
      failed(e, how);
      return;
    }
    if (!settles(r)) return;
    going.current = true;
    setPending(true);
    void Promise.resolve(r)
      .catch((e: unknown) => failed(e, how))
      .finally(() => {
        going.current = false;
        setPending(false);
      });
  };
  return [run, pending];
}

/**
 * `useAction` for many things one component shows (a list's rows), each by its key: `run(key, …)`
 * and `pending(key)`, one row's request never holding another's press.
 */
export function useActions<A extends unknown[]>(act: (key: string, ...args: A) => unknown, how?: Failed): [run: (key: string, ...args: A) => void, pending: (key: string) => boolean] {
  const [going, setGoing] = React.useState<ReadonlySet<string>>(() => new Set());
  const on = React.useRef(new Set<string>());
  const mark = (key: string, sending: boolean) => {
    if (sending) on.current.add(key);
    else on.current.delete(key);
    setGoing(new Set(on.current));
  };
  const run = (key: string, ...args: A) => {
    if (on.current.has(key)) return;
    let r: unknown;
    try {
      r = act(key, ...args);
    } catch (e) {
      failed(e, how);
      return;
    }
    if (!settles(r)) return;
    mark(key, true);
    void Promise.resolve(r)
      .catch((e: unknown) => failed(e, how))
      .finally(() => mark(key, false));
  };
  return [run, (key) => going.has(key)];
}

/**
 * Says a control's request is on its way, on any element (Button's `pending` does this): the prism
 * rule at its foot after a moment (index.css), `aria-busy` for whoever can't see it.
 */
export const sendingProps = (on: boolean): { "data-sending"?: ""; "aria-busy"?: true } => (on ? { "data-sending": "", "aria-busy": true } : {});
