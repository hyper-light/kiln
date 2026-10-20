/**
 * Live pages agents show in the conversation (docs/previews.md, "Streams"): an agent's
 * `show_live` call, drawn as the page itself running on the agent's machine (its dev server, its
 * files, its network), streamed here. Its picture comes as video (the size of this frame at this
 * screen's density), its sound as Opus once you turn it on: over WebRTC when the server has it and
 * it gets through, else over its socket (WebCodecs) as it does until then; in a browser with no
 * WebCodecs (any, on an http:// address other than localhost), over WebRTC alone. What you do in it goes back:
 * taps and the mouse, the wheel, keys by meaning, text from a phone's keyboard, a paste. Out of
 * view it's let go (the stream pauses, nothing is sent) and its last picture stays.
 *
 * A field of the page's that takes text has one of the viewer's own to type in, made like it: its
 * keyboard (a tap on the field brings it up at once), its password manager's autofill, its Enter.
 * What's selected in the page copies to the viewer's clipboard. Record keeps it (its video, a log
 * of what was done in it, and that as a test) on the agent's machine; each recording shows below.
 */
import * as React from "react";
import { ArrowLeft, ArrowRight, Circle, Copy, Keyboard, RotateCw, Square, Volume2, VolumeX, X } from "lucide-react";
import { harnessApi, recordingUrl, type Recording, type ToolCall } from "@/api/harness";
import { formatBytes } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { duration } from "./derive";
import { Beam, PrismLabel } from "./signals";
import { chunkOf, fieldAt, liveOf, modifiers, pagePoint, secret, shapeOf, type FieldPlace, type LiveStream, type PageField } from "./live-of";
import "./page.css";
import "./live.css";

type Status = "connecting" | "live" | "still" | "ended" | "unsupported";

const macViewer = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
const touchViewer = typeof window !== "undefined" && window.matchMedia?.("(pointer: coarse)").matches;
const decodes = typeof window !== "undefined" && "VideoDecoder" in window;
const plays = typeof window !== "undefined" && "AudioDecoder" in window;
const rtcs = typeof window !== "undefined" && "RTCPeerConnection" in window;
/** No WebCodecs (a browser hides them on an http:// address other than localhost): WebRTC is the
 *  one way its picture comes. */
const rtcOnly = !decodes && rtcs;
const insecure = typeof window !== "undefined" && window.isSecureContext === false;
/** WebRTC that didn't play here (this server's port out of reach, a codec this browser lacks):
 *  after two, this page doesn't try it again (the socket does). */
let rtcMisses = 0;

/** Why it can't be shown here (`rtc`: what became of WebRTC, when it was the one way left). */
function cantShow(rtc?: "off" | "missed"): string {
  if (!rtcs) return insecure ? "This browser can't show live pages on an http:// address: open kiln over https, or on localhost." : "This browser can't show live pages (it has neither WebCodecs nor WebRTC).";
  const no = insecure ? "On an http:// address this browser has no WebCodecs (they need https, or localhost)" : "This browser has no WebCodecs";
  return rtc === "off" ? `${no}, and this kiln has no WebRTC to show live pages with.` : `${no}, and WebRTC couldn't get through to kiln from here.`;
}

/** One viewing of a stream: its socket, its decoders, what it draws; its WebRTC peer, when the
 *  server has WebRTC (`rtc`: its picture plays in `player`, under the canvas, which is clear then). */
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
  private pc: RTCPeerConnection | null = null;
  private rtc = false;
  private rtcWait: ReturnType<typeof setTimeout> | undefined;
  /** With WebRTC the one way: the server's word that it has it, awaited; one try again once a peer
   *  that played goes. */
  private canWait: ReturnType<typeof setTimeout> | undefined;
  private retried = false;
  private soundOn = false;
  closed = false;

  constructor(
    url: string,
    private canvas: HTMLCanvasElement,
    private player: HTMLVideoElement | null,
    private on: { open: () => void; status: (s: Status, why?: string) => void; event: (e: Record<string, unknown>) => void; fps: (n: number) => void; rtc: (on: boolean) => void },
  ) {
    this.ws = new WebSocket(url);
    this.ws.binaryType = "arraybuffer";
    this.ws.onopen = () => {
      this.on.open();
      // The server says at once whether it has WebRTC.
      if (rtcOnly) this.canWait = setTimeout(() => this.cant("off"), 4000);
    };
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
    this.soundOn = on;
    if (this.player) this.player.muted = !on;
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
    clearTimeout(this.canWait);
    this.unpeer(false);
    this.ws.close(1000);
    for (const d of [this.video, this.audio]) if (d && d.state !== "closed") d.close();
    void this.ctx?.close();
  }

  private event(text: string) {
    let e: Record<string, unknown>;
    try {
      e = JSON.parse(text) as Record<string, unknown>;
    } catch {
      return; // Not one kiln sends.
    }
    const w = e.webrtc as { can?: boolean; answer?: string; error?: string; gone?: string } | undefined;
    if (!w) return this.on.event(e);
    if (w.can) {
      clearTimeout(this.canWait);
      void this.offer();
    }
    else if (w.answer && this.pc) this.pc.setRemoteDescription({ type: "answer", sdp: w.answer }).catch(() => this.unpeer(true));
    else if (w.error !== undefined || w.gone !== undefined) this.unpeer(false);
  }

  /** A peer offered to the server: the picture and sound come over it once it plays. */
  private async offer() {
    const player = this.player;
    if (!rtcs || !player || this.pc || this.closed || (rtcMisses >= 2 && !rtcOnly)) return;
    let pc: RTCPeerConnection;
    try {
      pc = new RTCPeerConnection({ bundlePolicy: "max-bundle", rtcpMuxPolicy: "require" });
    } catch {
      return;
    }
    this.pc = pc;
    pc.addTransceiver("video", { direction: "recvonly" });
    pc.addTransceiver("audio", { direction: "recvonly" });
    const media = new MediaStream();
    pc.ontrack = (e) => {
      media.addTrack(e.track);
      // Each picture as soon as it comes: there's nothing to smooth (a page changes when it does).
      const r = e.receiver as { jitterBufferTarget?: number | null; playoutDelayHint?: number };
      if ("jitterBufferTarget" in e.receiver) r.jitterBufferTarget = 0;
      else r.playoutDelayHint = 0;
      if (player.srcObject !== media) player.srcObject = media;
      player.muted = true;
      void player
        .play()
        .then(() => {
          if (!this.soundOn) return;
          // A browser that wants a gesture for sound (Safari) pauses it as it's unmuted: the
          // picture goes on without it, until Sound is pressed again.
          const keep = () => {
            player.muted = true;
            void player.play().catch(() => undefined);
          };
          player.addEventListener("pause", keep, { once: true });
          setTimeout(() => player.removeEventListener("pause", keep), 1000);
          player.muted = false;
        })
        .catch(() => undefined);
    };
    pc.onconnectionstatechange = () => {
      if (this.pc === pc && (pc.connectionState === "failed" || pc.connectionState === "closed")) this.unpeer(true);
    };
    this.watch(pc, player);
    // It plays soon or not at all (this server's port out of reach): the socket goes on either way.
    this.rtcWait = setTimeout(() => {
      if (this.pc === pc && !this.rtc) this.unpeer(true);
    }, 10_000);
    try {
      await pc.setLocalDescription(await pc.createOffer());
      if (this.pc === pc) this.send({ webrtc: { offer: pc.localDescription?.sdp } });
    } catch {
      if (this.pc === pc) this.unpeer(false);
    }
  }

  /** Its pictures as they're shown: the first makes it what's seen here; each is counted. */
  private watch(pc: RTCPeerConnection, player: HTMLVideoElement) {
    const v = player as HTMLVideoElement & { requestVideoFrameCallback?: (cb: () => void) => number };
    if (v.requestVideoFrameCallback) {
      const each = () => {
        if (this.pc !== pc || this.closed) return;
        if (player.videoWidth > 0) this.shown(player, 1);
        v.requestVideoFrameCallback?.(each);
      };
      v.requestVideoFrameCallback(each);
      return;
    }
    // No frame callbacks (an older Firefox): its count of pictures, a few times a second.
    let seen = 0;
    const t = setInterval(() => {
      if (this.pc !== pc || this.closed) return clearInterval(t);
      const n = player.getVideoPlaybackQuality?.().totalVideoFrames ?? 0;
      if (n > seen && player.videoWidth > 0) this.shown(player, n - seen);
      seen = n;
    }, 250);
  }

  /** A picture WebRTC showed: the canvas is clear over it (and the size of it, for where taps go). */
  private shown(player: HTMLVideoElement, n: number) {
    const c = this.canvas;
    if (!this.rtc) {
      this.rtc = true;
      this.retried = false;
      rtcMisses = 0;
      clearTimeout(this.rtcWait);
      this.send({ webrtc: { playing: true } });
      c.getContext("2d")?.clearRect(0, 0, c.width, c.height);
      this.on.rtc(true);
    }
    if (c.width !== player.videoWidth || c.height !== player.videoHeight) {
      c.width = player.videoWidth;
      c.height = player.videoHeight;
    }
    this.count(n);
  }

  /** WebRTC gone, or never there: the socket's pictures are drawn here again (`tell`: the server's
   *  told, when it didn't say so itself). */
  private unpeer(tell: boolean) {
    const pc = this.pc;
    if (!pc) return;
    this.pc = null;
    clearTimeout(this.rtcWait);
    pc.close();
    const was = this.rtc;
    this.rtc = false;
    const v = this.player;
    if (v) {
      // Its last picture stays until the socket's first.
      if (was && v.videoWidth > 0) this.canvas.getContext("2d")?.drawImage(v, 0, 0, this.canvas.width, this.canvas.height);
      v.srcObject = null;
    }
    if (was) {
      this.needKey = true;
      this.on.rtc(false);
    } else rtcMisses++;
    if (tell) this.send({ webrtc: { playing: false, close: true } });
    // With no other way here: a peer that played is tried again once; else it can't be shown.
    if (rtcOnly && !this.closed) {
      if (was && !this.retried) {
        this.retried = true;
        void this.offer();
      } else this.cant("missed");
    }
  }

  /** It can't be shown here: said, and its socket let go (nothing comes that would be shown). */
  private cant(rtc: "off" | "missed") {
    if (this.closed || this.rtc) return;
    this.on.status("unsupported", cantShow(rtc));
    this.close();
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
      if (this.ctx && !this.rtc && this.audio?.state === "configured") this.audio.decode(new EncodedAudioChunk({ type: "key", timestamp: c.ts, data }));
      return;
    }
    if (this.rtc) {
      this.needKey = true;
      return;
    }
    if (!this.video || this.video.state !== "configured") return;
    const key = c.flags === "k";
    if (this.needKey && !key) return;
    this.needKey = false;
    this.video.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: c.ts, data }));
  }

  private configureVideo(cfg: { codec: string; codedWidth?: number; codedHeight?: number; description?: string }) {
    if (!decodes) return;
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
    if (this.rtc) {
      f.close();
      return;
    }
    if (c.width !== f.displayWidth || c.height !== f.displayHeight) {
      c.width = f.displayWidth;
      c.height = f.displayHeight;
    }
    c.getContext("2d")?.drawImage(f, 0, 0, c.width, c.height);
    f.close();
    this.count(1);
  }

  /** Pictures shown, said each second as a rate. */
  private count(n: number) {
    this.frames += n;
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

/** What the viewer's own field keeps in it, so a phone's keyboard has something to delete (its
 *  Backspace says nothing in an empty field). Not in a password's, whose keys say themselves. */
const KEEP = "\u200b";

/** The viewer's field, made like the page's (`shapeOf`): set on the element itself, so it's right
 *  the moment it takes the focus (a phone's keyboard is chosen then), and its type kept (`setType`:
 *  React sets an input's type each time it renders it). */
function shape(el: HTMLInputElement, f: Pick<PageField, "type" | "mode" | "ac" | "hint">, setType: (t: "text" | "password") => void) {
  const s = shapeOf(f);
  if (el.type !== s.type) el.type = s.type;
  setType(s.type);
  el.inputMode = s.inputMode;
  el.autocomplete = s.autocomplete as AutoFill;
  el.enterKeyHint = s.enterKeyHint;
  el.value = s.type === "password" ? "" : KEEP;
}

/** Whether the viewer's field is already like the page's. */
function like(el: HTMLInputElement, f: Pick<PageField, "type" | "mode" | "ac" | "hint">): boolean {
  const s = shapeOf(f);
  return el.type === s.type && el.inputMode === s.inputMode && el.autocomplete === s.autocomplete && el.enterKeyHint === s.enterKeyHint;
}

/** What a field of the page's is, in a word: its label, else its kind. */
function fieldWord(f: PageField | null): string {
  if (f?.label) return f.label;
  const kinds: Record<string, string> = { password: "Password", email: "Email", url: "Address", tel: "Phone", search: "Search", number: "Number" };
  return kinds[f?.type ?? ""] ?? "Text";
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
  const [status, setStatus] = React.useState<Status>(decodes || rtcs ? "connecting" : "unsupported");
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
  /** WebRTC's picture, and whether it's what's seen (else the socket's, drawn on the canvas). */
  const player = React.useRef<HTMLVideoElement | null>(null);
  const [path, setPath] = React.useState<"webrtc" | "socket">("socket");
  /** The page's field that has the focus, and where its fields are; text selected in it. */
  const [field, setField] = React.useState<PageField | null>(null);
  const places = React.useRef<FieldPlace[]>([]);
  const [selected, setSelected] = React.useState(false);
  /** The viewer's own field (`shape`), typed in for the page's; whether it has the focus. */
  const mirror = React.useRef<HTMLInputElement | null>(null);
  const [mirrorType, setMirrorType] = React.useState<"text" | "password">("text");
  const [typing, setTyping] = React.useState(false);
  const mirrorRef = React.useCallback((el: HTMLInputElement | null) => {
    mirror.current = el;
    if (el) el.value = KEEP;
  }, []);
  const tapped = React.useRef(0);
  /** Recording it since (ms), and the recordings made while this was shown. */
  const [recording, setRecording] = React.useState<number | null>(null);
  const [recorded, setRecorded] = React.useState<Recording[]>([]);
  /** A copy asked of the page, answered by its `copied`. */
  const copying = React.useRef<((text: string) => void) | null>(null);
  const boxEl = React.useRef<HTMLDivElement | null>(null);
  const viewing = React.useRef<Viewing | null>(null);
  const size = React.useRef({ width: 0, height: 0 });
  /** Its engine, and the device it's held to (its window the device's: drawn whole in this frame). */
  const [engine, setEngine] = React.useState("chromium");
  const [held, setHeld] = React.useState<{ name: string; width: number; height: number } | null>(null);
  const live = React.useRef({ stream, sound, macPage, held });
  live.current = { stream, sound, macPage, held };
  const moved = React.useRef<Record<string, unknown> | null>(null);

  const send = React.useCallback((m: unknown) => viewing.current?.send(m), []);
  const input = React.useCallback((events: unknown[]) => send({ input: events }), [send]);
  const resize = React.useCallback(() => {
    const { width, height } = size.current;
    if (width > 0 && height > 0) send({ control: { resize: { width, height, scale: Math.min(2, Math.max(1, window.devicePixelRatio || 1)), mobile: touchViewer } } });
  }, [send]);

  const connect = React.useCallback(() => {
    const c = canvas.current;
    if (!c || !(decodes || rtcs) || viewing.current) return;
    const s = live.current.stream;
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const v = new Viewing(`${proto}//${location.host}/api/harness/sessions/${encodeURIComponent(s.session)}/streams/${s.id}/ws`, c, player.current, {
      // Its size first, before anything done in it: the page lays out at this frame's.
      open: () => {
        resize();
        // Still, its socket's pictures having come (over WebRTC alone, it's live once one shows).
        setTimeout(() => setStatus((st) => (st === "connecting" && !rtcOnly ? "still" : st)), 1500);
      },
      status: (st, w) => {
        setStatus(st);
        if (w !== undefined) setWhy(w);
        if ((st === "ended" || st === "unsupported") && viewing.current === v) viewing.current = null;
      },
      fps: setFps,
      rtc: (on) => setPath(on ? "webrtc" : "socket"),
      event: (e) => {
        const opened = e.opened as { platform?: string; engine?: string; device?: { name: string; width: number; height: number } | null } | undefined;
        if (opened) {
          setMacPage(opened.platform === "macos");
          setEngine(opened.engine ?? "chromium");
          setHeld(opened.device ? { name: opened.device.name, width: opened.device.width, height: opened.device.height } : null);
        }
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
        const focus = e.focus as PageField | undefined;
        if (focus) {
          setField(focus.editable ? focus : null);
          const m = mirror.current;
          if (m && focus.editable) {
            // The viewer's field is made like the page's (as it is, unless it's being typed in).
            const typingIn = document.activeElement === m;
            if (!typingIn || !like(m, focus)) shape(m, focus, setMirrorType);
            // Just tapped into it: the keyboard comes up (where the tap's gesture still lets it).
            if (!typingIn && touchViewer && performance.now() - tapped.current < 1500) m.focus({ preventScroll: true });
          } else if (m && document.activeElement === m) m.blur();
        }
        if (Array.isArray(e.fields)) places.current = e.fields as FieldPlace[];
        if (typeof e.selected === "boolean") setSelected(e.selected);
        const rec = e.recording as { on?: boolean; at?: number } | undefined;
        if (rec) setRecording(rec.on ? (rec.at ?? Date.now()) : null);
        const done = e.recorded as Recording | undefined;
        if (done?.id) setRecorded((l) => (l.some((r) => r.id === done.id) ? l : [...l, { ...done, stream: live.current.stream.id }]));
        const copied = e.copied as { text?: string } | undefined;
        if (copied && copying.current) {
          copying.current(copied.text ?? "");
          copying.current = null;
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
      boxEl.current = el;
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

  // Where in the page: the frame's own point, or (held to a device, drawn whole in it) the page's.
  const at = (e: React.PointerEvent | React.WheelEvent) => {
    const c = e.currentTarget as HTMLCanvasElement;
    const r = c.getBoundingClientRect();
    const p = { x: e.clientX - r.left, y: e.clientY - r.top };
    const h = live.current.held;
    return h ? pagePoint(p, r, { width: c.width, height: c.height }, h) : { x: Math.round(p.x), y: Math.round(p.y) };
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
    if (k === "up" && e.pointerType !== "mouse") {
      tapped.current = performance.now();
      // A tap on one of the page's fields: the keyboard comes up now, in the tap's own gesture
      // (a phone only lets it then), made for that field.
      const f = fieldAt(places.current, p.x, p.y);
      const m = mirror.current;
      if (f && m) {
        shape(m, f, setMirrorType);
        m.focus({ preventScroll: true });
      }
    }
  };
  // A key by name, down and up.
  const press = (name: string, text = "") => input([{ t: "k", k: "down", key: name, code: name, text }, { t: "k", k: "up", key: name, code: name }]);
  // What's selected in the page, to the viewer's clipboard: asked of the page now, written as it
  // answers (a clipboard item that waits for it, where the browser lets one; else its text then).
  const copyOut = () => {
    const text = new Promise<string>((resolve) => {
      copying.current = resolve;
      setTimeout(() => resolve(""), 3000);
    });
    send({ control: { copy: true } });
    const later = () => void text.then((t) => (t ? navigator.clipboard?.writeText(t) : undefined)).catch(() => undefined);
    try {
      // Nothing came: nothing's written (the clipboard keeps what it had).
      navigator.clipboard.write([new ClipboardItem({ "text/plain": text.then((t) => (t ? new Blob([t], { type: "text/plain" }) : Promise.reject(new Error("nothing selected")))) })]).catch(later);
    } catch {
      later();
    }
  };
  const key = (k: "down" | "up") => (e: React.KeyboardEvent) => {
    // A paste comes as its text (the page's own clipboard isn't the viewer's).
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "v") return;
    e.preventDefault();
    // A copy (or a cut) of what's selected comes to the viewer's clipboard; the page has the key too.
    if (k === "down" && (e.metaKey || e.ctrlKey) && ["c", "x"].includes(e.key.toLowerCase()) && selected) copyOut();
    const text = k === "down" && e.key.length === 1 && !e.ctrlKey && !e.metaKey ? e.key : "";
    input([{ t: "k", k, key: e.key, code: e.code, text, mods: mods(e), rep: e.repeat, loc: e.location }]);
  };

  // The viewer's own field: what's typed in it (a key, a word from a phone's keyboard, a password
  // its password manager filled, a paste) goes to the page's, and it's emptied again.
  const typed = (e: React.FormEvent<HTMLInputElement>) => {
    const ev = e.nativeEvent as InputEvent;
    // A composition goes as it's made, and whole as it ends.
    if (ev.isComposing || ev.inputType === "insertCompositionText") return;
    const t = e.currentTarget;
    const text = t.value.replaceAll(KEEP, "");
    if (ev.inputType === "deleteContentBackward" || (t.type !== "password" && !t.value.includes(KEEP) && !text)) press("Backspace");
    else if (ev.inputType === "insertLineBreak" || ev.inputType === "insertParagraph") press("Enter", "\r");
    else if (text) input([{ t: "x", text }]);
    t.value = t.type === "password" ? "" : KEEP;
  };
  const typedKey = (e: React.KeyboardEvent<HTMLInputElement>) => {
    const t = e.currentTarget;
    // Keys that type nothing go to the page's field as keys: moving in it, its Enter, Tab to the next.
    if (["Enter", "Tab", "Escape", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End", "Delete"].includes(e.key) || (e.key === "Backspace" && !t.value)) {
      e.preventDefault();
      press(e.key, e.key === "Enter" ? "\r" : "");
      if (e.key === "Escape") t.blur();
    }
  };
  // While it has the focus and a phone's keyboard takes part of the screen, the page's window is
  // what's left above it (the page then keeps its field in sight).
  const fit = React.useCallback(() => {
    const el = boxEl.current;
    const vv = window.visualViewport;
    if (!el || !vv) return;
    if (document.activeElement === mirror.current && vv.height < window.innerHeight * 0.85) el.style.setProperty("--lv-kb-h", `${Math.max(160, Math.round(vv.offsetTop + vv.height - el.getBoundingClientRect().top - 8))}px`);
    else el.style.removeProperty("--lv-kb-h");
  }, []);

  const go = (url: string) => {
    const u = /^[a-z]+:\/\//i.test(url) ? url : `http://${url}`;
    send({ control: { go: u } });
    setAddress(u);
    setEditing(null);
  };
  const startAgain = async () => {
    const r = await fetch(`/api/harness/sessions/${encodeURIComponent(stream.session)}/streams`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ url: address || stream.url, width: size.current.width || 390, height: size.current.height || 600, profile: stream.profile, engine: stream.engine, device: stream.device }) });
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

  // The viewer's own field shows while it's typed in, or (on a phone; anywhere, for what a password
  // manager fills) while the page's field has the focus.
  const shown = typing || (field !== null && (touchViewer || secret(field)));
  // Its engine and device, when they're not Chromium as this screen is.
  const what = [engine !== "chromium" ? engine : "", held?.name ?? ""].filter(Boolean).join(" · ");
  const word = (what ? `${what} · ` : "") + (status === "live" && fps > 0 ? `live · ${fps} fps` : status === "live" || status === "still" ? "live · still" : status === "connecting" ? "connecting" : status === "unsupported" ? "this browser can't show it" : "ended");
  return (
    <div className="pg lv" data-ev={ev} data-ev-live={stream.id} data-status={status} data-path={path}>
      <div className="pg-head">
        <PrismLabel live={status === "live"} className="pg-title">
          {title}
        </PrismLabel>
        <span className="pg-said" role="status">
          {recording !== null ? <Since at={recording} /> : agentDid || word}
        </span>
        <span className="pg-actions">
          {/* Its sound comes over WebRTC, or over its socket where WebCodecs' AudioDecoder plays it. */}
          {(plays || rtcs) && (
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
          <button type="button" className="pg-action" aria-pressed={recording !== null} onClick={() => send({ control: { record: recording === null } })}>
            {recording === null ? <Circle size={12} aria-hidden="true" /> : <Square size={12} aria-hidden="true" />}
            {recording === null ? "Record" : "Stop"}
          </button>
          {selected && (
            <button type="button" className="pg-action" onClick={copyOut}>
              <Copy size={12} aria-hidden="true" />
              Copy
            </button>
          )}
          <button
            type="button"
            className="pg-action"
            onClick={() => {
              const m = mirror.current;
              if (!m) return;
              shape(m, field ?? { type: "text" }, setMirrorType);
              m.focus({ preventScroll: true });
            }}
          >
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
        {/* While a field of the page's takes text, the viewer's own for it (`shape`) is here; the
            page's address otherwise. It's always there, so a tap can give it the focus at once. */}
        {shown && <span className="lv-typing-what">{fieldWord(field)}</span>}
        <input
          ref={mirrorRef}
          type={mirrorType}
          className={`hx-input lv-mirror${shown ? "" : " lv-mirror-off"}`}
          aria-label={`Type into the page's ${fieldWord(field).toLowerCase()} field`}
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          tabIndex={shown ? 0 : -1}
          onInput={typed}
          onKeyDown={typedKey}
          onCompositionUpdate={(e) => input([{ t: "i", text: e.data, start: e.data.length, end: e.data.length }])}
          onCompositionEnd={(e) => {
            input([{ t: "x", text: e.data }]);
            const t = e.currentTarget;
            setTimeout(() => (t.value = t.type === "password" ? "" : KEEP), 0);
          }}
          onFocus={() => {
            setTyping(true);
            window.visualViewport?.addEventListener("resize", fit);
            fit();
          }}
          onBlur={() => {
            setTyping(false);
            window.visualViewport?.removeEventListener("resize", fit);
            boxEl.current?.style.removeProperty("--lv-kb-h");
          }}
        />
        <input
          className="hx-input lv-address"
          hidden={shown}
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
        {/* WebRTC's picture, once it plays: under the canvas, which takes what's done in the page
            and is clear then. */}
        <video ref={player} className="lv-video" muted playsInline autoPlay aria-hidden="true" tabIndex={-1} />
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
        {(status === "ended" || status === "unsupported") && (
          <div className="lv-over">
            <p>{status === "unsupported" ? why || cantShow() : why || "This live page ended."}</p>
            {status === "ended" && (
              <button type="button" className="pg-action" onClick={() => void startAgain()}>
                Start it again
              </button>
            )}
            {status === "unsupported" && rtcOnly && (
              <button
                type="button"
                className="pg-action"
                onClick={() => {
                  setStatus("connecting");
                  setWhy("");
                  connect();
                }}
              >
                Try again
              </button>
            )}
          </div>
        )}
      </div>
      <React.Suspense fallback={null}>
        <Recordings session={stream.session} stream={stream.id} more={recorded} />
      </React.Suspense>
    </div>
  );
}

/** How long it's been recording, said each second ("recording · 0:12"). */
function Since({ at }: { at: number }) {
  const tick = React.useCallback(
    (el: HTMLSpanElement | null) => {
      if (!el) return;
      const say = () => (el.textContent = `recording · ${duration(Math.max(0, Date.now() - at))}`);
      say();
      const t = setInterval(say, 1000);
      return () => clearInterval(t);
    },
    [at],
  );
  return <span ref={tick} className="lv-recording" />;
}

/** The recordings of a stream: each its length and size, to watch here, and its test and log to keep. */
function Recordings({ session, stream, more }: { session: string; stream: string; more: Recording[] }) {
  const listed = useSuspenseResource(`harness/recordings/${session}`, () => harnessApi.recordings(session).catch((): Recording[] => []), { pollMs: 60_000 });
  const [watching, setWatching] = React.useState<string | null>(null);
  const all = [...listed.data.filter((r) => r.stream === stream), ...more.filter((m) => !listed.data.some((r) => r.id === m.id))];
  if (!all.length) return null;
  return (
    <ul className="lv-recs" aria-label="Recordings">
      {all.map((r) => (
        <li key={r.id}>
          <div className="lv-rec">
            <span className="lv-rec-what">
              Recorded {duration(r.seconds * 1000)} · {formatBytes(r.bytes)}
            </span>
            <button type="button" className="pg-action" aria-pressed={watching === r.id} onClick={() => setWatching(watching === r.id ? null : r.id)}>
              {watching === r.id ? "Hide" : "Watch"}
            </button>
            <a className="pg-action" href={recordingUrl(session, r.id, "test")} download>
              Test
            </a>
            <a className="pg-action" href={recordingUrl(session, r.id, "inputs")} download>
              Log
            </a>
          </div>
          {watching === r.id && <video className="lv-rec-video" src={recordingUrl(session, r.id, "video")} controls playsInline preload="metadata" />}
        </li>
      ))}
    </ul>
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
