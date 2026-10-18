/**
 * Mission control: one list of your agents, newest first, each keeping its place: a row says in one
 * line what that agent is doing — or what it wants, or what it finished — and you answer it right
 * there. Nothing moves as agents update; only their words and marks change.
 */
import * as React from "react";
import "./harness.css";
import "./mission.css";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { ArrowUpRight, ChevronRight, Plus, RotateCcw } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type ApprovalBrief } from "@/api/harness";
import type { AiSession } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { PageIntro } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { agentLabel, hasOwnName, plain } from "./derive";
import { NameField, useAgentActions } from "./actions";
import { MessageBar } from "./broadcast";
import { DEFAULT_VIEW, isFiltered, ListBar, useShownList, type ListView } from "./list-tools";
import { byStart, FoundSessions, sessionKey } from "./found";
import { Ago, machineName } from "./parts";
import { useSeen } from "./seen";
import { SpawnDialog } from "./spawn";
import { useAgentList, useAgents } from "./use-agents";

/** Where an agent stands. */
type Stand = "needs" | "crashed" | "working" | "finished" | "ready" | "offline" | "ended";

function standOf(a: AgentInfo, seen: number | undefined): Stand {
  if (a.approvals.length > 0 || a.status === "waiting") return "needs";
  if (a.status === "exited" && a.exit?.error) return "crashed";
  if (a.status === "running" || a.status === "starting") return "working";
  if (a.status === "idle") return a.last_activity > (seen ?? 0) + 1500 ? "finished" : "ready";
  // Its machine is reconnecting (a runner or server restart): it stays where it is.
  if (a.status === "disconnected") return "offline";
  return "ended";
}

const WORD: Record<Stand, string> = {
  needs: "Needs you",
  crashed: "Stopped",
  working: "Working",
  finished: "Finished",
  ready: "Ready",
  offline: "Offline",
  ended: "Ended",
};

/** The one line that matters for this agent right now. */
function lineOf(a: AgentInfo, stand: Stand): string {
  switch (stand) {
    case "needs": {
      const ap = a.approvals[0];
      if (!ap) return "Waiting for you";
      const more = a.approvals.length > 1 ? ` (+${a.approvals.length - 1} more)` : "";
      return ap.tool === "question" ? `Asks: ${ap.title}${more}` : ap.tool === "plan" ? `Proposes a plan${more}` : `${ap.tool_name || ap.tool}: ${ap.title}${more}`;
    }
    case "crashed":
      return a.exit?.error ?? "Stopped unexpectedly";
    case "working": {
      const plan = (a.todos_total ?? 0) > 0 ? `plan ${a.todos_done ?? 0}/${a.todos_total}` : "";
      return [a.status === "starting" ? "Starting" : a.doing || a.activity || "Working", plan].filter(Boolean).join(" · ");
    }
    case "finished":
      return a.summary ? plain(a.summary) : a.goal || a.title || "Done";
    case "ready":
      return a.goal || a.title || "No task yet";
    case "offline":
      return "Its machine is offline";
    case "ended":
      return a.goal || a.title || "Ended";
  }
}

function Decide({ a, ap }: { a: AgentInfo; ap: ApprovalBrief }) {
  const [busy, setBusy] = React.useState(false);
  const decide = async (decision: "allow" | "deny") => {
    setBusy(true);
    try {
      await harnessApi.command(a.id, { type: "approve", id: ap.id, decision });
    } catch (e) {
      toast.error("Couldn't answer", errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  if (ap.tool === "question" || ap.tool === "plan")
    return (
      <Button size="xs" asChild>
        <Link to={`/agents/${a.id}`}>{ap.tool === "plan" ? "Review the plan" : "Answer"}</Link>
      </Button>
    );
  return (
    <>
      <Button size="xs" disabled={busy} onClick={() => decide("allow")}>
        Allow
      </Button>
      <Button size="xs" variant="secondary" disabled={busy} onClick={() => decide("deny")}>
        Deny
      </Button>
      <Link to={`/agents/${a.id}`} className="ms-link">
        Review
      </Link>
    </>
  );
}

/** A row re-renders only when its own agent changes (or is selected); its clock counts by itself. */
const Row = React.memo(function Row({ a, stand, selected, onSelect }: { a: AgentInfo; stand: Stand; selected: boolean; onSelect: (id: string, on: boolean) => void }) {
  const navigate = useNavigate();
  const open = () => navigate(`/agents/${a.id}`);
  const ap = a.approvals[0];
  const actions = useAgentActions(a);
  const label = agentLabel(a);
  // The harness, said under a label that isn't its own name.
  const kind = label.toLowerCase() !== a.kind_name.toLowerCase();
  const line = (
    <span className="ms-line">
      <span className="ms-word">
        <i aria-hidden="true" />
        {WORD[stand]}
      </span>
      <span className="ms-text">{lineOf(a, stand)}</span>
    </span>
  );
  return (
    <li className="ms-row" data-stand={stand} data-selected={selected || undefined}>
      <label className="ms-check">
        <input type="checkbox" className="check" checked={selected} onChange={(e) => onSelect(a.id, e.target.checked)} aria-label={`Select ${label}`} />
      </label>
      {actions.renaming ? (
        <div className="ms-main">
          <NameField value={hasOwnName(a) ? a.name : ""} placeholder={label} onSave={actions.rename} onDone={actions.stopRenaming} className="ms-name-field" label="Agent name" />
          {line}
        </div>
      ) : (
        <button type="button" className="ms-main" onClick={open}>
          <span className="ms-name">{label}</span>
          {line}
        </button>
      )}
      <span className="ms-meta">
        <span>
          {machineName(a.target)} · <Ago className="ms-time" ts={Math.floor(a.last_activity / 1000)} />
        </span>
        <span className="ms-meta-2">
          {kind && <span className="ms-kind">{a.kind_name}</span>}
          {(a.files_changed ?? 0) > 0 && (
            <span className="ms-changes">
              {a.files_changed} file{a.files_changed === 1 ? "" : "s"} <b>+{a.lines_added ?? 0}</b> <i>−{a.lines_removed ?? 0}</i>
            </span>
          )}
          {(a.left_behind ?? 0) > 0 && <span className="ms-left">{a.left_behind} left running</span>}
        </span>
      </span>
      <span className="ms-actions">
        {/* What to answer; on a phone it takes the meta line's place, so the row's corner keeps one width. */}
        <span className="ms-decide">
          {stand === "needs" && ap && <Decide a={a} ap={ap} />}
          {stand === "crashed" && (
            <Button size="xs" variant="secondary" onClick={() => void harnessApi.command(a.id, { type: "restart" }).catch((e: unknown) => toast.error("Couldn't restart", errorMessage(e)))}>
              Restart
            </Button>
          )}
        </span>
        <span className="ms-tail">
          {actions.menu()}
          {actions.dialogs}
          <button type="button" className="ms-go" onClick={open} aria-label={`Open ${label}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
});

/** Where each agent stands, and how many stand where (the page's lead says so). */
function standsOf(list: AgentInfo[], seen: Record<string, number>) {
  return list.map((a) => ({ a, stand: standOf(a, seen[a.id]) }));
}

/** What the page's lead says before the agents are known. */
const LEAD = "Every coding agent kiln runs, on every machine.";

/** The lead: how your agents stand, and where. */
function Lead() {
  const list = useAgentList();
  const seen = useSeen();
  const all = React.useMemo(() => standsOf(list, seen), [list, seen]);
  const n = (s: Stand) => all.filter((r) => r.stand === s).length;
  const machines = [...new Set(all.filter((r) => r.stand !== "ended").map((r) => machineName(r.a.target)))];
  const status = [
    n("needs") && `${n("needs")} need${n("needs") === 1 ? "s" : ""} you`,
    n("crashed") && `${n("crashed")} stopped`,
    n("working") && `${n("working")} working`,
    n("finished") && `${n("finished")} finished`,
  ]
    .filter(Boolean)
    .join(", ");
  return <>{all.length === 0 ? "Start coding agents on any of your machines and run them all from here." : `${status || "All quiet"}${machines.length ? ` — on ${machines.join(" and ")}` : ""}.`}</>;
}

export default function Mission() {
  const [spawn, setSpawn] = React.useState(false);
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  // ?new=1 (links from elsewhere) opens New agents: open, it stays so by itself, and the link's
  // mark comes off the address.
  const fromLink = params.get("new") === "1";
  const opened = () => {
    if (!fromLink) return;
    setSpawn(true);
    const n = new URLSearchParams(params);
    n.delete("new");
    setParams(n, { replace: true });
  };
  return (
    <div className="container-console page ms-page">
      <title>Agents · kiln</title>
      <PageIntro
        index={6}
        label="Agents"
        title="Mission control."
        lead={
          <React.Suspense fallback={LEAD}>
            <Lead />
          </React.Suspense>
        }
        actions={
          <>
            <Button asChild variant="secondary">
              <Link to="/agents/plans">Plans</Link>
            </Button>
            <Button onClick={() => setSpawn(true)}>
              New agents <Plus />
            </Button>
          </>
        }
      />
      <Loadable label="Loading agents" fallback={<Loading label="Loading agents" block />}>
        <Agents onSpawn={() => setSpawn(true)} />
      </Loadable>
      <SpawnDialog
        open={spawn || fromLink}
        onOpenChange={setSpawn}
        onOpened={opened}
        onStarted={(started) => {
          setSpawn(false);
          if (started.length === 1 && started[0]) navigate(`/agents/${started[0].id}`);
          else invalidate("harness/agents");
        }}
      />
    </div>
  );
}

/** Sessions found on your machines, as last read (each kept a read longer than it was missing, so rows don't blink). */
const kept = new Map<string, { s: AiSession; missed: number }>();
async function readFound(signal: AbortSignal): Promise<AiSession[]> {
  const list = await api.aiSessions(signal);
  const here = new Set(list.map(sessionKey));
  // An unchanged session keeps its object, so its row doesn't re-render.
  for (const s of list) {
    const k = sessionKey(s);
    const was = kept.get(k)?.s;
    kept.set(k, { s: was && JSON.stringify(was) === JSON.stringify(s) ? was : s, missed: 0 });
  }
  for (const [k, v] of kept) if (!here.has(k) && ++v.missed >= 2) kept.delete(k);
  return [...kept.values()].map((v) => v.s).sort(byStart);
}
/**
 * The found sessions and the machines that can't say what runs on them, read together. A read that
 * fails leaves what was found (a server without these says nothing here).
 */
const readAll = (signal: AbortSignal) =>
  Promise.all([
    readFound(signal).catch(() => [...kept.values()].map((v) => v.s).sort(byStart)),
    api.aiSilent(signal).then(
      (r) => r.machines,
      (): string[] => [],
    ),
  ]);
const FOUND = { on: ["ai_sessions"], pollMs: 20_000 } as const;

/**
 * Sessions found on your machines that kiln doesn't run itself (its own agents are rows above):
 * told when they change, and read again now and then for their state and last activity.
 */
function Found({ all }: { all: { a: AgentInfo }[] }) {
  const [found, quiet] = useSuspenseResource("ai-sessions", readAll, FOUND).data;
  const elsewhere = found.filter((s) => !s.managed_by && !all.some((r) => r.a.harness_session && r.a.harness_session === s.id));
  return <FoundSessions sessions={elsewhere} quiet={quiet} />;
}

function Agents({ onSpawn }: { onSpawn: () => void }) {
  const { list, reload } = useAgents();
  const seen = useSeen();
  const [showEnded, setShowEnded] = useLocalStorage<boolean>("kiln.agents.showEnded", false);
  const [selected, setSelected] = React.useState<ReadonlySet<string>>(new Set());

  const all = React.useMemo(() => standsOf(list, seen), [list, seen]);
  // How the list is shown (the sort and the facets chosen) is kept across visits; the words typed
  // to find a name are not. The list holds still while agents work: see useShownList.
  const [stored, setView] = useLocalStorage<Partial<ListView>>("kiln.agents.list", DEFAULT_VIEW);
  const view = React.useMemo(() => ({ ...DEFAULT_VIEW, ...stored }), [stored]);
  const [q, setQ] = React.useState("");
  const shown = useShownList(all, q, view, true);
  const { live, ended } = shown;
  const filtered = isFiltered(q, view.facets);

  const select = React.useCallback((id: string, on: boolean) => {
    setSelected((s) => {
      const next = new Set(s);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);
  // Only what the list shows can be checked: a row the filter takes away is unchecked with it.
  const shownIds = [...live, ...ended].map((r) => r.a.id).join(",");
  const [wasShown, setWasShown] = React.useState(shownIds);
  if (wasShown !== shownIds) {
    setWasShown(shownIds);
    const here = new Set(shownIds.split(","));
    if (![...selected].every((id) => here.has(id))) setSelected(new Set([...selected].filter((id) => here.has(id))));
  }
  const checked = [...live, ...ended].filter((r) => selected.has(r.a.id));
  const liveChecked = live.filter((r) => selected.has(r.a.id)).length;
  const some = liveChecked > 0 && liveChecked < live.length;
  const allBox = React.useCallback(
    (el: HTMLInputElement | null) => {
      if (el) el.indeterminate = some;
    },
    [some],
  );
  // The message goes to what's checked; with nothing checked, to every agent the list shows.
  const targets = checked.length > 0 ? checked.map((r) => r.a) : live.map((r) => r.a);
  const reach = checked.length > 0 ? "selected" : filtered ? "shown" : "all";

  return (
    <>
      {all.length > 0 && (
        <>
          <ListBar rows={all} q={q} onQuery={setQ} view={view} onView={setView} />
          <MessageBar agents={targets} reach={reach} onEnded={() => setSelected(new Set())} />
        </>
      )}

      {all.length === 0 ? (
        <div className="ms-empty">
          <p className="ms-empty-title">No agents yet.</p>
          <p>Claude Code, Codex, or any harness your machines have — one or a dozen. kiln runs them: steer them, answer them, review what they change.</p>
          <Button onClick={onSpawn}>
            New agents <Plus />
          </Button>
        </div>
      ) : (
        <>
          <div className="ms-listhead">
            <label className="ms-all">
              <input
                ref={allBox}
                type="checkbox"
                className="check"
                disabled={live.length === 0}
                checked={live.length > 0 && liveChecked === live.length}
                onChange={(e) => setSelected(e.target.checked ? new Set([...selected, ...live.map((r) => r.a.id)]) : new Set([...selected].filter((id) => !live.some((r) => r.a.id === id))))}
                aria-label={filtered ? `Select the ${live.length} agents shown` : `Select all ${live.length} agents`}
              />
              <span>
                {checked.length > 0
                  ? `${checked.length} of ${live.length + ended.length} selected`
                  : filtered
                    ? `${live.length + ended.length} of ${all.length} agents`
                    : `${all.length} agent${all.length === 1 ? "" : "s"}`}
              </span>
            </label>
            {checked.length > 0 && (
              <button type="button" className="ms-link" onClick={() => setSelected(new Set())}>
                Clear selection
              </button>
            )}
            {shown.stale && (
              <button type="button" className="ms-link ms-refresh" onClick={shown.refresh} title="The list stays as it was while agents work. Refresh to sort and filter it again.">
                <RotateCcw size={12} aria-hidden="true" /> Refresh list
              </button>
            )}
          </div>
          {live.length > 0 ? (
            <ul className="ms-list">
              {live.map(({ a, stand }) => (
                <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} />
              ))}
            </ul>
          ) : filtered && ended.length === 0 ? (
            <p className="ms-quiet">
              No agent matches.{" "}
              <button
                type="button"
                className="ms-link"
                onClick={() => {
                  setQ("");
                  setView({ ...view, facets: {} });
                }}
              >
                Clear the filter
              </button>
            </p>
          ) : (
            <p className="ms-quiet">Nothing running. Start something new, or pick up an ended agent below.</p>
          )}
        </>
      )}

      {/* Once kiln's own agents are known, so its own sessions never show here for a moment. */}
      <React.Suspense fallback={null}>
        <Found all={all} />
      </React.Suspense>

      {ended.length > 0 && (
        <section className="ms-ended">
          <div className="ms-fold">
            <button type="button" className="ms-fold-btn" aria-expanded={showEnded} onClick={() => setShowEnded(!showEnded)}>
              <ChevronRight size={13} className="ms-chev" aria-hidden="true" />
              Ended · {ended.length}
            </button>
            <button type="button" className="ms-link" onClick={() => void Promise.allSettled(ended.map((r) => harnessApi.stop(r.a.id))).then(() => reload())}>
              Clear
            </button>
          </div>
          {showEnded && (
            <ul className="ms-list">
              {ended.map(({ a, stand }) => (
                <Row key={a.id} a={a} stand={stand} selected={selected.has(a.id)} onSelect={select} />
              ))}
            </ul>
          )}
        </section>
      )}
    </>
  );
}
