//! Sigstore's public trusted root through its TUF repository, as Sigstore's clients get it. kiln
//! ships the repository's root metadata (version 16, `root.json`: byte for byte the one Sigstore
//! publishes at tuf-repo-cdn.sigstore.dev and in sigstore/root-signing), and from it takes:
//! - each newer root, signed by a threshold of the root keys of the one before and of its own;
//! - the timestamp, snapshot and targets, each signed by a threshold of its role's keys, none
//!   expired, none older than what kiln last took, each the version (and hashes) the one before
//!   names;
//! - `trusted_root.json`, its length and SHA-256 the targets'.
//!
//! All of it is kept in a directory, and taken again after a day or when the timestamp expires;
//! while the repository can't be reached, what's kept serves until its timestamp expires.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;

use serde_json::Value;

use super::{Alg, Hash, Key, TrustedRoot, rfc3339};

/// Sigstore's public TUF repository.
pub const REPOSITORY: &str = "https://tuf-repo-cdn.sigstore.dev";
/// Its root, version 16, as kiln ships it.
const ROOT: &str = include_str!("root.json");
/// The most any metadata file may be.
const MAX_META: usize = 1 << 20;
/// The most root versions taken in one go.
const MAX_ROOTS: u64 = 64;
/// How long what's kept serves before the repository is asked again.
const FRESH: i64 = 86_400;

/// OLPC's canonical JSON, as TUF signs it: keys in order, no spaces, strings escaping only `"`
/// and `\`.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push('"');
            for c in s.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(&Value::String(k.clone()), out);
                out.push(':');
                if let Some(x) = m.get(k) {
                    canonical(x, out);
                }
            }
            out.push('}');
        }
    }
}

/// Whether TUF key `key` (`ecdsa-sha2-nistp256`, `-nistp384` or `ed25519`) signed `msg` (`sig`, hex).
fn key_signed(key: &Value, msg: &[u8], sig: &str) -> bool {
    let Ok(sig) = hex::decode(sig) else { return false };
    let public = key.pointer("/keyval/public").and_then(Value::as_str).unwrap_or_default();
    let (alg, key) = match key.get("scheme").and_then(Value::as_str).unwrap_or_default() {
        "ecdsa-sha2-nistp256" => (Alg::Ecdsa(Hash::Sha256), super::super::signing::der_of(public).and_then(|(_, der)| Key::from_der(&der).ok())),
        "ecdsa-sha2-nistp384" => (Alg::Ecdsa(Hash::Sha384), super::super::signing::der_of(public).and_then(|(_, der)| Key::from_der(&der).ok())),
        "ed25519" => (Alg::Ed25519, hex::decode(public).ok().map(Key::Ed25519)),
        _ => return false,
    };
    key.is_some_and(|k| k.verifies(alg, msg, &sig))
}

/// A threshold of `role`'s keys in `root` (a root's `signed`) signed `meta`.
fn signed_by_role(meta: &Value, root: &Value, role: &str) -> anyhow::Result<()> {
    let r = root.pointer(&format!("/roles/{role}")).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF root has no {role} role"))?;
    let threshold = r.get("threshold").and_then(Value::as_u64).filter(|t| *t >= 1).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF {role} role has no threshold"))?;
    let ids: BTreeSet<&str> = r.get("keyids").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let keys = root.get("keys").and_then(Value::as_object).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF root has no keys"))?;
    let mut text = String::new();
    canonical(meta.get("signed").ok_or_else(|| anyhow::anyhow!("Sigstore's TUF {role} metadata has nothing signed"))?, &mut text);
    let mut good = BTreeSet::new();
    for s in meta.get("signatures").and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(sig)) = (s.get("keyid").and_then(Value::as_str), s.get("sig").and_then(Value::as_str)) else { continue };
        if ids.contains(id) && !good.contains(id) && keys.get(id).is_some_and(|k| key_signed(k, text.as_bytes(), sig)) {
            good.insert(id);
        }
    }
    anyhow::ensure!(good.len() as u64 >= threshold, "Sigstore's TUF {role} metadata has {} of the {threshold} signatures it needs", good.len());
    Ok(())
}

fn version(meta: &Value) -> u64 {
    meta.pointer("/signed/version").and_then(Value::as_u64).unwrap_or(0)
}

/// `meta` is `kind` (`root`, `timestamp`…), and unexpired at `now`.
fn current(meta: &Value, kind: &str, now: i64) -> anyhow::Result<()> {
    anyhow::ensure!(meta.pointer("/signed/_type").and_then(Value::as_str) == Some(kind), "Sigstore's TUF {kind} metadata is of another kind");
    let expires = rfc3339(meta.pointer("/signed/expires").and_then(Value::as_str).unwrap_or_default())?;
    anyhow::ensure!(expires > now, "Sigstore's TUF {kind} metadata (version {}) expired", version(meta));
    Ok(())
}

/// Root `new` follows `trusted`: its next version, signed by both's root keys.
fn next_root(trusted: &Value, new: &Value) -> anyhow::Result<()> {
    let n = version(trusted) + 1;
    anyhow::ensure!(new.pointer("/signed/_type").and_then(Value::as_str) == Some("root") && version(new) == n, "Sigstore's TUF root {n} isn't version {n} of the root");
    signed_by_role(new, trusted.get("signed").unwrap_or(&Value::Null), "root")?;
    signed_by_role(new, new.get("signed").unwrap_or(&Value::Null), "root")
}

/// `bytes` are what `meta` (a file's entry in the timestamp's or snapshot's `meta`) says: its
/// length and hashes, when it says them.
fn as_listed(meta: &Value, bytes: &[u8], name: &str) -> anyhow::Result<()> {
    if let Some(n) = meta.get("length").and_then(Value::as_u64) {
        anyhow::ensure!(bytes.len() as u64 == n, "Sigstore's TUF {name} is {} bytes, not {n}", bytes.len());
    }
    if let Some(h) = meta.pointer("/hashes/sha256").and_then(Value::as_str) {
        anyhow::ensure!(crate::util::sha256_hex(bytes).eq_ignore_ascii_case(h), "Sigstore's TUF {name} isn't the one listed");
    }
    Ok(())
}

fn parse(bytes: &[u8], name: &str) -> anyhow::Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| anyhow::anyhow!("Sigstore's TUF {name} isn't JSON: {e}"))
}

fn keep(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn kept(dir: &Path, name: &str) -> Option<Vec<u8>> {
    std::fs::read(dir.join(name)).ok()
}

/// What's kept still serves at `now`: its timestamp unexpired, and (`fresh`) taken in the last day.
fn kept_root(dir: &Path, now: i64, fresh: bool) -> Option<Vec<u8>> {
    let ts = parse(&kept(dir, "timestamp.json")?, "timestamp").ok()?;
    current(&ts, "timestamp", now).ok()?;
    let checked = kept(dir, "checked").and_then(|b| String::from_utf8(b).ok()).and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0);
    if fresh && now - checked >= FRESH {
        return None;
    }
    kept(dir, "trusted_root.json")
}

/// Sigstore's public instance's trusted root, kept in `dir`.
pub async fn public_good(dir: &Path) -> anyhow::Result<TrustedRoot> {
    let http = crate::http::client::github()?;
    let fetch = |name: String, max: usize| async move {
        let url = format!("{REPOSITORY}/{name}");
        let r = http.get(&url).send().await.map_err(|e| anyhow::anyhow!("{url}: {e:#}"))?;
        match r.status() {
            404 => Ok(None),
            s if (200..300).contains(&s) => Ok(Some(r.bytes(max).await?.to_vec())),
            s => anyhow::bail!("{url}: {s}"),
        }
    };
    let (dir, now) = (dir.to_path_buf(), crate::util::now_unix());
    TrustedRoot::from_json(&refresh(&dir, now, fetch).await?)
}

/// The trusted root (its bytes) at `now`: what `dir` keeps while it's fresh, else taken through
/// the repository (`fetch` gets a file of it by name, at most so many bytes; `None` when it has
/// none), else, while the repository can't be reached, what's kept while its timestamp holds.
pub async fn refresh<F, Fut>(dir: &Path, now: i64, fetch: F) -> anyhow::Result<Vec<u8>>
where
    F: Fn(String, usize) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<Vec<u8>>>>,
{
    if let Some(b) = kept_root(dir, now, true) {
        return Ok(b);
    }
    match update(dir, now, &fetch).await {
        Ok(b) => Ok(b),
        Err(e) => match kept_root(dir, now, false) {
            Some(b) => {
                tracing::warn!(error = %format!("{e:#}"), "Sigstore's TUF repository: the trusted root kept serves until its timestamp expires");
                Ok(b)
            }
            None => Err(e),
        },
    }
}

async fn update<F, Fut>(dir: &Path, now: i64, fetch: &F) -> anyhow::Result<Vec<u8>>
where
    F: Fn(String, usize) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<Vec<u8>>>>,
{
    let need = |b: Option<Vec<u8>>, name: &str| b.ok_or_else(|| anyhow::anyhow!("Sigstore's TUF repository has no {name}"));
    // The root: the newest kept (it followed from kiln's when kept), or kiln's; then each newer.
    let shipped = parse(ROOT.as_bytes(), "root")?;
    let mut root = kept(dir, "root.json").and_then(|b| parse(&b, "root").ok()).filter(|k| version(k) > version(&shipped)).unwrap_or(shipped);
    let mut rotated = false;
    for _ in 0..MAX_ROOTS {
        let name = format!("{}.root.json", version(&root) + 1);
        let Some(bytes) = fetch(name.clone(), MAX_META).await? else { break };
        let new = parse(&bytes, &name)?;
        next_root(&root, &new)?;
        keep(dir, "root.json", &bytes)?;
        (root, rotated) = (new, true);
    }
    current(&root, "root", now)?;
    let rs = root.get("signed").cloned().unwrap_or(Value::Null);
    let consistent = rs.get("consistent_snapshot").and_then(Value::as_bool).unwrap_or(false);
    // What was taken before, against rollbacks: none once the root changed (its keys may have).
    let before = |name: &str| if rotated { None } else { kept(dir, name).and_then(|b| parse(&b, name).ok()) };

    let ts_bytes = need(fetch("timestamp.json".into(), MAX_META).await?, "timestamp")?;
    let ts = parse(&ts_bytes, "timestamp")?;
    signed_by_role(&ts, &rs, "timestamp")?;
    current(&ts, "timestamp", now)?;
    if let Some(old) = before("timestamp.json") {
        anyhow::ensure!(version(&ts) >= version(&old), "Sigstore's TUF timestamp went back from version {} to {}", version(&old), version(&ts));
    }

    let listed = ts.pointer("/signed/meta/snapshot.json").cloned().unwrap_or(Value::Null);
    let sv = listed.get("version").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF timestamp lists no snapshot"))?;
    let name = if consistent { format!("{sv}.snapshot.json") } else { "snapshot.json".to_owned() };
    let sn_bytes = need(fetch(name, MAX_META).await?, "snapshot")?;
    as_listed(&listed, &sn_bytes, "snapshot")?;
    let sn = parse(&sn_bytes, "snapshot")?;
    signed_by_role(&sn, &rs, "snapshot")?;
    current(&sn, "snapshot", now)?;
    anyhow::ensure!(version(&sn) == sv, "Sigstore's TUF snapshot is version {}, not {sv}", version(&sn));
    if let Some(old) = before("snapshot.json") {
        anyhow::ensure!(version(&sn) >= version(&old), "Sigstore's TUF snapshot went back from version {} to {}", version(&old), version(&sn));
    }

    let listed = sn.pointer("/signed/meta/targets.json").cloned().unwrap_or(Value::Null);
    let tv = listed.get("version").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF snapshot lists no targets"))?;
    let name = if consistent { format!("{tv}.targets.json") } else { "targets.json".to_owned() };
    let tg_bytes = need(fetch(name, MAX_META).await?, "targets")?;
    as_listed(&listed, &tg_bytes, "targets")?;
    let tg = parse(&tg_bytes, "targets")?;
    signed_by_role(&tg, &rs, "targets")?;
    current(&tg, "targets", now)?;
    anyhow::ensure!(version(&tg) == tv, "Sigstore's TUF targets is version {}, not {tv}", version(&tg));

    let target = tg.pointer("/signed/targets/trusted_root.json").ok_or_else(|| anyhow::anyhow!("Sigstore's TUF targets has no trusted_root.json"))?;
    let length = target.get("length").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF targets gives trusted_root.json no length"))?;
    let sha = target.pointer("/hashes/sha256").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Sigstore's TUF targets gives trusted_root.json no SHA-256"))?;
    let name = if consistent { format!("targets/{sha}.trusted_root.json") } else { "targets/trusted_root.json".to_owned() };
    let bytes = need(fetch(name, length).await?, "trusted_root.json")?;
    as_listed(target, &bytes, "trusted_root.json")?;
    TrustedRoot::from_json(&bytes)?;

    for (name, b) in [("timestamp.json", &ts_bytes), ("snapshot.json", &sn_bytes), ("targets.json", &tg_bytes), ("trusted_root.json", &bytes)] {
        keep(dir, name, b)?;
    }
    keep(dir, "checked", now.to_string().as_bytes())?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const ROOT_15: &[u8] = include_bytes!("testdata/tuf/15.root.json");
    const TIMESTAMP: &[u8] = include_bytes!("testdata/tuf/timestamp.json");
    const SNAPSHOT: &[u8] = include_bytes!("testdata/tuf/166.snapshot.json");
    const TARGETS: &[u8] = include_bytes!("testdata/tuf/14.targets.json");
    const TRUSTED_ROOT: &[u8] = include_bytes!("testdata/trusted_root.json");
    const TRUSTED_ROOT_SHA: &str = "6494e21ea73fa7ee769f85f57d5a3e6a08725eae1e38c755fc3517c9e6bc0b66";
    /// 2026-10-24T00:00:00Z: the fixtures' timestamp holds until 2026-10-30.
    const NOW: i64 = 1_792_800_000;

    type Files = Vec<(String, Option<Vec<u8>>)>;

    /// The repository as Sigstore published it on 2026-10-23, with `files` changed; `asked`
    /// counts what's asked of it.
    fn repository(files: Files, asked: Arc<AtomicUsize>) -> impl Fn(String, usize) -> std::future::Ready<anyhow::Result<Option<Vec<u8>>>> + Send + Sync + 'static {
        move |name: String, max: usize| {
            asked.fetch_add(1, Ordering::SeqCst);
            let base: Option<&[u8]> = match name.as_str() {
                "timestamp.json" => Some(TIMESTAMP),
                "166.snapshot.json" => Some(SNAPSHOT),
                "14.targets.json" => Some(TARGETS),
                n if n == format!("targets/{TRUSTED_ROOT_SHA}.trusted_root.json") => Some(TRUSTED_ROOT),
                _ => None,
            };
            let got = files.iter().find(|(n, _)| *n == name).map(|(_, b)| b.clone()).unwrap_or_else(|| base.map(<[u8]>::to_vec));
            std::future::ready(match got {
                Some(b) if b.len() > max => Err(anyhow::anyhow!("{name}: more than {max} bytes")),
                other => Ok(other),
            })
        }
    }

    fn take(dir: &Path, now: i64, files: Files) -> anyhow::Result<(Vec<u8>, usize)> {
        let asked = Arc::new(AtomicUsize::new(0));
        let (dir, fetch) = (dir.to_path_buf(), repository(files, asked.clone()));
        let b = crate::rt::block_on(async move { refresh(&dir, now, fetch).await })?;
        Ok((b, asked.load(Ordering::SeqCst)))
    }

    /// The trusted root comes through the chain of signed metadata from kiln's root, is kept, and
    /// is taken again only after a day.
    #[test]
    fn w18_sigstore_trusted_root_through_tuf() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (b, asked) = take(dir.path(), NOW, Vec::new())?;
        assert_eq!(b, TRUSTED_ROOT);
        assert_eq!(asked, 5, "17.root.json (none), the timestamp, snapshot, targets and trusted root");
        TrustedRoot::from_json(&b)?;
        assert_eq!(take(dir.path(), NOW + 3600, Vec::new())?, (TRUSTED_ROOT.to_vec(), 0), "kept for a day");
        assert_eq!(take(dir.path(), NOW + FRESH, Vec::new())?.1, 5, "then taken again");
        // While the repository can't be reached, what's kept serves until the timestamp expires.
        let gone = vec![("timestamp.json".to_owned(), None)];
        assert_eq!(take(dir.path(), NOW + 2 * FRESH, gone.clone())?.0, TRUSTED_ROOT);
        assert!(take(dir.path(), rfc3339("2026-10-30T19:24:57Z")?, gone).is_err(), "not after it expires");
        Ok(())
    }

    /// Changed, unsigned, expired or older metadata is refused, and nothing is kept of it.
    #[test]
    fn w18_sigstore_tuf_refuses_what_isnt_signed() -> anyhow::Result<()> {
        let altered = |b: &[u8], from: &str, to: &str| -> Option<Vec<u8>> { Some(String::from_utf8_lossy(b).replacen(from, to, 1).into_bytes()) };
        let cases: Vec<(&str, Files)> = vec![
            ("timestamp", vec![("timestamp.json".into(), altered(TIMESTAMP, "\"version\": 816", "\"version\": 817"))]),
            ("snapshot", vec![("166.snapshot.json".into(), altered(SNAPSHOT, "\"version\": 14", "\"version\": 15"))]),
            ("targets", vec![("14.targets.json".into(), altered(TARGETS, "6494e21e", "6494e21f"))]),
            ("trusted_root.json", vec![(format!("targets/{TRUSTED_ROOT_SHA}.trusted_root.json"), altered(TRUSTED_ROOT, "rekor.sigstore.dev", "rekor.example.com"))]),
            ("root 17", vec![("17.root.json".into(), altered(include_bytes!("root.json"), "\"version\": 16", "\"version\": 17"))]),
        ];
        for (what, files) in cases {
            let dir = tempfile::tempdir()?;
            let e = take(dir.path(), NOW, files).err().ok_or_else(|| anyhow::anyhow!("{what}: taken, though changed"))?;
            assert!(!format!("{e:#}").is_empty(), "{what}");
            assert!(kept(dir.path(), "trusted_root.json").is_none(), "{what}: nothing kept");
        }
        // Expired: the timestamp's time passed.
        let dir = tempfile::tempdir()?;
        assert!(format!("{:#}", take(dir.path(), rfc3339("2026-10-31T00:00:00Z")?, Vec::new()).err().ok_or_else(|| anyhow::anyhow!("expired, taken"))?).contains("expired"));
        // Older than what was taken: a timestamp kept from later.
        let dir = tempfile::tempdir()?;
        take(dir.path(), NOW, Vec::new())?;
        keep(dir.path(), "timestamp.json", &String::from_utf8_lossy(TIMESTAMP).replacen("\"version\": 816", "\"version\": 900", 1).into_bytes())?;
        let (d, fetch) = (dir.path().to_path_buf(), repository(Vec::new(), Arc::new(AtomicUsize::new(0))));
        let e = crate::rt::block_on(async move { update(&d, NOW, &fetch).await }).err().ok_or_else(|| anyhow::anyhow!("rolled back, taken"))?;
        assert!(format!("{e:#}").contains("went back"), "{e:#}");
        Ok(())
    }

    /// A newer root is taken when the one before (and itself) signed it: kiln's root follows
    /// Sigstore's 15th, and a changed one doesn't.
    #[test]
    fn w18_sigstore_tuf_root_rotation() -> anyhow::Result<()> {
        let (v15, v16) = (parse(ROOT_15, "15")?, parse(ROOT.as_bytes(), "16")?);
        next_root(&v15, &v16)?;
        assert!(next_root(&v16, &v15).is_err(), "not backwards");
        let mut changed = v16.clone();
        if let Some(e) = changed.pointer_mut("/signed/expires") {
            *e = Value::String("2099-01-01T00:00:00Z".into());
        }
        assert!(next_root(&v15, &changed).is_err(), "not changed");
        // Canonical JSON: keys in order, no spaces, only `"` and `\` escaped.
        let mut s = String::new();
        canonical(&serde_json::json!({ "b": [1, true, null], "a": "x\"y\\z\n" }), &mut s);
        assert_eq!(s, "{\"a\":\"x\\\"y\\\\z\n\",\"b\":[1,true,null]}");
        Ok(())
    }
}
