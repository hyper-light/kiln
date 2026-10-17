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

const MODES = [
  { id: "default", label: "Ask first", hint: "Asks before commands and edits" },
  { id: "acceptEdits", label: "Edits OK", hint: "Edits files freely, asks before commands" },
  { id: "plan", label: "Plan first", hint: "Researches and proposes a plan; changes nothing until you approve" },
];

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
  const [target, setTarget] = useLocalStorage("kiln.agents.target", "local");
  const [kind, setKind] = useLocalStorage("kiln.agents.kind", "claude");
  const [mode, setMode] = useLocalStorage("kiln.agents.mode", "default");
  const [cwd, setCwd] = useLocalStorage("kiln.agents.cwd", "~");
  const [kinds, setKinds] = React.useState<KindInfo[] | null>(null);
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
    if (defaults?.target) setTarget(defaults.target);
    if (defaults?.cwd) setCwd(defaults.cwd);
    setShell(defaults?.shell ?? "");
    api.agents().then((a) => setMachines(a.filter((m) => m.connected && !m.ephemeral)), () => undefined);
    api.terminalSessions().then(setShells, () => undefined);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  React.useEffect(() => {
    if (!open) return;
    let live = true;
    setKinds(null);
    harnessApi.kinds(target).then(
      (k) => live && setKinds(k),
      () => live && setKinds([]),
    );
    return () => {
      live = false;
    };
  }, [open, target]);

  const usable = (kinds ?? []).filter((k) => k.available);
  React.useEffect(() => {
    if (kinds && usable.length && !usable.some((k) => k.id === kind)) setKind(usable[0]?.id ?? "claude");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kinds]);

  const targetShells = shells.filter((s) => s.target === target && !s.exited);

  const start = async () => {
    setBusy(true);
    setError(null);
    const spec: Spec = {
      kind,
      cwd: cwd.trim() || undefined,
      mode,
      model: model.trim() || undefined,
      name: name.trim() || undefined,
      prompt: prompt.trim() || undefined,
      shell: shell || undefined,
    };
    try {
      const started = await harnessApi.start(target, spec, count);
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
            <span className="hx-eyebrow">Machine</span>
            <select className="hx-select" value={target} onChange={(e) => setTarget(e.target.value)}>
              <option value="local">server</option>
              {machines.map((m) => (
                <option key={m.name} value={`agent:${m.name}`}>
                  {m.name}
                  {m.hostname && m.hostname !== m.name ? ` (${m.hostname})` : ""}
                </option>
              ))}
            </select>
          </div>

          <div className="hx-field">
            <span className="hx-eyebrow">Harness</span>
            <div className="hx-kinds">
              {kinds === null && <span className="hx-dim text-[12px]">Looking…</span>}
              {kinds?.length === 0 && <span className="hx-dim text-[12px]">No harness found on this machine.</span>}
              {(kinds ?? []).map((k) => (
                <button key={k.id} type="button" className="hx-kind" aria-pressed={kind === k.id} disabled={!k.available} onClick={() => setKind(k.id)} title={k.available ? undefined : "Not installed on this machine"}>
                  <HarnessMark kind={k.id} name={k.name} size={20} />
                  <span>{k.name}</span>
                  {!k.verified && <span className="hx-tag">untested</span>}
                </button>
              ))}
            </div>
          </div>

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

          <div className="hx-field">
            <span className="hx-eyebrow">Permissions</span>
            <div className="hx-modes-pick">
              {MODES.map((m) => (
                <button key={m.id} type="button" className="hx-mode-card" aria-pressed={mode === m.id} onClick={() => setMode(m.id)}>
                  <span className="hx-mode-card-label">{m.label}</span>
                  <span className="hx-dim">{m.hint}</span>
                </button>
              ))}
            </div>
          </div>

          <div className="hx-field">
            <span className="hx-eyebrow">Model</span>
            <input className="hx-input" value={model} onChange={(e) => setModel(e.target.value)} placeholder="the harness's default" spellCheck={false} autoCapitalize="off" />
          </div>

          <div className="hx-field">
            <span className="hx-eyebrow">First message</span>
            <textarea className="hx-input" rows={4} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={count > 1 ? "Every agent gets this message" : "What should it do? (optional)"} />
          </div>

          {error && <div className="hx-bad text-[12px]">{error}</div>}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button onClick={start} disabled={busy || !usable.some((k) => k.id === kind)} className={cn(busy && "opacity-70")}>
              <Rocket /> {count > 1 ? `Start ${count} agents` : "Start agent"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
