/**
 * Everything a harness lets you set or do, rendered from what it reports (Meta.controls): effort,
 * sandboxes, MCP servers, compaction, rewinds, output styles … A harness that adds a control gets
 * it in the console without any change here.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { MoreHorizontal, SlidersHorizontal, X } from "lucide-react";
import type { Command, Control, Meta, State } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

type Send = (c: Command) => void;

const valueOf = (c: Control, state: State | null) => state?.control_values?.[c.id] ?? c.value;

/** Asks before anything marked dangerous; otherwise runs at once. */
function useConfirm() {
  const [ask, setAsk] = React.useState<{ title: string; body: string; run: () => void } | null>(null);
  const confirm = (dangerous: boolean | undefined, title: string, body: string, run: () => void) => (dangerous ? setAsk({ title, body, run }) : run());
  const dialog = (
    <Dialog open={ask !== null} onOpenChange={(o) => !o && setAsk(null)}>
      {ask && (
        <DialogContent title={ask.title} eyebrow="Confirm" description={ask.body}>
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={() => setAsk(null)}>
              Cancel
            </Button>
            <Button
              variant="danger"
              onClick={() => {
                ask.run();
                setAsk(null);
              }}
            >
              Go ahead
            </Button>
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
  return { confirm, dialog };
}

function ControlRow({ c, state, send, confirm }: { c: Control; state: State | null; send: Send; confirm: ReturnType<typeof useConfirm>["confirm"] }) {
  const v = valueOf(c, state);
  const [text, setText] = React.useState(typeof v === "string" ? v : "");
  const set = (value: unknown, dangerous?: boolean) =>
    confirm(dangerous ?? c.dangerous, `${c.label}?`, c.description ?? "This can't be undone from kiln.", () => send({ type: "control", id: c.id, value }));
  return (
    <div className="hx-ctl">
      <div className="hx-ctl-text">
        <span className="hx-ctl-label">{c.label}</span>
        {c.description && <span className="hx-dim text-[12px]">{c.description}</span>}
      </div>
      <div className="hx-ctl-input">
        {c.kind === "select" && (c.options?.length ?? 0) <= 4 && (
          <div className="hx-modes" role="radiogroup" aria-label={c.label}>
            {(c.options ?? []).map((o) => (
              <button key={o.id} type="button" role="radio" aria-checked={v === o.id} className="hx-mode" data-mode="queue" title={o.description} onClick={() => set(o.id, o.dangerous)}>
                {o.label}
              </button>
            ))}
          </div>
        )}
        {c.kind === "select" && (c.options?.length ?? 0) > 4 && (
          <select
            className="hx-select"
            value={typeof v === "string" ? v : ""}
            onChange={(e) => {
              const o = c.options?.find((x) => x.id === e.target.value);
              if (o) set(o.id, o.dangerous);
            }}
          >
            {typeof v !== "string" && <option value="">—</option>}
            {(c.options ?? []).map((o) => (
              <option key={o.id} value={o.id}>
                {o.label}
              </option>
            ))}
          </select>
        )}
        {c.kind === "toggle" && (
          <button type="button" role="switch" aria-checked={v === true} aria-label={c.label} className="hx-switch" onClick={() => set(!(v === true))}>
            <span />
          </button>
        )}
        {c.kind === "action" && (
          <Button size="xs" variant={c.dangerous ? "danger" : "secondary"} onClick={() => set(null)}>
            {c.label}
          </Button>
        )}
        {c.kind === "text" && (
          <form
            className="flex gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              set(text);
            }}
          >
            <input className="hx-input" value={text} onChange={(e) => setText(e.target.value)} aria-label={c.label} />
            <Button size="xs" variant="secondary" type="submit">
              Apply
            </Button>
          </form>
        )}
      </div>
    </div>
  );
}

/** Every session-wide control the harness offers, by group. */
export function SettingsSheet({ open, onOpenChange, meta, state, send }: { open: boolean; onOpenChange: (o: boolean) => void; meta: Meta | null; state: State | null; send: Send }) {
  const { confirm, dialog } = useConfirm();
  const controls = (meta?.controls ?? []).filter((c) => (c.scope ?? "session") === "session");
  const groups = React.useMemo(() => {
    const m = new Map<string, Control[]>();
    for (const c of controls) m.set(c.group || "General", [...(m.get(c.group || "General") ?? []), c]);
    return [...m.entries()];
  }, [controls]);
  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent
          title="Agent settings"
          eyebrow={meta?.version ? `Harness ${meta.version}` : "Agent"}
          description="Everything this harness lets kiln set or do. Changes apply to this agent from its next step."
        >
          <div className="hx-ctls">
            {groups.length === 0 && <div className="hx-dim text-[13px]">This harness reports no further settings.</div>}
            {groups.map(([g, cs]) => (
              <section key={g} className="hx-ctl-group">
                <div className="hx-eyebrow">{g}</div>
                {cs.map((c) => (
                  <ControlRow key={c.id} c={c} state={state} send={send} confirm={confirm} />
                ))}
              </section>
            ))}
          </div>
        </DialogContent>
      </Dialog>
      {dialog}
    </>
  );
}

export function SettingsButton({ onClick, count }: { onClick: () => void; count: number }) {
  return (
    <Button size="sm" variant="ghost" onClick={onClick} aria-label="Agent settings">
      <SlidersHorizontal />
      <span className="hidden lg:inline">Settings</span>
      {count > 0 && <span className="hx-tab-count">{count}</span>}
    </Button>
  );
}

/** Actions on one turn (rewind to here, branch from here), on its prompt card. */
export function TurnMenu({ meta, eventId, send }: { meta: Meta | null; eventId: string; send: Send }) {
  const { confirm, dialog } = useConfirm();
  const actions = (meta?.controls ?? []).filter((c) => c.scope === "turn");
  if (actions.length === 0) return null;
  return (
    <>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <button type="button" className="hx-icon hx-turn-menu" aria-label="Actions on this turn">
            <MoreHorizontal size={14} />
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content className="popover w-64" sideOffset={4} align="start">
            {actions.map((c) => (
              <DropdownMenu.Item
                key={c.id}
                className={cn("menu-item", c.dangerous && "text-[var(--color-fail)]")}
                onSelect={() => confirm(c.dangerous, `${c.label}?`, c.description ?? "", () => send({ type: "control", id: c.id, value: { event: eventId } }))}
              >
                <span className="flex-1">{c.label}</span>
              </DropdownMenu.Item>
            ))}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
      {dialog}
    </>
  );
}

/** Files and images to send with a message: pasted, dropped or picked. */
export function useAttachments(enabled: boolean) {
  const [files, setFiles] = React.useState<{ name: string; mime: string; data_base64: string; size: number }[]>([]);
  const add = React.useCallback(
    async (list: FileList | File[]) => {
      if (!enabled) return;
      const read = (f: File) =>
        new Promise<{ name: string; mime: string; data_base64: string; size: number }>((resolve, reject) => {
          const r = new FileReader();
          r.onload = () => resolve({ name: f.name || "pasted", mime: f.type || "application/octet-stream", data_base64: String(r.result).replace(/^data:[^,]*,/, ""), size: f.size });
          r.onerror = () => reject(r.error ?? new Error("read failed"));
          r.readAsDataURL(f);
        });
      const added = await Promise.all([...list].filter((f) => f.size <= 20 * 1024 * 1024).map(read));
      setFiles((cur) => [...cur, ...added]);
    },
    [enabled],
  );
  const remove = (i: number) => setFiles((cur) => cur.filter((_, j) => j !== i));
  const clear = () => setFiles([]);
  const chips = files.length ? (
    <div className="hx-attach">
      {files.map((f, i) => (
        <span key={i} className="hx-attach-chip">
          {f.mime.startsWith("image/") ? <img src={`data:${f.mime};base64,${f.data_base64}`} alt="" /> : null}
          <span className="hx-trunc">{f.name}</span>
          <button type="button" aria-label={`Remove ${f.name}`} onClick={() => remove(i)}>
            <X size={11} />
          </button>
        </span>
      ))}
    </div>
  ) : null;
  return { files, add, clear, chips };
}
