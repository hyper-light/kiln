/**
 * Live pages agents show in the conversation (docs/previews.md, "Streams"): an agent's
 * `show_live` call, drawn as the page itself running on the agent's machine (its dev server, its
 * files, its network), streamed here. Its picture comes as video (WebCodecs, the size of this frame
 * at this screen's density), its sound as Opus once you turn it on; what you do in it goes back:
 * taps and the mouse, the wheel, keys by meaning, text from a phone's keyboard, a paste. Out of
 * view it's let go (the stream pauses, nothing is sent) and its last picture stays.
 */
import * as React from "react";
import { ArrowLeft, ArrowRight, Keyboard, RotateCw, Volume2, VolumeX, X } from "lucide-react";
import type { ToolCall } from "@/api/harness";
import { Beam, PrismLabel } from "./signals";
import { chunkOf, liveOf, modifiers, type LiveStream } from "./live-of";
import "./page.css";
import "./live.css";

type Status = "connecting" | "live" | "still" | "ended" | "unsupported";

const macViewer = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
const touchViewer = typeof window !== "undefined" && window.matchMedia?.("(pointer: coarse)").matches;
const decodes = typeof window !== "undefined" && "VideoDecoder" in window;
const plays = typeof window !== "undefined" && "AudioDecoder" in window;

/** One viewing of a stream: its socket, its decoders, what it draws. */
class Viewing {
  private ws: WebSocket;
  private video: VideoDecoder | null = null;
  private audio: AudioDecoder | null = null;
  private ctx: AudioContext | null = null;
  private playAt = 0;
  private needKey = true;
  private parts = new Map<string, Uint8Array[]>();
  private frames = 0;
  private counted = performance.now();
  closed = false;

  constructor(
    url: string,
    private canvas: HTMLCanvasElement,
    private on: { open: () => void; status: (s: Status, why?: string) => void; event: (e: Record<string, unknown>) => void; fps: (n: number) => void },
  ) {
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => this.on.open();
    this.ws.onmessage = (m) => (typeof m.data === "string" ? this.event(m.data) : this.chunk(m.data as ArrayBuffer));
    this.ws.onclose = (e) => {
      if (!this.closed) this.on.status("ended", e.code === 1000 ? "" : "The connection to it was lost.");
      this.closed = true;
    };
  }

  send(m: unknown) {
    if (this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(m));
  }

  sound(on: boolean) {
    if (!plays) return;
    if (on && !this.ctx) {
      this.ctx = new AudioContext({ sampleRate: 48000 });
      this.playAt = 0;
    } else if (!on && this.ctx) {
      void this.ctx.close();
      this.ctx = null;
    }
  }

  close() {
    this.closed = true;
    this.ws.close(1000);
    for (const d of [this.video, this.audio]) if (d && d.state !== "closed") d.close();
    void this.ctx?.close();
  }

  private event(text: string) {
    try {
      this.on.event(JSON.parse(text) as Record<string, unknown>);
    } catch {
      // Not one kiln sends.
    }
  }

  private chunk(buf: ArrayBuffer) {
    const c = chunkOf(buf);
    if (!c) return;
    let data = c.data;
    if (c.parts > 1) {
      const key = `${c.kind}${c.ts}`;
      const got = this.parts.get(key) ?? [];
      got[c.part] = data;
      this.parts.set(key, got);
      if (got.filter(Boolean).length < c.parts) return;
      this.parts.delete(key);
      const n = got.reduce((s, x) => s + x.length, 0);
      data = new Uint8Array(n);
      let o = 0;
      for (const x of got) {
        data.set(x, o);
        o += x.length;
      }
    }
    if (c.kind === "c") {
      const cfg = JSON.parse(new TextDecoder().decode(data)) as { codec: string; codedWidth?: number; codedHeight?: number; sampleRate?: number; numberOfChannels?: number; description?: string };
      if (c.flags === "a") this.configureAudio(cfg);
      else this.configureVideo(cfg);
      return;
    }
    if (c.kind === "a") {
      if (this.ctx && this.audio?.state === "configured") this.audio.decode(new EncodedAudioChunk({ type: "key", timestamp: c.ts, data }));
      return;
    }
    if (!this.video || this.video.state !== "configured") return;
    const key = c.flags === "k";
    if (this.needKey && !key) return;
    this.needKey = false;
    this.video.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: c.ts, data }));
  }

  private configureVideo(cfg: { codec: string; codedWidth?: number; codedHeight?: number; description?: string }) {
    if (this.video && this.video.state !== "closed") this.video.close();
    this.video = new VideoDecoder({
      output: (f) => this.draw(f),
      error: () => {
        this.needKey = true;
        this.send({ control: { keyframe: true } });
      },
    });
    const description = cfg.description ? Uint8Array.from(atob(cfg.description), (ch) => ch.charCodeAt(0)) : undefined;
    this.video.configure({ codec: cfg.codec, codedWidth: cfg.codedWidth, codedHeight: cfg.codedHeight, optimizeForLatency: true, ...(description ? { description } : {}) });
    this.needKey = true;
  }

  private configureAudio(cfg: { codec: string; sampleRate?: number; numberOfChannels?: number }) {
    if (!plays) return;
    if (this.audio && this.audio.state !== "closed") this.audio.close();
    this.audio = new AudioDecoder({ output: (d) => this.play(d), error: () => {} });
    this.audio.configure({ codec: cfg.codec, sampleRate: cfg.sampleRate ?? 48000, numberOfChannels: cfg.numberOfChannels ?? 2 });
  }

  private draw(f: VideoFrame) {
    const c = this.canvas;
    if (c.width !== f.displayWidth || c.height !== f.displayHeight) {
      c.width = f.displayWidth;
      c.height = f.displayHeight;
    }
    c.getContext("2d")?.drawImage(f, 0, 0, c.width, c.height);
    f.close();
    this.frames++;
    const now = performance.now();
    if (now - this.counted >= 1000) {
      this.on.fps(Math.round((this.frames * 1000) / (now - this.counted)));
      this.frames = 0;
      this.counted = now;
    }
    this.on.status("live");
  }

  /** Sound into a small buffer ahead of now: steady, and late by no more than that. */
  private play(d: AudioData) {
    const ctx = this.ctx;
    if (!ctx) {
      d.close();
      return;
    }
    const buf = ctx.createBuffer(d.numberOfChannels, d.numberOfFrames, d.sampleRate);
    for (let ch = 0; ch < d.numberOfChannels; ch++) d.copyTo(buf.getChannelData(ch), { planeIndex: ch, format: "f32-planar" });
    d.close();
    const src = ctx.createBufferSource();
    src.buffer = buf;
    src.connect(ctx.destination);
    const now = ctx.currentTime;
    if (this.playAt < now + 0.02 || this.playAt > now + 0.4) this.playAt = now + 0.06;
    src.start(this.playAt);
    this.playAt += buf.duration;
  }
}

/** What the agent did, in a few words ("the agent clicked, typed"). */
function agentWords(did: Record<string, unknown>[]): string {
  const verbs: Record<string, string> = { click: "clicked", tap: "tapped", type: "typed", key: "pressed a key", scroll: "scrolled", go: "went to a page", back: "went back", forward: "went forward", reload: "reloaded" };
  const said = [...new Set(did.flatMap((d) => Object.keys(d).map((k) => verbs[k]).filter(Boolean)))];
  return said.length ? `the agent ${said.join(", ")}` : "";
}

/** The live page, its controls above it (`onClose`: it may be closed from here). */
export function Live({ stream: first, ev, onClose }: { stream: LiveStream; ev: string; onClose?: () => void }) {
  const [stream, setStream] = React.useState(first);
  const [status, setStatus] = React.useState<Status>(decodes ? "connecting" : "unsupported");
  const [why, setWhy] = React.useState("");
  const [address, setAddress] = React.useState(first.url);
  const [editing, setEditing] = React.useState<string | null>(null);
  const [title, setTitle] = React.useState(first.title);
  const [fps, setFps] = React.useState(0);
  const [sound, setSound] = React.useState(false);
  const [macPage, setMacPage] = React.useState(false);
  /** What the agent just did in it, said for a moment, and where it touched. */
  const [agentDid, setAgentDid] = React.useState("");
  const [marks, setMarks] = React.useState<{ n: number; x: number; y: number }[]>([]);
  const quiet = React.useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const canvas = React.useRef<HTMLCanvasElement | null>(null);
  const typing = React.useRef<HTMLTextAreaElement | null>(null);
  const viewing = React.useRef<Viewing | null>(null);
  const size = React.useRef({ width: 0, height: 0 });
  const live = React.useRef({ stream, sound, macPage });
  live.current = { stream, sound, macPage };
  const moved = React.useRef<Record<string, unknown> | null>(null);

  const send = React.useCallback((m: unknown) => viewing.current?.send(m), []);
  const input = React.useCallback((events: unknown[]) => send({ input: events }), [send]);
  const resize = React.useCallback(() => {
    const { width, height } = size.current;
    if (width > 0 && height > 0) send({ control: { resize: { width, height, scale: Math.min(2, Math.max(1, window.devicePixelRatio || 1)), mobile: touchViewer } } });
  }, [send]);

  const connect = React.useCallback(() => {
    const c = canvas.current;
    if (!c || !decodes || viewing.current) return;
    const s = live.current.stream;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const v = new Viewing(`${proto}//${location.host}/api/harness/sessions/${encodeURIComponent(s.session)}/streams/${s.id}/ws`, c, {
      // Its size first, before anything done in it: the page lays out at this frame's.
      open: () => {
        resize();
        setTimeout(() => setStatus((st) => (st === "connecting" ? "still" : st)), 1500);
      },
      status: (st, w) => {
        setStatus(st);
        if (w !== undefined) setWhy(w);
        if (st === "ended" && viewing.current === v) viewing.current = null;
      },
      fps: setFps,
      event: (e) => {
        const opened = e.opened as { platform?: string } | undefined;
        if (opened) setMacPage(opened.platform === "macos");
        const nav = e.navigated as { url?: string } | undefined;
        if (nav?.url) setAddress(nav.url);
        const loaded = e.loaded as { title?: string } | undefined;
        if (loaded?.title) setTitle(loaded.title);
        const closed = e.closed as { why?: string } | undefined;
        if (closed) {
          setStatus("ended");
          setWhy(closed.why ?? "");
        }
        if (e.error) {
          setStatus("ended");
          setWhy(String(e.error));
        }
        const agent = e.agent as { did?: Record<string, unknown>[] } | undefined;
        if (agent?.did?.length) {
          setAgentDid(agentWords(agent.did));
          const points = agent.did.flatMap((d) => {
            const p = (d.click ?? d.tap) as number[] | undefined;
            return Array.isArray(p) && p.length === 2 ? [{ n: Math.random(), x: Number(p[0]), y: Number(p[1]) }] : [];
          });
          if (points.length) setMarks(points);
          clearTimeout(quiet.current);
          quiet.current = setTimeout(() => {
            setAgentDid("");
            setMarks([]);
          }, 3000);
        }
      },
    });
    viewing.current = v;
    v.sound(live.current.sound);
  }, [resize]);

  const disconnect = React.useCallback(() => {
    viewing.current?.close();
    viewing.current = null;
  }, []);

  // The frame: its size is the page's window; in view it's live, out of view let go.
  const box = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      const root = el.closest<HTMLElement>(".hx-timeline");
      let pending: ReturnType<typeof setTimeout> | undefined;
      const ro = new ResizeObserver((entries) => {
        for (const e of entries) {
          if (e.target === root) {
            // As tall as the conversation shows, less its own controls: all of it in reach.
            el.style.setProperty("--lv-h", `${Math.round(e.contentRect.height) - 96}px`);
            continue;
          }
          size.current = { width: Math.round(e.contentRect.width), height: Math.round(e.contentRect.height) };
          clearTimeout(pending);
          pending = setTimeout(resize, 150);
        }
      });
      ro.observe(el);
      if (root) ro.observe(root);
      const io = new IntersectionObserver(
        ([e]) => {
          if (e?.isIntersecting) connect();
          else disconnect();
        },
        { root, rootMargin: "50% 0px" },
      );
      io.observe(el);
      return () => {
        ro.disconnect();
        io.disconnect();
        clearTimeout(pending);
        disconnect();
      };
    },
    [connect, disconnect, resize],
  );

  const at = (e: React.PointerEvent | React.WheelEvent) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return { x: Math.round(e.clientX - r.left), y: Math.round(e.clientY - r.top) };
  };
  const mods = (e: { altKey: boolean; ctrlKey: boolean; metaKey: boolean; shiftKey: boolean; key?: string }) => modifiers(e, macViewer, live.current.macPage);
  const pointer = (k: "down" | "move" | "up" | "cancel") => (e: React.PointerEvent<HTMLCanvasElement>) => {
    const p = { t: "p", k, ...at(e), button: e.button, buttons: e.buttons, pt: e.pointerType, id: e.pointerId, mods: mods(e), clicks: k === "move" ? 0 : Math.max(1, e.detail || 1) };
    if (k === "down") {
      e.currentTarget.setPointerCapture(e.pointerId);
      e.currentTarget.focus({ preventScroll: true });
    }
    if (k === "move") {
      // Moves at most once a frame: the last one.
      const first = !moved.current;
      moved.current = p;
      if (first)
        requestAnimationFrame(() => {
          if (moved.current) input([moved.current]);
          moved.current = null;
        });
      return;
    }
    if (moved.current) {
      input([moved.current, p]);
      moved.current = null;
    } else input([p]);
    if (e.pointerType === "touch") e.preventDefault();
  };
  const key = (k: "down" | "up") => (e: React.KeyboardEvent) => {
    // A paste comes as its text (the page's own clipboard isn't the viewer's).
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "v") return;
    e.preventDefault();
    const text = k === "down" && e.key.length === 1 && !e.ctrlKey && !e.metaKey ? e.key : "";
    input([{ t: "k", k, key: e.key, code: e.code, text, mods: mods(e), rep: e.repeat, loc: e.location }]);
  };

  // A phone's keyboard: a field of its own takes what it types, as text and compositions.
  const typed = (e: React.FormEvent<HTMLTextAreaElement>) => {
    const ev = e.nativeEvent as InputEvent;
    if (ev.isComposing) return;
    const t = e.currentTarget;
    if (ev.inputType === "deleteContentBackward") input([{ t: "k", k: "down", key: "Backspace", code: "Backspace" }, { t: "k", k: "up", key: "Backspace", code: "Backspace" }]);
    else if (ev.inputType === "insertLineBreak") input([{ t: "k", k: "down", key: "Enter", code: "Enter", text: "\r" }, { t: "k", k: "up", key: "Enter", code: "Enter" }]);
    else if (t.value) input([{ t: "x", text: t.value }]);
    t.value = "";
  };

  const go = (url: string) => {
    const u = /^[a-z]+:\/\//i.test(url) ? url : `http://${url}`;
    send({ control: { go: u } });
    setAddress(u);
    setEditing(null);
  };
  const startAgain = async () => {
    const r = await fetch(`/api/harness/sessions/${encodeURIComponent(stream.session)}/streams`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ url: address || stream.url, width: size.current.width || 390, height: size.current.height || 600, profile: stream.profile }) });
    if (!r.ok) {
      setWhy((await r.text()) || "It couldn't start again.");
      return;
    }
    const v = (await r.json()) as { id: string };
    disconnect();
    setStream({ ...stream, id: v.id });
    live.current.stream = { ...stream, id: v.id };
    setStatus("connecting");
    setWhy("");
    connect();
  };

  const word = status === "live" && fps > 0 ? `live · ${fps} fps` : status === "live" || status === "still" ? "live · still" : status === "connecting" ? "connecting" : status === "unsupported" ? "this browser can't show it" : "ended";
  return (
    <div className="pg lv" data-ev={ev} data-ev-live={stream.id} data-status={status}>
      <div className="pg-head">
        <PrismLabel live={status === "live"} className="pg-title">
          {title}
        </PrismLabel>
        <span className="pg-said" role="status">
          {agentDid || word}
        </span>
        <span className="pg-actions">
          {plays && (
            <button
              type="button"
              className="pg-action"
              aria-pressed={sound}
              onClick={() => {
                const on = !sound;
                setSound(on);
                viewing.current?.sound(on);
              }}
            >
              {sound ? <Volume2 size={12} aria-hidden="true" /> : <VolumeX size={12} aria-hidden="true" />}
              Sound
            </button>
          )}
          <button type="button" className="pg-action" onClick={() => typing.current?.focus()}>
            <Keyboard size={12} aria-hidden="true" />
            Type
          </button>
          {onClose && (
            <button type="button" className="pg-action" onClick={onClose}>
              <X size={12} aria-hidden="true" />
              Close
            </button>
          )}
        </span>
      </div>
      <div className="lv-bar">
        <button type="button" className="pg-action" aria-label="Back" title="Back" onClick={() => send({ control: { back: true } })}>
          <ArrowLeft size={12} aria-hidden="true" />
        </button>
        <button type="button" className="pg-action" aria-label="Forward" title="Forward" onClick={() => send({ control: { forward: true } })}>
          <ArrowRight size={12} aria-hidden="true" />
        </button>
        <button type="button" className="pg-action" aria-label="Reload" title="Reload" onClick={() => send({ control: { reload: true } })}>
          <RotateCw size={12} aria-hidden="true" />
        </button>
        <input
          className="hx-input lv-address"
          value={editing ?? address}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          aria-label="Address"
          onChange={(e) => setEditing(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && editing) go(editing.trim());
            if (e.key === "Escape") setEditing(null);
          }}
          onBlur={() => setEditing(null)}
        />
      </div>
      <div className="pg-box lv-box" ref={box}>
        <canvas
          ref={canvas}
          className="lv-canvas"
          tabIndex={0}
          aria-label={`${title}, live`}
          onPointerDown={pointer("down")}
          onPointerMove={pointer("move")}
          onPointerUp={pointer("up")}
          onPointerCancel={pointer("cancel")}
          onWheel={(e) => input([{ t: "w", ...at(e), dx: e.deltaX, dy: e.deltaY, mods: mods(e) }])}
          onKeyDown={key("down")}
          onKeyUp={key("up")}
          onPaste={(e) => {
            const text = e.clipboardData.getData("text");
            if (text) input([{ t: "x", text }]);
            e.preventDefault();
          }}
          onContextMenu={(e) => e.preventDefault()}
        />
        {marks.map((m) => (
          <span key={m.n} className="lv-mark" style={{ left: m.x, top: m.y }} aria-hidden="true" />
        ))}
        <textarea ref={typing} className="lv-typing" aria-label="Type into the page" autoCapitalize="off" autoCorrect="off" onInput={typed} onCompositionUpdate={(e) => input([{ t: "i", text: e.data, start: e.data.length, end: e.data.length }])} onCompositionEnd={(e) => input([{ t: "x", text: e.data }])} />
        {(status === "ended" || status === "unsupported") && (
          <div className="lv-over">
            <p>{status === "unsupported" ? "This browser can't show live pages (it has no WebCodecs)." : why || "This live page ended."}</p>
            {status === "ended" && (
              <button type="button" className="pg-action" onClick={() => void startAgain()}>
                Start it again
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/** An agent's `show_live` call: the live page, or (while it's being opened) where it'll be. */
export function LiveView({ call }: { call: ToolCall & { id: string } }) {
  const s = liveOf(call);
  if (s) return <Live stream={s} ev={call.id} />;
  const running = call.status === "running" || call.status === "pending";
  return (
    <div className="pg pg-coming" data-ev={call.id}>
      <div className="pg-head">
        <Beam status={running ? "running" : "error"} className="pg-beam" />
        <PrismLabel live={running} className="pg-title">
          {call.title || "A live page"}
        </PrismLabel>
        <span className="pg-said">{running ? "Opening it on the agent's machine" : ""}</span>
      </div>
    </div>
  );
}
