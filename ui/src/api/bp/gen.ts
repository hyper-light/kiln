// Made from kiln-bp's Rust types (crates/kiln-bp/src/frames.rs): change those, then run
// `KILN_WRITE_BP=1 cargo test -p kiln-bp --test ts`.
/* eslint-disable */
import { BpError, Gen, Reader, Why, Writer } from "./runtime";

/** The protocol version this build speaks, and the oldest it still serves. */
export const VERSION = 1;
export const OLDEST = 1;
/** The schema's hash, said in `Open`: two builds that disagree on a layout find out at once. */
export const SCHEMA_HASH = 17836181741169541297n;

export interface Accepted { version: number; caps: bigint; idle_ms: number; born: number; fragment: number; build: string }
export function putAccepted(w: Writer, v: Accepted): void {
  w.u32(v.version);
  w.bits(v.caps);
  w.u32(v.idle_ms);
  w.u53(v.born);
  w.u32(v.fragment);
  w.str(v.build);
}
export function takeAccepted(r: Reader): Accepted {
  const version = r.u32();
  const caps = r.bits();
  const idle_ms = r.u32();
  const born = r.u53();
  const fragment = r.u32();
  const build = r.str();
  return { version, caps, idle_ms, born, fragment, build };
}
export function defaultAccepted(): Accepted {
  return { version: 0, caps: 0n, idle_ms: 0, born: 0, fragment: 0, build: "" };
}
export function arbAccepted(g: Gen): Accepted {
  const version = g.u32();
  const caps = g.bits();
  const idle_ms = g.u32();
  const born = g.u53();
  const fragment = g.u32();
  const build = g.str();
  return { version, caps, idle_ms, born, fragment, build };
}
export function putAcceptedBody(w: Writer, v: Accepted, version: number): void {
  w.u32(v.version);
  w.bits(v.caps);
  w.u32(v.idle_ms);
  w.u53(v.born);
  w.u32(v.fragment);
  w.str(v.build);
  void version;
}
export function takeAcceptedBody(r: Reader): Accepted {
  const v = defaultAccepted();
  if (r.done()) return v;
  v.version = r.u32();
  if (r.done()) return v;
  v.caps = r.bits();
  if (r.done()) return v;
  v.idle_ms = r.u32();
  if (r.done()) return v;
  v.born = r.u53();
  if (r.done()) return v;
  v.fragment = r.u32();
  if (r.done()) return v;
  v.build = r.str();
  return v;
}

export interface Answer { id: number; outcome: Outcome; at: Mark | null }
export function putAnswer(w: Writer, v: Answer): void {
  w.u53(v.id);
  putOutcome(w, v.outcome);
  w.opt(v.at, (x0) => putMark(w, x0));
}
export function takeAnswer(r: Reader): Answer {
  const id = r.u53();
  const outcome = takeOutcome(r);
  const at = r.opt(() => takeMark(r));
  return { id, outcome, at };
}
export function defaultAnswer(): Answer {
  return { id: 0, outcome: defaultOutcome(), at: null };
}
export function arbAnswer(g: Gen): Answer {
  const id = g.u53();
  const outcome = arbOutcome(g);
  const at = g.opt(() => arbMark(g));
  return { id, outcome, at };
}
export function putAnswerBody(w: Writer, v: Answer, version: number): void {
  w.u53(v.id);
  putOutcome(w, v.outcome);
  w.opt(v.at, (x0) => putMark(w, x0));
  void version;
}
export function takeAnswerBody(r: Reader): Answer {
  const v = defaultAnswer();
  if (r.done()) return v;
  v.id = r.u53();
  if (r.done()) return v;
  v.outcome = takeOutcome(r);
  if (r.done()) return v;
  v.at = r.opt(() => takeMark(r));
  return v;
}

export interface Begin { sub: number; cursor: Cursor }
export function putBegin(w: Writer, v: Begin): void {
  w.u32(v.sub);
  putCursor(w, v.cursor);
}
export function takeBegin(r: Reader): Begin {
  const sub = r.u32();
  const cursor = takeCursor(r);
  return { sub, cursor };
}
export function defaultBegin(): Begin {
  return { sub: 0, cursor: defaultCursor() };
}
export function arbBegin(g: Gen): Begin {
  const sub = g.u32();
  const cursor = arbCursor(g);
  return { sub, cursor };
}
export function putBeginBody(w: Writer, v: Begin, version: number): void {
  w.u32(v.sub);
  putCursor(w, v.cursor);
  void version;
}
export function takeBeginBody(r: Reader): Begin {
  const v = defaultBegin();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.cursor = takeCursor(r);
  return v;
}

export type Call = { t: "Http"; method: Method; path: string; body: Uint8Array } | { t: "Command"; session: string; command: Uint8Array };
export function putCall(w: Writer, v: Call): void {
  switch (v.t) {
    case "Http":
      w.u32(0);
      putMethod(w, v.method);
      w.str(v.path);
      w.bytes(v.body);
      return;
    case "Command":
      w.u32(1);
      w.str(v.session);
      w.bytes(v.command);
      return;
  }
}
export function takeCall(r: Reader): Call {
  switch (r.variant()) {
    case 0: {
      const method = takeMethod(r);
      const path = r.str();
      const body = r.bytes();
      return { t: "Http", method, path, body };
    }
    case 1: {
      const session = r.str();
      const command = r.bytes();
      return { t: "Command", session, command };
    }
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultCall(): Call {
  return { t: "Http", method: defaultMethod(), path: "", body: new Uint8Array(0) };
}
export function arbCall(g: Gen): Call {
  switch (g.index(2)) {
    case 0: {
      const method = arbMethod(g);
      const path = g.str();
      const body = g.bytes();
      return { t: "Http", method, path, body };
    }
    case 1: {
      const session = g.str();
      const command = g.bytes();
      return { t: "Command", session, command };
    }
    default:
      return defaultCall();
  }
}

export interface Change { sub: number; rev: number; step: number; delta: Delta }
export function putChange(w: Writer, v: Change): void {
  w.u32(v.sub);
  w.u53(v.rev);
  w.u53(v.step);
  putDelta(w, v.delta);
}
export function takeChange(r: Reader): Change {
  const sub = r.u32();
  const rev = r.u53();
  const step = r.u53();
  const delta = takeDelta(r);
  return { sub, rev, step, delta };
}
export function defaultChange(): Change {
  return { sub: 0, rev: 0, step: 0, delta: defaultDelta() };
}
export function arbChange(g: Gen): Change {
  const sub = g.u32();
  const rev = g.u53();
  const step = g.u53();
  const delta = arbDelta(g);
  return { sub, rev, step, delta };
}
export function putChangeBody(w: Writer, v: Change, version: number): void {
  w.u32(v.sub);
  w.u53(v.rev);
  w.u53(v.step);
  putDelta(w, v.delta);
  void version;
}
export function takeChangeBody(r: Reader): Change {
  const v = defaultChange();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.rev = r.u53();
  if (r.done()) return v;
  v.step = r.u53();
  if (r.done()) return v;
  v.delta = takeDelta(r);
  return v;
}

export interface Close { code: CloseCode; why: string; build: string }
export function putClose(w: Writer, v: Close): void {
  putCloseCode(w, v.code);
  w.str(v.why);
  w.str(v.build);
}
export function takeClose(r: Reader): Close {
  const code = takeCloseCode(r);
  const why = r.str();
  const build = r.str();
  return { code, why, build };
}
export function defaultClose(): Close {
  return { code: defaultCloseCode(), why: "", build: "" };
}
export function arbClose(g: Gen): Close {
  const code = arbCloseCode(g);
  const why = g.str();
  const build = g.str();
  return { code, why, build };
}
export function putCloseBody(w: Writer, v: Close, version: number): void {
  putCloseCode(w, v.code);
  w.str(v.why);
  w.str(v.build);
  void version;
}
export function takeCloseBody(r: Reader): Close {
  const v = defaultClose();
  if (r.done()) return v;
  v.code = takeCloseCode(r);
  if (r.done()) return v;
  v.why = r.str();
  if (r.done()) return v;
  v.build = r.str();
  return v;
}

export type CloseCode = { t: "Bye" } | { t: "Expired" } | { t: "SwitchBuild" } | { t: "Revoked" } | { t: "Shutdown" } | { t: "Protocol" };
export function putCloseCode(w: Writer, v: CloseCode): void {
  switch (v.t) {
    case "Bye":
      w.u32(0);
      return;
    case "Expired":
      w.u32(1);
      return;
    case "SwitchBuild":
      w.u32(2);
      return;
    case "Revoked":
      w.u32(3);
      return;
    case "Shutdown":
      w.u32(4);
      return;
    case "Protocol":
      w.u32(5);
      return;
  }
}
export function takeCloseCode(r: Reader): CloseCode {
  switch (r.variant()) {
    case 0:
      return { t: "Bye" };
    case 1:
      return { t: "Expired" };
    case 2:
      return { t: "SwitchBuild" };
    case 3:
      return { t: "Revoked" };
    case 4:
      return { t: "Shutdown" };
    case 5:
      return { t: "Protocol" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultCloseCode(): CloseCode {
  return { t: "Bye" };
}
export function arbCloseCode(g: Gen): CloseCode {
  switch (g.index(6)) {
    case 0:
      return { t: "Bye" };
    case 1:
      return { t: "Expired" };
    case 2:
      return { t: "SwitchBuild" };
    case 3:
      return { t: "Revoked" };
    case 4:
      return { t: "Shutdown" };
    case 5:
      return { t: "Protocol" };
    default:
      return defaultCloseCode();
  }
}

export interface Compressed { ctx: number; len: number; data: Uint8Array }
export function putCompressed(w: Writer, v: Compressed): void {
  w.u8(v.ctx);
  w.u32(v.len);
  w.bytes(v.data);
}
export function takeCompressed(r: Reader): Compressed {
  const ctx = r.u8();
  const len = r.u32();
  const data = r.bytes();
  return { ctx, len, data };
}
export function defaultCompressed(): Compressed {
  return { ctx: 0, len: 0, data: new Uint8Array(0) };
}
export function arbCompressed(g: Gen): Compressed {
  const ctx = g.u8();
  const len = g.u32();
  const data = g.bytes();
  return { ctx, len, data };
}
export function putCompressedBody(w: Writer, v: Compressed, version: number): void {
  w.u8(v.ctx);
  w.u32(v.len);
  w.bytes(v.data);
  void version;
}
export function takeCompressedBody(r: Reader): Compressed {
  const v = defaultCompressed();
  if (r.done()) return v;
  v.ctx = r.u8();
  if (r.done()) return v;
  v.len = r.u32();
  if (r.done()) return v;
  v.data = r.bytes();
  return v;
}

export interface Credit { sub: number; upto: number }
export function putCredit(w: Writer, v: Credit): void {
  w.u32(v.sub);
  w.u53(v.upto);
}
export function takeCredit(r: Reader): Credit {
  const sub = r.u32();
  const upto = r.u53();
  return { sub, upto };
}
export function defaultCredit(): Credit {
  return { sub: 0, upto: 0 };
}
export function arbCredit(g: Gen): Credit {
  const sub = g.u32();
  const upto = g.u53();
  return { sub, upto };
}
export function putCreditBody(w: Writer, v: Credit, version: number): void {
  w.u32(v.sub);
  w.u53(v.upto);
  void version;
}
export function takeCreditBody(r: Reader): Credit {
  const v = defaultCredit();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.upto = r.u53();
  return v;
}

export interface Cursor { born: number; rev: number }
export function putCursor(w: Writer, v: Cursor): void {
  w.u53(v.born);
  w.u53(v.rev);
}
export function takeCursor(r: Reader): Cursor {
  const born = r.u53();
  const rev = r.u53();
  return { born, rev };
}
export function defaultCursor(): Cursor {
  return { born: 0, rev: 0 };
}
export function arbCursor(g: Gen): Cursor {
  const born = g.u53();
  const rev = g.u53();
  return { born, rev };
}

export type Delta = { t: "Set"; mask: bigint; fields: Uint8Array[] } | { t: "Put"; key: Key; fields: Uint8Array[] } | { t: "Patch"; key: Key; mask: bigint; fields: Uint8Array[] } | { t: "Remove"; key: Key } | { t: "Append"; at: number; data: Uint8Array } | { t: "Extend"; key: Key; field: number; at: number; data: Uint8Array };
export function putDelta(w: Writer, v: Delta): void {
  switch (v.t) {
    case "Set":
      w.u32(0);
      w.bits(v.mask);
      w.vec(v.fields, (x0) => w.bytes(x0));
      return;
    case "Put":
      w.u32(1);
      putKey(w, v.key);
      w.vec(v.fields, (x0) => w.bytes(x0));
      return;
    case "Patch":
      w.u32(2);
      putKey(w, v.key);
      w.bits(v.mask);
      w.vec(v.fields, (x0) => w.bytes(x0));
      return;
    case "Remove":
      w.u32(3);
      putKey(w, v.key);
      return;
    case "Append":
      w.u32(4);
      w.u53(v.at);
      w.bytes(v.data);
      return;
    case "Extend":
      w.u32(5);
      putKey(w, v.key);
      w.u32(v.field);
      w.u53(v.at);
      w.bytes(v.data);
      return;
  }
}
export function takeDelta(r: Reader): Delta {
  switch (r.variant()) {
    case 0: {
      const mask = r.bits();
      const fields = r.vec(() => r.bytes());
      return { t: "Set", mask, fields };
    }
    case 1: {
      const key = takeKey(r);
      const fields = r.vec(() => r.bytes());
      return { t: "Put", key, fields };
    }
    case 2: {
      const key = takeKey(r);
      const mask = r.bits();
      const fields = r.vec(() => r.bytes());
      return { t: "Patch", key, mask, fields };
    }
    case 3: {
      const key = takeKey(r);
      return { t: "Remove", key };
    }
    case 4: {
      const at = r.u53();
      const data = r.bytes();
      return { t: "Append", at, data };
    }
    case 5: {
      const key = takeKey(r);
      const field = r.u32();
      const at = r.u53();
      const data = r.bytes();
      return { t: "Extend", key, field, at, data };
    }
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultDelta(): Delta {
  return { t: "Set", mask: 0n, fields: [] };
}
export function arbDelta(g: Gen): Delta {
  switch (g.index(6)) {
    case 0: {
      const mask = g.bits();
      const fields = g.vec(() => g.bytes());
      return { t: "Set", mask, fields };
    }
    case 1: {
      const key = arbKey(g);
      const fields = g.vec(() => g.bytes());
      return { t: "Put", key, fields };
    }
    case 2: {
      const key = arbKey(g);
      const mask = g.bits();
      const fields = g.vec(() => g.bytes());
      return { t: "Patch", key, mask, fields };
    }
    case 3: {
      const key = arbKey(g);
      return { t: "Remove", key };
    }
    case 4: {
      const at = g.u53();
      const data = g.bytes();
      return { t: "Append", at, data };
    }
    case 5: {
      const key = arbKey(g);
      const field = g.u32();
      const at = g.u53();
      const data = g.bytes();
      return { t: "Extend", key, field, at, data };
    }
    default:
      return defaultDelta();
  }
}

export interface End { sub: number; code: EndCode; why: string }
export function putEnd(w: Writer, v: End): void {
  w.u32(v.sub);
  putEndCode(w, v.code);
  w.str(v.why);
}
export function takeEnd(r: Reader): End {
  const sub = r.u32();
  const code = takeEndCode(r);
  const why = r.str();
  return { sub, code, why };
}
export function defaultEnd(): End {
  return { sub: 0, code: defaultEndCode(), why: "" };
}
export function arbEnd(g: Gen): End {
  const sub = g.u32();
  const code = arbEndCode(g);
  const why = g.str();
  return { sub, code, why };
}
export function putEndBody(w: Writer, v: End, version: number): void {
  w.u32(v.sub);
  putEndCode(w, v.code);
  w.str(v.why);
  void version;
}
export function takeEndBody(r: Reader): End {
  const v = defaultEnd();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.code = takeEndCode(r);
  if (r.done()) return v;
  v.why = r.str();
  return v;
}

export type EndCode = { t: "Done" } | { t: "Gone" } | { t: "Denied" } | { t: "Unknown" } | { t: "Limit" };
export function putEndCode(w: Writer, v: EndCode): void {
  switch (v.t) {
    case "Done":
      w.u32(0);
      return;
    case "Gone":
      w.u32(1);
      return;
    case "Denied":
      w.u32(2);
      return;
    case "Unknown":
      w.u32(3);
      return;
    case "Limit":
      w.u32(4);
      return;
  }
}
export function takeEndCode(r: Reader): EndCode {
  switch (r.variant()) {
    case 0:
      return { t: "Done" };
    case 1:
      return { t: "Gone" };
    case 2:
      return { t: "Denied" };
    case 3:
      return { t: "Unknown" };
    case 4:
      return { t: "Limit" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultEndCode(): EndCode {
  return { t: "Done" };
}
export function arbEndCode(g: Gen): EndCode {
  switch (g.index(5)) {
    case 0:
      return { t: "Done" };
    case 1:
      return { t: "Gone" };
    case 2:
      return { t: "Denied" };
    case 3:
      return { t: "Unknown" };
    case 4:
      return { t: "Limit" };
    default:
      return defaultEndCode();
  }
}

export interface Fragment { lane: number; last: boolean; data: Uint8Array }
export function putFragment(w: Writer, v: Fragment): void {
  w.u32(v.lane);
  w.bool(v.last);
  w.bytes(v.data);
}
export function takeFragment(r: Reader): Fragment {
  const lane = r.u32();
  const last = r.bool();
  const data = r.bytes();
  return { lane, last, data };
}
export function defaultFragment(): Fragment {
  return { lane: 0, last: false, data: new Uint8Array(0) };
}
export function arbFragment(g: Gen): Fragment {
  const lane = g.u32();
  const last = g.bool();
  const data = g.bytes();
  return { lane, last, data };
}
export function putFragmentBody(w: Writer, v: Fragment, version: number): void {
  w.u32(v.lane);
  w.bool(v.last);
  w.bytes(v.data);
  void version;
}
export function takeFragmentBody(r: Reader): Fragment {
  const v = defaultFragment();
  if (r.done()) return v;
  v.lane = r.u32();
  if (r.done()) return v;
  v.last = r.bool();
  if (r.done()) return v;
  v.data = r.bytes();
  return v;
}

export interface Idle { got: number }
export function putIdle(w: Writer, v: Idle): void {
  w.u53(v.got);
}
export function takeIdle(r: Reader): Idle {
  const got = r.u53();
  return { got };
}
export function defaultIdle(): Idle {
  return { got: 0 };
}
export function arbIdle(g: Gen): Idle {
  const got = g.u53();
  return { got };
}
export function putIdleBody(w: Writer, v: Idle, version: number): void {
  w.u53(v.got);
  void version;
}
export function takeIdleBody(r: Reader): Idle {
  const v = defaultIdle();
  if (r.done()) return v;
  v.got = r.u53();
  return v;
}

export interface Input { sub: number; data: Uint8Array }
export function putInput(w: Writer, v: Input): void {
  w.u32(v.sub);
  w.bytes(v.data);
}
export function takeInput(r: Reader): Input {
  const sub = r.u32();
  const data = r.bytes();
  return { sub, data };
}
export function defaultInput(): Input {
  return { sub: 0, data: new Uint8Array(0) };
}
export function arbInput(g: Gen): Input {
  const sub = g.u32();
  const data = g.bytes();
  return { sub, data };
}
export function putInputBody(w: Writer, v: Input, version: number): void {
  w.u32(v.sub);
  w.bytes(v.data);
  void version;
}
export function takeInputBody(r: Reader): Input {
  const v = defaultInput();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.data = r.bytes();
  return v;
}

export type Key = { t: "Num"; n: number } | { t: "Str"; s: string } | { t: "Ref"; n: number };
export function putKey(w: Writer, v: Key): void {
  switch (v.t) {
    case "Num":
      w.u32(0);
      w.u53(v.n);
      return;
    case "Str":
      w.u32(1);
      w.str(v.s);
      return;
    case "Ref":
      w.u32(2);
      w.u32(v.n);
      return;
  }
}
export function takeKey(r: Reader): Key {
  switch (r.variant()) {
    case 0: {
      const n = r.u53();
      return { t: "Num", n };
    }
    case 1: {
      const s = r.str();
      return { t: "Str", s };
    }
    case 2: {
      const n = r.u32();
      return { t: "Ref", n };
    }
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultKey(): Key {
  return { t: "Num", n: 0 };
}
export function arbKey(g: Gen): Key {
  switch (g.index(3)) {
    case 0: {
      const n = g.u53();
      return { t: "Num", n };
    }
    case 1: {
      const s = g.str();
      return { t: "Str", s };
    }
    case 2: {
      const n = g.u32();
      return { t: "Ref", n };
    }
    default:
      return defaultKey();
  }
}

export interface Mark { topic: Topic; cursor: Cursor }
export function putMark(w: Writer, v: Mark): void {
  putTopic(w, v.topic);
  putCursor(w, v.cursor);
}
export function takeMark(r: Reader): Mark {
  const topic = takeTopic(r);
  const cursor = takeCursor(r);
  return { topic, cursor };
}
export function defaultMark(): Mark {
  return { topic: defaultTopic(), cursor: defaultCursor() };
}
export function arbMark(g: Gen): Mark {
  const topic = arbTopic(g);
  const cursor = arbCursor(g);
  return { topic, cursor };
}

export type Method = { t: "Get" } | { t: "Post" } | { t: "Put" } | { t: "Patch" } | { t: "Delete" };
export function putMethod(w: Writer, v: Method): void {
  switch (v.t) {
    case "Get":
      w.u32(0);
      return;
    case "Post":
      w.u32(1);
      return;
    case "Put":
      w.u32(2);
      return;
    case "Patch":
      w.u32(3);
      return;
    case "Delete":
      w.u32(4);
      return;
  }
}
export function takeMethod(r: Reader): Method {
  switch (r.variant()) {
    case 0:
      return { t: "Get" };
    case 1:
      return { t: "Post" };
    case 2:
      return { t: "Put" };
    case 3:
      return { t: "Patch" };
    case 4:
      return { t: "Delete" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultMethod(): Method {
  return { t: "Get" };
}
export function arbMethod(g: Gen): Method {
  switch (g.index(5)) {
    case 0:
      return { t: "Get" };
    case 1:
      return { t: "Post" };
    case 2:
      return { t: "Put" };
    case 3:
      return { t: "Patch" };
    case 4:
      return { t: "Delete" };
    default:
      return defaultMethod();
  }
}

export interface Open { version: number; oldest: number; caps: bigint; schema: bigint; ticket: Uint8Array; idle_ms: number; window: number; build: string }
export function putOpen(w: Writer, v: Open): void {
  w.u32(v.version);
  w.u32(v.oldest);
  w.bits(v.caps);
  w.bits(v.schema);
  w.bytes(v.ticket);
  w.u32(v.idle_ms);
  w.u53(v.window);
  w.str(v.build);
}
export function takeOpen(r: Reader): Open {
  const version = r.u32();
  const oldest = r.u32();
  const caps = r.bits();
  const schema = r.bits();
  const ticket = r.bytes();
  const idle_ms = r.u32();
  const window = r.u53();
  const build = r.str();
  return { version, oldest, caps, schema, ticket, idle_ms, window, build };
}
export function defaultOpen(): Open {
  return { version: 0, oldest: 0, caps: 0n, schema: 0n, ticket: new Uint8Array(0), idle_ms: 0, window: 0, build: "" };
}
export function arbOpen(g: Gen): Open {
  const version = g.u32();
  const oldest = g.u32();
  const caps = g.bits();
  const schema = g.bits();
  const ticket = g.bytes();
  const idle_ms = g.u32();
  const window = g.u53();
  const build = g.str();
  return { version, oldest, caps, schema, ticket, idle_ms, window, build };
}
export function putOpenBody(w: Writer, v: Open, version: number): void {
  w.u32(v.version);
  w.u32(v.oldest);
  w.bits(v.caps);
  w.bits(v.schema);
  w.bytes(v.ticket);
  w.u32(v.idle_ms);
  w.u53(v.window);
  w.str(v.build);
  void version;
}
export function takeOpenBody(r: Reader): Open {
  const v = defaultOpen();
  if (r.done()) return v;
  v.version = r.u32();
  if (r.done()) return v;
  v.oldest = r.u32();
  if (r.done()) return v;
  v.caps = r.bits();
  if (r.done()) return v;
  v.schema = r.bits();
  if (r.done()) return v;
  v.ticket = r.bytes();
  if (r.done()) return v;
  v.idle_ms = r.u32();
  if (r.done()) return v;
  v.window = r.u53();
  if (r.done()) return v;
  v.build = r.str();
  return v;
}

export type OpenRefusal = { t: "BadTicket" } | { t: "Expired" } | { t: "Reused" } | { t: "TooOld" } | { t: "TooNew" } | { t: "Schema" } | { t: "Busy" } | { t: "Malformed" };
export function putOpenRefusal(w: Writer, v: OpenRefusal): void {
  switch (v.t) {
    case "BadTicket":
      w.u32(0);
      return;
    case "Expired":
      w.u32(1);
      return;
    case "Reused":
      w.u32(2);
      return;
    case "TooOld":
      w.u32(3);
      return;
    case "TooNew":
      w.u32(4);
      return;
    case "Schema":
      w.u32(5);
      return;
    case "Busy":
      w.u32(6);
      return;
    case "Malformed":
      w.u32(7);
      return;
  }
}
export function takeOpenRefusal(r: Reader): OpenRefusal {
  switch (r.variant()) {
    case 0:
      return { t: "BadTicket" };
    case 1:
      return { t: "Expired" };
    case 2:
      return { t: "Reused" };
    case 3:
      return { t: "TooOld" };
    case 4:
      return { t: "TooNew" };
    case 5:
      return { t: "Schema" };
    case 6:
      return { t: "Busy" };
    case 7:
      return { t: "Malformed" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultOpenRefusal(): OpenRefusal {
  return { t: "BadTicket" };
}
export function arbOpenRefusal(g: Gen): OpenRefusal {
  switch (g.index(8)) {
    case 0:
      return { t: "BadTicket" };
    case 1:
      return { t: "Expired" };
    case 2:
      return { t: "Reused" };
    case 3:
      return { t: "TooOld" };
    case 4:
      return { t: "TooNew" };
    case 5:
      return { t: "Schema" };
    case 6:
      return { t: "Busy" };
    case 7:
      return { t: "Malformed" };
    default:
      return defaultOpenRefusal();
  }
}

export type Outcome = { t: "Taken" } | { t: "Accepted"; epoch: number; seq: number; at: number; repeat: boolean } | { t: "Unchecked" } | { t: "Pending" } | { t: "Parked" } | { t: "Nack"; code: string; why: string; again: boolean } | { t: "Reply"; status: number; body: Uint8Array };
export function putOutcome(w: Writer, v: Outcome): void {
  switch (v.t) {
    case "Taken":
      w.u32(0);
      return;
    case "Accepted":
      w.u32(1);
      w.u53(v.epoch);
      w.u53(v.seq);
      w.i53(v.at);
      w.bool(v.repeat);
      return;
    case "Unchecked":
      w.u32(2);
      return;
    case "Pending":
      w.u32(3);
      return;
    case "Parked":
      w.u32(4);
      return;
    case "Nack":
      w.u32(5);
      w.str(v.code);
      w.str(v.why);
      w.bool(v.again);
      return;
    case "Reply":
      w.u32(6);
      w.u16(v.status);
      w.bytes(v.body);
      return;
  }
}
export function takeOutcome(r: Reader): Outcome {
  switch (r.variant()) {
    case 0:
      return { t: "Taken" };
    case 1: {
      const epoch = r.u53();
      const seq = r.u53();
      const at = r.i53();
      const repeat = r.bool();
      return { t: "Accepted", epoch, seq, at, repeat };
    }
    case 2:
      return { t: "Unchecked" };
    case 3:
      return { t: "Pending" };
    case 4:
      return { t: "Parked" };
    case 5: {
      const code = r.str();
      const why = r.str();
      const again = r.bool();
      return { t: "Nack", code, why, again };
    }
    case 6: {
      const status = r.u16();
      const body = r.bytes();
      return { t: "Reply", status, body };
    }
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultOutcome(): Outcome {
  return { t: "Taken" };
}
export function arbOutcome(g: Gen): Outcome {
  switch (g.index(7)) {
    case 0:
      return { t: "Taken" };
    case 1: {
      const epoch = g.u53();
      const seq = g.u53();
      const at = g.i53();
      const repeat = g.bool();
      return { t: "Accepted", epoch, seq, at, repeat };
    }
    case 2:
      return { t: "Unchecked" };
    case 3:
      return { t: "Pending" };
    case 4:
      return { t: "Parked" };
    case 5: {
      const code = g.str();
      const why = g.str();
      const again = g.bool();
      return { t: "Nack", code, why, again };
    }
    case 6: {
      const status = g.u16();
      const body = g.bytes();
      return { t: "Reply", status, body };
    }
    default:
      return defaultOutcome();
  }
}

export interface Probe { nonce: number }
export function putProbe(w: Writer, v: Probe): void {
  w.u32(v.nonce);
}
export function takeProbe(r: Reader): Probe {
  const nonce = r.u32();
  return { nonce };
}
export function defaultProbe(): Probe {
  return { nonce: 0 };
}
export function arbProbe(g: Gen): Probe {
  const nonce = g.u32();
  return { nonce };
}
export function putProbeBody(w: Writer, v: Probe, version: number): void {
  w.u32(v.nonce);
  void version;
}
export function takeProbeBody(r: Reader): Probe {
  const v = defaultProbe();
  if (r.done()) return v;
  v.nonce = r.u32();
  return v;
}

export interface ProbeAck { nonce: number; got: number }
export function putProbeAck(w: Writer, v: ProbeAck): void {
  w.u32(v.nonce);
  w.u53(v.got);
}
export function takeProbeAck(r: Reader): ProbeAck {
  const nonce = r.u32();
  const got = r.u53();
  return { nonce, got };
}
export function defaultProbeAck(): ProbeAck {
  return { nonce: 0, got: 0 };
}
export function arbProbeAck(g: Gen): ProbeAck {
  const nonce = g.u32();
  const got = g.u53();
  return { nonce, got };
}
export function putProbeAckBody(w: Writer, v: ProbeAck, version: number): void {
  w.u32(v.nonce);
  w.u53(v.got);
  void version;
}
export function takeProbeAckBody(r: Reader): ProbeAck {
  const v = defaultProbeAck();
  if (r.done()) return v;
  v.nonce = r.u32();
  if (r.done()) return v;
  v.got = r.u53();
  return v;
}

export interface Refused { code: OpenRefusal; why: string; build: string }
export function putRefused(w: Writer, v: Refused): void {
  putOpenRefusal(w, v.code);
  w.str(v.why);
  w.str(v.build);
}
export function takeRefused(r: Reader): Refused {
  const code = takeOpenRefusal(r);
  const why = r.str();
  const build = r.str();
  return { code, why, build };
}
export function defaultRefused(): Refused {
  return { code: defaultOpenRefusal(), why: "", build: "" };
}
export function arbRefused(g: Gen): Refused {
  const code = arbOpenRefusal(g);
  const why = g.str();
  const build = g.str();
  return { code, why, build };
}
export function putRefusedBody(w: Writer, v: Refused, version: number): void {
  putOpenRefusal(w, v.code);
  w.str(v.why);
  w.str(v.build);
  void version;
}
export function takeRefusedBody(r: Reader): Refused {
  const v = defaultRefused();
  if (r.done()) return v;
  v.code = takeOpenRefusal(r);
  if (r.done()) return v;
  v.why = r.str();
  if (r.done()) return v;
  v.build = r.str();
  return v;
}

export interface Request { id: number; call: Call }
export function putRequest(w: Writer, v: Request): void {
  w.u53(v.id);
  putCall(w, v.call);
}
export function takeRequest(r: Reader): Request {
  const id = r.u53();
  const call = takeCall(r);
  return { id, call };
}
export function defaultRequest(): Request {
  return { id: 0, call: defaultCall() };
}
export function arbRequest(g: Gen): Request {
  const id = g.u53();
  const call = arbCall(g);
  return { id, call };
}
export function putRequestBody(w: Writer, v: Request, version: number): void {
  w.u53(v.id);
  putCall(w, v.call);
  void version;
}
export function takeRequestBody(r: Reader): Request {
  const v = defaultRequest();
  if (r.done()) return v;
  v.id = r.u53();
  if (r.done()) return v;
  v.call = takeCall(r);
  return v;
}

export interface Resize { sub: number; cols: number; rows: number }
export function putResize(w: Writer, v: Resize): void {
  w.u32(v.sub);
  w.u16(v.cols);
  w.u16(v.rows);
}
export function takeResize(r: Reader): Resize {
  const sub = r.u32();
  const cols = r.u16();
  const rows = r.u16();
  return { sub, cols, rows };
}
export function defaultResize(): Resize {
  return { sub: 0, cols: 0, rows: 0 };
}
export function arbResize(g: Gen): Resize {
  const sub = g.u32();
  const cols = g.u16();
  const rows = g.u16();
  return { sub, cols, rows };
}
export function putResizeBody(w: Writer, v: Resize, version: number): void {
  w.u32(v.sub);
  w.u16(v.cols);
  w.u16(v.rows);
  void version;
}
export function takeResizeBody(r: Reader): Resize {
  const v = defaultResize();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.cols = r.u16();
  if (r.done()) return v;
  v.rows = r.u16();
  return v;
}

export interface Subscribe { sub: number; topic: Topic; cursor: Cursor | null; credit: number }
export function putSubscribe(w: Writer, v: Subscribe): void {
  w.u32(v.sub);
  putTopic(w, v.topic);
  w.opt(v.cursor, (x0) => putCursor(w, x0));
  w.u53(v.credit);
}
export function takeSubscribe(r: Reader): Subscribe {
  const sub = r.u32();
  const topic = takeTopic(r);
  const cursor = r.opt(() => takeCursor(r));
  const credit = r.u53();
  return { sub, topic, cursor, credit };
}
export function defaultSubscribe(): Subscribe {
  return { sub: 0, topic: defaultTopic(), cursor: null, credit: 0 };
}
export function arbSubscribe(g: Gen): Subscribe {
  const sub = g.u32();
  const topic = arbTopic(g);
  const cursor = g.opt(() => arbCursor(g));
  const credit = g.u53();
  return { sub, topic, cursor, credit };
}
export function putSubscribeBody(w: Writer, v: Subscribe, version: number): void {
  w.u32(v.sub);
  putTopic(w, v.topic);
  w.opt(v.cursor, (x0) => putCursor(w, x0));
  w.u53(v.credit);
  void version;
}
export function takeSubscribeBody(r: Reader): Subscribe {
  const v = defaultSubscribe();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.topic = takeTopic(r);
  if (r.done()) return v;
  v.cursor = r.opt(() => takeCursor(r));
  if (r.done()) return v;
  v.credit = r.u53();
  return v;
}

export interface Tail { sub: number; data: Uint8Array }
export function putTail(w: Writer, v: Tail): void {
  w.u32(v.sub);
  w.bytes(v.data);
}
export function takeTail(r: Reader): Tail {
  const sub = r.u32();
  const data = r.bytes();
  return { sub, data };
}
export function defaultTail(): Tail {
  return { sub: 0, data: new Uint8Array(0) };
}
export function arbTail(g: Gen): Tail {
  const sub = g.u32();
  const data = g.bytes();
  return { sub, data };
}
export function putTailBody(w: Writer, v: Tail, version: number): void {
  w.u32(v.sub);
  w.bytes(v.data);
  void version;
}
export function takeTailBody(r: Reader): Tail {
  const v = defaultTail();
  if (r.done()) return v;
  v.sub = r.u32();
  if (r.done()) return v;
  v.data = r.bytes();
  return v;
}

export interface Topic { kind: number; key: string }
export function putTopic(w: Writer, v: Topic): void {
  w.u32(v.kind);
  w.str(v.key);
}
export function takeTopic(r: Reader): Topic {
  const kind = r.u32();
  const key = r.str();
  return { kind, key };
}
export function defaultTopic(): Topic {
  return { kind: 0, key: "" };
}
export function arbTopic(g: Gen): Topic {
  const kind = g.u32();
  const key = g.str();
  return { kind, key };
}

export interface Unread { kind: number; code: UnreadCode; why: UnreadWhy }
export function putUnread(w: Writer, v: Unread): void {
  w.u32(v.kind);
  putUnreadCode(w, v.code);
  putUnreadWhy(w, v.why);
}
export function takeUnread(r: Reader): Unread {
  const kind = r.u32();
  const code = takeUnreadCode(r);
  const why = takeUnreadWhy(r);
  return { kind, code, why };
}
export function defaultUnread(): Unread {
  return { kind: 0, code: defaultUnreadCode(), why: defaultUnreadWhy() };
}
export function arbUnread(g: Gen): Unread {
  const kind = g.u32();
  const code = arbUnreadCode(g);
  const why = arbUnreadWhy(g);
  return { kind, code, why };
}
export function putUnreadBody(w: Writer, v: Unread, version: number): void {
  w.u32(v.kind);
  putUnreadCode(w, v.code);
  putUnreadWhy(w, v.why);
  void version;
}
export function takeUnreadBody(r: Reader): Unread {
  const v = defaultUnread();
  if (r.done()) return v;
  v.kind = r.u32();
  if (r.done()) return v;
  v.code = takeUnreadCode(r);
  if (r.done()) return v;
  v.why = takeUnreadWhy(r);
  return v;
}

export type UnreadCode = { t: "Unknown" } | { t: "Malformed" };
export function putUnreadCode(w: Writer, v: UnreadCode): void {
  switch (v.t) {
    case "Unknown":
      w.u32(0);
      return;
    case "Malformed":
      w.u32(1);
      return;
  }
}
export function takeUnreadCode(r: Reader): UnreadCode {
  switch (r.variant()) {
    case 0:
      return { t: "Unknown" };
    case 1:
      return { t: "Malformed" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultUnreadCode(): UnreadCode {
  return { t: "Unknown" };
}
export function arbUnreadCode(g: Gen): UnreadCode {
  switch (g.index(2)) {
    case 0:
      return { t: "Unknown" };
    case 1:
      return { t: "Malformed" };
    default:
      return defaultUnreadCode();
  }
}

export type UnreadWhy = { t: "CutShort" } | { t: "TooLarge" } | { t: "BadVarint" } | { t: "BadValue" } | { t: "TooDeep" } | { t: "UnknownKind" } | { t: "UnknownVariant" } | { t: "LeftOver" } | { t: "OutOfOrder" };
export function putUnreadWhy(w: Writer, v: UnreadWhy): void {
  switch (v.t) {
    case "CutShort":
      w.u32(0);
      return;
    case "TooLarge":
      w.u32(1);
      return;
    case "BadVarint":
      w.u32(2);
      return;
    case "BadValue":
      w.u32(3);
      return;
    case "TooDeep":
      w.u32(4);
      return;
    case "UnknownKind":
      w.u32(5);
      return;
    case "UnknownVariant":
      w.u32(6);
      return;
    case "LeftOver":
      w.u32(7);
      return;
    case "OutOfOrder":
      w.u32(8);
      return;
  }
}
export function takeUnreadWhy(r: Reader): UnreadWhy {
  switch (r.variant()) {
    case 0:
      return { t: "CutShort" };
    case 1:
      return { t: "TooLarge" };
    case 2:
      return { t: "BadVarint" };
    case 3:
      return { t: "BadValue" };
    case 4:
      return { t: "TooDeep" };
    case 5:
      return { t: "UnknownKind" };
    case 6:
      return { t: "UnknownVariant" };
    case 7:
      return { t: "LeftOver" };
    case 8:
      return { t: "OutOfOrder" };
    default:
      throw new BpError(Why.UnknownVariant);
  }
}
export function defaultUnreadWhy(): UnreadWhy {
  return { t: "CutShort" };
}
export function arbUnreadWhy(g: Gen): UnreadWhy {
  switch (g.index(9)) {
    case 0:
      return { t: "CutShort" };
    case 1:
      return { t: "TooLarge" };
    case 2:
      return { t: "BadVarint" };
    case 3:
      return { t: "BadValue" };
    case 4:
      return { t: "TooDeep" };
    case 5:
      return { t: "UnknownKind" };
    case 6:
      return { t: "UnknownVariant" };
    case 7:
      return { t: "LeftOver" };
    case 8:
      return { t: "OutOfOrder" };
    default:
      return defaultUnreadWhy();
  }
}

export interface Unsubscribe { sub: number }
export function putUnsubscribe(w: Writer, v: Unsubscribe): void {
  w.u32(v.sub);
}
export function takeUnsubscribe(r: Reader): Unsubscribe {
  const sub = r.u32();
  return { sub };
}
export function defaultUnsubscribe(): Unsubscribe {
  return { sub: 0 };
}
export function arbUnsubscribe(g: Gen): Unsubscribe {
  const sub = g.u32();
  return { sub };
}
export function putUnsubscribeBody(w: Writer, v: Unsubscribe, version: number): void {
  w.u32(v.sub);
  void version;
}
export function takeUnsubscribeBody(r: Reader): Unsubscribe {
  const v = defaultUnsubscribe();
  if (r.done()) return v;
  v.sub = r.u32();
  return v;
}

/** Every kind, by its index on the wire. */
export const KINDS = ["Open", "Accepted", "Refused", "Request", "Answer", "Subscribe", "Unsubscribe", "Snapshot", "Resumed", "Change", "Tail", "Ready", "End", "Credit", "Fragment", "Idle", "Probe", "ProbeAck", "Unread", "Compressed", "Close", "Input", "SecretAnswer", "Resize"] as const;
export type KindName = (typeof KINDS)[number];
export type Frame = { kind: "Open"; body: Open } | { kind: "Accepted"; body: Accepted } | { kind: "Refused"; body: Refused } | { kind: "Request"; body: Request } | { kind: "Answer"; body: Answer } | { kind: "Subscribe"; body: Subscribe } | { kind: "Unsubscribe"; body: Unsubscribe } | { kind: "Snapshot"; body: Begin } | { kind: "Resumed"; body: Begin } | { kind: "Change"; body: Change } | { kind: "Tail"; body: Tail } | { kind: "Ready"; body: Begin } | { kind: "End"; body: End } | { kind: "Credit"; body: Credit } | { kind: "Fragment"; body: Fragment } | { kind: "Idle"; body: Idle } | { kind: "Probe"; body: Probe } | { kind: "ProbeAck"; body: ProbeAck } | { kind: "Unread"; body: Unread } | { kind: "Compressed"; body: Compressed } | { kind: "Close"; body: Close } | { kind: "Input"; body: Input } | { kind: "SecretAnswer"; body: Answer } | { kind: "Resize"; body: Resize };
export const KIND_INDEX: { readonly [K in KindName]: number } = { Open: 0, Accepted: 1, Refused: 2, Request: 3, Answer: 4, Subscribe: 5, Unsubscribe: 6, Snapshot: 7, Resumed: 8, Change: 9, Tail: 10, Ready: 11, End: 12, Credit: 13, Fragment: 14, Idle: 15, Probe: 16, ProbeAck: 17, Unread: 18, Compressed: 19, Close: 20, Input: 21, SecretAnswer: 22, Resize: 23 };
/** Writes a frame's body for a peer speaking `version`. */
export function putFrameBody(w: Writer, f: Frame, version: number): void {
  switch (f.kind) {
    case "Open":
      return putOpenBody(w, f.body, version);
    case "Accepted":
      return putAcceptedBody(w, f.body, version);
    case "Refused":
      return putRefusedBody(w, f.body, version);
    case "Request":
      return putRequestBody(w, f.body, version);
    case "Answer":
      return putAnswerBody(w, f.body, version);
    case "Subscribe":
      return putSubscribeBody(w, f.body, version);
    case "Unsubscribe":
      return putUnsubscribeBody(w, f.body, version);
    case "Snapshot":
      return putBeginBody(w, f.body, version);
    case "Resumed":
      return putBeginBody(w, f.body, version);
    case "Change":
      return putChangeBody(w, f.body, version);
    case "Tail":
      return putTailBody(w, f.body, version);
    case "Ready":
      return putBeginBody(w, f.body, version);
    case "End":
      return putEndBody(w, f.body, version);
    case "Credit":
      return putCreditBody(w, f.body, version);
    case "Fragment":
      return putFragmentBody(w, f.body, version);
    case "Idle":
      return putIdleBody(w, f.body, version);
    case "Probe":
      return putProbeBody(w, f.body, version);
    case "ProbeAck":
      return putProbeAckBody(w, f.body, version);
    case "Unread":
      return putUnreadBody(w, f.body, version);
    case "Compressed":
      return putCompressedBody(w, f.body, version);
    case "Close":
      return putCloseBody(w, f.body, version);
    case "Input":
      return putInputBody(w, f.body, version);
    case "SecretAnswer":
      return putAnswerBody(w, f.body, version);
    case "Resize":
      return putResizeBody(w, f.body, version);
  }
}
/** Reads a body of the kind at `index` (tolerantly: missing trailing fields default, newer ones are left). */
export function takeFrameBody(index: number, r: Reader): Frame {
  switch (index) {
    case 0:
      return { kind: "Open", body: takeOpenBody(r) };
    case 1:
      return { kind: "Accepted", body: takeAcceptedBody(r) };
    case 2:
      return { kind: "Refused", body: takeRefusedBody(r) };
    case 3:
      return { kind: "Request", body: takeRequestBody(r) };
    case 4:
      return { kind: "Answer", body: takeAnswerBody(r) };
    case 5:
      return { kind: "Subscribe", body: takeSubscribeBody(r) };
    case 6:
      return { kind: "Unsubscribe", body: takeUnsubscribeBody(r) };
    case 7:
      return { kind: "Snapshot", body: takeBeginBody(r) };
    case 8:
      return { kind: "Resumed", body: takeBeginBody(r) };
    case 9:
      return { kind: "Change", body: takeChangeBody(r) };
    case 10:
      return { kind: "Tail", body: takeTailBody(r) };
    case 11:
      return { kind: "Ready", body: takeBeginBody(r) };
    case 12:
      return { kind: "End", body: takeEndBody(r) };
    case 13:
      return { kind: "Credit", body: takeCreditBody(r) };
    case 14:
      return { kind: "Fragment", body: takeFragmentBody(r) };
    case 15:
      return { kind: "Idle", body: takeIdleBody(r) };
    case 16:
      return { kind: "Probe", body: takeProbeBody(r) };
    case 17:
      return { kind: "ProbeAck", body: takeProbeAckBody(r) };
    case 18:
      return { kind: "Unread", body: takeUnreadBody(r) };
    case 19:
      return { kind: "Compressed", body: takeCompressedBody(r) };
    case 20:
      return { kind: "Close", body: takeCloseBody(r) };
    case 21:
      return { kind: "Input", body: takeInputBody(r) };
    case 22:
      return { kind: "SecretAnswer", body: takeAnswerBody(r) };
    case 23:
      return { kind: "Resize", body: takeResizeBody(r) };
    default:
      throw new BpError(Why.UnknownKind);
  }
}
/** A frame drawn from `g`: its kind, then its body (as kiln-bp's `Frame::arb`). */
export function arbFrame(g: Gen): Frame {
  switch (g.index(KINDS.length)) {
    case 0:
      return { kind: "Open", body: arbOpen(g) };
    case 1:
      return { kind: "Accepted", body: arbAccepted(g) };
    case 2:
      return { kind: "Refused", body: arbRefused(g) };
    case 3:
      return { kind: "Request", body: arbRequest(g) };
    case 4:
      return { kind: "Answer", body: arbAnswer(g) };
    case 5:
      return { kind: "Subscribe", body: arbSubscribe(g) };
    case 6:
      return { kind: "Unsubscribe", body: arbUnsubscribe(g) };
    case 7:
      return { kind: "Snapshot", body: arbBegin(g) };
    case 8:
      return { kind: "Resumed", body: arbBegin(g) };
    case 9:
      return { kind: "Change", body: arbChange(g) };
    case 10:
      return { kind: "Tail", body: arbTail(g) };
    case 11:
      return { kind: "Ready", body: arbBegin(g) };
    case 12:
      return { kind: "End", body: arbEnd(g) };
    case 13:
      return { kind: "Credit", body: arbCredit(g) };
    case 14:
      return { kind: "Fragment", body: arbFragment(g) };
    case 15:
      return { kind: "Idle", body: arbIdle(g) };
    case 16:
      return { kind: "Probe", body: arbProbe(g) };
    case 17:
      return { kind: "ProbeAck", body: arbProbeAck(g) };
    case 18:
      return { kind: "Unread", body: arbUnread(g) };
    case 19:
      return { kind: "Compressed", body: arbCompressed(g) };
    case 20:
      return { kind: "Close", body: arbClose(g) };
    case 21:
      return { kind: "Input", body: arbInput(g) };
    case 22:
      return { kind: "SecretAnswer", body: arbAnswer(g) };
    case 23:
      return { kind: "Resize", body: arbResize(g) };
    default:
      return { kind: "Idle", body: arbIdle(g) };
  }
}
