/**
 * The text box the agent chat writes in (composer.tsx), shared by review comments (W26-R08): what's
 * typed is marked under the field (marks.tsx: Markdown, and the commands `names` holds), whose own
 * text is clear. Inside a `.cp` (the composer's root).
 *
 * It grows with the text (measured as each change is shown) up to the stylesheet's most lines, ten,
 * or four on a phone; past that it scrolls inside, so a long message never pushes what's around it
 * off the screen. Words added can only make it taller, which its scroll height says as it is; only
 * words taken away let it shrink, so only then is it set back to measure: typing never makes the
 * whole page lay itself out again.
 */
import * as React from "react";
import { Marked } from "./marks";
import "./harness.css";
import "./console.css";

const NO_NAMES: ReadonlySet<string> = new Set();

type FieldProps = Omit<React.TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "rows" | "className" | "children">;

export function ComposeField({
  text,
  names = NO_NAMES,
  field,
  onScroll,
  ...rest
}: FieldProps & {
  text: string;
  /** Commands to mark wherever they're typed (none: Markdown only). */
  names?: ReadonlySet<string>;
  /** Given the field as it's shown (focus, the caret, the mic). */
  field?: React.RefObject<HTMLTextAreaElement | null>;
}) {
  /** The marked copy of the text under the field. */
  const mirror = React.useRef<HTMLDivElement>(null);
  const lengthWas = React.useRef(0);
  const grow = React.useCallback(
    (el: HTMLTextAreaElement | null) => {
      if (field) field.current = el;
      if (!el) return;
      if (text.length < lengthWas.current || !text) el.style.height = "auto";
      lengthWas.current = text.length;
      const h = `${el.scrollHeight}px`;
      if (el.style.height !== h) el.style.height = h;
      if (mirror.current) mirror.current.scrollTop = el.scrollTop;
    },
    [text, field],
  );
  return (
    <div className="cp-field">
      <div ref={mirror} className="hx-compose-input cp-mirror" aria-hidden="true">
        <Marked text={text} names={names} />
      </div>
      <textarea
        ref={grow}
        className="hx-compose-input cp-input"
        rows={1}
        value={text}
        onScroll={(e) => {
          if (mirror.current) mirror.current.scrollTop = e.currentTarget.scrollTop;
          onScroll?.(e);
        }}
        {...rest}
      />
    </div>
  );
}
