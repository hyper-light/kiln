//! Recordings of live pages (docs/previews.md, "Recordings"): what a stream sends, kept as it's
//! sent, never encoded again. Its H.264 pictures and Opus sound go into a fragmented MP4 (`avc3`,
//! so a new size mid-way is a new key frame's own; a fragment a key frame or two seconds, so a
//! recording cut short plays to there). Beside it goes what was done in the page
//! (`.inputs.jsonl`: a person's input, the agent's actions, where it went, its window's size, each
//! at its time), and that as a Playwright test (`.spec.ts`) to turn into a real one. The log can
//! be played back into a live page, too ([`replay_steps`]).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Video's time: ticks a second.
const VIDEO_SCALE: u32 = 90_000;
/// Sound's: Opus's own rate.
const AUDIO_SCALE: u32 = 48_000;
/// Opus's look-ahead at 48 kHz (the samples a decoder drops first).
const PRE_SKIP: u16 = 312;
/// A fragment is cut at a key frame once it holds this much, or anyway after twice it.
const FRAGMENT: u64 = 1_000_000;
/// The last picture of a recording lasts until it stops, at most this long.
const LAST_AT_MOST: Duration = Duration::from_secs(600);

// ------------------------------------------------------------------------------ boxes

/// ISO BMFF boxes, written big-endian into one buffer.
#[derive(Default)]
struct Boxes(Vec<u8>);

impl Boxes {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    /// A box: its size and type, then what `f` writes in it.
    fn put(&mut self, kind: &[u8; 4], f: impl FnOnce(&mut Self)) {
        let start = self.0.len();
        self.u32(0);
        self.bytes(kind);
        f(self);
        let size = u32::try_from(self.0.len() - start).unwrap_or(u32::MAX);
        if let Some(at) = self.0.get_mut(start..start + 4) {
            at.copy_from_slice(&size.to_be_bytes());
        }
    }
    /// A full box: a box with a version and flags first.
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32, f: impl FnOnce(&mut Self)) {
        self.put(kind, |b| {
            b.u32((u32::from(version) << 24) | (flags & 0x00ff_ffff));
            f(b);
        });
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
    }
    /// Overwrite the four bytes at `at` (a data offset known once what's around it is written).
    fn set_u32(&mut self, at: usize, v: u32) {
        if let Some(b) = self.0.get_mut(at..at + 4) {
            b.copy_from_slice(&v.to_be_bytes());
        }
    }
}

// ------------------------------------------------------------------------------ H.264

/// The NAL units of an Annex B access unit (each after its start code).
pub fn nal_units(annexb: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annexb.len() {
        let code = match annexb.get(i..i + 4) {
            Some([0, 0, 0, 1]) => 4,
            _ if annexb.get(i..i + 3) == Some(&[0, 0, 1]) => 3,
            _ => 0,
        };
        if code > 0 {
            starts.push((i, i + code));
            i += code;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, (_, from)) in starts.iter().enumerate() {
        let to = starts.get(n + 1).map_or(annexb.len(), |(at, _)| *at);
        if let Some(unit) = annexb.get(*from..to).filter(|u| !u.is_empty()) {
            out.push(unit);
        }
    }
    out
}

/// An access unit as MP4 keeps one: each NAL unit after its length (four bytes).
fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.iter().map(|u| u.len() + 4).sum());
    for u in units {
        out.extend_from_slice(&u32::try_from(u.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(u);
    }
    out
}

/// A unit's type (7: a sequence parameter set, 8: a picture's, 5: a key picture's slice).
fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| b & 0x1f)
}

// ------------------------------------------------------------------------------ the file

struct Sample {
    /// Its decode time (its track's ticks from the recording's start).
    dts: u64,
    duration: u32,
    key: bool,
    data: Vec<u8>,
}

/// A track's samples on their way into fragments: those complete (their durations known), and the
/// last one (its duration is when the next comes).
#[derive(Default)]
struct Track {
    done: Vec<Sample>,
    last: Option<Sample>,
}

impl Track {
    fn push(&mut self, dts: u64, key: bool, data: Vec<u8>) {
        // The same picture again, sharp (a still frame, sent a microsecond after it): in its place.
        // A key frame stands alone, and what comes next starts from one.
        if key
            && let Some(l) = &mut self.last
            && dts <= l.dts + 1
        {
            l.key = true;
            l.data = data;
            return;
        }
        // Each after the one before.
        let dts = self.last.as_ref().map_or(dts, |l| dts.max(l.dts + 1));
        if let Some(mut l) = self.last.take() {
            l.duration = u32::try_from(dts - l.dts).unwrap_or(u32::MAX).max(1);
            self.done.push(l);
        }
        self.last = Some(Sample { dts, duration: 0, key, data });
    }

    /// The last sample, given its duration (the recording ends).
    fn end(&mut self, duration: u32) {
        if let Some(mut l) = self.last.take() {
            l.duration = duration.max(1);
            self.done.push(l);
        }
    }

    fn span(&self) -> u64 {
        self.done.iter().map(|s| u64::from(s.duration)).sum()
    }
}

/// A fragmented MP4 being written: its header from the first key frame (the decoder's
/// configuration in it), then a fragment at a time.
pub struct Mp4 {
    /// The first key frame's time (µs, the stream's clock).
    t0: i64,
    seq: u32,
    audio: bool,
    video: Track,
    sound: Track,
}

impl Mp4 {
    /// From the stream's first key frame (`annexb`, at `ts` µs, `width` by `height`): the file's
    /// header, and what writes the rest. None when it isn't one (no sequence and picture
    /// parameter sets in it).
    pub fn start(annexb: &[u8], ts: i64, width: u16, height: u16, audio: bool) -> Option<(Self, Vec<u8>)> {
        let units = nal_units(annexb);
        let sps = units.iter().find(|u| nal_type(u) == 7)?;
        let pps = units.iter().find(|u| nal_type(u) == 8)?;
        let mut mp4 = Self { t0: ts, seq: 0, audio, video: Track::default(), sound: Track::default() };
        let header = header(sps, pps, width, height, audio);
        mp4.video.push(0, true, length_prefixed(&units));
        Some((mp4, header))
    }

    fn ticks(&self, ts: i64, scale: u32) -> u64 {
        let us = u128::try_from(ts.saturating_sub(self.t0).max(0)).unwrap_or(0);
        u64::try_from(us * u128::from(scale) / 1_000_000).unwrap_or(u64::MAX)
    }

    /// A picture: a fragment of what came before it, when it's a key frame and enough has.
    pub fn video(&mut self, ts: i64, key: bool, annexb: &[u8]) -> Vec<u8> {
        let dts = self.ticks(ts, VIDEO_SCALE);
        let data = length_prefixed(&nal_units(annexb));
        // What came before it, now complete, is cut off at a key frame.
        let mut cut = Vec::new();
        if key && let Some(l) = &self.video.last {
            let lasted = u64::from(u32::try_from(dts.saturating_sub(l.dts)).unwrap_or(u32::MAX).max(1));
            if self.video.span() + lasted >= u64::from(VIDEO_SCALE) * FRAGMENT / 1_000_000 {
                self.video.push(dts, key, data);
                // The key frame just pushed starts the next fragment.
                let next = self.video.last.take();
                cut = self.fragment();
                self.video.last = next;
                return cut;
            }
        }
        self.video.push(dts, key, data);
        if self.video.span() >= 2 * u64::from(VIDEO_SCALE) * FRAGMENT / 1_000_000 {
            cut = self.fragment();
        }
        cut
    }

    /// A packet of sound (its picture's clock); before the first picture, none.
    pub fn audio(&mut self, ts: i64, data: &[u8]) -> Vec<u8> {
        if !self.audio || ts < self.t0 {
            return Vec::new();
        }
        let dts = self.ticks(ts, AUDIO_SCALE);
        self.sound.push(dts, true, data.to_vec());
        // Sound with no pictures for a while (a still page that plays) goes out on its own.
        if self.sound.span() >= 2 * u64::from(AUDIO_SCALE) * FRAGMENT / 1_000_000 && self.video.done.is_empty() {
            return self.fragment();
        }
        Vec::new()
    }

    /// The rest: its last picture lasting `last` (until the recording stopped).
    pub fn finish(&mut self, last: Duration) -> Vec<u8> {
        let ticks = u32::try_from(last.min(LAST_AT_MOST).as_micros() * u128::from(VIDEO_SCALE) / 1_000_000).unwrap_or(u32::MAX);
        self.video.end(ticks);
        self.sound.end(AUDIO_SCALE / 50);
        self.fragment()
    }

    /// A fragment (`moof` and `mdat`) of the complete samples; empty when there are none.
    fn fragment(&mut self) -> Vec<u8> {
        let v = std::mem::take(&mut self.video.done);
        let a = std::mem::take(&mut self.sound.done);
        if v.is_empty() && a.is_empty() {
            return Vec::new();
        }
        self.seq += 1;
        let seq = self.seq;
        let mut b = Boxes::default();
        let mut offsets = Vec::new();
        b.put(b"moof", |b| {
            b.full(b"mfhd", 0, 0, |b| b.u32(seq));
            for (id, samples) in [(1u32, &v), (2, &a)] {
                let Some(first) = samples.first() else { continue };
                b.put(b"traf", |b| {
                    // Data offsets from the fragment's own start.
                    b.full(b"tfhd", 0, 0x02_0000, |b| b.u32(id));
                    b.full(b"tfdt", 1, 0, |b| b.u64(first.dts));
                    b.full(b"trun", 0, 0x01 | 0x100 | 0x200 | 0x400, |b| {
                        b.u32(u32::try_from(samples.len()).unwrap_or(u32::MAX));
                        offsets.push((id, b.0.len()));
                        b.u32(0);
                        for s in samples {
                            b.u32(s.duration);
                            b.u32(u32::try_from(s.data.len()).unwrap_or(u32::MAX));
                            // A key frame depends on no other; the rest do, and aren't sync samples.
                            b.u32(if s.key { 0x0200_0000 } else { 0x0101_0000 });
                        }
                    });
                });
            }
        });
        let moof = b.0.len();
        let video_bytes: usize = v.iter().map(|s| s.data.len()).sum();
        let audio_bytes: usize = a.iter().map(|s| s.data.len()).sum();
        for (id, at) in offsets {
            let off = moof + 8 + if id == 1 { 0 } else { video_bytes };
            b.set_u32(at, u32::try_from(off).unwrap_or(u32::MAX));
        }
        b.u32(u32::try_from(8 + video_bytes + audio_bytes).unwrap_or(u32::MAX));
        b.bytes(b"mdat");
        for s in v.iter().chain(a.iter()) {
            b.bytes(&s.data);
        }
        b.0
    }
}

/// `ftyp` and `moov`: a video track (H.264 as `avc3`, its parameter sets in band as well), a sound
/// track when there's sound (Opus), their samples all in fragments (`mvex`).
fn header(sps: &[u8], pps: &[u8], width: u16, height: u16, audio: bool) -> Vec<u8> {
    let mut b = Boxes::default();
    b.put(b"ftyp", |b| {
        b.bytes(b"isom");
        b.u32(0x200);
        for c in [b"isom", b"iso6", b"avc1", b"mp41"] {
            b.bytes(c);
        }
    });
    b.put(b"moov", |b| {
        b.full(b"mvhd", 0, 0, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(1000);
            b.u32(0);
            b.u32(0x0001_0000);
            b.u16(0x0100);
            b.u16(0);
            b.u32(0);
            b.u32(0);
            b.matrix();
            for _ in 0..6 {
                b.u32(0);
            }
            b.u32(3);
        });
        track(b, 1, VIDEO_SCALE, b"vide", width, height, |b| {
            b.put(b"avc3", |b| {
                b.bytes(&[0; 6]);
                b.u16(1);
                b.bytes(&[0; 16]);
                b.u16(width);
                b.u16(height);
                b.u32(0x0048_0000);
                b.u32(0x0048_0000);
                b.u32(0);
                b.u16(1);
                b.bytes(&[0; 32]);
                b.u16(0x0018);
                b.u16(0xffff);
                b.put(b"avcC", |b| {
                    b.u8(1);
                    for i in 1..4 {
                        b.u8(sps.get(i).copied().unwrap_or(0));
                    }
                    // Lengths of four bytes; one of each parameter set.
                    b.u8(0xff);
                    b.u8(0xe1);
                    b.u16(u16::try_from(sps.len()).unwrap_or(u16::MAX));
                    b.bytes(sps);
                    b.u8(1);
                    b.u16(u16::try_from(pps.len()).unwrap_or(u16::MAX));
                    b.bytes(pps);
                });
            });
        });
        if audio {
            track(b, 2, AUDIO_SCALE, b"soun", 0, 0, |b| {
                b.put(b"Opus", |b| {
                    b.bytes(&[0; 6]);
                    b.u16(1);
                    b.u32(0);
                    b.u32(0);
                    b.u16(2);
                    b.u16(16);
                    b.u16(0);
                    b.u16(0);
                    b.u32(u32::from(u16::try_from(AUDIO_SCALE).unwrap_or(u16::MAX)) << 16);
                    b.put(b"dOps", |b| {
                        b.u8(0);
                        b.u8(2);
                        b.u16(PRE_SKIP);
                        b.u32(AUDIO_SCALE);
                        b.u16(0);
                        b.u8(0);
                    });
                });
            });
        }
        b.put(b"mvex", |b| {
            for id in if audio { &[1u32, 2][..] } else { &[1u32][..] } {
                b.full(b"trex", 0, 0, |b| {
                    b.u32(*id);
                    b.u32(1);
                    b.u32(0);
                    b.u32(0);
                    b.u32(0);
                });
            }
        });
    });
    b.0
}

/// A `trak` with no samples of its own (they're in fragments): its kind (`vide` or `soun`) and
/// its sample description (`entry`).
fn track(b: &mut Boxes, id: u32, scale: u32, kind: &[u8; 4], width: u16, height: u16, entry: impl FnOnce(&mut Boxes)) {
    let video = kind == b"vide";
    b.put(b"trak", |b| {
        b.full(b"tkhd", 0, 3, |b| {
            b.u32(0);
            b.u32(0);
            b.u32(id);
            b.u32(0);
            b.u32(0);
            b.u32(0);
            b.u32(0);
            b.u16(0);
            b.u16(0);
            b.u16(if video { 0 } else { 0x0100 });
            b.u16(0);
            b.matrix();
            b.u32(u32::from(width) << 16);
            b.u32(u32::from(height) << 16);
        });
        b.put(b"mdia", |b| {
            b.full(b"mdhd", 0, 0, |b| {
                b.u32(0);
                b.u32(0);
                b.u32(scale);
                b.u32(0);
                // "und", packed.
                b.u16(0x55c4);
                b.u16(0);
            });
            b.full(b"hdlr", 0, 0, |b| {
                b.u32(0);
                b.bytes(kind);
                b.bytes(&[0; 12]);
                b.bytes(if video { b"kiln video\0" } else { b"kiln sound\0" });
            });
            b.put(b"minf", |b| {
                if video {
                    b.full(b"vmhd", 0, 1, |b| b.bytes(&[0; 8]));
                } else {
                    b.full(b"smhd", 0, 0, |b| b.u32(0));
                }
                b.put(b"dinf", |b| {
                    b.full(b"dref", 0, 0, |b| {
                        b.u32(1);
                        b.full(b"url ", 0, 1, |_| {});
                    });
                });
                b.put(b"stbl", |b| {
                    b.full(b"stsd", 0, 0, |b| {
                        b.u32(1);
                        entry(b);
                    });
                    for k in [b"stts", b"stsc", b"stco"] {
                        b.full(k, 0, 0, |b| b.u32(0));
                    }
                    b.full(b"stsz", 0, 0, |b| {
                        b.u32(0);
                        b.u32(0);
                    });
                });
            });
        });
    });
}

// ------------------------------------------------------------------------------ a recording

/// Where a recording's files are.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Files {
    pub video: PathBuf,
    pub inputs: PathBuf,
    pub test: PathBuf,
}

/// A recording, done: its files, how long, how big.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Recorded {
    #[serde(flatten)]
    pub files: Files,
    pub seconds: f64,
    pub bytes: u64,
    pub pictures: u64,
}

enum Msg {
    Config(Value),
    Video { ts: i64, key: bool, data: Vec<u8> },
    Audio { ts: i64, data: Vec<u8> },
    Log(Value),
    Stop { title: String },
}

/// A recording under way: what the stream sends goes to its writer (a thread of its own, the file
/// written as it comes).
pub struct Recording {
    tx: std::sync::mpsc::Sender<Msg>,
    done: parking_lot::Mutex<Option<std::sync::mpsc::Receiver<anyhow::Result<Recorded>>>>,
    started: Instant,
    pub files: Files,
}

impl Recording {
    /// Start one at `dir/<name>.mp4` (and its log and test beside it), for a page at `url` in a
    /// window `viewport`.
    pub fn start(dir: &Path, name: &str, url: &str, viewport: Value) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let files = Files { video: dir.join(format!("{name}.mp4")), inputs: dir.join(format!("{name}.inputs.jsonl")), test: dir.join(format!("{name}.spec.ts")) };
        let mut video = std::io::BufWriter::new(std::fs::File::create(&files.video)?);
        let mut inputs = std::io::BufWriter::new(std::fs::File::create(&files.inputs)?);
        let started = Instant::now();
        let first = json!({"t": 0, "start": {"url": url, "viewport": viewport, "at": crate::util::now_ms()}});
        writeln!(inputs, "{first}")?;
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (done_tx, done) = std::sync::mpsc::channel();
        let f = files.clone();
        std::thread::Builder::new().name("kiln-record".into()).spawn(move || {
            let r = write(&rx, &mut video, &mut inputs, &f, started, first);
            let _ = done_tx.send(r);
        })?;
        Ok(Self { tx, done: parking_lot::Mutex::new(Some(done)), started, files })
    }

    /// A chunk of the stream's (`crate::browser::stream::frame`'s kind, flags, time, bytes).
    pub fn chunk(&self, kind: u8, flags: u8, ts: i64, bytes: &[u8]) {
        let m = match kind {
            b'c' => serde_json::from_slice::<Value>(bytes).ok().map(|mut v| {
                if let Some(o) = v.as_object_mut() {
                    o.insert("audio".into(), json!(flags == b'a'));
                }
                Msg::Config(v)
            }),
            b'v' => Some(Msg::Video { ts, key: flags == b'k', data: bytes.to_vec() }),
            b'a' => Some(Msg::Audio { ts, data: bytes.to_vec() }),
            _ => None,
        };
        if let Some(m) = m {
            let _ = self.tx.send(m);
        }
    }

    /// Something done in the page or to it (`{"by", "input"}`, `{"by", "act"}`, `{"navigated"}`,
    /// `{"viewport"}`), at its time.
    pub fn log(&self, mut what: Value) {
        if let Some(o) = what.as_object_mut() {
            o.insert("t".into(), json!(u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)));
        }
        let _ = self.tx.send(Msg::Log(what));
    }

    /// Stop it: the files finished (its test written from its log, `title` its name).
    pub fn stop(&self, title: &str) -> anyhow::Result<Recorded> {
        let _ = self.tx.send(Msg::Stop { title: title.to_owned() });
        let rx = self.done.lock().take().ok_or_else(|| anyhow::anyhow!("it stopped already"))?;
        rx.recv_timeout(Duration::from_secs(30)).map_err(|_| anyhow::anyhow!("the recording didn't finish"))?
    }
}

/// The writer: the video from its first key frame, the log as it comes; at the end, the test.
fn write(rx: &std::sync::mpsc::Receiver<Msg>, video: &mut impl std::io::Write, inputs: &mut impl std::io::Write, files: &Files, started: Instant, first: Value) -> anyhow::Result<Recorded> {
    let mut mp4: Option<Mp4> = None;
    let (mut size, mut audio) = ((0u16, 0u16), false);
    let (mut bytes, mut pictures) = (0u64, 0u64);
    let mut last_picture = Instant::now();
    let mut log = vec![first];
    let mut put = |b: &[u8], bytes: &mut u64| -> anyhow::Result<()> {
        if !b.is_empty() {
            video.write_all(b)?;
            *bytes += u64::try_from(b.len()).unwrap_or(0);
        }
        Ok(())
    };
    let title = loop {
        match rx.recv() {
            Ok(Msg::Config(c)) => {
                if c.get("audio").and_then(Value::as_bool) == Some(true) {
                    audio = true;
                } else {
                    let dim = |k: &str| c.get(k).and_then(Value::as_u64).and_then(|v| u16::try_from(v).ok()).unwrap_or(0);
                    size = (dim("codedWidth"), dim("codedHeight"));
                }
            }
            Ok(Msg::Video { ts, key, data }) => {
                last_picture = Instant::now();
                match &mut mp4 {
                    Some(m) => {
                        put(&m.video(ts, key, &data), &mut bytes)?;
                        pictures += 1;
                    }
                    None if key => {
                        if let Some((m, header)) = Mp4::start(&data, ts, size.0, size.1, audio) {
                            put(&header, &mut bytes)?;
                            mp4 = Some(m);
                            pictures += 1;
                        }
                    }
                    None => {}
                }
            }
            Ok(Msg::Audio { ts, data }) => {
                if let Some(m) = &mut mp4 {
                    put(&m.audio(ts, &data), &mut bytes)?;
                }
            }
            Ok(Msg::Log(v)) => {
                writeln!(inputs, "{v}")?;
                log.push(v);
            }
            Ok(Msg::Stop { title }) => break title,
            Err(_) => break String::new(),
        }
    };
    let end = json!({"t": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX), "end": {}});
    writeln!(inputs, "{end}")?;
    inputs.flush()?;
    if let Some(m) = &mut mp4 {
        put(&m.finish(last_picture.elapsed()), &mut bytes)?;
    }
    video.flush()?;
    log.push(end);
    std::fs::write(&files.test, spec(&log, &title))?;
    Ok(Recorded { files: files.clone(), seconds: started.elapsed().as_secs_f64(), bytes, pictures })
}

// ------------------------------------------------------------------------------ the log, used

/// A recording's log (`.inputs.jsonl`), read.
pub fn read_log(path: &Path) -> anyhow::Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    Ok(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// A string as JavaScript writes it.
fn js(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn point(v: Option<&Value>) -> Option<(f64, f64)> {
    let p = v?.as_array()?;
    Some((p.first()?.as_f64()?, p.get(1)?.as_f64()?))
}

/// A key as Playwright presses it: its modifiers first (`Control+a`).
fn chord(key: &str, mods: i64) -> String {
    let mut c = String::new();
    for (bit, name) in [(2, "Control"), (1, "Alt"), (4, "Meta"), (8, "Shift")] {
        if mods & bit != 0 && key != name {
            c.push_str(name);
            c.push('+');
        }
    }
    c.push_str(if key == " " { "Space" } else { key });
    c
}

/// A recording's log as a Playwright test: the window it had, the page it began at, what was done
/// (pointer presses as clicks, taps or drags, keys and text, the wheel, the agent's actions), where
/// it went after each, and where it ended, to say what it should show.
pub fn spec(log: &[Value], title: &str) -> String {
    let start = log.iter().find_map(|l| l.get("start"));
    let url = start.and_then(|s| s.get("url")).and_then(Value::as_str).unwrap_or("about:blank");
    let vp = start.and_then(|s| s.get("viewport"));
    let n = |k: &str, d: f64| vp.and_then(|v| v.get(k)).and_then(Value::as_f64).unwrap_or(d);
    let mobile = vp.and_then(|v| v.get("mobile")).and_then(Value::as_bool).unwrap_or(false);
    let mut body: Vec<String> = vec![format!("  await page.goto({});", js(url))];
    let mut typing = String::new();
    let mut down: Option<(f64, f64, String, bool)> = None;
    let mut wheel = (0.0, 0.0, None::<(f64, f64)>);
    let mut last_t = 0u64;
    let mut where_now = url.to_owned();
    let flush = |body: &mut Vec<String>, typing: &mut String, wheel: &mut (f64, f64, Option<(f64, f64)>)| {
        if !typing.is_empty() {
            body.push(format!("  await page.keyboard.type({});", js(typing)));
            typing.clear();
        }
        if let Some((x, y)) = wheel.2.take() {
            body.push(format!("  await page.mouse.move({x}, {y});"));
            body.push(format!("  await page.mouse.wheel({}, {});", wheel.0.round(), wheel.1.round()));
            wheel.0 = 0.0;
            wheel.1 = 0.0;
        }
    };
    for l in log {
        let t = l.get("t").and_then(Value::as_u64).unwrap_or(last_t);
        // A pause before it (the page may have been doing something), as a hint.
        if t.saturating_sub(last_t) > 1000 && (l.get("input").is_some() || l.get("act").is_some()) {
            flush(&mut body, &mut typing, &mut wheel);
            body.push(format!("  await page.waitForTimeout({});", (t - last_t).min(1000)));
        }
        if l.get("input").is_some() || l.get("act").is_some() {
            last_t = t;
        }
        if let Some(v) = l.get("viewport").filter(|_| l.get("start").is_none()) {
            flush(&mut body, &mut typing, &mut wheel);
            let w = v.get("width").and_then(Value::as_u64).unwrap_or(0);
            let h = v.get("height").and_then(Value::as_u64).unwrap_or(0);
            if w > 0 && h > 0 {
                body.push(format!("  await page.setViewportSize({{ width: {w}, height: {h} }});"));
            }
        }
        if let Some(u) = l.get("navigated").and_then(Value::as_str) {
            if u != where_now && !body.is_empty() && body.len() > 1 {
                flush(&mut body, &mut typing, &mut wheel);
                body.push(format!("  await page.waitForURL({});", js(u)));
            }
            u.clone_into(&mut where_now);
        }
        if let Some(i) = l.get("input") {
            let s = |k: &str| i.get(k).and_then(Value::as_str).unwrap_or("");
            let f = |k: &str| i.get(k).and_then(Value::as_f64).unwrap_or(0.0);
            let mods = i.get("mods").and_then(Value::as_i64).unwrap_or(0);
            match s("t") {
                "p" => match s("k") {
                    "down" => {
                        flush(&mut body, &mut typing, &mut wheel);
                        down = Some((f("x"), f("y"), s("pt").to_owned(), false));
                    }
                    "move" => {
                        if let Some((x0, y0, _, moved)) = &mut down {
                            *moved |= (f("x") - *x0).hypot(f("y") - *y0) > 8.0;
                        }
                    }
                    "up" => {
                        if let Some((x0, y0, pt, moved)) = down.take() {
                            let (x1, y1) = (f("x").round(), f("y").round());
                            let (x0, y0) = (x0.round(), y0.round());
                            if moved || (x1 - x0).hypot(y1 - y0) > 8.0 {
                                body.push(format!("  await page.mouse.move({x0}, {y0});"));
                                body.push("  await page.mouse.down();".to_owned());
                                body.push(format!("  await page.mouse.move({x1}, {y1}, {{ steps: 10 }});"));
                                body.push("  await page.mouse.up();".to_owned());
                            } else if pt == "touch" {
                                body.push(format!("  await page.touchscreen.tap({x1}, {y1});"));
                            } else if i.get("button").and_then(Value::as_i64) == Some(2) {
                                body.push(format!("  await page.mouse.click({x1}, {y1}, {{ button: \"right\" }});"));
                            } else {
                                body.push(format!("  await page.mouse.click({x1}, {y1});"));
                            }
                        }
                    }
                    _ => {}
                },
                "w" => {
                    if !typing.is_empty() {
                        flush(&mut body, &mut typing, &mut wheel);
                    }
                    wheel.0 += f("dx");
                    wheel.1 += f("dy");
                    wheel.2 = Some((f("x").round(), f("y").round()));
                }
                "k" if s("k") == "down" => {
                    let text = s("text");
                    if !text.is_empty() && mods & (2 | 4) == 0 && text != "\r" {
                        typing.push_str(text);
                    } else if !["Shift", "Control", "Alt", "Meta"].contains(&s("key")) {
                        flush(&mut body, &mut typing, &mut wheel);
                        body.push(format!("  await page.keyboard.press({});", js(&chord(s("key"), mods))));
                    }
                }
                "x" => {
                    flush(&mut body, &mut typing, &mut wheel);
                    body.push(format!("  await page.keyboard.insertText({});", js(s("text"))));
                }
                _ => {}
            }
        }
        if let Some(a) = l.get("act").and_then(Value::as_object).and_then(|o| o.iter().next()) {
            flush(&mut body, &mut typing, &mut wheel);
            let (verb, arg) = a;
            match verb.as_str() {
                "click" | "tap" => {
                    if let Some((x, y)) = point(Some(arg)) {
                        body.push(if verb == "tap" { format!("  await page.touchscreen.tap({x}, {y});") } else { format!("  await page.mouse.click({x}, {y});") });
                    }
                }
                "type" => body.push(format!("  await page.keyboard.type({});", js(arg.as_str().unwrap_or("")))),
                "key" => body.push(format!("  await page.keyboard.press({});", js(arg.as_str().unwrap_or("")))),
                "scroll" => {
                    if let Some((dx, dy)) = point(Some(arg)) {
                        body.push(format!("  await page.mouse.wheel({dx}, {dy});"));
                    }
                }
                "go" => body.push(format!("  await page.goto({});", js(arg.as_str().unwrap_or("")))),
                "back" => body.push("  await page.goBack();".to_owned()),
                "forward" => body.push("  await page.goForward();".to_owned()),
                "reload" => body.push("  await page.reload();".to_owned()),
                "wait_ms" => body.push(format!("  await page.waitForTimeout({});", arg.as_u64().unwrap_or(0).min(10_000))),
                _ => {}
            }
        }
    }
    flush(&mut body, &mut typing, &mut wheel);
    body.push(format!("  // Where it ended. Say what the page should show there, too.\n  await expect(page).toHaveURL({});", js(&where_now)));
    let name = if title.trim().is_empty() { url } else { title };
    format!(
        "// Recorded by kiln from a live page ({url}): what was done in it, played again. Add what\n// it should show.\nimport {{ test, expect }} from \"@playwright/test\";\n\ntest.use({{ viewport: {{ width: {w}, height: {h} }}, deviceScaleFactor: {s}, hasTouch: {mobile}, isMobile: {mobile} }});\n\ntest({name}, async ({{ page }}) => {{\n{body}\n}});\n",
        w = n("width", 1280.0),
        h = n("height", 800.0),
        s = n("scale", 1.0),
        name = js(name),
        body = body.join("\n"),
    )
}

/// What a recording's log does again in a live page, in order, each after its pause (at most two
/// seconds): `{"input": ...}` (a person's) or `{"act": ...}` (the agent's), the window first.
pub fn replay_steps(log: &[Value]) -> (Option<String>, Option<Value>, Vec<(Duration, Value)>) {
    let start = log.iter().find_map(|l| l.get("start"));
    let url = start.and_then(|s| s.get("url")).and_then(Value::as_str).map(str::to_owned);
    let viewport = start.and_then(|s| s.get("viewport")).cloned();
    let mut steps = Vec::new();
    let mut last = 0u64;
    for l in log {
        let t = l.get("t").and_then(Value::as_u64).unwrap_or(last);
        let step = if let Some(i) = l.get("input") { Some(json!({"input": i})) } else { l.get("act").map(|a| json!({"act": a})) };
        if let Some(step) = step {
            steps.push((Duration::from_millis(t.saturating_sub(last).min(2000)), step));
            last = t;
        }
    }
    (url, viewport, steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key frame as an encoder sends one: a sequence parameter set, a picture's, a key slice.
    fn key_frame() -> Vec<u8> {
        [&[0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0xab][..], &[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80], &[0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33]].concat()
    }

    /// The top boxes of a file: their types and sizes.
    fn boxes(b: &[u8]) -> Vec<(String, usize)> {
        let mut out = Vec::new();
        let mut at = 0;
        while let (Some(size), Some(kind)) = (b.get(at..at + 4), b.get(at + 4..at + 8)) {
            let size = size.iter().fold(0usize, |n, x| (n << 8) | usize::from(*x));
            out.push((String::from_utf8_lossy(kind).into_owned(), size));
            if size < 8 {
                break;
            }
            at += size;
        }
        out
    }

    #[test]
    fn annex_b_becomes_length_prefixed_units() {
        let frame = key_frame();
        let units = nal_units(&frame);
        assert_eq!(units.len(), 3);
        assert_eq!(units.iter().map(|u| nal_type(u)).collect::<Vec<_>>(), [7, 8, 5]);
        let lp = length_prefixed(&units);
        assert_eq!(lp.get(..5), Some(&[0, 0, 0, 5, 0x67][..]));
        assert!(nal_units(&[0, 0, 0]).is_empty());
    }

    #[test]
    fn a_recording_is_a_header_then_fragments_each_from_a_key_frame() -> anyhow::Result<()> {
        let (mut m, header) = Mp4::start(&key_frame(), 1_000_000, 390, 640, true).ok_or_else(|| anyhow::anyhow!("no start"))?;
        let tops = boxes(&header);
        assert_eq!(tops.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["ftyp", "moov"]);
        assert_eq!(tops.iter().map(|(_, n)| n).sum::<usize>(), header.len(), "its boxes are its whole");
        for kind in [b"avc3", b"avcC", b"Opus", b"dOps", b"mvex", b"trex"] {
            assert!(header.windows(4).any(|w| w == kind), "{}", String::from_utf8_lossy(kind));
        }
        let delta = [0u8, 0, 0, 1, 0x41, 0x9a, 0x02];
        let mut out = Vec::new();
        // Thirty pictures a second for a second and a half, with sound.
        for i in 1..45 {
            out.extend(m.video(1_000_000 + i * 33_333, false, &delta));
            out.extend(m.audio(1_000_000 + i * 20_000, &[0xfc, 1, 2]));
        }
        assert!(out.is_empty() || boxes(&out).iter().all(|(k, _)| k == "moof" || k == "mdat"));
        // A key frame past a second: what came before goes out as a fragment.
        let cut = m.video(2_600_000, true, &key_frame());
        let tops = boxes(&cut);
        assert_eq!(tops.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["moof", "mdat"], "{tops:?}");
        assert_eq!(tops.iter().map(|(_, n)| n).sum::<usize>(), cut.len());
        let rest = m.finish(Duration::from_millis(500));
        assert_eq!(boxes(&rest).iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["moof", "mdat"]);
        // Nothing but a key frame starts one.
        assert!(Mp4::start(&delta, 0, 1, 1, false).is_none());
        Ok(())
    }

    #[test]
    fn a_still_frame_takes_its_pictures_place() {
        let mut t = Track::default();
        t.push(0, true, vec![1]);
        t.push(3000, false, vec![2]);
        // Sent a microsecond on (a tick at most): the same picture, sharp.
        t.push(3000, true, vec![3]);
        t.push(6000, false, vec![4]);
        t.end(3000);
        let got: Vec<(u64, u32, bool, Vec<u8>)> = t.done.iter().map(|x| (x.dts, x.duration, x.key, x.data.clone())).collect();
        assert_eq!(got, [(0, 3000, true, vec![1]), (3000, 3000, true, vec![3]), (6000, 3000, false, vec![4])]);
        // Pictures close together, each its own still.
        let mut t = Track::default();
        t.push(0, false, vec![1]);
        t.push(0, false, vec![2]);
        t.end(1);
        assert_eq!(t.done.iter().map(|x| x.dts).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn a_log_becomes_a_test_of_what_was_done() {
        let log: Vec<Value> = [
            json!({"t": 0, "start": {"url": "http://localhost:5173/", "viewport": {"width": 390, "height": 640, "scale": 2, "mobile": true}}}),
            json!({"t": 100, "by": "person", "input": {"t": "p", "k": "down", "x": 60.2, "y": 25, "pt": "touch"}}),
            json!({"t": 150, "by": "person", "input": {"t": "p", "k": "up", "x": 60, "y": 25, "pt": "touch"}}),
            json!({"t": 300, "by": "person", "input": {"t": "k", "k": "down", "key": "h", "text": "h"}}),
            json!({"t": 320, "by": "person", "input": {"t": "k", "k": "down", "key": "i", "text": "i"}}),
            json!({"t": 400, "by": "person", "input": {"t": "k", "k": "down", "key": "Enter", "text": "\r"}}),
            json!({"t": 450, "navigated": "http://localhost:5173/done"}),
            json!({"t": 3000, "by": "agent", "act": {"click": [10, 20]}}),
            json!({"t": 3100, "by": "person", "input": {"t": "k", "k": "down", "key": "a", "text": "a", "mods": 2}}),
            json!({"t": 3200, "by": "person", "input": {"t": "x", "text": "pasted \"words\""}}),
            json!({"t": 3500, "end": {}}),
        ]
        .into();
        let s = spec(&log, "Signs in");
        for line in [
            "test.use({ viewport: { width: 390, height: 640 }, deviceScaleFactor: 2, hasTouch: true, isMobile: true });",
            "test(\"Signs in\", async ({ page }) => {",
            "  await page.goto(\"http://localhost:5173/\");",
            "  await page.touchscreen.tap(60, 25);",
            "  await page.keyboard.type(\"hi\");",
            "  await page.keyboard.press(\"Enter\");",
            "  await page.waitForURL(\"http://localhost:5173/done\");",
            "  await page.waitForTimeout(1000);",
            "  await page.mouse.click(10, 20);",
            "  await page.keyboard.press(\"Control+a\");",
            "  await page.keyboard.insertText(\"pasted \\\"words\\\"\");",
            "  await expect(page).toHaveURL(\"http://localhost:5173/done\");",
        ] {
            assert!(s.contains(line), "{line}\n---\n{s}");
        }
        let (url, viewport, steps) = replay_steps(&log);
        assert_eq!(url.as_deref(), Some("http://localhost:5173/"));
        assert_eq!(viewport.and_then(|v| v.get("width").cloned()), Some(json!(390)));
        assert_eq!(steps.len(), 8);
        assert_eq!(steps.get(5).map(|(d, _)| *d), Some(Duration::from_secs(2)), "a long pause is shortened");
    }
}
