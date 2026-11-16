/**
 * What a terminal answers of what it's sent (W21-R02): its place, what it is, a mode's state, a colour. A session's
 * replay holds every query its programs made, and the part sent ahead of it holds the newest of them again (docs/wire.md,
 * "A terminal's screen first"): read out of the part they aren't answered, or the shell would be answered twice.
 */
import type { Terminal } from "@xterm/xterm";

/** Whether an `OSC 4`, `10`, `11` or `12` only asks for colours (each value `?`) and sets none: it's then nothing but its answers. */
export function asksOnly(id: number, data: string): boolean {
  const slots = data.split(";");
  // A palette's are pairs: a colour's number, then its value.
  if (id === 4) return slots.length % 2 === 0 && slots.every((slot, i) => i % 2 === 0 || slot === "?");
  return slots.every((slot) => slot === "?");
}

/**
 * While `quiet()` says so, `term` answers nothing it reads: the sequences xterm.js answers (a device's status and
 * attributes, a mode asked about, a setting asked for, a colour asked for) are taken here and dropped. They change
 * nothing a terminal keeps, so what it draws is the same.
 */
export function unanswered(term: Terminal, quiet: () => boolean): { dispose(): void }[] {
  const parser = term.parser;
  return [
    parser.registerCsiHandler({ final: "n" }, quiet),
    parser.registerCsiHandler({ prefix: "?", final: "n" }, quiet),
    parser.registerCsiHandler({ final: "c" }, quiet),
    parser.registerCsiHandler({ prefix: ">", final: "c" }, quiet),
    parser.registerCsiHandler({ intermediates: "$", final: "p" }, quiet),
    parser.registerCsiHandler({ prefix: "?", intermediates: "$", final: "p" }, quiet),
    parser.registerDcsHandler({ intermediates: "$", final: "q" }, quiet),
    ...[4, 10, 11, 12].map((id) => parser.registerOscHandler(id, (data) => quiet() && asksOnly(id, data))),
  ];
}
