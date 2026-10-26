/**
 * Everything a harness lets you set or do, rendered from what it reports (Meta.controls): effort,
 * sandboxes, MCP servers, compaction, rewinds, output styles … A harness that adds a control gets
 * it in the console without any change here.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowDownToLine, MoreHorizontal, SlidersHorizontal, X } from "lucide-react";
import { harnessApi, type AgentInfo, type Attachment, type Command, type Control, type Meta, type State } from "@/api/harness";
import { formatBytes } from "@/lib/format";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { Loadable } from "@/components/ui/loading";
import { AtLimit } from "./elsewhere";

/** Its copies on other runners and its rules for them (docs/reach.md §5.1), loaded with the sheet's group. */
const AgentKeep = React.lazy(() => import("../keep/agent").then((m) => ({ default: m.AgentKeep })));

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
  const [option, setOption] = React.useState("");
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
        {c.kind === "action" && !c.input && !c.options?.length && (
          <Button size="xs" variant={c.dangerous ? "danger" : "secondary"} onClick={() => set(null)}>
            {c.label}
          </Button>
        )}
        {c.kind === "action" && (c.input || (c.options?.length ?? 0) > 0) && (
          <form
            className="flex flex-wrap gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              const chosen = c.options?.find((o) => o.id === option) ?? c.options?.[0];
              set(c.options?.length ? { option: chosen?.id, text } : text, chosen?.dangerous);
              setText("");
            }}
          >
            {(c.options?.length ?? 0) > 0 && (
              <select className="hx-select" value={option || c.options?.[0]?.id || ""} onChange={(e) => setOption(e.target.value)} aria-label={`${c.label}: kind`}>
                {(c.options ?? []).map((o) => (
                  <option key={o.id} value={o.id} title={o.description}>
                    {o.label}
                  </option>
                ))}
              </select>
            )}
            {c.input && <input className="hx-input" value={text} placeholder={c.input} onChange={(e) => setText(e.target.value)} aria-label={c.label} />}
            <Button size="xs" variant={c.dangerous ? "danger" : "secondary"} type="submit">
              {c.label}
            </Button>
          </form>
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
/** kiln's own settings for usage limits, whatever the harness: where it pauses, and whether it goes on elsewhere. */
function LimitRows({ agent, state, send }: { agent: AgentInfo | null; state: State | null; send: Send }) {
  return (
    <section className="hx-ctl-group">
      <div className="hx-eyebrow">Usage limits</div>
      <div className="hx-ctl">
        <div className="hx-ctl-text">
          <span className="hx-ctl-label">Pause</span>
          <span className="hx-dim text-[12px]">At the limit, or once one is this full: the step under way finishes first, and it goes on once the limit resets.</span>
        </div>
        <div className="hx-ctl-input">
          <select
            className="hx-select"
            aria-label="Pause"
            value={state?.pause_at ? String(state.pause_at) : ""}
            onChange={(e) => send({ type: "pause_at", percent: e.target.value ? Number(e.target.value) : null })}
          >
            <option value="">At the limit</option>
            {[80, 90, 95].map((p) => (
              <option key={p} value={String(p)}>
                At {p}% used
              </option>
            ))}
          </select>
        </div>
      </div>
      {agent && (
        <div className="hx-ctl">
          <div className="hx-ctl-text">
            <span className="hx-ctl-label">At a limit</span>
            <span className="hx-dim text-[12px]">
              Wait for the reset, or go on on another of your machines with room: the one chosen or whichever has room, in its own harness signed in to another account or one its conversation can be
              carried into (Claude Code ⇄ Codex). Its conversation and repository go along.
            </span>
          </div>
          <div className="hx-ctl-input">
            <React.Suspense fallback={null}>
              <AtLimit agent={agent} state={state} send={send} />
            </React.Suspense>
          </div>
        </div>
      )}
    </section>
  );
}

/** The most subagents at once a person may choose. */
const SUBAGENTS = [1, 2, 3, 4, 6, 8, 12, 16];

/** kiln's own, whatever the harness: the most subagents it runs at once. */
function SubagentRows({ agent, state, send }: { agent: AgentInfo | null; state: State | null; send: Send }) {
  const most = state?.max_subagents ?? agent?.max_subagents;
  return (
    <section className="hx-ctl-group">
      <div className="hx-eyebrow">Subagents</div>
      <div className="hx-ctl">
        <div className="hx-ctl-text">
          <span className="hx-ctl-label">At once</span>
          <span className="hx-dim text-[12px]">
            The most subagents it runs at the same time: its harness&apos;s own, and the agents it starts through kiln. Past that, it&apos;s told to wait for one to finish.
            {agent?.kind === "codex" ? " Codex takes a change when it next starts." : ""}
          </span>
        </div>
        <div className="hx-ctl-input">
          <select
            className="hx-select"
            aria-label="Subagents at once"
            value={most ? String(most) : ""}
            onChange={(e) => send({ type: "max_subagents", count: e.target.value ? Number(e.target.value) : null })}
          >
            <option value="">No limit of its own</option>
            {SUBAGENTS.map((n) => (
              <option key={n} value={String(n)}>
                {n === 1 ? "One at a time" : `Up to ${n}`}
              </option>
            ))}
          </select>
        </div>
      </div>
    </section>
  );
}

export function SettingsSheet({
  open,
  onOpenChange,
  agent,
  meta,
  state,
  send,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  agent: AgentInfo | null;
  meta: Meta | null;
  state: State | null;
  send: Send;
}) {
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
            <LimitRows agent={agent} state={state} send={send} />
            <SubagentRows agent={agent} state={state} send={send} />
            {agent && (
              <Loadable label="its copies on other runners" fallback={null}>
                <AgentKeep id={agent.id} />
              </Loadable>
            )}
            {groups.length === 0 && !meta?.hooks?.length && <div className="hx-dim text-[13px]">This harness reports no further settings.</div>}
            {groups.map(([g, cs]) => (
              <section key={g} className="hx-ctl-group">
                <div className="hx-eyebrow">{g}</div>
                {cs.map((c) => (
                  <ControlRow key={c.id} c={c} state={state} send={send} confirm={confirm} />
                ))}
              </section>
            ))}
            {(meta?.hooks?.length ?? 0) > 0 && (
              <section className="hx-ctl-group">
                <div className="hx-eyebrow">Hooks</div>
                <ul className="hx-hooks">
                  {(meta?.hooks ?? []).map((h, i) => (
                    <li key={i}>
                      <span className="hx-tag">{h.event}</span>
                      {h.matcher && <span className="hx-dim">{h.matcher}</span>}
                      <code className="hx-trunc">{h.command}</code>
                      {h.source && <span className="hx-dim ml-auto">{h.source}</span>}
                    </li>
                  ))}
                </ul>
              </section>
            )}
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

/** Longest edge an attached image is sent at: plenty for a model to read a screenshot, quick to upload from a phone. */
const IMAGE_MAX_EDGE = 2560;
const FILE_MAX_BYTES = 20 * 1024 * 1024;

export interface Attached {
  name: string;
  mime: string;
  size: number;
  /** Uploaded: the server's id for it. */
  file_id?: string;
  /** Not uploaded (the upload failed): sent inline instead. */
  data_base64?: string;
  /** A local preview of an image (object URL). */
  preview?: string;
}

const toBase64 = (b: Blob) =>
  new Promise<string>((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).replace(/^data:[^,]*,/, ""));
    r.onerror = () => reject(r.error ?? new Error("read failed"));
    r.readAsDataURL(b);
  });

/** An image the model can read: decoded, scaled to IMAGE_MAX_EDGE, and re-encoded when the browser can (HEIC and other formats become JPEG). */
async function prepareImage(f: File): Promise<Blob> {
  const native = ["image/png", "image/jpeg", "image/gif", "image/webp"].includes(f.type);
  let bmp: ImageBitmap;
  try {
    bmp = await createImageBitmap(f);
  } catch {
    return f; // can't decode here: send as is
  }
  const scale = Math.min(1, IMAGE_MAX_EDGE / Math.max(bmp.width, bmp.height));
  if (scale === 1 && native) {
    bmp.close();
    return f;
  }
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(bmp.width * scale));
  canvas.height = Math.max(1, Math.round(bmp.height * scale));
  canvas.getContext("2d")?.drawImage(bmp, 0, 0, canvas.width, canvas.height);
  bmp.close();
  const type = f.type === "image/png" || f.type === "image/webp" ? f.type : "image/jpeg";
  const blob = await new Promise<Blob | null>((r) => canvas.toBlob(r, type, 0.9));
  return blob ?? f;
}

const stamp = () => new Date().toTimeString().slice(0, 8).replace(/:/g, "");

/** What a send carries for one attached file. */
const asAttachment = (f: Attached): Attachment => (f.file_id ? { name: f.name, mime: f.mime, file_id: f.file_id } : { name: f.name, mime: f.mime, data_base64: f.data_base64 ?? "" });

/** Previews of files attached in this page's lifetime, by upload id (a queued message's chips tap through to them). */
const previews = new Map<string, string>();

/** The local preview kept for an uploaded attachment (queue/held rows show the image full size). */
export const attachedPreview = (fileId: string) => previews.get(fileId);

/** Files and screenshots to send with a message: pasted, dropped anywhere on the page, or picked (Photos, Camera or Files on a phone). */
export function useAttachments(accepts: "any" | "images" | "none") {
  const enabled = accepts !== "none";
  const [files, setFiles] = React.useState<Attached[]>([]);
  const [pending, setPending] = React.useState(0);
  const [failed, setFailed] = React.useState<string | null>(null);
  const add = React.useCallback(
    async (list: FileList | File[]): Promise<Attachment[]> => {
      if (accepts === "none") return [];
      const picked = [...list].filter((f) => accepts === "any" || f.type.startsWith("image/"));
      setPending((n) => n + picked.length);
      setFailed(null);
      const added: Attached[] = [];
      // Each file uploads as soon as it is attached, so sending is instant.
      await Promise.all(
        picked.map(async (f) => {
          try {
            const blob = f.type.startsWith("image/") ? await prepareImage(f) : f;
            if (blob.size > FILE_MAX_BYTES) {
              setFailed(`${f.name} is larger than ${FILE_MAX_BYTES / 1024 / 1024} MB`);
              return;
            }
            const ext = blob.type === "image/jpeg" ? "jpg" : (blob.type.split("/")[1] ?? "bin");
            const unnamed = !f.name || f.name === "image.png" || f.name === "image";
            const name = unnamed ? `screenshot-${stamp()}.${ext}` : blob !== f && blob.type !== f.type ? f.name.replace(/\.[^.]+$/, `.${ext}`) : f.name;
            const mime = blob.type || "application/octet-stream";
            const preview = mime.startsWith("image/") ? URL.createObjectURL(blob) : undefined;
            let att: Attached;
            try {
              att = { name, mime, size: blob.size, preview, file_id: await harnessApi.upload(blob, name, mime) };
            } catch {
              // Older servers without uploads: small files can still go inline.
              if (blob.size > 8 * 1024 * 1024) throw new Error(`Couldn't upload ${name}`);
              att = { name, mime, size: blob.size, preview, data_base64: await toBase64(blob) };
            }
            if (att.preview && att.file_id) previews.set(att.file_id, att.preview);
            added.push(att);
            setFiles((cur) => [...cur, att]);
          } catch (e) {
            setFailed(e instanceof Error ? e.message : String(e));
          } finally {
            setPending((n) => n - 1);
          }
        }),
      );
      // What this batch turned into, for a caller that merges it straight into a queued message.
      return added.map(asAttachment);
    },
    [accepts],
  );
  const remove = (i: number) =>
    setFiles((cur) => {
      const f = cur[i];
      if (f?.preview) URL.revokeObjectURL(f.preview);
      return cur.filter((_, j) => j !== i);
    });
  const clear = React.useCallback(() => {
    setFiles((cur) => {
      cur.forEach((f) => f.preview && URL.revokeObjectURL(f.preview));
      return [];
    });
    setFailed(null);
  }, []);
  /** What a send carries. */
  const payload = (): Attachment[] => files.map(asAttachment);
  // An attached image, full size, before it's sent (as a phone shows it): its name and size, Remove/Keep.
  const [viewing, setViewing] = React.useState<Attached | null>(null);
  const chips =
    files.length || pending || failed ? (
      <>
        <div className="hx-attach">
          {files.map((f, i) => (
            <span key={i} className={cn("hx-attach-chip", f.preview && "hx-attach-view")} onClick={() => f.preview && setViewing(f)} role={f.preview ? "button" : undefined}>
              {f.preview ? <img src={f.preview} alt="" /> : null}
              <span className="hx-trunc">{f.name}</span>
              <button
                type="button"
                aria-label={`Remove ${f.name}`}
                onClick={(e) => {
                  e.stopPropagation();
                  remove(i);
                }}
              >
                <X size={11} />
              </button>
            </span>
          ))}
          {pending > 0 && <span className="hx-attach-chip hx-dim">Uploading {pending}…</span>}
          {failed && <span className="hx-attach-chip hx-bad">{failed}</span>}
        </div>
        {viewing?.preview && (
          <div className="hx-cover" role="dialog" aria-modal="true" aria-label={viewing.name} onClick={() => setViewing(null)}>
            <div className="hx-cover-card hx-viewer" onClick={(e) => e.stopPropagation()}>
              <div className="hx-viewer-head">
                <button type="button" className="hx-icon" aria-label="Close" onClick={() => setViewing(null)}>
                  <X size={14} />
                </button>
                <span className="hx-viewer-name hx-trunc">{viewing.name}</span>
                <span className="hx-dim">{formatBytes(viewing.size, 0)}</span>
                <a className="hx-icon" href={viewing.preview} download={viewing.name} aria-label="Download" onClick={(e) => e.stopPropagation()}>
                  <ArrowDownToLine size={14} />
                </a>
              </div>
              <div className="hx-viewer-body">
                <img src={viewing.preview} alt={viewing.name} />
              </div>
              <div className="hx-viewer-foot">
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    const i = files.indexOf(viewing);
                    if (i >= 0) remove(i);
                    setViewing(null);
                  }}
                >
                  Remove
                </Button>
                <Button size="sm" variant="secondary" onClick={() => setViewing(null)}>
                  Keep attached
                </Button>
              </div>
            </div>
          </div>
        )}
      </>
    ) : null;
  return { enabled, accepts, files, pending, add, clear, chips, payload };
}

export type Attachments = ReturnType<typeof useAttachments>;
