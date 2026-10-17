/** Small pieces of Runners → Add machines: controls that read as text, inline disclosure, wrapping code, the OS words. */
import * as React from "react";
import { Check, ChevronRight, Copy } from "lucide-react";
import type { InstallOs } from "@/api/types";
import { cn } from "@/lib/utils";
import { OS_NAME } from "./data";

/** Put text on the clipboard. Servers reached over plain http have no Clipboard API: fall back to a selected textarea. */
export async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}

/** `copied` stays true for a moment after `copy`. */
export function useCopy(): [boolean, (text: string) => void] {
  const [copied, setCopied] = React.useState(false);
  const timer = React.useRef<number | null>(null);
  React.useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  const copy = React.useCallback((text: string) => {
    void copyText(text).then(() => {
      setCopied(true);
      if (timer.current !== null) window.clearTimeout(timer.current);
      timer.current = window.setTimeout(() => setCopied(false), 1600);
    });
  }, []);
  return [copied, copy];
}

/** Copy as a text control: the boxed button stays the one primary action of a section. */
export function CopyText({ text, label = "Copy", name, iconOnly, className }: { text: string; label?: string; name?: string; iconOnly?: boolean; className?: string }) {
  const [copied, copy] = useCopy();
  return (
    <button type="button" className={cn("am-act", className)} data-tone={copied ? "ok" : "quiet"} onClick={() => copy(text)} aria-label={iconOnly || name ? (copied ? "Copied" : (name ?? label)) : undefined}>
      {copied ? <Check aria-hidden="true" /> : <Copy aria-hidden="true" />}
      {!iconOnly && <span aria-live="polite">{copied ? "Copied" : label}</span>}
    </button>
  );
}

/**
 * A command or a zone-file line, wrapped like one: between words, never inside a word that fits on
 * a line (`--token`); a long word (a URL, a path, a DNS name) breaks after `/` or a `.` before a
 * letter, and anywhere only when a piece is longer than the line. Copying the text stays exact.
 */
export function Wrapped({ text }: { text: string }) {
  return (
    <>
      {text.split(/(\s+)/).map((word, i) =>
        !word || /^\s+$/.test(word) ? (
          word
        ) : (
          <React.Fragment key={i}>
            {(word.length > 24 ? word.split(/(?<=\/)|(?<=\.(?=[A-Za-z_]))/) : [word]).map((seg, j, all) => (
              <React.Fragment key={j}>
                <span className="am-nobr">{seg}</span>
                {j < all.length - 1 && <wbr />}
              </React.Fragment>
            ))}
          </React.Fragment>
        ),
      )}
    </>
  );
}

/** A command (or any line to paste) that wraps instead of scrolling sideways, with Copy. */
export function CodeLine({ command, prompt = "$", copyName = "Copy the command" }: { command: string; prompt?: string; copyName?: string }) {
  return (
    <div className="am-code">
      <code>
        {prompt && (
          <span className="am-prompt" aria-hidden="true">
            {prompt}{" "}
          </span>
        )}
        <Wrapped text={command} />
      </code>
      <CopyText text={command} name={copyName} />
    </div>
  );
}

/** Inline disclosure: a line of text that opens what it names in place. */
export function Fold({ label, children, className, bodyClassName }: { label: React.ReactNode; children: React.ReactNode; className?: string; bodyClassName?: string }) {
  const [open, setOpen] = React.useState(false);
  const id = React.useId();
  return (
    <div className={className}>
      <button type="button" className="am-fold" aria-expanded={open} aria-controls={id} onClick={() => setOpen((o) => !o)}>
        <ChevronRight size={13} aria-hidden="true" />
        {label}
      </button>
      <div id={id} hidden={!open} className={cn("am-fold-body", bodyClassName)}>
        {open && children}
      </div>
    </div>
  );
}

const OS_ORDER: InstallOs[] = ["macos", "windows", "linux"];

/** MACOS · WINDOWS · LINUX: a radio group of words (arrow keys move it, like any radio group). */
export function OsWords({ value, onChange, label }: { value: InstallOs; onChange: (os: InstallOs) => void; label: string }) {
  const refs = React.useRef<(HTMLButtonElement | null)[]>([]);
  const select = (i: number) => {
    const n = (i + OS_ORDER.length) % OS_ORDER.length;
    onChange(OS_ORDER[n]!);
    refs.current[n]?.focus();
  };
  return (
    <div role="radiogroup" aria-label={label} className="am-os">
      {OS_ORDER.map((os, i) => (
        <React.Fragment key={os}>
          {i > 0 && (
            <span className="am-os-sep" aria-hidden="true">
              ·
            </span>
          )}
          <button
            ref={(el) => {
              refs.current[i] = el;
            }}
            type="button"
            role="radio"
            aria-checked={value === os}
            tabIndex={value === os ? 0 : -1}
            className="am-os-opt"
            onClick={() => onChange(os)}
            onKeyDown={(e) => {
              if (e.key === "ArrowRight" || e.key === "ArrowDown") select(i + 1);
              else if (e.key === "ArrowLeft" || e.key === "ArrowUp") select(i - 1);
              else if (e.key === "Home") select(0);
              else if (e.key === "End") select(OS_ORDER.length - 1);
              else return;
              e.preventDefault();
            }}
          >
            {OS_NAME[os]}
          </button>
        </React.Fragment>
      ))}
    </div>
  );
}
