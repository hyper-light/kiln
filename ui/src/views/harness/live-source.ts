/**
 * A live page's pictures where this browser has no WebCodecs (any, at an http:// address other
 * than localhost) and WebRTC doesn't reach kiln's server (it isn't turned on there, or the network
 * between carries no UDP): its stream's H.264 as its socket brings it, played by the browser's own
 * Media Source Extensions, which work at any address. Each picture goes as a fragment of MP4 of its
 * own, on a timeline with no gaps: each lasts a sixtieth of a second and playing waits at the last
 * for the next, so a picture shows as it comes.
 */

/** The browser's Media Source (an iPhone's is the managed kind), or none. */
const Source: typeof MediaSource | undefined =
  typeof window === "undefined" ? undefined : ((window as unknown as { ManagedMediaSource?: typeof MediaSource }).ManagedMediaSource ?? window.MediaSource);

/** Whether this browser plays a stream's H.264 so. */
export const sourcePlays = Boolean(Source && typeof Source.isTypeSupported === "function" && Source.isTypeSupported('video/mp4; codecs="avc1.42E01F"'));

/** Its timeline's ticks a second, and how long each picture lasts in them. */
const SCALE = 90_000;
const LASTS = SCALE / 60;

/** Bytes, as boxes of MP4 are written. */
class Boxes {
  private buf = new Uint8Array(4096);
  private n = 0;

  private room(k: number) {
    if (this.n + k <= this.buf.length) return;
    const b = new Uint8Array(Math.max(this.buf.length * 2, this.n + k));
    b.set(this.buf.subarray(0, this.n));
    this.buf = b;
  }

  u8(v: number) {
    this.room(1);
    this.buf[this.n++] = v & 0xff;
  }

  u16(v: number) {
    this.u8(v >>> 8);
    this.u8(v);
  }

  u32(v: number) {
    this.u16(v >>> 16);
    this.u16(v & 0xffff);
  }

  /** Up to 2^53, as a timeline's time gets. */
  u64(v: number) {
    this.u32(Math.floor(v / 2 ** 32));
    this.u32(v >>> 0);
  }

  bytes(b: Uint8Array | string) {
    const bytes = typeof b === "string" ? Uint8Array.from(b, (c) => c.charCodeAt(0)) : b;
    this.room(bytes.length);
    this.buf.set(bytes, this.n);
    this.n += bytes.length;
  }

  zeros(k: number) {
    this.bytes(new Uint8Array(k));
  }

  /** A box: its size (known once its insides are written), its kind, its insides. */
  box(kind: string, insides: () => void) {
    const at = this.n;
    this.u32(0);
    this.bytes(kind);
    insides();
    this.set32(at, this.n - at);
  }

  /** A full box: a box with a version and flags first. */
  full(kind: string, version: number, flags: number, insides: () => void) {
    this.box(kind, () => {
      this.u32(version * 2 ** 24 + (flags & 0xffffff));
      insides();
    });
  }

  matrix() {
    for (const v of [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000]) this.u32(v);
  }

  set32(at: number, v: number) {
    new DataView(this.buf.buffer).setUint32(at, v);
  }

  get length(): number {
    return this.n;
  }

  done(): Uint8Array {
    return this.buf.slice(0, this.n);
  }
}

/** The NAL units of a picture in Annex B (each after its start code). */
export function units(annexb: Uint8Array): Uint8Array[] {
  const out: Uint8Array[] = [];
  let start = -1;
  for (let i = 0; i + 2 < annexb.length; i++) {
    if (annexb[i] !== 0 || annexb[i + 1] !== 0 || annexb[i + 2] !== 1) continue;
    if (start >= 0) {
      // A four-byte start code's leading zero isn't the unit before's.
      let end = i;
      while (end > start && annexb[end - 1] === 0) end--;
      out.push(annexb.subarray(start, end));
    }
    start = i + 3;
    i += 2;
  }
  if (start >= 0 && start < annexb.length) out.push(annexb.subarray(start));
  return out;
}

const kind = (u: Uint8Array) => (u[0] ?? 0) & 0x1f;

/** `avc1.PPCCLL`: its profile, constraints and level, from its sequence parameter set. */
export function codecOf(sps: Uint8Array): string {
  const hex = (v: number | undefined) => (v ?? 0).toString(16).padStart(2, "0").toUpperCase();
  return `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
}

/** The file's start: `ftyp` and `moov`, one video track (H.264 as `avc1`, its parameter sets in
 *  its `avcC`), its samples in fragments. */
export function initSegment(sps: Uint8Array, pps: Uint8Array, width: number, height: number): Uint8Array {
  const b = new Boxes();
  b.box("ftyp", () => {
    b.bytes("isom");
    b.u32(0x200);
    for (const c of ["isom", "iso6", "avc1", "mp41"]) b.bytes(c);
  });
  b.box("moov", () => {
    b.full("mvhd", 0, 0, () => {
      b.u32(0);
      b.u32(0);
      b.u32(1000);
      b.u32(0);
      b.u32(0x10000);
      b.u16(0x100);
      b.zeros(10);
      b.matrix();
      b.zeros(24);
      b.u32(2);
    });
    b.box("trak", () => {
      b.full("tkhd", 0, 3, () => {
        b.zeros(8);
        b.u32(1);
        b.zeros(20);
        b.u16(0);
        b.u16(0);
        b.matrix();
        b.u32(width * 0x10000);
        b.u32(height * 0x10000);
      });
      b.box("mdia", () => {
        b.full("mdhd", 0, 0, () => {
          b.zeros(8);
          b.u32(SCALE);
          b.u32(0);
          // "und", packed.
          b.u16(0x55c4);
          b.u16(0);
        });
        b.full("hdlr", 0, 0, () => {
          b.u32(0);
          b.bytes("vide");
          b.zeros(12);
          b.bytes("kiln video\0");
        });
        b.box("minf", () => {
          b.full("vmhd", 0, 1, () => b.zeros(8));
          b.box("dinf", () => b.full("dref", 0, 0, () => {
            b.u32(1);
            b.full("url ", 0, 1, () => {});
          }));
          b.box("stbl", () => {
            b.full("stsd", 0, 0, () => {
              b.u32(1);
              b.box("avc1", () => {
                b.zeros(6);
                b.u16(1);
                b.zeros(16);
                b.u16(width);
                b.u16(height);
                b.u32(0x480000);
                b.u32(0x480000);
                b.u32(0);
                b.u16(1);
                b.zeros(32);
                b.u16(0x18);
                b.u16(0xffff);
                b.box("avcC", () => {
                  b.u8(1);
                  b.u8(sps[1] ?? 0);
                  b.u8(sps[2] ?? 0);
                  b.u8(sps[3] ?? 0);
                  // Lengths of four bytes; one of each parameter set.
                  b.u8(0xff);
                  b.u8(0xe1);
                  b.u16(sps.length);
                  b.bytes(sps);
                  b.u8(1);
                  b.u16(pps.length);
                  b.bytes(pps);
                });
              });
            });
            for (const k of ["stts", "stsc", "stco"]) b.full(k, 0, 0, () => b.u32(0));
            b.full("stsz", 0, 0, () => {
              b.u32(0);
              b.u32(0);
            });
          });
        });
      });
    });
    b.box("mvex", () => b.full("trex", 0, 0, () => {
      b.u32(1);
      b.u32(1);
      b.u32(0);
      b.u32(0);
      b.u32(0);
    }));
  });
  return b.done();
}

/** One picture as a fragment (`moof` and `mdat`): its units (but its parameter sets) each after
 *  its length, at `at` on the timeline, lasting `LASTS`. */
export function pictureSegment(seq: number, at: number, key: boolean, picture: Uint8Array[]): Uint8Array {
  const b = new Boxes();
  const size = picture.reduce((s, u) => s + 4 + u.length, 0);
  let offsetAt = 0;
  b.box("moof", () => {
    b.full("mfhd", 0, 0, () => b.u32(seq));
    b.box("traf", () => {
      // Its data's offsets from the fragment's own start.
      b.full("tfhd", 0, 0x20000, () => b.u32(1));
      b.full("tfdt", 1, 0, () => b.u64(at));
      b.full("trun", 0, 0x1 | 0x100 | 0x200 | 0x400, () => {
        b.u32(1);
        offsetAt = b.length;
        b.u32(0);
        b.u32(LASTS);
        b.u32(size);
        // A key frame depends on no other; the rest do, and aren't sync samples.
        b.u32(key ? 0x2000000 : 0x1010000);
      });
    });
  });
  b.set32(offsetAt, b.length + 8);
  b.u32(8 + size);
  b.bytes("mdat");
  for (const u of picture) {
    b.u32(u.length);
    b.bytes(u);
  }
  return b.done();
}

/** A stream's pictures played in `video` through Media Source. */
export class SourcePlayer {
  private source: MediaSource;
  private url = "";
  private buffer: SourceBuffer | null = null;
  private queue: (Uint8Array | { remove: number })[] = [];
  /** The parameter sets the last start of the file had, and its codec. */
  private params = "";
  private codec = "";
  private seq = 0;
  private at = 0;
  private trimmed = 0;
  private open = false;
  /** Where playing last was, and since when: one that stopped there with newer pictures behind it
   *  is moved on to the newest (`chase`). */
  private moved = { t: -1, at: 0 };
  private again: ReturnType<typeof setTimeout> | undefined;
  /** Pictures that came before it opened (a key frame first). */
  private early: { data: Uint8Array; key: boolean; width: number; height: number }[] = [];
  failed = false;

  constructor(
    private video: HTMLVideoElement,
    private onFail: (why: string) => void,
  ) {
    const S = Source as typeof MediaSource;
    this.source = new S();
    // An iPhone's managed Media Source plays only where AirPlay isn't offered.
    video.disableRemotePlayback = true;
    video.muted = true;
    this.source.addEventListener("sourceopen", () => {
      this.open = true;
      try {
        // A live stream: no end (Chrome then shows each picture as soon as it's decoded).
        this.source.duration = Infinity;
      } catch {
        // Not this browser's to set: it plays all the same.
      }
      const early = this.early;
      this.early = [];
      for (const p of early) this.picture(p.data, p.key, p.width, p.height);
    });
    this.url = URL.createObjectURL(this.source);
    video.src = this.url;
  }

  /** A picture (Annex B) at `width` by `height`; false when it can't be played (from a key frame
   *  on, it can). */
  picture(data: Uint8Array, key: boolean, width: number, height: number): boolean {
    if (this.failed) return false;
    if (!this.open) {
      if (key) this.early = [];
      if (key || this.early.length) this.early.push({ data, key, width, height });
      return true;
    }
    const all = units(data);
    if (key) {
      const sps = all.find((u) => kind(u) === 7);
      const pps = all.find((u) => kind(u) === 8);
      if (sps && pps) {
        const params = `${Array.from(sps).join(",")};${Array.from(pps).join(",")}`;
        if (params !== this.params && !this.start(sps, pps, width, height)) return false;
        this.params = params;
      }
    }
    if (!this.buffer) return false;
    const picture = all.filter((u) => ![7, 8, 9].includes(kind(u)));
    this.push(pictureSegment(++this.seq, this.at, key, picture));
    this.at += LASTS;
    return true;
  }

  /** A start of the file for these parameter sets: the buffer made for it, or changed to it. */
  private start(sps: Uint8Array, pps: Uint8Array, width: number, height: number): boolean {
    const codec = codecOf(sps);
    const type = `video/mp4; codecs="${codec}"`;
    try {
      if (!this.buffer) {
        const b = this.source.addSourceBuffer(type);
        b.addEventListener("updateend", () => this.next());
        b.addEventListener("error", () => this.fail("this browser couldn't take the stream's pictures"));
        this.buffer = b;
      } else if (codec !== this.codec) {
        (this.buffer as SourceBuffer & { changeType?: (t: string) => void }).changeType?.(type);
      }
    } catch (e) {
      this.fail(`this browser can't play ${codec}: ${String(e)}`);
      return false;
    }
    this.codec = codec;
    this.push(initSegment(sps, pps, width, height));
    return true;
  }

  private push(part: Uint8Array | { remove: number }) {
    this.queue.push(part);
    this.next();
  }

  /** The next part into the buffer, once it's done with the last; then playing kept near now. */
  private next() {
    const b = this.buffer;
    if (!b || b.updating || this.failed) return;
    this.chase();
    const part = this.queue.shift();
    if (!part) return;
    try {
      if (part instanceof Uint8Array) b.appendBuffer(part as Uint8Array<ArrayBuffer>);
      else b.remove(0, part.remove);
    } catch (e) {
      // Full: what's long past goes, then this again.
      if (e instanceof DOMException && e.name === "QuotaExceededError" && b.buffered.length) {
        this.queue.unshift(part);
        b.remove(0, Math.max(0.1, this.video.currentTime - 1));
        return;
      }
      this.fail(`this browser stopped taking the stream's pictures: ${String(e)}`);
    }
  }

  /** Playing as near the last picture as it can be: a little faster when it's behind, at it when
   *  it's far behind; what's long past let go. */
  private chase() {
    const v = this.video;
    const r = v.buffered;
    if (!r.length) return;
    const end = r.end(r.length - 1);
    const behind = end - v.currentTime;
    const now = performance.now();
    if (v.currentTime !== this.moved.t) this.moved = { t: v.currentTime, at: now };
    // A moment on, again: the last pictures of a burst are shown too.
    clearTimeout(this.again);
    if (behind > 0.001) this.again = setTimeout(() => this.chase(), 80);
    if (behind > 1 || v.currentTime < r.start(r.length - 1)) v.currentTime = Math.max(r.start(r.length - 1), end - 0.02);
    // Playing stopped at a picture with newer ones behind it (a browser that holds them until
    // more come, as for a still page that changed once): the newest, shown now.
    else if (behind > 0.001 && now - this.moved.at > 50) {
      v.currentTime = end - 0.001;
      this.moved = { t: v.currentTime, at: now };
    } else v.playbackRate = behind > 0.25 ? 2 : behind > 0.1 ? 1.25 : 1;
    if (v.paused) void v.play().catch(() => undefined);
    if (v.currentTime - this.trimmed > 20) {
      this.trimmed = v.currentTime - 5;
      this.queue.push({ remove: this.trimmed });
    }
  }

  private fail(why: string) {
    if (this.failed) return;
    this.failed = true;
    this.onFail(why);
  }

  close() {
    this.failed = true;
    this.queue = [];
    clearTimeout(this.again);
    try {
      if (this.source.readyState === "open") this.source.endOfStream();
    } catch {
      // Gone already.
    }
    this.video.removeAttribute("src");
    this.video.load();
    if (this.url) URL.revokeObjectURL(this.url);
  }
}
