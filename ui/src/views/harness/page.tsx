/**
 * Pages agents show in the conversation (docs/previews.md, "Pages"): an agent's `show_page` call,
 * drawn as the page itself, live, at the conversation's width. Its height is known before it
 * loads (measured in the agent machine's browser at each width a viewer might have), so nothing
 * moves as it comes in. It runs in a sandbox of its own (`/v/<id>/`, an opaque origin: no kiln
 * cookies or storage, nothing of this page), and speaks to this frame through kiln's bridge in it
 * (MCP Apps' messages, and kiln's): its height as it changes, links to open, a choice to say back
 * (it lands in the composer, for the person to send), what was done in it to keep. What moves in it
 * pauses while it's out of view; far out of view it's let go, and what was done in it comes back
 * with it.
 */
import * as React from "react";
import { ExternalLink, RotateCw } from "lucide-react";
import type { ToolCall } from "@/api/harness";
import { useAuth } from "@/lib/auth";
import { heightAt, pageOf, type ShownPage } from "./page-of";
import { FilesContext } from "./files";
import { Beam, PrismLabel } from "./signals";
import { useDrafts } from "./state";
import "./page.css";

/** A page that fills its window (its own height is the window's): a frame of a pleasant shape. */
const FILL_UNDER = 48;
const MOST = 4000;
const fillHeight = (w: number) => Math.min(640, Math.max(240, Math.round(w * 0.625)));

/** Where pages are served: the server's own address, or its pages address when it has one. */
function usePagesOrigin(): string {
  return useAuth((s) => (s.kind === "authed" ? (s.me?.pages_url ?? "") : "")).replace(/\/+$/, "");
}

const STATE_KEY = (id: string) => `kiln.page.${id}`;
const MOST_STATE = 64 * 1024;

function savedState(id: string): unknown {
  try {
    const s = localStorage.getItem(STATE_KEY(id));
    return s == null ? undefined : (JSON.parse(s) as unknown);
  } catch {
    return undefined;
  }
}

/** What a page says back, as a line of the person's next message. */
function replyLine(params: Record<string, unknown> | undefined): string | null {
  if (!params) return null;
  // MCP Apps' ui/message: {role, content: {type: "text", text} | [{...}]}.
  const content = params.content;
  const parts = Array.isArray(content) ? content : content ? [content] : [];
  const text = parts
    .map((p) => (p && typeof p === "object" && (p as { type?: unknown }).type === "text" ? String((p as { text?: unknown }).text ?? "") : ""))
    .filter(Boolean)
    .join("\n");
  if (text) return text.slice(0, 4000);
  // kiln.reply(value, label).
  const label = typeof params.label === "string" ? params.label.trim() : "";
  const value = params.value;
  const said = typeof value === "string" ? value : value === undefined ? "" : JSON.stringify(value);
  if (!label && !said) return null;
  if (!label || label === said) return (label || said).slice(0, 4000);
  return `${label} (${said.slice(0, 1000)})`.slice(0, 4000);
}

type Rpc = { jsonrpc?: string; id?: string | number; method?: string; params?: Record<string, unknown> };

/** The page itself, in its frame, at its own height. */
function Frame({ page, ev }: { page: ShownPage; ev: string }) {
  const origin = usePagesOrigin();
  const agent = React.useContext(FilesContext)?.session;
  const src = `${origin}${page.path}`;
  const [width, setWidth] = React.useState(0);
  const [reported, setReported] = React.useState<number | null>(null);
  const [near, setNear] = React.useState(false);
  const [gen, setGen] = React.useState(0);
  const [said, setSaid] = React.useState(false);
  const frame = React.useRef<HTMLIFrameElement | null>(null);
  const visible = React.useRef(false);
  const ready = React.useRef(false);
  /** The document what was done in the page was last handed back to (the bridge's mark). */
  const restoredTo = React.useRef<string | null>(null);
  const live = React.useRef({ page, agent, width });
  live.current = { page, agent, width };

  const measured = heightAt(page.heights, width);
  const height = Math.min(MOST, reported ?? (measured != null && measured >= FILL_UNDER ? measured : fillHeight(width || 390)));

  const tell = React.useCallback((method: string, params: unknown) => {
    frame.current?.contentWindow?.postMessage({ jsonrpc: "2.0", method, params }, "*");
  }, []);

  // The box: its width, whether it's in view (what moves in the page pauses when it isn't), whether
  // it's near (far away the page is let go), and what the page says.
  const box = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      const root = el.closest<HTMLElement>(".hx-timeline");
      const ro = new ResizeObserver(([e]) => {
        if (e) setWidth(Math.round(e.contentRect.width));
      });
      ro.observe(el);
      const inView = new IntersectionObserver(
        ([e]) => {
          visible.current = Boolean(e?.isIntersecting);
          if (ready.current) tell("kiln/visibility", { visible: visible.current });
        },
        { root, threshold: 0 },
      );
      inView.observe(el);
      const around = new IntersectionObserver(
        ([e]) => {
          const isNear = Boolean(e?.isIntersecting);
          if (!isNear) ready.current = false;
          setNear(isNear);
        },
        { root, rootMargin: "150% 0px" },
      );
      around.observe(el);
      let quiet: ReturnType<typeof setTimeout> | undefined;
      const onMessage = (e: MessageEvent) => {
        const f = frame.current;
        // Only its own frame, still showing its page (an opaque origin: a page that went
        // elsewhere has another, and is told nothing of this one).
        if (!f || e.source !== f.contentWindow || e.origin !== "null") return;
        const m = e.data as Rpc | null;
        if (!m || typeof m !== "object" || typeof m.method !== "string") return;
        const { page: p, agent: a, width: w } = live.current;
        const reply = (result: unknown) => m.id != null && f.contentWindow?.postMessage({ jsonrpc: "2.0", id: m.id, result }, "*");
        const refuse = (code: number, message: string) => m.id != null && f.contentWindow?.postMessage({ jsonrpc: "2.0", id: m.id, error: { code, message } }, "*");
        switch (m.method) {
          case "ui/initialize":
            reply({
              protocolVersion: "2026-01-26",
              hostInfo: { name: "kiln", version: "1" },
              hostCapabilities: { openLinks: {}, message: { text: {} } },
              hostContext: {
                theme: "dark",
                displayMode: "inline",
                availableDisplayModes: ["inline"],
                containerDimensions: { width: w, maxHeight: MOST },
                platform: window.matchMedia("(pointer: coarse)").matches ? "mobile" : "web",
                deviceCapabilities: { touch: window.matchMedia("(pointer: coarse)").matches, hover: window.matchMedia("(hover: hover)").matches },
                locale: navigator.language,
                timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
              },
            });
            break;
          case "ui/notifications/initialized": {
            ready.current = true;
            tell("kiln/visibility", { visible: visible.current });
            const doc = m.params?.kiln;
            if (typeof doc === "string" && doc !== restoredTo.current) {
              restoredTo.current = doc;
              const state = savedState(p.id);
              if (state !== undefined) tell("kiln/restore", { state });
            }
            break;
          }
          case "ui/notifications/size-changed": {
            const h = Number(m.params?.height);
            if (Number.isFinite(h) && h > 0) setReported(Math.ceil(h));
            break;
          }
          case "ui/open-link": {
            const url = String(m.params?.url ?? "");
            if (!/^https?:\/\//i.test(url)) {
              refuse(-32602, "only web addresses open");
              break;
            }
            window.open(url, "_blank", "noopener,noreferrer");
            reply({});
            break;
          }
          case "ui/message":
          case "kiln/reply": {
            const line = replyLine(m.params);
            if (!line || !a) {
              refuse(-32602, "nothing to say");
              break;
            }
            const drafts = useDrafts.getState();
            const was = drafts.drafts[a] ?? "";
            drafts.setDraft(a, was.trim() ? `${was.replace(/\s+$/, "")}\n${line}` : line);
            setSaid(true);
            clearTimeout(quiet);
            quiet = setTimeout(() => setSaid(false), 4000);
            reply({});
            break;
          }
          case "kiln/state": {
            try {
              const s = JSON.stringify(m.params?.state ?? null);
              if (s.length <= MOST_STATE) localStorage.setItem(STATE_KEY(p.id), s);
            } catch {
              // Too big, or storage is full: it's kept no longer than the page is shown.
            }
            break;
          }
          default:
            refuse(-32601, `kiln doesn't do ${m.method}`);
        }
      };
      window.addEventListener("message", onMessage);
      return () => {
        ro.disconnect();
        inView.disconnect();
        around.disconnect();
        clearTimeout(quiet);
        window.removeEventListener("message", onMessage);
      };
    },
    [tell],
  );

  return (
    <div className="pg" data-ev={ev} data-ev-page={page.id}>
      <div className="pg-head">
        <PrismLabel className="pg-title">{page.title}</PrismLabel>
        <span className="pg-said" role="status">
          {said ? "Added to your message" : ""}
        </span>
        <span className="pg-actions">
          <button
            type="button"
            className="pg-action"
            onClick={() => {
              ready.current = false;
              setGen((g) => g + 1);
            }}
          >
            <RotateCw size={12} aria-hidden="true" />
            Reload
          </button>
          <a className="pg-action" href={src} target="_blank" rel="noopener noreferrer">
            <ExternalLink size={12} aria-hidden="true" />
            Open
          </a>
        </span>
      </div>
      <div className="pg-box" ref={box}>
        {near ? (
          <iframe
            key={gen}
            ref={frame}
            className="pg-frame"
            src={src}
            title={page.title}
            sandbox="allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-downloads allow-pointer-lock"
            allow="fullscreen; clipboard-write; autoplay"
            referrerPolicy="no-referrer"
            style={{ height }}
          />
        ) : (
          <div className="pg-frame pg-away" style={{ height }} aria-hidden="true" />
        )}
      </div>
    </div>
  );
}

/** An agent's `show_page` call: the page, or (while it's being shown) where it'll be. */
export function PageView({ call }: { call: ToolCall & { id: string } }) {
  const page = pageOf(call);
  if (page) return <Frame page={page} ev={call.id} />;
  const running = call.status === "running" || call.status === "pending";
  return (
    <div className="pg pg-coming" data-ev={call.id}>
      <div className="pg-head">
        <Beam status={running ? "running" : "error"} className="pg-beam" />
        <PrismLabel live={running} className="pg-title">
          {call.title || "A page"}
        </PrismLabel>
        <span className="pg-said">{running ? "Keeping it and its files" : ""}</span>
      </div>
    </div>
  );
}
