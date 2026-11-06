use std::path::Path;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

/// A new id: a UUIDv7 ([`new_v7`]) as 32 lowercase hex digits, which sort as they were made.
pub fn new_id() -> String {
    new_v7().simple().to_string()
}

/// A v7 id's counter (RFC 9562 §6.2, method 1: a counter of 12 to 42 bits after the millisecond, in `rand_a` and on
/// into `rand_b`), in bits: 16, so the millisecond (48 bits) and the counter fit one atomic word.
const V7_COUNTER_BITS: u32 = 16;
const V7_COUNTER: u64 = (1 << V7_COUNTER_BITS) - 1;
/// A v7 id's millisecond, in bits (RFC 9562 §5.7, `unix_ts_ms`).
const V7_MS: u64 = (1 << 48) - 1;

/// The last v7 id's millisecond and counter, as `millisecond << V7_COUNTER_BITS | counter`.
static V7_LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A UUIDv7 that sorts after every one this process made before it, made with no lock (goal 5 of the API runtime
/// program, plan 01a1a59c2aae, its top goal). `uuid::Uuid::now_v7` keeps its millisecond and counter in one context
/// behind a std `Mutex` the whole process shares, which an agent session, an upload or a terminal session took as it
/// was made (main's census16: 0.1 a second). Here they're one atomic word, moved on by compare-and-swap
/// ([`v7_next`]), and the rest of the id is random (`rand::random`, a thread's own generator).
pub fn new_v7() -> uuid::Uuid {
    let now = u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0) & V7_MS;
    // Half the counter's range at most, so a millisecond has the other half to count in (uuid's own leaves its
    // counter's top bit clear for the same).
    let start = u64::from(rand::random::<u16>() >> 1);
    let mut last = V7_LAST.load(std::sync::atomic::Ordering::Acquire);
    let next = loop {
        let next = v7_next(last, now, start);
        match V7_LAST.compare_exchange_weak(last, next, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire) {
            Ok(_) => break next,
            Err(seen) => last = seen,
        }
    };
    let counter = next & V7_COUNTER;
    let [_, _, r2, r3, r4, r5, r6, r7, r8, r9]: [u8; 10] = rand::random();
    // The counter's top 12 bits are `rand_a` (byte 0's low nibble and byte 1, after the version), its low 4 begin
    // `rand_b` (byte 2's bits 5 to 2, after the variant's 2), so ids sort by millisecond, then by counter.
    let bytes = [((counter >> 12) as u8) & 0x0F, (counter >> 4) as u8, (((counter & 0x0F) as u8) << 2) | (r2 & 0x03), r3, r4, r5, r6, r7, r8, r9];
    uuid::Builder::from_unix_timestamp_millis(next >> V7_COUNTER_BITS, &bytes).into_uuid()
}

/// The v7 millisecond and counter after `last` (as [`V7_LAST`] keeps them) at `now` (a millisecond): a later
/// millisecond starts at `start`; the same or an earlier one (the clock stepped back) counts on from `last`; and a
/// counter run out moves on to the millisecond after `last`'s, from `start`, as uuid's own context does. So each is
/// past the one before.
fn v7_next(last: u64, now: u64, start: u64) -> u64 {
    let (ms, count) = (last >> V7_COUNTER_BITS, last & V7_COUNTER);
    if now > ms {
        (now << V7_COUNTER_BITS) | (start & V7_COUNTER)
    } else if count < V7_COUNTER {
        last.saturating_add(1)
    } else {
        (ms.saturating_add(1).min(V7_MS) << V7_COUNTER_BITS) | (start & V7_COUNTER)
    }
}

/// An id written as [`new_id`] writes them, 32 lowercase hex digits: what a client may make one of
/// its own as (the agents a page starts, `POST /api/harness/sessions`' `ids`).
pub fn is_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An id made from `parts` (a namespace, then what it names), the same every time they're the same
/// and as unlikely to meet another as a random one (a UUIDv8 of their SHA-256): how a retry finds
/// the id its first try made, from the key its sender gave (CP-T3.2, an idempotency key).
pub fn derive_id(parts: &[&str]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for p in parts {
        // Each part's length first, so no two lists of parts read the same.
        h.update(u64::try_from(p.len()).unwrap_or(u64::MAX).to_le_bytes());
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    for (b, d) in bytes.iter_mut().zip(digest.iter()) {
        *b = *d;
    }
    uuid::Builder::from_custom_bytes(bytes).into_uuid().simple().to_string()
}

/// 32 random bytes, URL-safe base64. Used for tokens and secrets.
/// 32 random bytes, base64url: never beginning with `-`, which a command line would take for a
/// flag (`kiln agent --token -Hq3…`).
pub fn random_token() -> String {
    use base64::Engine as _;
    loop {
        let bytes: [u8; 32] = rand::random();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        if !token.starts_with('-') {
            return token;
        }
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(data))
}

/// Constant-time comparison for secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Write a file atomically with the given mode (temp file + rename; the mode is Unix-only).
pub fn write_secret(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    write_private(path, contents, mode, true)
}

/// As [`write_secret`], without waiting for the disk: scratch files a crash may lose (a test's
/// throwaway keys; headroom's note of what it paced, written while the disk may be stalled).
pub fn write_secret_unsynced(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    write_private(path, contents, mode, false)
}

fn write_private(path: &Path, contents: &[u8], mode: u32, sync: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = crate::os::with_mode(std::fs::OpenOptions::new().create(true).truncate(true).write(true), mode)
            .open(&tmp)?;
        f.write_all(contents)?;
        if sync {
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)
}

/// Read a secret file, or create it with a fresh random token. Never prompts.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
        _ => {
            let t = random_token();
            write_secret(path, t.as_bytes(), 0o600)?;
            Ok(t)
        }
    }
}

pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or_default()
}

/// The brotli window for `len` bytes, as bits: the smallest that holds them all, between brotli's least (10
/// bits) and 22 bits (4 MiB). A window past the data finds no match the data's own doesn't, and memory follows
/// the window on both sides: the encoder's at quality 11 takes eight bytes a place of it (22 bits took 34 MB
/// for each UI file), and a decoder's ring buffer grows to the window as the output passes it (goal 11, plan
/// 01a1a87b2e56 steps 3 and 5).
pub fn brotli_window(len: usize) -> u32 {
    // A window of `bits` holds (1 << bits) - 16 bytes.
    (10..22).find(|bits| (1usize << bits).saturating_sub(16) >= len).unwrap_or(22)
}

/// HMAC-SHA256 of `msg` under `key`, hex encoded.
pub fn hmac_hex(key: &[u8], msg: &[u8]) -> String {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            hex::encode(m.finalize().into_bytes())
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    /// CP-T3.2: an id made from a key is the same each time it's made from it, a UUID like any
    /// other id, and another for other parts (however they're split).
    #[test]
    fn an_id_made_from_a_key_is_the_same_each_time_and_apart_from_others() {
        let a = super::derive_id(&["agents.send", "caller", "agent", "key-1"]);
        assert_eq!(a, super::derive_id(&["agents.send", "caller", "agent", "key-1"]));
        assert_eq!(a.len(), 32);
        assert!(uuid::Uuid::parse_str(&a).is_ok_and(|u| u.get_version_num() == 8));
        assert_ne!(a, super::derive_id(&["agents.send", "caller", "agent", "key-2"]));
        assert_ne!(super::derive_id(&["ab", "c"]), super::derive_id(&["a", "bc"]), "split differently, different");
        assert_ne!(super::derive_id(&["a"]), super::new_id());
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): v7 ids made with no lock are UUIDv7s of now, each thread's sorting
    /// in the order it made them and none made twice, from many threads at once.
    #[test]
    fn g5_v7_ids_made_with_no_lock_sort_as_made_and_are_never_made_twice() -> anyhow::Result<()> {
        let before = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)?;
        let threads: Vec<_> = (0..8).map(|_| std::thread::spawn(|| (0..5_000).map(|_| super::new_v7()).collect::<Vec<_>>())).collect();
        let mut all = std::collections::HashSet::new();
        for t in threads {
            let made = t.join().map_err(|_| anyhow::anyhow!("a thread making ids panicked"))?;
            anyhow::ensure!(made.windows(2).all(|w| matches!(w, [a, b] if a < b)), "a thread's ids out of the order it made them");
            for u in made {
                anyhow::ensure!(u.get_version_num() == 7 && u.get_variant() == uuid::Variant::RFC4122, "{u} isn't a UUIDv7");
                let (secs, nanos) = u.get_timestamp().map(|t| t.to_unix()).ok_or_else(|| anyhow::anyhow!("{u} has no time"))?;
                let ms = secs.saturating_mul(1_000).saturating_add(u64::from(nanos / 1_000_000));
                anyhow::ensure!(ms + 1_000 >= before, "{u} is of {ms}, before {before}");
                anyhow::ensure!(all.insert(u), "{u} made twice");
            }
        }
        let id = super::new_id();
        anyhow::ensure!(super::is_id(&id) && uuid::Uuid::parse_str(&id).is_ok_and(|u| u.get_version_num() == 7), "{id}");
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): the millisecond and counter move on as uuid's own context moves
    /// them: a later millisecond starts afresh, the same one or one the clock stepped back to counts on, and a counter
    /// run out goes on in the next millisecond.
    #[test]
    fn g5_v7s_millisecond_and_counter_move_on_as_uuids_do() {
        use super::{V7_COUNTER, V7_COUNTER_BITS, v7_next};
        let at = |ms: u64, count: u64| (ms << V7_COUNTER_BITS) | count;
        assert_eq!(v7_next(at(10, 5), 11, 300), at(11, 300), "a later millisecond starts at its start");
        assert_eq!(v7_next(at(10, 5), 10, 300), at(10, 6), "the same millisecond counts on");
        assert_eq!(v7_next(at(10, 5), 9, 300), at(10, 6), "the clock stepped back: it counts on from the last");
        assert_eq!(v7_next(at(10, V7_COUNTER), 10, 300), at(11, 300), "a counter run out goes on in the next millisecond");
        assert!(v7_next(at(10, V7_COUNTER), 10, 300) > at(10, V7_COUNTER), "and sorts after it");
    }

    #[test]
    fn a_token_never_begins_with_a_dash() {
        // One in 64 would, without the care.
        assert!((0..10_000).map(|_| super::random_token()).all(|t| !t.starts_with('-') && t.len() == 43));
    }
}
