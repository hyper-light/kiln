/**
 * Start agents. Written like a message: what it should do comes first; which agent, where, how
 * many, how careful and how hard it thinks sit in one row of chips underneath; the button to start
 * is always in view. On a phone it's a full-screen sheet.
 */
import * as React from "react";
import "./harness.css";
import { Dialog as D, DropdownMenu, Popover } from "radix-ui";
import { Check, ChevronDown, Folder, Gauge, Rocket, Server, ShieldCheck, ShieldOff, SlidersHorizontal, SquareTerminal, X } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type KindInfo, type PastSession, type Spec } from "@/api/harness";
import type { Agent, TerminalSessionInfo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { errorMessage, useLocalStorage, useNow } from "@/lib/hooks";
import { relativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { HarnessMark } from "./parts";
import { tildify } from "./derive";
import { InlinePath } from "./inline-path";

interface ModeChoice {
  id: string;
  label: string;
  hint: string;
  /** Turns checks off: chosen only with an explicit "Turn them off", and never remembered. */
  dangerous?: boolean;
}

/** Each harness's own permission modes (what the machine reports wins). */
const MODES: Record<string, ModeChoice[]> = {
  claude: [
    { id: "default", label: "Ask first", hint: "Asks before commands and edits" },
    { id: "acceptEdits", label: "Edits OK", hint: "Edits files freely, asks before commands" },
    { id: "plan", label: "Plan first", hint: "Researches and proposes a plan; changes nothing until you approve" },
  ],
  codex: [
    { id: "untrusted", label: "Ask first", hint: "Asks before anything beyond reading" },
    { id: "auto", label: "Auto", hint: "Works in its folder; asks to go outside it or online" },
    { id: "read-only", label: "Read only", hint: "Reads and answers; changes nothing" },
  ],
};
const modesFor = (k: KindInfo | undefined): ModeChoice[] => k?.modes?.map((m) => ({ id: m.id, label: m.label, hint: m.description, dangerous: m.dangerous })) ?? MODES[k?.id ?? ""] ?? [];

/** Each harness's effort levels, lowest first, for runners that don't report them yet. */
const EFFORTS: Record<string, string[]> = { claude: ["low", "medium", "high", "xhigh", "max"], codex: ["low", "medium", "high"] };
const capital = (v: string) => v.charAt(0).toUpperCase() + v.slice(1);
const effortsFor = (k: KindInfo | undefined): { id: string; label: string; hint?: string }[] =>
  k?.efforts?.map((e) => ({ id: e.id, label: e.label || capital(e.id), hint: e.description || undefined })) ?? (EFFORTS[k?.id ?? ""] ?? []).map((id) => ({ id, label: capital(id) }));

const COUNTS = [1, 2, 3, 4, 6, 8, 12];

/** A compact chip that opens a menu of choices. */
function Chip<T extends string | number>({
  icon,
  label,
  value,
  options,
  onPick,
  title,
}: {
  icon?: React.ReactNode;
  label: React.ReactNode;
  value: T;
  options: { value: T; label: React.ReactNode; hint?: string; disabled?: boolean }[];
  onPick: (v: T) => void;
  title: string;
}) {
  return (
    <DropdownMenu.Root>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="sp-chip" aria-label={title}>
          {icon}
          <span className="sp-chip-text">{label}</span>
          <ChevronDown size={13} className="sp-chip-chev" />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="popover sp-menu" sideOffset={6} align="start" collisionPadding={12}>
          <DropdownMenu.Label className="label px-2.5 pt-1.5 pb-1">{title}</DropdownMenu.Label>
          {options.map((o) => (
            <DropdownMenu.Item key={String(o.value)} className="menu-item sp-menu-item" disabled={o.disabled} onSelect={() => onPick(o.value)}>
              <span className="sp-menu-check">{o.value === value ? <Check size={13} /> : null}</span>
              <span className="flex min-w-0 flex-1 flex-col">
                <span className="truncate">{o.label}</span>
                {o.hint && <span className="sp-menu-hint">{o.hint}</span>}
              </span>
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** The folder chip: type a path or pick one used before on this machine. */
function FolderChip({ value, onChange, recent, shellLabel }: { value: string; onChange: (v: string) => void; recent: string[]; shellLabel?: string }) {
  const [open, setOpen] = React.useState(false);
  const [draft, setDraft] = React.useState(value);
  React.useEffect(() => setDraft(value), [value, open]);
  if (shellLabel)
    return (
      <span className="sp-chip sp-chip-static" title="Starts where the shell is">
        <SquareTerminal size={13} />
        <span className="sp-chip-text">in {shellLabel}</span>
      </span>
    );
  return (
    <Popover.Root open={open} onOpenChange={setOpen}>
      <Popover.Trigger asChild>
        <button type="button" className="sp-chip" aria-label="Folder">
          <Folder size={13} />
          <span className="sp-chip-text sp-mono">{value || "~"}</span>
          <ChevronDown size={13} className="sp-chip-chev" />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content className="popover sp-folder" sideOffset={6} align="start" collisionPadding={12}>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              onChange(draft.trim() || "~");
              setOpen(false);
            }}
          >
            <label className="label px-1 pb-1.5 block" htmlFor="sp-folder-input">
              Folder on the machine
            </label>
            <div className="flex gap-2">
              <input
                id="sp-folder-input"
                className="hx-input sp-mono"
                autoFocus
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                placeholder="~/Projects/repo"
                spellCheck={false}
                autoCapitalize="off"
                autoCorrect="off"
              />
              <Button size="xs" type="submit">
                Use
              </Button>
            </div>
          </form>
          {recent.length > 0 && (
            <div className="mt-2">
              <div className="label px-1 pb-1">Recent</div>
              {recent.map((r) => (
                <button
                  key={r}
                  type="button"
                  className="menu-item sp-mono w-full"
                  onClick={() => {
                    onChange(r);
                    setOpen(false);
                  }}
                >
                  <Folder size={13} className="text-[#7f838d]" />
                  <span className="truncate">{r}</span>
                </button>
              ))}
            </div>
          )}
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/** Below the prompt: the machine's past conversations with this harness, to pick one up again. */
function PastSessions({ target, kind, kindName, onPick, busy }: { target: string; kind: string; kindName: string; onPick: (p: PastSession) => void; busy: boolean }) {
  const [open, setOpen] = React.useState(false);
  const [list, setList] = React.useState<PastSession[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const now = useNow();
  React.useEffect(() => {
    if (!open || !target) return;
    let live = true;
    setList(null);
    setError(null);
    harnessApi.history(target, kind).then(
      (l) => live && setList(l),
      (e: unknown) => live && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      live = false;
    };
  }, [open, target, kind]);
  if (!target) return null;
  return (
    <div className="ps">
      <button type="button" className="ps-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span>Continue a past {kindName} session</span>
        <ChevronDown size={12} className="ps-chev" aria-hidden="true" />
      </button>
      {open && (
        <div className="ps-body">
          {error ? (
            <p className="ps-none">{/(502|404|not found)/i.test(error) ? "This machine's runner can't list its past sessions yet — upgrade it in Settings → Updates." : error}</p>
          ) : !list ? (
            <p className="ps-none">Looking…</p>
          ) : list.length === 0 ? (
            <p className="ps-none">No past {kindName} sessions on this machine.</p>
          ) : (
            <ul className="ps-list">
              {list.map((p) => (
                <li key={p.id}>
                  <button type="button" className="ps-row" disabled={busy} onClick={() => onPick(p)}>
                    <span className="ps-title">{p.title || p.last_prompt || p.id}</span>
                    <span className="ps-meta">
                      {tildify(p.cwd)}
                      {p.branch ? ` · ${p.branch}` : ""} · {relativeTime(p.last_activity, now)}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

export function SpawnDialog({
  open,
  onOpenChange,
  onStarted,
  defaults,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onStarted: (agents: AgentInfo[]) => void;
  /** Start inside this shell (its machine and folder), or like another agent (Duplicate). */
  defaults?: { target?: string; shell?: string; cwd?: string; kind?: string };
}) {
  const [machines, setMachines] = React.useState<Agent[]>([]);
  const [shells, setShells] = React.useState<TerminalSessionInfo[]>([]);
  const [lastTarget, setLastTarget] = useLocalStorage("kiln.agents.target", "");
  const [kind, setKind] = useLocalStorage("kiln.agents.kind", "claude");
  const [modeByKind, setModeByKind] = useLocalStorage<Record<string, string>>("kiln.agents.modes", {});
  const [effortByKind, setEffortByKind] = useLocalStorage<Record<string, string>>("kiln.agents.efforts", {});
  /** A mode that turns checks off: `armed` waits for "Turn them off", `unchecked` is confirmed (for this start only). */
  const [armed, setArmed] = React.useState("");
  const [unchecked, setUnchecked] = React.useState("");
  const [dirs, setDirs] = useLocalStorage<Record<string, string[]>>("kiln.agents.recentDirs", {});
  const [byTarget, setByTarget] = React.useState<Record<string, KindInfo[]> | null>(null);
  const [picked, setPicked] = React.useState<string | null>(null);
  const [cwd, setCwd] = React.useState("~");
  const [count, setCount] = React.useState(1);
  const [prompt, setPrompt] = React.useState("");
  const [more, setMore] = React.useState(false);
  const [name, setName] = React.useState("");
  const [model, setModel] = React.useState("");
  const [shell, setShell] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const promptRef = React.useRef<HTMLTextAreaElement>(null);

  React.useEffect(() => {
    if (!open) return;
    setError(null);
    setArmed("");
    setUnchecked("");
    setPicked(defaults?.target ?? null);
    setShell(defaults?.shell ?? "");
    if (defaults?.kind) setKind(defaults.kind);
    setByTarget(null);
    harnessApi.kindsAll().then(setByTarget, () => setByTarget({}));
    api.agents().then(
      (a) => setMachines(a.filter((m) => m.connected && !m.ephemeral)),
      () => undefined,
    );
    api.terminalSessions().then(setShells, () => undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Every harness any machine has; the machines that have the one you picked.
  const all = React.useMemo(() => {
    const m = new Map<string, KindInfo>();
    for (const ks of Object.values(byTarget ?? {}))
      for (const k of ks) {
        const cur = m.get(k.id);
        m.set(k.id, cur ? { ...cur, available: cur.available || k.available, verified: cur.verified || k.verified } : k);
      }
    return [...m.values()].sort((a, b) => Number(b.available) - Number(a.available));
  }, [byTarget]);
  const installed = all.filter((k) => k.available);
  React.useEffect(() => {
    if (installed.length && !installed.some((k) => k.id === kind)) setKind(installed[0]?.id ?? "claude");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [all]);
  const where = Object.entries(byTarget ?? {})
    .filter(([, ks]) => ks.some((x) => x.id === kind && x.available))
    .map(([t]) => t);
  const target = [picked, defaults?.target, lastTarget].find((t): t is string => Boolean(t) && where.includes(t ?? "")) ?? where[0] ?? "";
  const machineName = (t: string) => {
    if (t === "local") return "kiln server";
    const n = t.replace(/^agent:/, "");
    const m = machines.find((x) => x.name === n);
    return m?.hostname && m.hostname !== n ? `${n} (${m.hostname})` : n;
  };
  const recent = dirs[target] ?? [];
  React.useEffect(() => {
    if (open && !defaults?.cwd) setCwd(recent[0] ?? "~");
    else if (open && defaults?.cwd) setCwd(defaults.cwd);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, target]);

  const kindInfo = all.find((k) => k.id === kind);
  const modes = modesFor(kindInfo);
  const safe = (id: string | undefined) => modes.some((m) => m.id === id && !m.dangerous);
  const mode = modes.some((m) => m.id === unchecked) ? unchecked : safe(modeByKind[kind]) ? (modeByKind[kind] ?? "") : (kindInfo?.default_mode ?? modes.find((m) => !m.dangerous)?.id ?? "");
  const modeInfo = modes.find((m) => m.id === mode);
  const armedInfo = modes.find((m) => m.id === armed);
  const pickMode = (id: string) => {
    setArmed("");
    if (modes.find((m) => m.id === id)?.dangerous) {
      if (id !== unchecked) setArmed(id);
      return;
    }
    setUnchecked("");
    setModeByKind({ ...modeByKind, [kind]: id });
  };
  const efforts = effortsFor(kindInfo);
  const effort = efforts.some((e) => e.id === effortByKind[kind]) ? (effortByKind[kind] ?? "") : "";
  const effortInfo = efforts.find((e) => e.id === effort);
  const shellInfo = shell ? shells.find((s) => s.id === shell) : undefined;
  const targetShells = shells.filter((s) => s.target === target && !s.exited);

  /** Pick up a past conversation where it ended: same harness, folder and history. */
  const resume = async (p: PastSession) => {
    setBusy(true);
    setError(null);
    try {
      const started = await harnessApi.start(
        target,
        { kind: p.kind, cwd: p.cwd || undefined, resume: p.id, mode: mode || undefined, effort: effort || undefined, name: (p.title || "").slice(0, 80) || undefined },
        1,
      );
      setDirs({ ...dirs, [target]: [p.cwd, ...recent.filter((d) => d !== p.cwd)].slice(0, 8) });
      onStarted(started);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const start = async () => {
    if (!target) return;
    setBusy(true);
    setError(null);
    const spec: Spec = {
      kind,
      // In a shell, the agent starts where the shell is; "~" (or nothing) is the machine's home.
      cwd: shell || !cwd.trim() || cwd.trim() === "~" ? undefined : cwd.trim(),
      mode: mode || undefined,
      effort: effort || undefined,
      model: model.trim() || undefined,
      name: name.trim() || undefined,
      prompt: prompt.trim() || undefined,
      shell: shell || undefined,
    };
    try {
      const started = await harnessApi.start(target, spec, count);
      setLastTarget(target);
      if (!shell && cwd.trim()) setDirs({ ...dirs, [target]: [cwd.trim(), ...recent.filter((d) => d !== cwd.trim())].slice(0, 8) });
      onStarted(started);
      setPrompt("");
      setName("");
      onOpenChange(false);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const noHarness = byTarget !== null && installed.length === 0;
  // Connected machines whose runner never reported its harnesses: too old to run agents (not "not installed").
  const stale = byTarget === null ? [] : machines.filter((m) => !(`agent:${m.name}` in byTarget));
  const upgradeHint =
    stale.length > 0 ? (
      <div className="sp-note sp-note-warn">
        <b>{stale.map((m) => m.name).join(", ")}</b> {stale.length === 1 ? "is" : "are"} connected, but {stale.length === 1 ? "its kiln runner is" : "their kiln runners are"} too old to run agents, so
        kiln can't see what's installed there. Upgrade without a restart (shells keep running) by running this on {stale.length === 1 ? "that machine" : "each machine"} with the new kiln binary:
        <code className="sp-cmd">kiln install --agent --upgrade</code>
      </div>
    ) : null;
  return (
    <D.Root open={open} onOpenChange={onOpenChange}>
      <D.Portal>
        <D.Overlay className="overlay" />
        <D.Content
          className="sp-sheet"
          aria-describedby={undefined}
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            promptRef.current?.focus();
          }}
        >
          <header className="sp-head">
            <D.Title className="sp-title">New agents</D.Title>
            <D.Close asChild>
              <button type="button" className="sp-close" aria-label="Close">
                <X size={17} />
              </button>
            </D.Close>
          </header>

          <form
            className="sp-body"
            onSubmit={(e) => {
              e.preventDefault();
              void start();
            }}
          >
            <textarea
              ref={promptRef}
              className="sp-prompt"
              rows={4}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                  e.preventDefault();
                  void start();
                }
              }}
              placeholder={count > 1 ? `What should these ${count} agents do? (each gets this message)` : "What should it do? (optional — you can tell it later)"}
              aria-label="First message"
            />

            {noHarness ? (
              (upgradeHint ?? <div className="sp-note">No connected machine has a coding agent installed. Install Claude Code or Codex on any machine running kiln, then try again.</div>)
            ) : byTarget === null ? (
              <div className="sp-note">Looking at your machines…</div>
            ) : (
              <div className="sp-chips">
                <Chip
                  title="Agent"
                  icon={<HarnessMark kind={kind} size={18} />}
                  label={kindInfo?.name ?? kind}
                  value={kind}
                  onPick={(k) => setKind(k)}
                  options={all.map((k) => ({
                    value: k.id,
                    label: `${k.name}${k.verified ? "" : " · untested"}`,
                    hint: k.available ? undefined : "Not installed on any connected machine",
                    disabled: !k.available,
                  }))}
                />
                <Chip
                  title="Machine"
                  icon={<Server size={13} />}
                  label={machineName(target)}
                  value={target}
                  onPick={(t) => {
                    setPicked(t);
                    setShell("");
                  }}
                  options={where.map((t) => ({ value: t, label: machineName(t) }))}
                />
                {shell && <FolderChip value={cwd} onChange={setCwd} recent={recent} shellLabel={shellInfo ? shellInfo.title || "this shell" : "this shell"} />}
                <Chip title="How many" label={`×${count}`} value={count} onPick={(n) => setCount(n)} options={COUNTS.map((n) => ({ value: n, label: n === 1 ? "1 agent" : `${n} agents` }))} />
                {modes.length > 0 && (
                  <Chip
                    title="Permissions"
                    icon={modeInfo?.dangerous ? <ShieldOff size={13} className="sp-chip-warn" /> : <ShieldCheck size={13} />}
                    label={modeInfo?.label ?? mode}
                    value={mode}
                    onPick={pickMode}
                    options={modes.map((m) => ({ value: m.id, label: m.label, hint: m.hint }))}
                  />
                )}
                {efforts.length > 0 && (
                  <Chip
                    title="Effort"
                    icon={<Gauge size={13} />}
                    label={effortInfo ? `${effortInfo.label} effort` : "Default effort"}
                    value={effort}
                    onPick={(e) => setEffortByKind({ ...effortByKind, [kind]: e })}
                    options={[{ value: "", label: "Default", hint: "The model's own" }, ...efforts.map((e) => ({ value: e.id, label: e.label, hint: e.hint }))]}
                  />
                )}
                <button type="button" className={cn("sp-chip", more && "sp-chip-on")} aria-expanded={more} onClick={() => setMore(!more)}>
                  <SlidersHorizontal size={13} />
                  <span className="sp-chip-text">More</span>
                </button>
              </div>
            )}

            {more && (
              <div className="sp-more">
                <label className="sp-field">
                  <span>Name</span>
                  <input className="hx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder={count > 1 ? "fix-tests (numbered)" : "optional"} />
                </label>
                <label className="sp-field">
                  <span>Model</span>
                  <input className="hx-input" value={model} onChange={(e) => setModel(e.target.value)} placeholder="the harness's default" spellCheck={false} autoCapitalize="off" />
                </label>
                {targetShells.length > 0 && (
                  <label className="sp-field">
                    <span>Run in</span>
                    <select className="hx-select" value={shell} onChange={(e) => setShell(e.target.value)}>
                      <option value="">A folder</option>
                      {targetShells.map((s) => (
                        <option key={s.id} value={s.id}>
                          Shell: {s.title || s.id.slice(0, 8)}
                        </option>
                      ))}
                    </select>
                  </label>
                )}
              </div>
            )}

            {!shell && target && <InlinePath target={target} value={cwd} onChange={setCwd} recent={recent} className="sp-path" />}
            {!noHarness && upgradeHint}
            {armedInfo ? (
              <div className="sp-hint sp-hint-warn">
                {armedInfo.label}: no checks at all.{" "}
                <button
                  type="button"
                  className="sp-confirm"
                  onClick={() => {
                    setUnchecked(armedInfo.id);
                    setArmed("");
                  }}
                >
                  Turn them off
                </button>
              </div>
            ) : (
              modeInfo && <div className={cn("sp-hint", modeInfo.dangerous && "sp-hint-warn")}>{modeInfo.hint}.</div>
            )}
            {error && <div className="sp-error">{error}</div>}
            <PastSessions target={target} kind={kind} kindName={kindInfo?.name ?? kind} onPick={(p) => void resume(p)} busy={busy} />

            <footer className="sp-foot">
              <span className="sp-foot-note">{target ? `${kindInfo?.name ?? kind} on ${machineName(target)}` : ""}</span>
              <Button type="submit" disabled={busy || !target}>
                <Rocket /> {busy ? "Starting…" : count > 1 ? `Start ${count} agents` : "Start agent"}
              </Button>
            </footer>
          </form>
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}
