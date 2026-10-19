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
  }

  codecFor(w, h) {
    const want = this.opts.codec || "h264";
    if (want === "vp8") return "vp8";
    if (want === "vp9") return "vp09.00.10.08";
    if (want === "av1") return "av01.0.04M.08";
    return avcCodec(w, h, this.opts.framerate || 60);
  }

  async configure(w, h) {
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
    const reader = new MediaStreamTrackProcessor({ track: this.video }).readable.getReader();
    (async () => {
      for (;;) {
        const { value: frame, done } = await reader.read();
        if (done) break;
        this.frame(frame);
      }
    })();
    if (this.audio) {
      this.aenc = new AudioEncoder({ output: (c) => out(this.id, "a", "k", c.timestamp, chunkData(c)), error: () => {} });
      this.aenc.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2, bitrate: 96_000, opus: { usedtx: true, complexity: 10 } });
      out(this.id, "c", "a", 0, btoa(JSON.stringify({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 })));
      const ar = new MediaStreamTrackProcessor({ track: this.audio }).readable.getReader();
      (async () => {
        for (;;) {
          const { value, done } = await ar.read();
          if (done) break;
          if (!this.paused && this.aenc.state === "configured") this.aenc.encode(value);
          value.close();
        }
      })();
    }
  }

  frame(frame) {
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
      frame.close();
      this.forceKey = true;
      return;
    }
    const key = this.forceKey;
    this.forceKey = false;
    this.enc.encode(frame, { keyFrame: key });
    frame.close();
    // Still for a moment: the same picture again, sharp; what moves next starts from a key frame.
    this.stillTimer = setTimeout(() => this.still(), this.opts.still_ms || 400);
  }

  still() {
    if (this.paused || !this.last || !this.stillEnc || this.stillEnc.state !== "configured") return;
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

  // The page's new size (in pixels): the capture takes it (it would scale into the old one).
  async resize(w, h) {
    await this.video.applyConstraints({ width: { max: w }, height: { max: h }, aspectRatio: { exact: w / h }, frameRate: { max: this.opts.framerate || 60 } });
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

chrome.runtime.onMessage.addListener((m, _sender, reply) => {
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
      const media = await navigator.mediaDevices.getUserMedia({
        video: { mandatory: { chromeMediaSource: "tab", chromeMediaSourceId: m.mediaId, maxWidth: o.width || 1280, maxHeight: o.height || 800, maxFrameRate: o.framerate || 60 } },
        audio: { mandatory: { chromeMediaSource: "tab", chromeMediaSourceId: m.mediaId } },
      });
      const s = new Stream(m.stream, media, o);
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
  sizes: () => Object.fromEntries([...streams].map(([id, s]) => [id, { size: s.size, frame: s.last ? [s.last.displayWidth, s.last.displayHeight, s.last.codedWidth, s.last.codedHeight] : null, track: s.video ? s.video.getSettings() : null }])),
};
