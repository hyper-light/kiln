/**
 * Copy and paste for terminal sessions that work everywhere: over plain HTTP (where browsers turn
 * the clipboard API off), on phones (where a terminal can't use the system's text selection), and
 * on any keyboard. A plain-text view of the session takes the system's own selection handles.
 */
import * as React from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";

/**
 * Put text on the clipboard: the clipboard API where the page may use it (HTTPS, localhost), else
 * (or where it refuses) the copy command. Every copy in the UI comes here.
 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false;
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      /* refused (permission, focus): the copy command, while the tap still counts */
    }
  }
  return copyCommand(text);
}

/**
 * The copy command, its copy event handed the text: nothing selected and no focus moved, so it
 * works inside a dialog or a menu too (their focus trap takes focus back from a textarea outside
 * them before it's copied). False where the browser refuses it: outside a tap or a key press.
 */
function copyCommand(text: string): boolean {
  let handed = false;
  const hand = (e: ClipboardEvent) => {
    if (!e.clipboardData) return;
    e.clipboardData.setData("text/plain", text);
    e.preventDefault();
    // Before a terminal's or an editor's own copy handler puts its selection there instead.
    e.stopImmediatePropagation();
    handed = true;
  };
  window.addEventListener("copy", hand, true);
  try {
    // Whether the event came is the answer: Safari's command says false with nothing selected,
    // though the text it was handed is on the clipboard.
    document.execCommand("copy");
  } catch {
    /* no copy command */
  } finally {
    window.removeEventListener("copy", hand, true);
  }
  return handed;
}

/**
 * Put text that's still coming (fetched, or asked of a page) on the clipboard: called in the tap or
 * key press itself. A clipboard item that waits for it where the browser has one (Safari takes a
 * write only during the gesture); else the text once it comes, while the gesture still counts.
 */
export function copyLater(text: Promise<string>): Promise<boolean> {
  const then = () => text.then(copyText, () => false);
  if (!window.isSecureContext || typeof ClipboardItem === "undefined" || !navigator.clipboard?.write) return then();
  try {
    const blob = text.then((t) => (t ? new Blob([t], { type: "text/plain" }) : Promise.reject(new Error("nothing to copy"))));
    return navigator.clipboard.write([new ClipboardItem({ "text/plain": blob })]).then(() => true, then);
  } catch {
    return then();
  }
}

/** Read the clipboard, where the browser allows it (secure pages, after a tap or key press). */
export async function readClipboard(): Promise<string | null> {
  try {
    if (window.isSecureContext && navigator.clipboard?.readText) return await navigator.clipboard.readText();
  } catch {
    /* denied */
  }
  return null;
}

/** A band of the terminal page, between its tabs and the terminal: a title, what it says, its close. */
function TermBand({ eyebrow, title, description, onClose, children }: { eyebrow: string; title: string; description: string; onClose: () => void; children: React.ReactNode }) {
  return (
    <section
      className="term-band"
      aria-label={title}
      onKeyDown={(e) => {
        if (e.key === "Escape") onClose();
      }}
    >
      <div className="term-band-head">
        <div className="min-w-0">
          <span className="panel-eyebrow">{eyebrow}</span>
          <h2 className="term-band-title">{title}</h2>
          <p className="term-band-desc">{description}</p>
        </div>
        <button type="button" className="icon-btn -mr-2" data-variant="bare" aria-label="Close" onClick={onClose}>
          <X size={17} />
        </button>
      </div>
      {children}
    </section>
  );
}

/** The session as plain text, to select with the system's own handles or copy whole. */
export function SelectTextPanel({ text, onClose }: { text: string; onClose: () => void }) {
  const [copied, setCopied] = React.useState<string | null>(null);
  const ref = React.useRef<HTMLTextAreaElement | null>(null);
  // Opens at the end, where the latest output is.
  const atEnd = React.useCallback((el: HTMLTextAreaElement | null) => {
    ref.current = el;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);
  const copy = async (all: boolean) => {
    const el = ref.current;
    const sel = el && !all ? el.value.slice(el.selectionStart, el.selectionEnd) : "";
    const ok = await copyText(all || !sel ? text : sel);
    setCopied(ok ? (all || !sel ? "Copied everything" : "Copied the selection") : "Couldn't copy — select the text and use your system's Copy");
  };
  return (
    <TermBand eyebrow="Terminal" title="Select text" description="Select what you need, or copy everything." onClose={onClose}>
      <textarea ref={atEnd} className="clip-text" readOnly value={text} spellCheck={false} aria-label="Session text" />
      <div className="clip-actions">
        {copied && <span className="clip-note">{copied}</span>}
        <Button size="sm" variant="secondary" onClick={() => void copy(false)}>
          Copy selection
        </Button>
        <Button size="sm" onClick={() => void copy(true)}>
          Copy all
        </Button>
      </div>
    </TermBand>
  );
}

/** Where the browser won't hand over the clipboard: paste here, then send it to the session. */
export function PastePanel({ onPaste, onClose }: { onPaste: (text: string) => void; onClose: () => void }) {
  const [text, setText] = React.useState("");
  return (
    <TermBand eyebrow="Terminal" title="Paste" description="Paste here (long-press → Paste, or Ctrl/Cmd+V), then send it to the session." onClose={onClose}>
      <textarea
        className="clip-text clip-paste"
        autoFocus
        value={text}
        onChange={(e) => setText(e.target.value)}
        placeholder="Paste here"
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
        aria-label="Text to paste"
      />
      <div className="clip-actions">
        <Button
          size="sm"
          disabled={!text}
          onClick={() => {
            onPaste(text);
            onClose();
          }}
        >
          Paste into the session
        </Button>
      </div>
    </TermBand>
  );
}
