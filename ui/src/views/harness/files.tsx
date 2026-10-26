/**
 * Files a conversation's messages mention, shown in it (docs/agents.md, "Files in messages"). A
 * picture the message shows (`![…](/path.png)`) appears in place at its own size from the start,
 * so nothing moves as it loads; anything else, or a picture it only names, is a chip that opens in
 * place: the picture, a player for a recording or a sound, a document's text (as written: it sits
 * in a paragraph of the message, where only words go). What the
 * server kept shows as it was when the message was done; what it didn't is read from the agent's
 * machine when it's opened.
 */
import * as React from "react";
import { ExternalLink, File as FileIcon, FileText, Film, Image as ImageIcon, Music } from "lucide-react";
import type { KeptFile } from "@/api/harness";
import { formatBytes } from "@/lib/format";
import { apiPath } from "@/lib/peer-base";
import "./files.css";

/** The conversation whose messages are being drawn: its id, its folder, what's kept of its files. */
export interface ConversationFiles {
  session: string;
  cwd: string;
  files: KeptFile[];
}
export const FilesContext = React.createContext<ConversationFiles | null>(null);
/** The message (its `seq`) whose words are being drawn: a file shows as it was then. */
export const MessageSeq = React.createContext<number | undefined>(undefined);

/** A path as the session sees it: a relative one is in its folder, dots resolved (as the server's
 *  `crate::harness::files::resolve`). */
export function resolvePath(path: string, cwd: string): string | null {
  const full = path.startsWith("/") || path.startsWith("~/") ? path : cwd ? `${cwd.replace(/\/+$/, "")}/${path}` : null;
  if (!full) return null;
  const home = full.startsWith("~/");
  const out: string[] = [];
  for (const part of full.slice(home ? 2 : 1).split("/")) {
    if (part === "" || part === ".") continue;
    if (part === "..") out.pop();
    else out.push(part);
  }
  return (home ? "~/" : "/") + out.join("/");
}

/** The file as it was when message `seq` was done: the last kept for that path by then. */
export function keptFor(files: KeptFile[], path: string, seq: number | undefined): KeptFile | undefined {
  let best: KeptFile | undefined;
  for (const f of files) {
    if (f.path === path && (seq == null || f.seq <= seq) && (!best || f.seq >= best.seq)) best = f;
  }
  return best;
}

/** Where a viewer gets it: the kept copy (for good), else the agent's machine, now. */
export function fileUrl(session: string, path: string, kept?: KeptFile): string {
  const id = encodeURIComponent(session);
  return apiPath(kept?.sha ? `/api/harness/sessions/${id}/files/${kept.sha}` : `/api/harness/sessions/${id}/file?path=${encodeURIComponent(path)}`);
}

type Kind = "image" | "video" | "audio" | "pdf" | "text" | "file";

const BY_EXT: Record<string, Kind> = {
  png: "image",
  jpg: "image",
  jpeg: "image",
  gif: "image",
  webp: "image",
  avif: "image",
  heic: "image",
  svg: "image",
  bmp: "image",
  mp4: "video",
  webm: "video",
  mov: "video",
  mkv: "video",
  mp3: "audio",
  wav: "audio",
  ogg: "audio",
  oga: "audio",
  flac: "audio",
  m4a: "audio",
  aac: "audio",
  pdf: "pdf",
  md: "text",
  log: "text",
  csv: "text",
  txt: "text",
  json: "text",
};

export function kindOf(path: string, mime?: string): Kind {
  if (mime) {
    if (mime.startsWith("image/")) return "image";
    if (mime.startsWith("video/")) return "video";
    if (mime.startsWith("audio/")) return "audio";
    if (mime === "application/pdf") return "pdf";
    if (mime.startsWith("text/") || mime === "application/json") return "text";
  }
  return BY_EXT[path.split(".").pop()?.toLowerCase() ?? ""] ?? "file";
}

const ICON: Record<Kind, typeof FileIcon> = { image: ImageIcon, video: Film, audio: Music, pdf: FileText, text: FileText, file: FileIcon };
const baseName = (path: string) => path.split("/").pop() || path;

/** What a piece of a message says, as plain words. */
function wordsOf(n: React.ReactNode): string {
  if (typeof n === "string" || typeof n === "number") return String(n);
  if (Array.isArray(n)) return n.map(wordsOf).join("");
  if (React.isValidElement<{ children?: React.ReactNode }>(n)) return wordsOf(n.props.children);
  return "";
}

/** The words a link gave it, unless they're only its path (then its name says it). */
function labelOf(children: React.ReactNode, path: string, full: string): React.ReactNode {
  const words = wordsOf(children).trim();
  return words === "" || words === path || words === full ? baseName(full) : children;
}

/** A known size as the box's own (files.css takes the box from it before a byte comes). */
const boxOf = (w?: number, h?: number) => (w && h ? ({ "--w": w, "--h": h } as React.CSSProperties) : undefined);

/**
 * A file a message mentions, where the message mentions it: `image` when the message shows it
 * (`![…](…)`), else a link or a name in its words.
 */
export function MessageFile({ path, image, children }: { path: string; image: boolean; children?: React.ReactNode }) {
  const ctx = React.useContext(FilesContext);
  const seq = React.useContext(MessageSeq);
  const full = ctx ? resolvePath(path, ctx.cwd) : null;
  if (!ctx || !full) return <span className="mf-plain">{children ?? path}</span>;
  const kept = keptFor(ctx.files, full, seq);
  const kind = kindOf(full, kept?.meta?.mime);
  const url = fileUrl(ctx.session, full, kept);
  const sized = kept?.meta?.width && kept.meta.height ? { width: kept.meta.width, height: kept.meta.height } : null;
  // A picture the message shows, at its own size from the start; one whose size isn't known yet
  // (not kept) opens from a chip instead, so nothing jumps as it loads.
  if (image && kind === "image" && sized && !kept?.error) {
    return <Picture url={url} alt={typeof children === "string" && children ? children : baseName(full)} size={sized} kept={kept} path={full} />;
  }
  return <FileChip full={full} kind={kind} url={url} kept={kept} label={labelOf(children, path, full)} />;
}

/** A picture in place: bounded to the column and a screen's height, whole at a tap. */
function Picture({ url, alt, size, kept, path }: { url: string; alt: string; size: { width: number; height: number }; kept?: KeptFile; path: string }) {
  const [whole, setWhole] = React.useState(false);
  return (
    <span className="mf-picture" data-whole={whole ? "" : undefined} style={boxOf(size.width, size.height)}>
      <button type="button" className="mf-picture-tap" aria-expanded={whole} aria-label={whole ? `${alt}, shown whole: show it smaller` : `${alt}: show it whole`} onClick={() => setWhole(!whole)}>
        <img src={url} alt={alt} width={size.width} height={size.height} loading="lazy" decoding="async" />
      </button>
      <span className="mf-caption">
        <span className="mf-caption-name" title={path}>
          {baseName(path)}
        </span>
        <span>
          {size.width}×{size.height}
        </span>
        {kept?.meta && <span>{formatBytes(kept.meta.size)}</span>}
        <a className="mf-open" href={url} target="_blank" rel="noopener noreferrer" aria-label={`Open ${baseName(path)} on its own`}>
          <ExternalLink size={11} aria-hidden="true" />
        </a>
      </span>
    </span>
  );
}

/** A file as a chip in the words: its kind, its name, its size; it opens in place. */
function FileChip({ full, kind, url, kept, label }: { full: string; kind: Kind; url: string; kept?: KeptFile; label: React.ReactNode }) {
  const [open, setOpen] = React.useState(false);
  const Icon = ICON[kind];
  // A PDF opens in the browser's own reader, whatever the device.
  if (kind === "pdf" && !kept?.error) {
    return (
      <a className="mf-chip" href={url} target="_blank" rel="noopener noreferrer" title={full}>
        <Icon size={12} aria-hidden="true" />
        <span className="mf-name">{label}</span>
        {kept?.meta && <span className="mf-size">{formatBytes(kept.meta.size)}</span>}
      </a>
    );
  }
  return (
    <span className="mf" data-open={open ? "" : undefined}>
      <button
        type="button"
        className="mf-chip"
        title={kept?.error ?? full}
        aria-expanded={open}
        aria-label={kept?.error ? `${baseName(full)}: ${kept.error}` : undefined}
        disabled={Boolean(kept?.error)}
        onClick={() => setOpen(!open)}
      >
        <Icon size={12} aria-hidden="true" />
        <span className="mf-name">{label}</span>
        {kept?.meta && <span className="mf-size">{formatBytes(kept.meta.size)}</span>}
        {kept?.error && <span className="mf-size">unavailable</span>}
      </button>
      {open && <Opened kind={kind} url={url} kept={kept} name={baseName(full)} />}
    </span>
  );
}

/** What a chip opens, in place under it. */
function Opened({ kind, url, kept, name }: { kind: Kind; url: string; kept?: KeptFile; name: string }) {
  const w = kept?.meta?.width;
  const h = kept?.meta?.height;
  switch (kind) {
    case "image":
      return (
        <span className="mf-opened">
          <img src={url} alt={name} width={w} height={h} style={boxOf(w, h)} decoding="async" />
        </span>
      );
    case "video":
      return (
        <span className="mf-opened">
          <video src={url} controls playsInline preload="metadata" width={w} height={h} style={boxOf(w, h) ?? { aspectRatio: "16 / 9" }} />
        </span>
      );
    case "audio":
      return (
        <span className="mf-opened">
          <audio src={url} controls preload="metadata" />
        </span>
      );
    case "text":
      return <TextFile url={url} name={name} />;
    default:
      return (
        <span className="mf-opened mf-note">
          <a href={url} target="_blank" rel="noopener noreferrer">
            Open {name}
          </a>
        </span>
      );
  }
}

/** The most of a text file shown in place; the whole of it opens on its own. */
const MOST_TEXT = 256 * 1024;

type Text = { state: "loading" } | { state: "ok"; text: string; more: boolean } | { state: "failed"; why: string };

/** A text file's words, read once as it opens (no effect: the read starts as its box attaches). */
function TextFile({ url, name }: { url: string; name: string }) {
  const [text, setText] = React.useState<Text>({ state: "loading" });
  const reading = React.useCallback(
    (el: HTMLSpanElement | null) => {
      if (!el) return;
      const ctl = new AbortController();
      fetch(url, { credentials: "same-origin", signal: ctl.signal, headers: { Range: `bytes=0-${MOST_TEXT - 1}` } })
        .then(async (r) => {
          if (!r.ok) throw new Error((await r.json().catch(() => null))?.error ?? `${r.status}`);
          const body = await r.text();
          const total = Number(r.headers.get("Content-Range")?.split("/")[1] ?? body.length);
          setText({ state: "ok", text: body, more: total > MOST_TEXT });
        })
        .catch((e: unknown) => {
          if (!ctl.signal.aborted) setText({ state: "failed", why: e instanceof Error ? e.message : String(e) });
        });
      return () => ctl.abort();
    },
    [url],
  );
  return (
    <span ref={reading} className="mf-opened mf-text">
      {text.state === "loading" && <span className="mf-note">Reading {name}…</span>}
      {text.state === "failed" && <span className="mf-note">{text.why}</span>}
      {text.state === "ok" && <span className="mf-pre">{text.text}</span>}
      {text.state === "ok" && text.more && (
        <a className="mf-note" href={url} target="_blank" rel="noopener noreferrer">
          The rest of {name}
        </a>
      )}
    </span>
  );
}
