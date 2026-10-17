/** Start agents: any harness a machine has, as many as you want, in a folder or one of your shells. */
import * as React from "react";
import { Minus, Plus, Rocket } from "lucide-react";
import { api } from "@/api/client";
import { harnessApi, type AgentInfo, type KindInfo, type Spec } from "@/api/harness";
import type { Agent, TerminalSessionInfo } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { errorMessage, useLocalStorage } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { HarnessMark } from "./parts";

interface ModeChoice {
  id: string;
  label: string;
  hint: string;
}

/** Each harness's own permission modes (a machine that reports them wins). Never the skip-all modes. */
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
const modesFor = (k: KindInfo | undefined): ModeChoice[] =>
  (k as (KindInfo & { modes?: { id: string; label: string; description: string; dangerous?: boolean }[] }) | undefined)?.modes
    ?.filter((m) => !m.dangerous)
    .map((m) => ({ id: m.id, label: m.label, hint: m.description })) ??
  MODES[k?.id ?? ""] ??
  [];

export function SpawnDialog({
  open,
  onOpenChange,
  onStarted,
  defaults,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onStarted: (agents: AgentInfo[]) => void;
  /** Start inside this shell (its machine and folder). */
  defaults?: { target?: string; shell?: string; cwd?: string };
}) {
  const [machines, setMachines] = React.useState<Agent[]>([]);
  const [shells, setShells] = React.useState<TerminalSessionInfo[]>([]);
  const [lastTarget, setLastTarget] = useLocalStorage("kiln.agents.target", "");
  const [kind, setKind] = useLocalStorage("kiln.agents.kind", "claude");
  const [modeByKind, setModeByKind] = useLocalStorage<Record<string, string>>("kiln.agents.modes", {});
  const [cwd, setCwd] = useLocalStorage("kiln.agents.cwd", "~");
  /** Every machine's harnesses (null while loading). */
  const [byTarget, setByTarget] = React.useState<Record<string, KindInfo[]> | null>(null);
  const [picked, setPicked] = React.useState<string | null>(null);
  const [count, setCount] = React.useState(1);
  const [name, setName] = React.useState("");
  const [model, setModel] = React.useState("");
  const [prompt, setPrompt] = React.useState("");
  const [shell, setShell] = React.useState<string>("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  React.useEffect(() => {
    if (!open) return;
    setError(null);
    setPicked(defaults?.target ?? null);
    if (defaults?.cwd) setCwd(defaults.cwd);
    setShell(defaults?.shell ?? "");
    setByTarget(null);
    harnessApi.kindsAll().then(setByTarget, () => setByTarget({}));
    api.agents().then(
      (a) => setMachines(a.filter((m) => m.connected && !m.ephemeral)),
      () => undefined,
    );
    api.terminalSessions().then(setShells, () => undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Harnesses: every one any machine has. Machines: every one that has the chosen harness.
  const all = React.useMemo(() => {
    const m = new Map<string, KindInfo>();
    for (const ks of Object.values(byTarget ?? {}))
      for (const k of ks) {
        const cur = m.get(k.id);
        m.set(k.id, cur ? { ...cur, available: cur.available || k.available, verified: cur.verified || k.verified } : k);
      }
    return [...m.values()].sort((a, b) => Number(b.available) - Number(a.available));
  }, [byTarget]);
  const hosts = (k: string) =>
    Object.entries(byTarget ?? {})
      .filter(([, ks]) => ks.some((x) => x.id === k && x.available))
      .map(([t]) => t);
  React.useEffect(() => {
    if (all.length && !all.some((k) => k.id === kind && k.available)) {
      const first = all.find((k) => k.available);
      if (first) setKind(first.id);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [all]);
  const where = hosts(kind);
  // The machine: the one you picked, else the shell's, else the last one used, else the first that has it.
  const target = [picked, defaults?.target, lastTarget].find((t): t is string => Boolean(t) && where.includes(t ?? "")) ?? where[0] ?? "";
  const machineName = (t: string) => {
    if (t === "local") return "kiln server";
    const n = t.replace(/^agent:/, "");
    const m = machines.find((x) => x.name === n);
    return m?.hostname && m.hostname !== n ? `${n} (${m.hostname})` : n;
  };

  const targetShells = shells.filter((s) => s.target === target && !s.exited);
  const kindInfo = all.find((k) => k.id === kind);
  const modes = modesFor(kindInfo);
  const mode = modes.some((m) => m.id === modeByKind[kind]) ? (modeByKind[kind] ?? "") : (modes[0]?.id ?? "");
  const setMode = (m: string) => setModeByKind({ ...modeByKind, [kind]: m });

  const start = async () => {
    setBusy(true);
    setError(null);
    const spec: Spec = {
      kind,
      // In a shell, the agent starts where the shell is.
      cwd: shell ? undefined : cwd.trim() || undefined,
      mode: mode || undefined,
      model: model.trim() || undefined,
      name: name.trim() || undefined,
      prompt: prompt.trim() || undefined,
      shell: shell || undefined,
    };
    try {
      const started = await harnessApi.start(target, spec, count);
      setLastTarget(target);
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

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title="New agents" eyebrow="Agents" description="Start any harness a machine has, one or a dozen at a time. kiln runs them and is their whole front end.">
        <div className="hx-form">
          <div className="hx-field">
            <span className="hx-eyebrow">Agent</span>
            <div className="hx-kinds">
              {byTarget === null && <span className="hx-dim text-[12px]">Looking at your machines…</span>}
              {byTarget !== null && !all.some((k) => k.available) && (
                <span className="hx-dim text-[12px]">No connected machine has a coding agent installed. Install Claude Code or Codex on any machine running kiln, then try again.</span>
              )}
              {all
                .filter((k) => k.available)
                .map((k) => (
                  <button key={k.id} type="button" className="hx-kind" aria-pressed={kind === k.id} onClick={() => setKind(k.id)}>
                    <HarnessMark kind={k.id} name={k.name} size={20} />
                    <span>{k.name}</span>
                    {!k.verified && <span className="hx-tag">untested</span>}
                  </button>
                ))}
            </div>
            {all.some((k) => !k.available) && (
              <span className="hx-dim text-[11px]">
                Not installed on any connected machine:{" "}
                {all
                  .filter((k) => !k.available)
                  .map((k) => k.name)
                  .join(", ")}
              </span>
            )}
          </div>

          {where.length > 0 && (
            <div className="hx-field">
              <span className="hx-eyebrow">Machine</span>
              <select className="hx-select" value={target} onChange={(e) => setPicked(e.target.value)}>
                {where.map((t) => (
                  <option key={t} value={t}>
                    {machineName(t)}
                  </option>
                ))}
              </select>
              {where.length < Object.keys(byTarget ?? {}).length && <span className="hx-dim text-[11px]">Only machines with {kindInfo?.name ?? "it"} installed are listed.</span>}
            </div>
          )}

          <div className="hx-row2">
            <div className="hx-field">
              <span className="hx-eyebrow">How many</span>
              <div className="hx-stepper">
                <button type="button" aria-label="Fewer" onClick={() => setCount(Math.max(1, count - 1))}>
                  <Minus size={13} />
                </button>
                <span className="hx-meta-num">{count}</span>
                <button type="button" aria-label="More" onClick={() => setCount(Math.min(24, count + 1))}>
                  <Plus size={13} />
                </button>
              </div>
            </div>
            <div className="hx-field flex-1">
              <span className="hx-eyebrow">Name</span>
              <input className="hx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder={count > 1 ? "e.g. fix-tests (numbered)" : "optional"} />
            </div>
          </div>

          <div className="hx-field">
            <span className="hx-eyebrow">Where</span>
            {targetShells.length > 0 && (
              <select className="hx-select mb-2" value={shell} onChange={(e) => setShell(e.target.value)}>
                <option value="">A folder</option>
                {targetShells.map((s) => (
                  <option key={s.id} value={s.id}>
                    In shell: {s.title || s.id.slice(0, 8)}
                  </option>
                ))}
              </select>
            )}
            {!shell && <input className="hx-input hx-mono" value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="~/Projects/repo" spellCheck={false} autoCapitalize="off" />}
          </div>

          {modes.length > 0 && (
            <div className="hx-field">
              <span className="hx-eyebrow">Permissions</span>
              <div className="hx-modes-pick">
                {modes.map((m) => (
                  <button key={m.id} type="button" className="hx-mode-card" aria-pressed={mode === m.id} onClick={() => setMode(m.id)}>
                    <span className="hx-mode-card-label">{m.label}</span>
                    <span className="hx-dim">{m.hint}</span>
                  </button>
                ))}
              </div>
            </div>
          )}

          <div className="hx-field">
            <span className="hx-eyebrow">Model</span>
            <input className="hx-input" value={model} onChange={(e) => setModel(e.target.value)} placeholder="the harness's default" spellCheck={false} autoCapitalize="off" />
          </div>

          <div className="hx-field">
            <span className="hx-eyebrow">First message</span>
            <textarea
              className="hx-input"
              rows={4}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              placeholder={count > 1 ? "Every agent gets this message" : "What should it do? (optional)"}
            />
          </div>

          {error && <div className="hx-bad text-[12px]">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button onClick={start} disabled={busy || !target} className={cn(busy && "opacity-70")}>
              <Rocket /> {count > 1 ? `Start ${count} agents` : "Start agent"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
