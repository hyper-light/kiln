/**
 * Speak instead of typing, from any browser: the browser's own speech recognition where it has one
 * (Chrome, Edge, Safari), else a recording kiln turns into text (Firefox, once `[voice]` is set up
 * on the server: docs/voice.md). The words go in where the cursor is, as they're heard; nothing is
 * sent until you send it. Browsers lend the microphone only to a secure page (https, or localhost).
 */
import * as React from "react";
import { LoaderCircle, Mic, Square } from "lucide-react";
import { api } from "@/api/client";
import { Tip } from "@/components/ui/tooltip";
import { errorMessage } from "@/lib/hooks";
import { cn } from "@/lib/utils";

/** The parts of the Web Speech API used here (not in TypeScript's DOM types). */
interface Recognition {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  onresult: ((e: { resultIndex: number; results: ArrayLike<{ isFinal: boolean; 0: { transcript: string } }> }) => void) | null;
  onerror: ((e: { error: string }) => void) | null;
  onend: (() => void) | null;
  start: () => void;
  stop: () => void;
  abort: () => void;
}
type RecognitionCtor = new () => Recognition;

type Way = { kind: "speech"; Rec: RecognitionCtor; lang: string } | { kind: "record"; mime: string } | { kind: "off"; why: string };

/** The longest recording sent for transcription. */
const MAX_RECORDING_MS = 5 * 60_000;
const MIMES = ["audio/webm;codecs=opus", "audio/ogg;codecs=opus", "audio/mp4"];

/** The server's part, asked once a page. */
let server: Promise<{ transcribe: boolean; language: string }> | null = null;
const serverVoice = () => (server ??= api.voice().catch(() => ({ transcribe: false, language: "" })));

function speech(): RecognitionCtor | undefined {
  const w = window as unknown as { SpeechRecognition?: RecognitionCtor; webkitSpeechRecognition?: RecognitionCtor };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition;
}

/** How this browser can take speech, or why it can't. */
function useWay(): Way | null {
  const [way, setWay] = React.useState<Way | null>(null);
  React.useEffect(() => {
    let live = true;
    if (!window.isSecureContext || !navigator.mediaDevices) {
      setWay({ kind: "off", why: "The microphone needs HTTPS: browsers lend it only to an https:// page." });
      return;
    }
    void serverVoice().then((s) => {
      if (!live) return;
      const Rec = speech();
      if (Rec) setWay({ kind: "speech", Rec, lang: s.language || navigator.language || "en-US" });
      else if (typeof MediaRecorder !== "undefined" && s.transcribe) setWay({ kind: "record", mime: MIMES.find((m) => MediaRecorder.isTypeSupported(m)) ?? "" });
      else setWay({ kind: "off", why: "This browser can't turn speech into text; set up [voice] on the server (docs/voice.md) and it will." });
    });
    return () => {
      live = false;
    };
  }, []);
  return way;
}

const WHY: Record<string, string> = {
  "not-allowed": "The microphone isn't allowed for this page (the browser's site settings).",
  "service-not-allowed": "The browser's speech recognition isn't allowed here.",
  "audio-capture": "No microphone was found.",
  network: "The browser's speech service couldn't be reached.",
  "no-speech": "Nothing was heard.",
  "language-not-supported": "The browser can't recognise this language.",
};

const clock = (ms: number) => `${Math.floor(ms / 60_000)}:${String(Math.floor(ms / 1000) % 60).padStart(2, "0")}`;

/**
 * The microphone, beside a text field: tap to speak, tap again to stop. `field` is the text field
 * and `setText` sets its text; `onNote` says, in a line, why it can't listen or what went wrong.
 */
export function MicButton({
  field,
  setText,
  onNote,
  disabled,
  className,
}: {
  field: React.RefObject<HTMLTextAreaElement | null>;
  setText: (text: string) => void;
  onNote: (note: string) => void;
  disabled?: boolean;
  className?: string;
}) {
  const way = useWay();
  const [state, setState] = React.useState<"idle" | "listening" | "transcribing">("idle");
  const [since, setSince] = React.useState(0);
  const [now, setNow] = React.useState(0);
  /** The text around the cursor when it started, and the words heard since. */
  const words = React.useRef<{ before: string; after: string; heard: string } | null>(null);
  const stopper = React.useRef<(() => void) | null>(null);

  React.useEffect(() => {
    if (state !== "listening") return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [state]);
  // Leaving the page stops listening.
  React.useEffect(() => () => stopper.current?.(), []);

  /** Puts the words heard (and those still being heard) in at the cursor, and the cursor after them. */
  const show = (pending = "") => {
    const w = words.current;
    if (!w) return;
    const said = `${w.heard}${pending}`.replace(/\s+/g, " ").trim();
    const head = said && w.before && !/\s$/.test(w.before) ? `${w.before} ` : w.before;
    const tail = said && w.after && !/^\s/.test(w.after) ? ` ${w.after}` : w.after;
    setText(head + said + tail);
    const at = (head + said).length;
    requestAnimationFrame(() => field.current?.setSelectionRange(at, at));
  };

  const begin = () => {
    const el = field.current;
    const v = el?.value ?? "";
    const from = el?.selectionStart ?? v.length;
    const to = el?.selectionEnd ?? v.length;
    words.current = { before: v.slice(0, from), after: v.slice(to), heard: "" };
    setSince(Date.now());
    setNow(Date.now());
    setState("listening");
  };
  const done = () => {
    words.current = null;
    stopper.current = null;
    setState("idle");
  };

  const listen = (w: Extract<Way, { kind: "speech" }>) => {
    const rec = new w.Rec();
    rec.lang = w.lang;
    rec.continuous = true;
    rec.interimResults = true;
    rec.onresult = (e) => {
      const ws = words.current;
      if (!ws) return;
      let pending = "";
      for (let i = e.resultIndex; i < e.results.length; i++) {
        const r = e.results[i];
        if (!r) continue;
        if (r.isFinal) ws.heard += ` ${r[0].transcript}`;
        else pending += ` ${r[0].transcript}`;
      }
      show(pending);
    };
    rec.onerror = (e) => {
      if (e.error !== "aborted") onNote(WHY[e.error] ?? `Speech recognition stopped (${e.error}).`);
    };
    rec.onend = done;
    begin();
    stopper.current = () => rec.stop();
    try {
      rec.start();
    } catch (e) {
      onNote(`Couldn't start listening: ${errorMessage(e)}`);
      done();
    }
  };

  const record = async (w: Extract<Way, { kind: "record" }>) => {
    let stream: MediaStream;
    try {
      stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    } catch (e) {
      onNote(e instanceof DOMException && e.name === "NotAllowedError" ? WHY["not-allowed"]! : `Couldn't use the microphone: ${errorMessage(e)}`);
      return;
    }
    const rec = new MediaRecorder(stream, w.mime ? { mimeType: w.mime } : undefined);
    const chunks: Blob[] = [];
    rec.ondataavailable = (e) => {
      if (e.data.size) chunks.push(e.data);
    };
    const cap = window.setTimeout(() => rec.state !== "inactive" && rec.stop(), MAX_RECORDING_MS);
    rec.onstop = () => {
      window.clearTimeout(cap);
      for (const t of stream.getTracks()) t.stop();
      const audio = new Blob(chunks, { type: rec.mimeType || w.mime || "audio/webm" });
      setState("transcribing");
      api.transcribe(audio).then(
        ({ text }) => {
          if (words.current) words.current.heard = text;
          show();
          done();
        },
        (e: unknown) => {
          onNote(`Couldn't turn the recording into text: ${errorMessage(e)}`);
          done();
        },
      );
    };
    begin();
    stopper.current = () => rec.state !== "inactive" && rec.stop();
    rec.start(1000);
  };

  const click = () => {
    if (!way) return;
    if (way.kind === "off") {
      onNote(way.why);
      return;
    }
    if (state === "listening") {
      stopper.current?.();
      return;
    }
    if (state !== "idle") return;
    if (way.kind === "speech") listen(way);
    else void record(way);
  };

  const listening = state === "listening";
  const label = way?.kind === "off" ? "Speak (unavailable here)" : listening ? "Stop listening" : state === "transcribing" ? "Turning it into text" : "Speak";
  return (
    <Tip content={way?.kind === "off" ? way.why : listening ? "Stop" : "Speak: the words go in at the cursor"}>
      <button
        type="button"
        className={cn("cp-icon cp-mic", className)}
        data-state={way?.kind === "off" ? "off" : state}
        aria-label={label}
        aria-pressed={listening}
        aria-disabled={way?.kind === "off" || undefined}
        disabled={disabled || way === null || state === "transcribing"}
        onClick={click}
      >
        {state === "transcribing" ? (
          <LoaderCircle size={15} className="animate-spin motion-reduce:animate-none" aria-hidden="true" />
        ) : listening ? (
          <Square size={11} fill="currentColor" aria-hidden="true" />
        ) : (
          <Mic size={15} aria-hidden="true" />
        )}
        {listening && <span className="cp-mic-time">{clock(now - since)}</span>}
      </button>
    </Tip>
  );
}
