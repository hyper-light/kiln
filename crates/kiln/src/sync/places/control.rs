//! U17: injected host control cells, with atomic, checked, read-only legacy import.
use std::path::Path;
use amphora::database::Database;
use serde::{Serialize, de::DeserializeOwned};
use sha2::Digest as _;
use crate::sql::{Connection, params};

#[derive(Clone)]
pub(crate) struct Control {
    database: Database,
    host: String,
    kind: String,
}

const VERSION: i64 = 1;
const MAX_BYTES: usize = 32 << 20;

fn source(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut raw = Vec::new();
    file.take(u64::try_from(MAX_BYTES).unwrap_or(0) + 1).read_to_end(&mut raw)?;
    anyhow::ensure!(raw.len() <= MAX_BYTES, "U17 legacy control source exceeds 32 MiB");
    Ok(Some(raw))
}

fn fingerprint(raw: Option<&[u8]>) -> String {
    raw.map_or_else(|| "absent".to_owned(), |raw| format!("sha256:{}", hex::encode(sha2::Sha256::digest(raw))))
}

impl Control {
    pub(crate) fn open<T: DeserializeOwned + Serialize + Default + Send + 'static>(database: Database, host: &str, kind: &str, path: &Path, validate: fn(&T) -> anyhow::Result<()>) -> anyhow::Result<(Self, T)> {
        anyhow::ensure!(!host.is_empty(), "U17 control host identity is empty");
        let cell = Self { database, host: host.to_owned(), kind: kind.to_owned() };
        let read = cell.clone();
        let existing = cell.database.call(crate::store::deadline(), move |c| read.read::<T>(c).map(Some).or_else(|e| {
            if e.downcast_ref::<crate::sql::Error>().is_some_and(|e| matches!(e, crate::sql::Error::QueryReturnedNoRows)) { Ok(None) } else { Err(e) }
        }))?;
        // A completed marker retires the old writer; stale JSON can never resurrect a deletion.
        if let Some(value) = existing { validate(&value)?; return Ok((cell, value)); }
        let path = std::path::absolute(path)?;
        let raw = source(&path)?;
        let value: T = match raw.as_deref() { Some(raw) => serde_json::from_value(serde_json::from_slice::<Checked>(raw)?.0)?, None => T::default() };
        validate(&value)?;
        let value = serde_json::to_vec(&value)?;
        let digest = fingerprint(raw.as_deref());
        let insert = cell.clone();
        let loaded = cell.database.call(crate::store::deadline(), move |c| {
            // Another opener may have completed while the source was being checked. Never merge
            // two independently edited sources or replace an already authoritative control cell.
            let exists: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM host_controls WHERE host = ?1 AND kind = ?2)", params![insert.host, insert.kind], |r| r.get(0))?;
            if exists { return insert.read::<T>(c); }
            anyhow::ensure!(fingerprint(source(&path)?.as_deref()) == digest, "U17 legacy control source changed during import; keep it unchanged and retry");
            c.execute("INSERT INTO host_controls (host, kind, version, source, source_digest, value) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![insert.host, insert.kind, VERSION, path.to_string_lossy().as_ref(), digest, value])?;
            insert.read::<T>(c)
        })?;
        Ok((cell, loaded))
    }

    /// U19: standby frontends attach to an existing completed cell without importing or writing.
    pub(crate) fn attach<T: DeserializeOwned + Send + 'static>(database: Database, host: &str, kind: &str) -> anyhow::Result<(Self, T)> {
        anyhow::ensure!(!host.is_empty(), "U17 control host identity is empty");
        let cell = Self { database, host: host.to_owned(), kind: kind.to_owned() };
        let value = cell.snapshot()?;
        Ok((cell, value))
    }

    pub(crate) fn snapshot<T: DeserializeOwned + Send + 'static>(&self) -> anyhow::Result<T> {
        let cell = self.clone();
        self.database.call(crate::store::deadline(), move |c| cell.read(c)).map_err(Into::into)
    }

    fn read<T: DeserializeOwned>(&self, c: &Connection<'_>) -> anyhow::Result<T> {
        let (version, source, digest, raw): (i64, String, String, Vec<u8>) = c.query_row("SELECT version, source, source_digest, value FROM host_controls WHERE host = ?1 AND kind = ?2", params![self.host, self.kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        anyhow::ensure!(version == VERSION && !source.is_empty() && (digest == "absent" || digest.strip_prefix("sha256:").is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))), "U17 unsupported or partial control import marker");
        Ok(serde_json::from_slice(&raw)?)
    }

    /// U19: product waits and quorum/SDK commits never occupy a runtime shard.
    pub(crate) async fn change_async<T, R>(&self, f: impl FnOnce(&mut T) -> anyhow::Result<R> + Send + 'static) -> anyhow::Result<(T, R)>
    where T: DeserializeOwned + Serialize + Send + 'static, R: Send + 'static {
        let cell = self.clone();
        crate::rt::blocking_in(crate::rt::SHARE_STORE, move || cell.change(f)).await?
    }

    /// The closure sees the latest committed cell. Failure exposes no speculative cache state.
    pub(crate) fn change<T, R>(&self, f: impl FnOnce(&mut T) -> anyhow::Result<R> + Send + 'static) -> anyhow::Result<(T, R)>
    where T: DeserializeOwned + Serialize + Send + 'static, R: Send + 'static {
        let cell = self.clone();
        self.database.call(crate::store::deadline(), move |c| {
            let mut value: T = cell.read(c)?;
            let result = f(&mut value)?;
            let raw = serde_json::to_vec(&value)?;
            anyhow::ensure!(raw.len() <= MAX_BYTES, "U17 control metadata exceeds 32 MiB");
            c.execute("UPDATE host_controls SET value = ?3 WHERE host = ?1 AND kind = ?2", params![cell.host, cell.kind, raw])?;
            Ok((value, result))
        }).map_err(Into::into)
    }
}

/// A legacy object with duplicate keys is ambiguous even when serde would retain its last one.
struct Checked(serde_json::Value);
impl<'de> serde::Deserialize<'de> for Checked {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Checked;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("unambiguous control JSON") }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Checked, E> { Ok(Checked(v.into())) }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Checked, E> { Ok(Checked(v.into())) }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Checked, E> { Ok(Checked(v.into())) }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Checked, E> { serde_json::Number::from_f64(v).map(|v| Checked(v.into())).ok_or_else(|| E::custom("invalid control number")) }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Checked, E> { Ok(Checked(v.into())) }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Checked, E> { Ok(Checked(serde_json::Value::Null)) }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut a: A) -> Result<Checked, A::Error> {
                let mut out = Vec::new(); while let Some(Checked(v)) = a.next_element()? { out.push(v); }
                Ok(Checked(serde_json::Value::Array(out)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut a: A) -> Result<Checked, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some((k, Checked(v))) = a.next_entry::<String, Checked>()? {
                    if out.insert(k, v).is_some() { return Err(serde::de::Error::custom("U17 duplicate legacy control key")); }
                }
                Ok(Checked(serde_json::Value::Object(out)))
            }
        }
        d.deserialize_any(Visit)
    }
}
