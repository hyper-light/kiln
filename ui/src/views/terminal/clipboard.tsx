/**
 * Copy and paste for terminal sessions that work everywhere: over plain HTTP (where browsers turn
 * the clipboard API off), on phones (where a terminal can't use the system's text selection), and
 * on any keyboard. A plain-text view of the session takes the system's own selection handles.
 */
import * as React from "react";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent } from "@/components/ui/dialog";

/** Put text on the clipboard: the clipboard API where the page may use it, else the old copy command. */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false;
  try {
    if (window.isSecureContext && navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* fall through */
  }
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.setAttribute("readonly", "");
  ta.style.cssText = "position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;pointer-events:none";
  const active = document.activeElement as HTMLElement | null;
  document.body.appendChild(ta);
  ta.select();
  ta.setSelectionRange(0, text.length);
  let ok: boolean;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  ta.remove();
  active?.focus?.();
  return ok;
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

/** The session as plain text, to select with the system's own handles or copy whole. */
export function SelectTextDialog({ open, onOpenChange, text, onClosed }: { open: boolean; onOpenChange: (o: boolean) => void; text: string; onClosed?: () => void }) {
  const ref = React.useRef<HTMLTextAreaElement>(null);
  const [copied, setCopied] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!open) return;
    setCopied(null);
    // Open at the end, where the latest output is.
    requestAnimationFrame(() => {
      const el = ref.current;
      if (el) el.scrollTop = el.scrollHeight;
    });
  }, [open]);
  const copy = async (all: boolean) => {
    const el = ref.current;
    const sel = el && !all ? el.value.slice(el.selectionStart, el.selectionEnd) : "";
    const ok = await copyText(all || !sel ? text : sel);
    setCopied(ok ? (all || !sel ? "Copied everything" : "Copied the selection") : "Couldn't copy — select the text and use your system's Copy");
  };
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title="Select text"
        eyebrow="Terminal"
        description="Select what you need, or copy everything."
        onCloseAutoFocus={(e) => {
          e.preventDefault();
          onClosed?.();
        }}
      >
        <textarea ref={ref} className="clip-text" readOnly value={text} spellCheck={false} aria-label="Session text" />
        <div className="clip-actions">
          {copied && <span className="clip-note">{copied}</span>}
          <Button size="sm" variant="secondary" onClick={() => void copy(false)}>
            Copy selection
          </Button>
          <Button size="sm" onClick={() => void copy(true)}>
            Copy all
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** Where the browser won't hand over the clipboard: paste here, then send it to the session. */
export function PasteDialog({ open, onOpenChange, onPaste, onClosed }: { open: boolean; onOpenChange: (o: boolean) => void; onPaste: (text: string) => void; onClosed?: () => void }) {
  const [text, setText] = React.useState("");
  React.useEffect(() => {
    if (open) setText("");
  }, [open]);
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title="Paste"
        eyebrow="Terminal"
        description="Paste here (long-press → Paste, or Ctrl/Cmd+V), then send it to the session."
        onCloseAutoFocus={(e) => {
          // Back to the terminal (not the button that opened this), so the next key goes to the shell.
          e.preventDefault();
          onClosed?.();
        }}
      >
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
              onOpenChange(false);
            }}
          >
            Paste into the session
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
