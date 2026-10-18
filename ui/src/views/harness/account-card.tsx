/**
 * Which account each harness runs as on a machine, and signing it in or out, right where you are:
 * under the message that says it isn't signed in, or over the composer after /login or /logout.
 * Never a popup. The harness's own CLI does the work on that machine, in a terminal kiln keeps out
 * of sight; the card shows what it asks of you: a page to open, a code to enter there or to paste
 * back here, or an API key.
 *
 * One line per harness (CLAUDE CODE · signed in · ada@… · Max), the agent's own lit and last,
 * nearest the composer: the machine's other harnesses load in above it and never move it. A step
 * opens under its line, in place: how to sign in (a sensible way already picked), then the page,
 * the code or the field, what it's waiting for, and the CLI's own words behind "Details". Nothing
 * pops while it waits: the rows a sign-in needs are laid out from its start, and polling changes
 * words, never the layout or the order.
 */
import * as React from "react";
import { flushSync } from "react-dom";
import { ArrowUpRight, Check, ChevronRight, Copy, TriangleAlert, X } from "lucide-react";
import { Dialog as D } from "radix-ui";
import { accountsApi, type Account, type AccountFlow, type AccountMethod } from "@/api/harness";
import { Button, buttonVariants } from "@/components/ui/button";
import { errorMessage } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { copyText } from "@/views/terminal/clipboard";
import { machineName } from "./parts";
import { Optic } from "./signals";
import "./account-card.css";

export type AccountIntent = "login" | "logout";

/* ------------------------------------------------------------------ "it isn't signed in" */

/** How each harness says it isn't signed in, or that its sign-in ran out. */
const SIGNED_OUT: Record<string, RegExp> = {
  claude: /please run \/login|invalid api key|oauth token has expired|not logged in|authentication_error/i,
  codex: /\b401\b|unauthorized|not logged in|login required|refresh token/i,
};

/** Whether a harness's message says it needs signing in (only for harnesses kiln can sign in; an MCP server's own 401 is not the harness's account). */
export function saysSignedOut(kind: string, text: string): boolean {
  return (SIGNED_OUT[kind]?.test(text) ?? false) && !/\bmcp\b/i.test(text);
}

/* ------------------------------------------------------------------ the store */

interface FlowEntry {
  flow: AccountFlow;
  /** How many lines of output there were when the code or key went in: until more arrive, it's being checked. */
  sentAt?: number;
  /** Since then it stopped asking (so asking again means what was given was turned down). */
  cleared?: boolean;
  sendError?: string;
  /** When this page saw it end (ms). */
  endedAt?: number;
}

interface MachineView {
  /** null until first read. */
  accounts: Account[] | null;
  /** Why the list couldn't be read. */
  error: string | null;
  /** By kind: the sign-in or sign-out started from this page, running or as it ended. */
  flows: Readonly<Record<string, FlowEntry>>;
}

const NO_VIEW: MachineView = { accounts: null, error: null, flows: {} };
const POLL_MS = 1000;
/** How long an ended sign-in is still news to a card opened after it. */
const OUTCOME_MS = 2 * 60_000;
/** Polls in a row that may fail before kiln says it lost track. */
const MISSES = 5;
const nowSec = () => Math.floor(Date.now() / 1000);

/**
 * Accounts and flows per machine, shared by every card on the page: a sign-in started in one card
 * shows in another, outlives its card (a phone's tab switch), and is polled while any card for its
 * machine is on screen. Codes and keys pass through to the request and are never kept here.
 */
class AccountStore {
  private views = new Map<string, MachineView>();
  private listeners = new Map<string, Set<() => void>>();
  private reading = new Set<string>();
  private polling = new Set<string>();
  /** The way each harness was last signed in from here, by machine: offered first next time. */
  private ways = new Map<string, string>();

  lastWay = (target: string, kind: string) => this.ways.get(`${target}\n${kind}`);

  view = (target: string): MachineView => this.views.get(target) ?? NO_VIEW;

  subscribe = (target: string, fn: () => void) => {
    let set = this.listeners.get(target);
    if (!set) {
      set = new Set();
      this.listeners.set(target, set);
    }
    set.add(fn);
    // A sign-in left running while no card showed it is followed again.
    for (const e of Object.values(this.view(target).flows)) this.poll(e.flow);
    // A card that shows the machine reads its accounts (again: the last read may be old).
    this.load(target);
    return () => {
      set.delete(fn);
    };
  };

  private patch(target: string, p: Partial<MachineView>) {
    this.views.set(target, { ...this.view(target), ...p });
    this.listeners.get(target)?.forEach((l) => l());
  }

  private entry(target: string, kind: string): FlowEntry | undefined {
    return this.view(target).flows[kind];
  }

  /** Reads the machine's accounts; what's known stays on screen meanwhile. */
  load(target: string) {
    if (this.reading.has(target)) return;
    this.reading.add(target);
    accountsApi.list(target).then(
      (accounts) => {
        this.reading.delete(target);
        this.patch(target, { accounts, error: null });
      },
      (e: unknown) => {
        this.reading.delete(target);
        this.patch(target, { error: errorMessage(e) });
      },
    );
  }

  /** Lets go of outcomes that are old news. */
  prune(target: string) {
    const flows = this.view(target).flows;
    const keep = Object.entries(flows).filter(([, e]) => !e.endedAt || Date.now() - e.endedAt < OUTCOME_MS);
    if (keep.length !== Object.keys(flows).length) this.patch(target, { flows: Object.fromEntries(keep) });
  }

  /** Forgets how a harness's last flow ended (a new step begins, or its card closes). */
  forget(target: string, kind: string) {
    const e = this.entry(target, kind);
    if (!e || e.flow.state === "running") return;
    this.drop(target, kind);
  }

  private drop(target: string, kind: string) {
    const rest = { ...this.view(target).flows };
    delete rest[kind];
    this.patch(target, { flows: rest });
  }

  /** Puts a flow's state in, replacing `cur` (the entry it updates). Unchanged polls change nothing. */
  private set(cur: FlowEntry | undefined, f: AccountFlow, extra?: Partial<FlowEntry>) {
    const v = this.view(f.target);
    const same = cur?.flow.id === f.id;
    if (same && !extra && JSON.stringify(cur.flow) === JSON.stringify(f)) return;
    const ended = f.state !== "running";
    const next: FlowEntry = { ...(same ? cur : {}), ...extra, flow: f, endedAt: ended ? ((same ? cur?.endedAt : undefined) ?? Date.now()) : undefined };
    if (next.sentAt !== undefined && !f.asks) next.cleared = true;
    let accounts = v.accounts;
    const a = ended ? f.account : undefined;
    if (a && accounts) accounts = accounts.some((x) => x.kind === a.kind) ? accounts.map((x) => (x.kind === a.kind ? a : x)) : [...accounts, a];
    this.patch(f.target, { flows: { ...v.flows, [f.kind]: next }, accounts });
    if (ended && f.state !== "cancelled" && (!a || !v.accounts)) this.load(f.target);
  }

  /** A flow's latest state, if it is still the one shown for its harness. */
  private update(f: AccountFlow) {
    const cur = this.entry(f.target, f.kind);
    if (cur && cur.flow.id === f.id) this.set(cur, f);
  }

  private async run(target: string, kind: string, action: AccountFlow["action"], method: string | undefined, start: () => Promise<AccountFlow>) {
    this.forget(target, kind);
    // On screen at once, before the machine answers.
    const pending: AccountFlow = { id: "", target, kind, action, method, state: "running", lines: [], started_at: nowSec() };
    this.set(undefined, pending);
    try {
      const f = await start();
      if (this.entry(target, kind)?.flow !== pending) {
        // Cancelled while it started: nobody wants it.
        if (f.state === "running") void accountsApi.cancel(f.id).catch(() => undefined);
        return;
      }
      this.set(this.entry(target, kind), f);
      this.poll(f);
    } catch (e) {
      const cur = this.entry(target, kind);
      if (cur?.flow === pending) this.set(cur, { ...pending, state: "failed", error: errorMessage(e), ended_at: nowSec() });
    }
  }

  login(target: string, kind: string, method: string) {
    this.ways.set(`${target}\n${kind}`, method);
    return this.run(target, kind, "login", method, () => accountsApi.login(target, kind, method));
  }

  logout(target: string, kind: string) {
    return this.run(target, kind, "logout", undefined, () => accountsApi.logout(target, kind));
  }

  /** Gives a running flow what it asks for. The text goes into the request body and nowhere else. */
  async send(target: string, kind: string, text: string): Promise<boolean> {
    const e = this.entry(target, kind);
    if (!e?.flow.id || e.flow.state !== "running") return false;
    const id = e.flow.id;
    try {
      await accountsApi.input(id, text);
      const cur = this.entry(target, kind);
      if (cur?.flow.id === id) this.set(cur, cur.flow, { sentAt: cur.flow.lines.length, cleared: false, sendError: undefined });
      return true;
    } catch (err) {
      const cur = this.entry(target, kind);
      if (cur?.flow.id === id) this.set(cur, cur.flow, { sendError: errorMessage(err) });
      return false;
    }
  }

  /** Stops a sign-in: gone from the card at once, then ended on the machine. */
  async cancel(target: string, kind: string) {
    const e = this.entry(target, kind);
    if (!e) return;
    this.drop(target, kind);
    if (e.flow.id && e.flow.state === "running") await accountsApi.cancel(e.flow.id).catch(() => undefined);
  }

  private poll(f: AccountFlow) {
    if (f.state !== "running" || !f.id || this.polling.has(f.id)) return;
    this.polling.add(f.id);
    let misses = 0;
    const tick = async () => {
      const cur = this.entry(f.target, f.kind);
      // It stops once the flow is gone or ended, or no card is looking (one that shows it again resumes).
      if (cur?.flow.id !== f.id || cur.flow.state !== "running" || !this.listeners.get(f.target)?.size) {
        this.polling.delete(f.id);
        return;
      }
      try {
        const next = await accountsApi.flow(f.id);
        misses = 0;
        this.update(next);
        if (next.state !== "running") {
          this.polling.delete(f.id);
          return;
        }
      } catch (e) {
        misses += 1;
        if (misses >= MISSES) {
          const c = this.entry(f.target, f.kind);
          if (c?.flow.id === f.id) this.set(c, { ...c.flow, state: "failed", error: `kiln lost track of it: ${errorMessage(e)}`, ended_at: nowSec() });
          this.polling.delete(f.id);
          return;
        }
      }
      window.setTimeout(() => void tick(), POLL_MS);
    };
    window.setTimeout(() => void tick(), POLL_MS);
  }
}

const store = new AccountStore();

function useMachine(target: string): MachineView {
  const subscribe = React.useCallback((fn: () => void) => store.subscribe(target, fn), [target]);
  const get = React.useCallback(() => store.view(target), [target]);
  return React.useSyncExternalStore(subscribe, get, get);
}

/* ------------------------------------------------------------------ words */

const NAMES: Record<string, string> = { claude: "Claude Code", codex: "Codex" };
const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const nameOf = (kind: string) => NAMES[kind] ?? capital(kind);
/** For sentences: "the server", or the machine's name. */
const whereIn = (target: string) => (target === "local" ? "the server" : machineName(target));
const planName = (plan: string) => capital(plan.replace(/[_-]+/g, " "));

/** A sign-in page kiln will link to: http(s) only, whatever the CLI printed. */
const pageUrl = (url: string | undefined) => (url && /^https?:\/\//i.test(url) ? url : undefined);

/** The page's address without its query: what someone could type on another device. */
function pageOf(url: string): string {
  try {
    const u = new URL(url);
    return `${u.host}${u.pathname === "/" ? "" : u.pathname}`;
  } catch {
    return url;
  }
}

/** The way it's signed in now when that's offered, else the first that opens a page, else the first. */
function defaultMethod(a: Account): string | undefined {
  const now = a.method?.toLowerCase();
  const same = now ? a.methods.find((m) => m.label.toLowerCase() === now || m.id.toLowerCase() === now) : undefined;
  return (same ?? a.methods.find((m) => m.asks !== "api_key") ?? a.methods[0])?.id;
}

/** Who it's signed in as (or how), and on what plan. */
function factsOf(a: Account): string[] {
  if (a.signed_in === null) return a.error ? [a.error] : [];
  if (!a.signed_in) return [];
  const who = a.email ?? a.org ?? a.method;
  const plan = a.plan ? planName(a.plan) : a.email && a.method ? a.method : undefined;
  return [who, plan].filter((x): x is string => Boolean(x));
}

/**
 * The machine's other harnesses in its own order, then the agent's: last, nearest the composer, so
 * the others arriving above it never move it (or the step open under it).
 */
function rowsOf(accounts: Account[] | null, kind: string, compact: boolean): Account[] {
  const mine = accounts?.find((a) => a.kind === kind) ?? {
    kind,
    name: nameOf(kind),
    signed_in: null,
    methods: [],
    error: accounts ? "kiln can't sign it in on this machine" : undefined,
  };
  return compact ? [mine] : [...(accounts ?? []).filter((a) => a.kind !== kind), mine];
}

type Phase = "starting" | "waiting" | "checking" | "done" | "failed";

/** It stopped asking once the code or key went in, and asks again: what was given was turned down. */
const askedAgain = (e: FlowEntry) => e.sentAt !== undefined && Boolean(e.cleared) && Boolean(e.flow.asks) && e.flow.state === "running";

function phaseOf(e: FlowEntry): Phase {
  const f = e.flow;
  if (f.state === "done") return "done";
  if (f.state !== "running") return "failed";
  if (!f.id) return "starting";
  // After a code or key went in, it's being checked: until it asks again, or says more while still asking.
  if (e.sentAt !== undefined && !askedAgain(e) && (f.lines.length <= e.sentAt || !f.asks)) return "checking";
  return f.url || f.code || f.asks ? "waiting" : "starting";
}

const fine = () => typeof window !== "undefined" && window.matchMedia?.("(pointer: fine)").matches;

/** What a sign-in under way is doing, briefly (for a phone's line) and whole. */
function liveState(phase: Phase, asks: AccountFlow["asks"], where: string): [string, string] {
  if (phase === "starting") return ["starting…", `starting the sign-in on ${where}…`];
  if (phase === "checking") return ["checking…", "finishing the sign-in…"];
  if (asks === "api_key") return ["waiting for the key…", "waiting for the API key…"];
  if (asks === "code") return ["waiting for you…", "waiting for the code from that page…"];
  return ["waiting for you…", "waiting for you to finish signing in…"];
}

/* ------------------------------------------------------------------ small parts */

function CopyButton({ text, label, className }: { text?: string; label: string; className?: string }) {
  const [done, setDone] = React.useState(false);
  /** The timer that takes "Copied" away (another copy meanwhile doesn't keep it longer). */
  const undo = React.useRef(0);
  const copied = (ok: boolean) => {
    setDone(ok);
    if (!ok) {
      window.clearTimeout(undo.current);
      undo.current = 0;
    } else if (!undo.current) {
      undo.current = window.setTimeout(() => {
        undo.current = 0;
        setDone(false);
      }, 1500);
    }
  };
  return (
    <button
      type="button"
      className={cn("hx-icon ac-copy", className)}
      aria-label={done ? "Copied" : label}
      title={done ? "Copied" : label}
      disabled={!text}
      data-away={text ? undefined : ""}
      onClick={() => text && void copyText(text).then(copied)}
    >
      {done ? <Check size={13} aria-hidden="true" /> : <Copy size={13} aria-hidden="true" />}
    </button>
  );
}

/** The CLI's own output, folded until asked for. */
function useDetails(lines: string[]) {
  const [open, setOpen] = React.useState(false);
  const id = React.useId();
  const pre = React.useRef<HTMLPreElement | null>(null);
  const count = lines.length;
  // The newest output stays in view: when it opens, and again as more arrives.
  const follow = React.useCallback(
    (el: HTMLPreElement | null) => {
      pre.current = el;
      void count;
      if (el) el.scrollTop = el.scrollHeight;
    },
    [count],
  );
  const toggle = (
    <button
      type="button"
      className="ac-more"
      aria-expanded={open}
      aria-controls={open ? id : undefined}
      onClick={() => {
        if (open) return setOpen(false);
        // Opened, it's brought into view (over the composer the card's lines scroll within it).
        flushSync(() => setOpen(true));
        pre.current?.scrollIntoView({ block: "nearest" });
      }}
    >
      Details <ChevronRight size={11} aria-hidden="true" />
    </button>
  );
  const body = open ? (
    <pre ref={follow} id={id} className="ac-lines" tabIndex={0} aria-label="What the sign-in printed">
      {lines.length ? lines.join("\n") : "Nothing yet."}
    </pre>
  ) : null;
  return { toggle, body };
}

/* ------------------------------------------------------------------ panels */

/** What last had focus inside the card (it may since have gone). */
const LastFocus = React.createContext<React.RefObject<Element | null> | null>(null);

/**
 * A step that took the place of what had focus in the card (the button that opened it, a field
 * that's done) takes the focus itself, so a keyboard or screen reader carries on from here and not
 * from the top of the page. Focus that was never in the card is left where it is. The step's ref:
 * it looks when the step shows, and again whenever `when` changes.
 */
function useTakeFocus(when: unknown = null) {
  const last = React.useContext(LastFocus);
  return React.useCallback(
    (el: HTMLElement | null) => {
      void when;
      const was = last?.current;
      if (!el || !was) return;
      const gone = !was.isConnected || (was instanceof HTMLElement && was.checkVisibility?.({ visibilityProperty: true }) === false);
      const active = document.activeElement;
      if (gone && (!active || active === document.body || active === was)) el.focus({ preventScroll: true });
    },
    [when, last],
  );
}

/** How to sign in, as words (the usual way already picked), then Sign in. */
function ChoosePanel({ a, target, loading, where, onStart, onCancel }: { a: Account; target: string; loading: boolean; where: string; onStart: (method: string) => void; onCancel: () => void }) {
  const methods = a.methods;
  const offered = (id: string | null | undefined) => (id && methods.some((m) => m.id === id) ? id : undefined);
  // The way tried last from here comes first (after a failed try, it's the one to try again).
  const [picked, setPicked] = React.useState<string | null>(() => store.lastWay(target, a.kind) ?? null);
  const chosen = offered(picked) ?? defaultMethod(a);
  const takeFocus = useTakeFocus();
  // Arrow keys move between the ways, as in any radio group.
  const rove = (e: React.KeyboardEvent<HTMLDivElement>) => {
    const radios = Array.from(e.currentTarget.querySelectorAll<HTMLButtonElement>('[role="radio"]'));
    const i = radios.findIndex((r) => r === document.activeElement);
    if (i < 0) return;
    const step = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const next = radios[(i + step + radios.length) % radios.length];
    next?.focus();
    next?.click();
  };
  return (
    <div ref={takeFocus} tabIndex={-1} className="ac-panel" role="group" aria-label={`Sign in to ${a.name}`}>
      {/* WITH, then the ways: words that wrap under the first of them, never sideways. */}
      <div className="ac-with">
        <span className="ac-key" aria-hidden="true">
          With
        </span>
        <div className="ac-words" role="radiogroup" aria-label={`Sign in to ${a.name} with`} onKeyDown={rove}>
          {methods.length === 0 ? (
            <span className="ac-word ac-none">{loading ? "…" : `nothing kiln can run on ${where}`}</span>
          ) : (
            methods.map((m, i) => (
              <span key={m.id} className="ac-cell">
                <button type="button" role="radio" aria-checked={m.id === chosen} tabIndex={m.id === chosen ? 0 : -1} className="ac-word" onClick={() => setPicked(m.id)}>
                  {m.label}
                </button>
                {i < methods.length - 1 && (
                  <span className="ac-sep" aria-hidden="true">
                    ·
                  </span>
                )}
              </span>
            ))
          )}
        </div>
      </div>
      <div className="ac-actions">
        <Button size="xs" className="ac-btn" disabled={!chosen} onClick={() => chosen && onStart(chosen)}>
          Sign in
        </Button>
        <Button size="xs" variant="ghost" className="ac-btn" onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </div>
  );
}

/** Signing out takes a second word: agents there stop working. */
function ConfirmPanel({ a, where, busy, onConfirm, onKeep }: { a: Account; where: string; busy: boolean; onConfirm: () => void; onKeep: () => void }) {
  const takeFocus = useTakeFocus();
  return (
    <div ref={takeFocus} tabIndex={-1} className="ac-panel" role="group" aria-label={`Sign out of ${a.name}`}>
      <p className="ac-ask">
        Sign out of {a.name} on {where}? Agents there that use it stop working until you sign in again.
      </p>
      <div className="ac-actions">
        <Button size="xs" variant="danger" className="ac-btn" disabled={busy} onClick={onConfirm}>
          {busy ? "Signing out…" : "Sign out"}
        </Button>
        <Button size="xs" variant="ghost" className="ac-btn" disabled={busy} onClick={onKeep}>
          Keep
        </Button>
      </div>
    </div>
  );
}

/** A sign-in under way (or as it ended): the page, the code or the field, and what it waits for. */
function FlowPanel({
  a,
  entry,
  target,
  where,
  compact,
  onCancel,
  onRetry,
}: {
  a: Account;
  entry: FlowEntry;
  target: string;
  where: string;
  compact: boolean;
  onCancel: () => void;
  /** After a failed sign-in: back to the ways to sign in, the one that failed picked. */
  onRetry: () => void;
}) {
  const f = entry.flow;
  const phase = phaseOf(entry);
  const method: AccountMethod | undefined = a.methods.find((m) => m.id === f.method);
  // What it will ask for, known from the way it signs in before the machine says so.
  const asks = f.asks ?? method?.asks;
  const [code, setCode] = React.useState("");
  const [key, setKey] = React.useState("");
  const [sending, setSending] = React.useState(false);
  const field = React.useRef<HTMLInputElement | null>(null);
  const fieldId = React.useId();
  const details = useDetails(f.lines);
  const wantsKey = f.action === "login" && asks === "api_key" && phase !== "done" && phase !== "failed";
  const takeFocus = useTakeFocus(phase === "done" || phase === "failed" ? phase : "under way");

  // Asked for a key right after choosing to give one: the field is ready for it as it shows (keyboards
  // and mice only; a phone's keyboard shouldn't jump up unasked).
  const keyField = React.useCallback(
    (el: HTMLInputElement | null) => {
      field.current = el;
      if (el && wantsKey && fine()) el.focus({ preventScroll: true });
    },
    [wantsKey],
  );

  if (phase === "done") {
    const acct = f.account ?? a;
    const plan = acct.plan ? ` · ${planName(acct.plan)}` : "";
    return (
      <div ref={takeFocus} tabIndex={-1} className="ac-panel" role="status">
        <p className="ac-done">
          <Check size={13} aria-hidden="true" />
          <span className="ac-done-text">
            {acct.email ? (
              <>
                Signed in as <b>{acct.email}</b>
                {plan}
              </>
            ) : acct.method ? (
              <>
                Signed in with <b>{acct.method}</b>
                {plan}
              </>
            ) : (
              <>Signed in{plan}</>
            )}
          </span>
        </p>
        {compact && <p className="ac-quiet">Send your message again to retry it.</p>}
      </div>
    );
  }

  if (phase === "failed") {
    const what = f.action === "logout" ? "Couldn't sign out" : "The sign-in didn't finish";
    const why = f.error ?? (f.state === "cancelled" ? "it was cancelled on the machine" : undefined);
    return (
      <div ref={takeFocus} tabIndex={-1} className="ac-panel" data-tone="fail">
        <p className="ac-fail" role="alert">
          <TriangleAlert size={13} aria-hidden="true" />
          <span>{why ? `${what}: ${why}` : `${what}.`}</span>
        </p>
        <div className="ac-foot">
          <Button size="xs" variant="secondary" className="ac-btn" onClick={() => (f.action === "logout" ? void store.logout(target, a.kind) : onRetry())}>
            Try again
          </Button>
          <span className="ac-gap" />
          {f.lines.length > 0 && details.toggle}
        </div>
        {details.body}
      </div>
    );
  }

  // Waiting for it: the rows it will need are laid out from the start.
  const page = pageUrl(f.url);
  const showLink = asks !== "api_key" || Boolean(f.url);
  // A device sign-in: a code to enter on the page (until a page comes without one).
  const showCode = !asks && (Boolean(f.code) || !f.url);
  const showPaste = asks === "code";
  const showKey = asks === "api_key";
  const numbered = showLink && (showCode || showPaste);
  // The CLI is waiting for the code or key right now.
  const ready = phase === "waiting" && f.asks === asks && !sending;
  const hint = entry.sendError
    ? `Couldn't pass it on: ${entry.sendError}`
    : askedAgain(entry)
      ? showKey
        ? "It asked again: that key didn't work."
        : "It asked again: that code didn't work."
      : undefined;

  const submitCode = async (e: React.FormEvent) => {
    e.preventDefault();
    const text = code.trim();
    if (!text || !ready) return;
    setSending(true);
    const ok = await store.send(target, a.kind, text);
    setSending(false);
    if (ok) setCode("");
  };
  const submitKey = async () => {
    const text = key.trim();
    if (!text || !ready) return;
    // Gone from the field before it's even sent: a key is never kept, here or anywhere on the page.
    setKey("");
    setSending(true);
    await store.send(target, a.kind, text);
    setSending(false);
  };

  return (
    <div ref={takeFocus} tabIndex={-1} className="ac-panel" data-live="">
      <span className="ac-sr" role="status">
        {liveState(phase, asks, where)[1]}
      </span>
      <ol className="ac-steps">
        {showLink && (
          <li className="ac-step">
            {numbered && <span className="ac-n">1</span>}
            <div className="ac-step-main ac-linkrow">
              {page ? (
                <a
                  className={cn(buttonVariants({ variant: "primary", size: "xs" }), "ac-btn ac-open")}
                  href={page}
                  target="_blank"
                  rel="noopener noreferrer"
                  onClick={() => {
                    // Back from the page with its code, the field is ready for it.
                    if (showPaste && fine()) window.setTimeout(() => field.current?.focus({ preventScroll: true }), 0);
                  }}
                >
                  Open sign-in page <ArrowUpRight aria-hidden="true" />
                </a>
              ) : (
                <span className={cn(buttonVariants({ variant: "primary", size: "xs" }), "ac-btn ac-open")} aria-disabled="true">
                  Open sign-in page <ArrowUpRight aria-hidden="true" />
                </span>
              )}
              <span className="ac-url" title={page}>
                {page ? pageOf(page) : ""}
              </span>
              <CopyButton text={page} label="Copy the link" />
            </div>
          </li>
        )}
        {showCode && (
          <li className="ac-step">
            {numbered && <span className="ac-n">2</span>}
            <div className="ac-step-main">
              <p className="ac-step-text">Enter this code on that page</p>
              <div className="ac-coderow">
                <code className="ac-code" data-empty={f.code ? undefined : ""} aria-label={f.code ? `Code: ${f.code.split("").join(" ")}` : "The code is on its way"}>
                  {f.code ?? "····-·····"}
                </code>
                <CopyButton text={f.code} label="Copy the code" />
              </div>
            </div>
          </li>
        )}
        {showPaste && (
          <li className="ac-step">
            {numbered && <span className="ac-n">2</span>}
            <form className="ac-step-main" onSubmit={(e) => void submitCode(e)}>
              <label className="ac-step-text" htmlFor={fieldId}>
                Paste the code the page shows
              </label>
              <div className="ac-fieldrow">
                <input
                  ref={field}
                  id={fieldId}
                  className="ac-input ac-mono"
                  value={code}
                  onChange={(e) => setCode(e.target.value)}
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck={false}
                  enterKeyHint="go"
                  disabled={phase === "checking"}
                />
                <Button type="submit" size="xs" variant={code.trim() && ready ? "primary" : "secondary"} className="ac-btn" disabled={!code.trim() || !ready}>
                  Submit
                </Button>
              </div>
              {hint && <p className="ac-hint">{hint}</p>}
            </form>
          </li>
        )}
        {showKey && (
          <li className="ac-step">
            {/* Not a form: browsers offer to save what a form submits. */}
            <div className="ac-step-main">
              <label className="ac-step-text" htmlFor={fieldId}>
                API key
              </label>
              <div className="ac-fieldrow">
                <input
                  ref={keyField}
                  id={fieldId}
                  type="password"
                  className="ac-input ac-mono"
                  value={key}
                  onChange={(e) => setKey(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void submitKey();
                    }
                  }}
                  autoComplete="off"
                  autoCorrect="off"
                  autoCapitalize="off"
                  spellCheck={false}
                  enterKeyHint="go"
                  data-1p-ignore=""
                  data-lpignore="true"
                  data-bwignore=""
                  disabled={phase === "checking"}
                />
                <Button size="xs" variant={key.trim() && ready ? "primary" : "secondary"} className="ac-btn" disabled={!key.trim() || !ready} onClick={() => void submitKey()}>
                  Sign in
                </Button>
              </div>
              {hint && <p className="ac-hint">{hint}</p>}
            </div>
          </li>
        )}
      </ol>
      <div className="ac-foot">
        <Button size="xs" variant="ghost" className="ac-btn ac-cancel" onClick={onCancel}>
          Cancel
        </Button>
        <span className="ac-gap" />
        {details.toggle}
      </div>
      {details.body}
    </div>
  );
}

/* ------------------------------------------------------------------ one harness */

type Open = "signin" | "signout";

function AccountRow({
  a,
  entry,
  open,
  loading,
  listError,
  lit,
  compact,
  target,
  onOpen,
}: {
  a: Account;
  entry: FlowEntry | undefined;
  open: Open | null;
  loading: boolean;
  listError: string | null;
  lit: boolean;
  compact: boolean;
  target: string;
  onOpen: (what: Open | null) => void;
}) {
  const where = whereIn(target);
  const flow = entry?.flow;
  const running = flow?.state === "running";
  const phase = entry ? phaseOf(entry) : null;
  // A flow shows itself; a sign-out that went through leaves just the new status.
  const panel: Open | "flow" | "note" | null = flow
    ? flow.action === "logout"
      ? running
        ? "signout"
        : flow.state === "done"
          ? null
          : "flow"
      : "flow"
    : open === "signout" && a.signed_in === false
      ? "note"
      : open;

  const tone = running ? (phase === "waiting" ? "wait" : "busy") : loading ? "loading" : listError ? "unknown" : a.signed_in === true ? "in" : a.signed_in === false ? "out" : "unknown";
  // While it runs, the line says what it's waiting for: briefly on a phone, whole where there's room.
  const [short, long] =
    running && flow
      ? flow.action === "logout"
        ? ["signing out…", "signing out…"]
        : liveState(phase ?? "starting", flow.asks ?? a.methods.find((m) => m.id === flow.method)?.asks, where)
      : [loading ? "checking…" : listError ? "couldn't check" : a.signed_in === true ? "signed in" : a.signed_in === false ? "not signed in" : "couldn't tell"];
  // While it runs the line says what it waits for, and once through, who it is says it in full
  // below: either way the line doesn't repeat who.
  const facts = listError ? [listError] : loading || running || (panel === "flow" && phase === "done") ? [] : factsOf(a);
  // The line's own action steps aside (keeping its room) while a step is open under it.
  const away = loading || panel === "signin" || panel === "signout" || (panel === "flow" && (running || phase === "failed"));
  const act = listError ? "Retry" : a.signed_in === true ? "Sign out" : "Sign in";

  let body: React.ReactNode = null;
  if (panel === "flow" && entry)
    body = (
      <FlowPanel
        a={a}
        entry={entry}
        target={target}
        where={where}
        compact={compact}
        onCancel={() => void store.cancel(target, a.kind)}
        onRetry={() => {
          store.forget(target, a.kind);
          onOpen("signin");
        }}
      />
    );
  else if (panel === "signin")
    body = (
      <ChoosePanel
        a={a}
        target={target}
        loading={loading}
        where={where}
        onStart={(m) => {
          onOpen(null);
          void store.login(target, a.kind, m);
        }}
        onCancel={() => onOpen(null)}
      />
    );
  else if (panel === "signout")
    body = (
      <ConfirmPanel
        a={a}
        where={where}
        busy={Boolean(running)}
        onConfirm={() => {
          onOpen(null);
          void store.logout(target, a.kind);
        }}
        onKeep={() => onOpen(null)}
      />
    );
  else if (panel === "note")
    body = (
      <div className="ac-panel">
        <p className="ac-quiet">
          {a.name} isn't signed in on {where}.
        </p>
      </div>
    );

  return (
    <li className="ac-acct" data-lit={lit ? "" : undefined} data-open={body ? "" : undefined}>
      <div className="ac-line">
        <span className="ac-glyph" aria-hidden="true">
          {running ? <Optic state={phase === "waiting" ? "waiting" : "thinking"} /> : <i className="ac-dot" data-state={tone} />}
        </span>
        <span className="ac-name">{a.name}</span>
        <span className="ac-status" data-state={tone}>
          {long && long !== short ? (
            <>
              <span className="ac-short">{short}</span>
              <span className="ac-long">{long}</span>
            </>
          ) : (
            short
          )}
        </span>
        {facts.length > 0 && (
          <span className="ac-facts" title={facts.join(" · ")}>
            {facts.join(" · ")}
          </span>
        )}
        <span className="ac-slot" data-away={away ? "" : undefined}>
          <Button
            size="xs"
            variant={act === "Sign in" ? "secondary" : "ghost"}
            className="ac-btn ac-act"
            data-act={act === "Sign out" ? "out" : undefined}
            tabIndex={away ? -1 : undefined}
            aria-hidden={away || undefined}
            aria-label={`${act === "Retry" ? "Check again" : act}: ${a.name}`}
            onClick={() => {
              if (listError) return store.load(target);
              store.forget(target, a.kind);
              onOpen(a.signed_in === true ? "signout" : "signin");
            }}
          >
            {act}
          </Button>
        </span>
      </div>
      {body}
    </li>
  );
}

/* ------------------------------------------------------------------ the card */

export interface AccountCardProps {
  /** Where the harness runs: "local" or "agent:<machine>". */
  target: string;
  /** The harness it's about (the agent's): its line is lit, and last. */
  kind: string;
  /** Opens its sign-in, or its sign-out to confirm. */
  intent?: AccountIntent;
  /** Just that harness, no heading: for under a message that says it isn't signed in. */
  compact?: boolean;
  /** Shows a close button; closing stops a sign-in still under way. */
  onClose?: () => void;
  className?: string;
  /** Closing it from outside (its dialog dismissed), and whether a step of it is open. */
  ref?: React.Ref<AccountCardHandle>;
}

export interface AccountCardHandle {
  close: () => void;
  stepOpen: () => boolean;
}

/** The machine's accounts in a dialog: opened from the conversation (/login, /logout), where laid
 *  out in it they crowded a phone's screen. Dismissing it closes the card as its own close does
 *  (a sign-in still under way stops); Escape closes an open step first. */
export function AccountDialog({ target, kind, intent, onClose }: { target: string; kind: string; intent?: AccountIntent; onClose: () => void }) {
  const card = React.useRef<AccountCardHandle | null>(null);
  return (
    <D.Root
      open
      onOpenChange={(open) => {
        if (!open) (card.current?.close ?? onClose)();
      }}
    >
      <D.Portal>
        <D.Overlay className="overlay" />
        <D.Content
          className="dialog ac-dialog"
          aria-describedby={undefined}
          onEscapeKeyDown={(e) => {
            if (card.current?.stepOpen()) e.preventDefault();
          }}
        >
          <D.Title className="sr-only">Accounts on {machineName(target)}</D.Title>
          <AccountCard ref={card} target={target} kind={kind} intent={intent} onClose={onClose} />
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}

export function AccountCard({ target, kind, intent, compact = false, onClose, className, ref }: AccountCardProps) {
  const view = useMachine(target);
  const shown = React.useCallback(
    (el: HTMLElement | null) => {
      if (!el) return;
      // As it shows, before it's painted: old outcomes go, and opened to sign in or out (/login,
      // /logout) it shows that, not how the last one ended.
      store.prune(target);
      if (intent && !compact) store.forget(target, kind);
      // Under a message, the conversation only follows its own new messages: when the card grows (its
      // ways arrive, a step opens) while you're looking at it, the rest of it is brought into view.
      if (!compact) return;
      // Watched from once this commit is through: the conversation keeps its place with an observer
      // made as it shows (maybe in this same commit), and observers are told in the order they were
      // made, so this one's "into view" comes after that.
      let watch: ResizeObserver | null = null;
      let gone = false;
      queueMicrotask(() => {
        if (gone) return;
        let last = el.offsetHeight;
        watch = new ResizeObserver(() => {
          const grew = el.offsetHeight > last;
          last = el.offsetHeight;
          const top = el.getBoundingClientRect().top;
          if (grew && top >= 0 && top < window.innerHeight) el.scrollIntoView({ block: "nearest" });
        });
        watch.observe(el);
      });
      return () => {
        gone = true;
        watch?.disconnect();
      };
    },
    [target, kind, intent, compact],
  );
  /** What you opened or closed here; until then, what the intent asks for. */
  const [chosen, setOpen] = React.useState<{ kind: string; what: Open } | null | undefined>(undefined);
  const lastFocus = React.useRef<Element | null>(null);
  const [said, setSaid] = React.useState("");
  const loading = view.accounts === null && view.error === null;
  const listError = view.accounts === null ? view.error : null;
  const rows = React.useMemo(() => rowsOf(view.accounts, kind, compact), [view.accounts, kind, compact]);
  const where = whereIn(target);
  // Said aloud: a sign-out leaves no panel to read. Noticed as the flows change: a harness shown
  // here whose sign-out was under way and is now through.
  const [flowsWere, setFlowsWere] = React.useState(view.flows);
  if (flowsWere !== view.flows) {
    setFlowsWere(view.flows);
    const through = (r: Account) => {
      const f = view.flows[r.kind]?.flow;
      return f?.action === "logout" && f.state === "done" && flowsWere[r.kind]?.flow.state === "running";
    };
    const out = compact && listError ? undefined : rows.find(through);
    if (out) setSaid(`Signed out of ${out.name} on ${where}.`);
  }
  // Under a message, a harness that turns out to be signed in (now) needs no sign-in offered.
  const signedIn = rows.find((r) => r.kind === kind)?.signed_in === true;
  const asked = intent === "logout" ? { kind, what: "signout" as const } : intent === "login" && !(compact && signedIn) ? { kind, what: "signin" as const } : null;
  const open = chosen === undefined ? asked : chosen;

  const close = () => {
    for (const r of rows) {
      const e = view.flows[r.kind];
      if (e?.flow.state === "running" && e.flow.action === "login") void store.cancel(target, r.kind);
      else store.forget(target, r.kind);
    }
    onClose?.();
  };
  React.useImperativeHandle(ref, () => ({ close, stepOpen: () => open !== null }));

  // Under a message, a card that can't read the machine's accounts says nothing (the message stands);
  // opened with /login it says why, with a way to try again.
  if (compact && listError) return null;

  return (
    <section
      ref={shown}
      className={cn("ac", compact && "ac-compact", className)}
      aria-label={compact ? `${nameOf(kind)} account` : `Accounts on ${where}`}
      onFocus={(e) => {
        lastFocus.current = e.target;
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape" && open) {
          e.stopPropagation();
          setOpen(null);
        }
      }}
    >
      {!compact && (
        <div className="ac-head">
          <span className="ac-eyebrow">Accounts on {where}</span>
          {onClose && (
            <button type="button" className="hx-icon ac-close" aria-label="Close" onClick={close}>
              <X size={13} aria-hidden="true" />
            </button>
          )}
        </div>
      )}
      <LastFocus.Provider value={lastFocus}>
        <ul className="ac-list">
          {rows.map((a) => (
            <AccountRow
              key={a.kind}
              a={a}
              entry={view.flows[a.kind]}
              open={open?.kind === a.kind ? open.what : null}
              loading={loading}
              listError={listError}
              lit={!compact && rows.length > 1 && a.kind === kind}
              compact={compact}
              target={target}
              onOpen={(what) => setOpen(what ? { kind: a.kind, what } : null)}
            />
          ))}
        </ul>
      </LastFocus.Provider>
      <span className="ac-sr" role="status">
        {said}
      </span>
    </section>
  );
}
