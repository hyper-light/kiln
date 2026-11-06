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
 * Does `act` now, in the press itself (what needs the press, a copy or a window opened, still has
 * it). Its request, when it returns one: true, and `done` once that settles, a failure gone to `how`
 * first; else false (nothing to wait for).
 */
function begin(act: () => unknown, how: Failed | undefined, done: () => void): boolean {
  let r: unknown;
  try {
    r = act();
  } catch (e) {
    failed(e, how);
    return false;
  }
  if (!settles(r)) return false;
  void Promise.resolve(r)
    .catch((e: unknown) => failed(e, how))
    .finally(done);
  return true;
}

/** A component's own copy of `T`, made once, and a way to render it again as it changes. */
function useOwn<T>(make: (changed: () => void) => T): T {
  const [, bump] = React.useReducer((n: number) => n + 1, 0);
  const [own] = React.useState(() => make(bump));
  return own;
}

/**
 * `[run, pending]`: `run` does `act` at once, in the press itself, and `pending` holds from then until
 * what it returned settles; a press meanwhile does nothing. A failure goes to `how` (a toast, unless
 * it says otherwise).
 */
export function useAction<A extends unknown[]>(act: (...args: A) => unknown, how?: Failed): [run: (...args: A) => void, pending: boolean] {
  const holds = useOwn((changed) => new Holds(changed));
  return [(...args: A) => holds.change("", "", () => act(...args), how), holds.going("") !== undefined];
}

/**
 * `useAction` for many things one component shows (a list's rows), each by its key: `run(key, …)`
 * and `pending(key)`, one row's request never holding another's press.
 */
export function useActions<A extends unknown[]>(act: (key: string, ...args: A) => unknown, how?: Failed): [run: (key: string, ...args: A) => void, pending: (key: string) => boolean] {
  const holds = useOwn((changed) => new Holds(changed));
  return [(key: string, ...args: A) => holds.change(key, "", () => act(key, ...args), how), (key) => holds.going(key) !== undefined];
}

/**
 * One thing's changes, one at a time (W21-R15). What a control of the thing asks goes at once
 * (`change`), unless one of the thing's changes is on its way: then that one's control says so
 * (`doing`), the others wait (`held`), and a press meanwhile does nothing. So no answer lands out of
 * order, and none puts back a later change, as two presses on one thing (Approve and Reject, Save
 * and Delete, a switch flipped twice) could otherwise. A page whose parts change one thing passes its
 * `Hold` down to them.
 */
export interface Hold {
  /**
   * Control `which` asks `act` (what it returns: its request, or an answer already on its way, a
   * dialog's): at once, unless the thing's held. A failure goes to `how`, else the hold's own.
   */
  change: (which: string, act: () => unknown, how?: Failed) => void;
  /** Control `which`'s change is on its way. */
  doing: (which: string) => boolean;
  /** Another control's change of the thing is on its way: `which` waits. */
  held: (which: string) => boolean;
  /** A change of the thing is on its way, whichever control's. */
  going: boolean;
}

/**
 * The changes of things, one at a time each (what `useHolds` keeps, and `useAction`'s and
 * `useActions`' presses, one control each): `changed` hears each change of what's on its way.
 */
export class Holds {
  private on = new Map<string, string>();

  constructor(private readonly changed: () => void) {}

  /** Control `which` of thing `thing` asks `act`: unless one of the thing's changes is on its way. */
  change(thing: string, which: string, act: () => unknown, how?: Failed) {
    if (this.on.has(thing)) return;
    const went = begin(act, how, () => {
      this.on.delete(thing);
      this.changed();
    });
    if (!went) return;
    this.on.set(thing, which);
    this.changed();
  }

  /** The control of `thing` whose change is on its way (none: undefined). */
  going(thing: string): string | undefined {
    return this.on.get(thing);
  }
}

/** The changes of one thing a component shows, one at a time (`Hold`); a failure goes to `how` (a toast, unless it says otherwise). */
export function useHold(how?: Failed): Hold {
  return useHolds(how)("");
}

/** `useHold` for many things one component shows (a list's rows), each by its key: `hold(thing)` is that thing's, one thing's change never holding another's. */
export function useHolds(how?: Failed): (thing: string) => Hold {
  const holds = useOwn((changed) => new Holds(changed));
  return (thing) => {
    const now = holds.going(thing);
    return {
      change: (which, act, said) => holds.change(thing, which, act, said ?? how),
      doing: (which) => now === which,
      held: (which) => now !== undefined && now !== which,
      going: now !== undefined,
    };
  };
}

/**
 * Says a control's request is on its way, on any element (Button's `pending` does this): the prism
 * rule at its foot after a moment (index.css), `aria-busy` for whoever can't see it.
 */
export const sendingProps = (on: boolean): { "data-sending"?: ""; "aria-busy"?: true } => (on ? { "data-sending": "", "aria-busy": true } : {});
