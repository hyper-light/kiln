// kiln's capture extension, its offscreen document (crate::browser::stream): each stream's tab
// captured (picture and sound), encoded here with WebCodecs, and handed to kiln through its binding
// (`kilnOut`, added over the DevTools protocol) as `<stream> <kind> <flags> <timestamp> <data>`:
// kind `c` a decoder's configuration (JSON), `v` a video chunk, `a` an audio chunk, `e` an event
// (JSON); flag `k` a key frame. Frames come only when the page changes; after a moment still, the
// last frame is sent again sharp.
"use strict";
const streams = new Map();
const out = (id, kind, flags, ts, data) => {
  try {
    globalThis.kilnOut(`${id} ${kind} ${flags} ${Math.round(ts)} ${data}`);
  } catch (e) {
    // kiln isn't listening (yet): nothing to hand it to.
  }
};
const b64 = (buf) => {
  const bytes = new Uint8Array(buf);
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(s);
};
const chunkData = (c) => {
  const buf = new ArrayBuffer(c.byteLength);
  c.copyTo(buf);
  return b64(buf);
};
// H.264's level for a size and rate (baseline profile): what decoders everywhere take.
const avcCodec = (w, h, fps) => {
  const mbs = Math.ceil(w / 16) * Math.ceil(h / 16);
  const levels = [[3600, 108000, "1f"], [8192, 245760, "28"], [8704, 522240, "2a"], [22080, 589824, "32"], [36864, 983040, "33"], [139264, 4177920, "3c"]];
  const l = levels.find(([f, r]) => mbs <= f && mbs * fps <= r) || levels[levels.length - 1];
  return `avc1.42E0${l[2]}`;
};
// H.264 takes only even sizes (its colour comes in 2×2 blocks): an odd size (an iPhone 15's 393
// points at 3×, 1179 pixels) loses its last column or row.
const even = (n) => Math.max(2, n - (n % 2));

class Stream {
  constructor(id, media, opts) {
    this.id = id;
    this.media = media;
    this.opts = opts;
    this.paused = false;
    this.forceKey = true;
    this.last = null;
    this.stillTimer = null;
    this.video = media.getVideoTracks()[0];
    this.audio = media.getAudioTracks()[0];
    if (this.video) this.video.contentHint = "detail";
    // What its pictures' times are moved by (a tab captured since a switch counts from its own
    // start), and the last one's.
    this.shift = 0;
    this.lastTs = -1;
    this.counted();
  }

  // Its counts from now (`kiln.stats`): pictures taken, encoded and let go, key frames, bytes,
  // how long each took to encode.
  counted() {
    const was = this.counts;
    this.counts = { since: performance.now(), taken: 0, encoded: 0, dropped: 0, keys: 0, bytes: 0, encodeMs: 0, encodeMax: 0 };
    this.encoding = new Map();
    return was;
  }

  codecFor(w, h) {
    const want = this.opts.codec || "h264";
    if (want === "vp8") return "vp8";
    if (want === "vp9") return "vp09.00.10.08";
    if (want === "av1") return "av01.0.04M.08";
    return avcCodec(w, h, this.opts.framerate || 60);
  }

  async configure(width, height) {
    const [w, h] = [even(width), even(height)];
    this.size = [w, h];
    const codec = this.codecFor(w, h);
    const base = { codec, width: w, height: h, framerate: this.opts.framerate || 60, latencyMode: "realtime", hardwareAcceleration: "no-preference", ...(codec.startsWith("avc1") ? { avc: { format: "annexb" } } : {}) };
    const live = { ...base, bitrate: this.opts.bitrate || 4_000_000, bitrateMode: "variable" };
    const still = { ...base, bitrate: (this.opts.bitrate || 4_000_000) * 4, latencyMode: "quality" };
    const ok = await VideoEncoder.isConfigSupported(live);
    if (!ok.supported) throw new Error(`this browser can't encode ${codec} at ${w}x${h}`);
    for (const e of [this.enc, this.stillEnc]) if (e && e.state !== "closed") e.close();
    this.enc = new VideoEncoder({ output: (c, meta) => this.sent(c, meta), error: (e) => out(this.id, "e", "-", 0, btoa(JSON.stringify({ error: String(e) }))) });
    this.enc.configure(live);
    this.stillEnc = new VideoEncoder({ output: (c, meta) => this.sent(c, meta), error: () => {} });
    if ((await VideoEncoder.isConfigSupported(still)).supported) this.stillEnc.configure(still);
    this.forceKey = true;
    out(this.id, "c", "-", 0, btoa(JSON.stringify({ codec, codedWidth: w, codedHeight: h })));
  }

  sent(c, meta) {
    const n = this.counts;
    n.encoded++;
    n.bytes += c.byteLength;
    if (c.type === "key") n.keys++;
    const at = this.encoding.get(c.timestamp);
    if (at !== undefined) {
      const ms = performance.now() - at;
      this.encoding.delete(c.timestamp);
      n.encodeMs += ms;
      n.encodeMax = Math.max(n.encodeMax, ms);
    }
    if (meta && meta.decoderConfig && meta.decoderConfig.description) {
      // Out-of-band configuration (VP9/AV1 may have one): sent ahead of the chunk.
      const d = meta.decoderConfig;
      out(this.id, "c", "-", 0, btoa(JSON.stringify({ codec: d.codec, codedWidth: d.codedWidth, codedHeight: d.codedHeight, description: b64(d.description) })));
    }
    out(this.id, "v", c.type === "key" ? "k" : "-", c.timestamp, chunkData(c));
  }

  async start() {
    const s = this.video.getSettings();
    await this.configure(s.width, s.height);
    this.see(this.video);
    if (this.audio) this.hear(this.audio);
  }

  // Each picture of `track`, while it's the one captured.
  see(track) {
    const reader = new MediaStreamTrackProcessor({ track }).readable.getReader();
    (async () => {
      for (;;) {
        const { value: frame, done } = await reader.read();
        if (done) break;
        if (track !== this.video) {
          frame.close();
          reader.cancel().catch(() => {});
          break;
        }
        this.frame(frame);
      }
    })();
  }

  // Its sound, while `track` is the one captured.
  hear(track) {
    if (!this.aenc) {
      this.aenc = new AudioEncoder({ output: (c) => out(this.id, "a", "k", c.timestamp, chunkData(c)), error: () => {} });
      this.aenc.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2, bitrate: 96_000, opus: { usedtx: true, complexity: 10 } });
      out(this.id, "c", "a", 0, btoa(JSON.stringify({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 })));
    }
    const ar = new MediaStreamTrackProcessor({ track }).readable.getReader();
    (async () => {
      for (;;) {
        const { value, done } = await ar.read();
        if (done) break;
        if (track !== this.audio) {
          value.close();
          ar.cancel().catch(() => {});
          break;
        }
        if (!this.paused && this.aenc.state === "configured") this.aenc.encode(value);
        value.close();
      }
    })();
  }

  // Another tab's picture and sound from now on (its viewer went to it): the same encoders, from
  // a key frame, its times after the last sent.
  switchTo(media) {
    const before = this.media;
    this.media = media;
    this.video = media.getVideoTracks()[0];
    this.audio = media.getAudioTracks()[0];
    if (this.video) this.video.contentHint = "detail";
    this.shift = null;
    this.forceKey = true;
    if (this.video) this.see(this.video);
    if (this.audio) this.hear(this.audio);
    for (const t of before.getTracks()) t.stop();
  }

  // Its capture let go (to be taken again at another size): the encoders stay, its last picture too.
  release() {
    for (const t of this.media.getTracks()) t.stop();
    this.video = null;
    this.audio = null;
  }

  // A picture at its time after the last one's.
  stamped(frame) {
    if (this.shift === null) this.shift = Math.max(0, this.lastTs + 16_667 - frame.timestamp);
    if (!this.shift) return frame;
    const moved = new VideoFrame(frame, { timestamp: frame.timestamp + this.shift });
    frame.close();
    return moved;
  }

  frame(taken) {
    const frame = this.evened(this.stamped(taken));
    this.lastTs = frame.timestamp;
    this.counts.taken++;
    const [w, h] = this.size;
    if (frame.displayWidth !== w || frame.displayHeight !== h) {
      // The page's size changed: the encoders follow, from a key frame (what comes meanwhile waits
      // for them).
      const next = [frame.displayWidth, frame.displayHeight];
      this.size = next;
      // Once they have, what the page looks like now goes out at once (it may not change again).
      this.configuring = this.configure(next[0], next[1])
        .catch(() => {})
        .finally(() => {
          this.configuring = null;
          this.still();
        });
    }
    if (this.configuring) {
      if (this.last) this.last.close();
      this.last = frame.clone();
      frame.close();
      this.forceKey = true;
      return;
    }
    if (this.last) this.last.close();
    this.last = frame.clone();
    clearTimeout(this.stillTimer);
    if (this.paused || this.enc.state !== "configured" || this.enc.encodeQueueSize > 2) {
      if (!this.paused) this.counts.dropped++;
      frame.close();
      this.forceKey = true;
      return;
    }
    const key = this.forceKey;
    this.forceKey = false;
    if (this.encoding.size < 64) this.encoding.set(frame.timestamp, performance.now());
    this.enc.encode(frame, { keyFrame: key });
    frame.close();
    // Still for a moment: the same picture again, sharp; what moves next starts from a key frame.
    this.stillTimer = setTimeout(() => this.still(), this.opts.still_ms || 400);
  }

  // A picture of an odd size, cut to the even one its encoders take.
  evened(frame) {
    const r = frame.visibleRect;
    if (!r || (r.width % 2 === 0 && r.height % 2 === 0)) return frame;
    const [w, h] = [even(r.width), even(r.height)];
    const cut = new VideoFrame(frame, { visibleRect: { x: r.x, y: r.y, width: w, height: h }, displayWidth: w, displayHeight: h });
    frame.close();
    return cut;
  }

  async still() {
    if (this.paused || !this.last || !this.stillEnc || this.stillEnc.state !== "configured") return;
    // After what the main encoder still has: its pictures go out first, so none of them comes
    // after the still one (a viewer's decoder would take it on the still one's key frame).
    const was = this.last;
    if (this.enc && this.enc.state === "configured") await this.enc.flush().catch(() => {});
    // The page changed meanwhile: its own pictures go on, the still one when it stops again.
    if (this.last !== was || this.paused || this.stillEnc.state !== "configured") return;
    const f = new VideoFrame(this.last, { timestamp: this.last.timestamp + 1 });
    this.stillEnc.encode(f, { keyFrame: true });
    f.close();
    this.forceKey = true;
  }

  keyframe() {
    // What a decoder needs first (kiln may have started listening after it was sent).
    if (this.size) {
      const [w, h] = this.size;
      out(this.id, "c", "-", 0, btoa(JSON.stringify({ codec: this.codecFor(w, h), codedWidth: w, codedHeight: h })));
    }
    if (this.audio) out(this.id, "c", "a", 0, btoa(JSON.stringify({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 })));
    this.forceKey = true;
    if (this.last && !this.paused) this.still();
  }

  // The page's new size (in pixels), where kiln can't capture it again (`kiln.recapture`): the
  // capture's own choice within it.
  async resize(w, h) {
    const [W, H] = [even(w), even(h)];
    this.opts.width = W;
    this.opts.height = H;
    if (!this.video) return;
    await this.video.applyConstraints({ width: { max: W }, height: { max: H }, aspectRatio: { exact: W / H }, frameRate: { max: this.opts.framerate || 60 } });
  }

  pause(p) {
    this.paused = p;
    if (!p) this.keyframe();
  }

  stop() {
    clearTimeout(this.stillTimer);
    for (const t of this.media.getTracks()) t.stop();
    for (const e of [this.enc, this.stillEnc, this.aenc]) if (e && e.state !== "closed") e.close();
    if (this.last) this.last.close();
  }
}

// A stream of pictures kiln brings (another engine's page, taken as it changes), each encoded as
// a captured tab's frame is: a new size reconfigures, a still one is sent again sharp.
class Pushed extends Stream {
  constructor(id, opts) {
    super(id, { getVideoTracks: () => [], getAudioTracks: () => [], getTracks: () => [] }, opts);
    this.size = [opts.width || 780, opts.height || 1280];
  }

  async start() {
    await this.configure(this.size[0], this.size[1]);
  }

  async push(b64, mime) {
    const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const bitmap = await createImageBitmap(new Blob([bytes], { type: mime || "image/jpeg" }));
    const frame = new VideoFrame(bitmap, { timestamp: Math.round(performance.now() * 1000) });
    bitmap.close();
    this.frame(frame);
  }

  // Its size is its pictures'.
  async resize() {}
}

// A tab's picture and sound, at a stream's size: a capture comes at the size it's started at
// (even), where one asked for another later picks its own, often odd (and each picture cut to even
// is a key frame of its own). A new size is a new capture (`kiln.recapture`).
const tab = (mediaId, o) =>
  new Promise((ok, no) => {
    // One that doesn't start soon is tried again (`captured`, in the worker), and let go of if it
    // starts after all.
    let late = false;
    const wait = setTimeout(() => {
      late = true;
      no(new Error("the tab's capture didn't start"));
    }, 3000);
    navigator.mediaDevices
      .getUserMedia({
        video: { mandatory: { chromeMediaSource: "tab", chromeMediaSourceId: mediaId, maxWidth: even(o.width || 1280), maxHeight: even(o.height || 800), maxFrameRate: o.framerate || 60 } },
        audio: { mandatory: { chromeMediaSource: "tab", chromeMediaSourceId: mediaId } },
      })
      .then(
        (media) => {
          clearTimeout(wait);
          if (late) for (const t of media.getTracks()) t.stop();
          else ok(media);
        },
        (e) => {
          clearTimeout(wait);
          no(e);
        },
      );
  });

chrome.runtime.onMessage.addListener((m, _sender, reply) => {
  if (m.kind === "release") {
    const s = streams.get(m.stream);
    if (s && !(s instanceof Pushed)) s.release();
    reply({ ok: true });
    return false;
  }
  if (m.kind === "switch") {
    (async () => {
      try {
        const s = streams.get(m.stream);
        if (!s || s instanceof Pushed) throw new Error("no such tab's stream");
        if (m.width && m.height) [s.opts.width, s.opts.height] = [even(m.width), even(m.height)];
        s.switchTo(await tab(m.mediaId, s.opts));
        reply({ ok: true, audio: Boolean(s.audio) });
      } catch (e) {
        reply({ ok: false, error: String(e) });
      }
    })();
    return true;
  }
  if (m.kind === "pushed") {
    (async () => {
      try {
        const s = new Pushed(m.stream, m.opts || {});
        streams.set(m.stream, s);
        await s.start();
        reply({ ok: true, size: s.size, audio: false });
      } catch (e) {
        reply({ ok: false, error: String(e) });
      }
    })();
    return true;
  }
  if (m.kind !== "start") return;
  (async () => {
    try {
      const o = m.opts || {};
      const s = new Stream(m.stream, await tab(m.mediaId, o), o);
      streams.set(m.stream, s);
      await s.start();
      reply({ ok: true, size: s.size, audio: Boolean(s.audio) });
    } catch (e) {
      reply({ ok: false, error: String(e) });
    }
  })();
  return true;
});

// What kiln calls here.
globalThis.kiln = {
  keyframe: (id) => streams.get(id)?.keyframe(),
  pause: (id, p) => streams.get(id)?.pause(Boolean(p)),
  resize: (id, w, h) => streams.get(id)?.resize(w, h),
  bitrate: (id, bps) => {
    const s = streams.get(id);
    if (s) s.opts.bitrate = bps;
  },
  stop: (id) => {
    streams.get(id)?.stop();
    streams.delete(id);
  },
  // A picture of a pushed stream's page (base64, its type).
  push: (id, b64, mime) => streams.get(id)?.push(b64, mime),
  streams: () => [...streams.keys()],
  // A stream's counts since they were last asked for, and how it's encoded now.
  stats: (id) => {
    const s = streams.get(id);
    if (!s) return null;
    const n = s.counted();
    const secs = (performance.now() - n.since) / 1000;
    const track = s.video ? s.video.getSettings() : null;
    return { seconds: Math.round(secs * 100) / 100, taken: n.taken, encoded: n.encoded, dropped: n.dropped, keys: n.keys, kbps: Math.round((n.bytes * 8) / 1000 / Math.max(secs, 0.001)), encodeMs: n.encoded ? Math.round((n.encodeMs / n.encoded) * 10) / 10 : null, encodeMax: Math.round(n.encodeMax), size: s.size, codec: s.enc ? s.codecFor(s.size[0], s.size[1]) : null, queue: s.enc ? s.enc.encodeQueueSize : null, track: track ? [track.width, track.height, track.frameRate] : null, bitrate: s.opts.bitrate || 4_000_000 };
  },
  sizes: () => Object.fromEntries([...streams].map(([id, s]) => [id, { size: s.size, frame: s.last ? [s.last.displayWidth, s.last.displayHeight, s.last.codedWidth, s.last.codedHeight] : null, track: s.video ? s.video.getSettings() : null }])),
};
