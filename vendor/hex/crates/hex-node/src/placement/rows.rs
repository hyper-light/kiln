//! AC-T7.4.1: payload and decision row share the existing atomic Files replacement.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::BuildHasher;
use std::io::Write;
use std::path::Path;

use hex_sim::peer::{Budget, Key, Records, Stop};
use serde_json::{Value, json};

use super::{Payload, bad, checked_namespace, name};
use crate::records::Files;

#[derive(Clone)]
struct Stored {
    row: String,
    payload: Option<Payload>,
}

pub(super) enum Context {
    None,
    NextOwned(Payload),
    Start(Key, Payload),
}

pub(super) struct Rows {
    files: Files,
    held: BTreeMap<String, Stored>,
    bound: usize,
    ceiling: Budget,
    namespace: String,
    pub context: Context,
}

pub(super) fn record_bound(budget: Budget) -> Result<usize, Stop> {
    budget
        .owned
        .checked_add(budget.ran)
        .and_then(|n| n.checked_add(1))
        .filter(|n| n.checked_mul(2).is_some())
        .ok_or_else(|| bad("placement record budget is not representable"))
}

fn read_value(path: &Path) -> Result<Value, Stop> {
    let text =
        crate::records::read_record(File::open(path).map_err(|e| Stop::Io(e.to_string()))?, path)?;
    serde_json::from_str(&text).map_err(|e| bad(e.to_string()))
}

fn read_ceiling(dir: &Path, current: Budget) -> Result<usize, Stop> {
    let epoch = dir.join("epoch.rec");
    let value = match File::open(&epoch) {
        Ok(file) => {
            let text = crate::records::read_record(file, &epoch)?;
            serde_json::from_str::<Value>(&text).map_err(|e| bad(e.to_string()))?
        }
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
            return record_bound(current);
        }
        Err(why) => return Err(Stop::Io(why.to_string())),
    };
    let saved = match value.get("read_budget") {
        Some(saved) => {
            let count = |key| {
                saved
                    .get(key)
                    .and_then(Value::as_u64)
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| bad("invalid persisted placement read budget"))
            };
            Budget {
                owned: count("owned")?,
                ran: count("ran")?,
            }
        }
        None => {
            // Compatibility with the published 8d2a66cd epoch envelope: both row kinds
            // were capped at 65,536. This bounds only the legacy read, never new admission.
            Budget {
                owned: 65_536,
                ran: 65_536,
            }
        }
    };
    record_bound(Budget {
        owned: current.owned.max(saved.owned),
        ran: current.ran.max(saved.ran),
    })
}

pub(super) fn payload_value(payload: &Payload) -> Result<Value, Stop> {
    let mut bytes = Bounded(Vec::new());
    serde_json::to_writer(&mut bytes, &payload.spec).map_err(|e| bad(e.to_string()))?;
    let spec: Value = serde_json::from_slice(&bytes.0).map_err(|e| bad(e.to_string()))?;
    Ok(json!({"spec":spec,"start":payload.start}))
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > hex_core::wire::MAX_MESSAGE)
        {
            return Err(std::io::Error::other(
                "placement record exceeds wire byte bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn payload_from(value: &Value) -> Result<Option<Payload>, Stop> {
    if value.is_null() {
        return Ok(None);
    }
    let spec = serde_json::from_value(
        value
            .get("spec")
            .cloned()
            .ok_or_else(|| bad("missing spec"))?,
    )
    .map_err(|e| bad(e.to_string()))?;
    let start = value
        .get("start")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("missing start"))?;
    Ok(Some(Payload { spec, start }))
}

impl Rows {
    pub fn open(dir: &Path, budget: Budget, supplied: Option<&str>) -> Result<Self, Stop> {
        let bound = read_ceiling(dir, budget)?;
        let files = Files::open_with_limit(dir, bound)?;
        let mut held = BTreeMap::new();
        let mut namespace = None;
        for entry in fs::read_dir(dir).map_err(|e| Stop::Io(e.to_string()))? {
            let path = entry.map_err(|e| Stop::Io(e.to_string()))?.path();
            if path.extension().is_none_or(|e| e != "rec") {
                continue;
            }
            if held.len() >= bound {
                return Err(Stop::Full {
                    what: "placement records",
                    bound,
                });
            }
            let key = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| bad("record name"))?
                .to_owned();
            let value = read_value(&path)?;
            let row = value
                .get("row")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("record row"))?
                .to_owned();
            let payload = payload_from(value.get("payload").ok_or_else(|| bad("record payload"))?)?;
            if key == "epoch" {
                let value = value
                    .get("namespace")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("epoch has no placement namespace"))?;
                checked_namespace(value).map_err(|why| bad(why.to_string()))?;
                if payload.is_some() {
                    return Err(bad("epoch contains a box payload"));
                }
                namespace = Some(value.to_owned());
            }
            if (key.starts_with("own-") || key.starts_with("ran-")) && payload.is_none() {
                return Err(bad("placement row has no atomic payload"));
            }
            held.insert(key, Stored { row, payload });
        }
        let fresh = held.is_empty();
        let namespace = match namespace {
            Some(namespace) => {
                if supplied.is_some_and(|given| given != namespace) {
                    return Err(bad(
                        "configured placement namespace differs from durable epoch",
                    ));
                }
                namespace
            }
            None if fresh => match supplied {
                Some(namespace) => checked_namespace(namespace)
                    .map_err(|why| bad(why.to_string()))?
                    .to_owned(),
                None => {
                    // Standard UUID-width entropy, two domain-separated 64-bit hashes under the
                    // standard library's OS-seeded RandomState. This is identity, not a tuning knob.
                    let entropy = RandomState::new();
                    format!(
                        "{:016x}{:016x}",
                        entropy.hash_one("hex-placement-namespace-first"),
                        entropy.hash_one("hex-placement-namespace-second")
                    )
                }
            },
            None => return Err(bad("nonempty placement records have no namespaced epoch")),
        };
        // Loading existing rows does not grant new admission. The core validates their
        // identities/states under this recovery floor, then adopts the current ceiling.
        let ceiling = Budget {
            owned: budget
                .owned
                .max(held.keys().filter(|n| n.starts_with("own-")).count()),
            ran: budget
                .ran
                .max(held.keys().filter(|n| n.starts_with("ran-")).count()),
        };
        let mut rows = Self {
            files,
            held,
            bound: record_bound(ceiling)?,
            ceiling,
            namespace,
            context: Context::None,
        };
        // No runtime inspect/create may precede this durable namespace. A failed first boot may
        // leave epoch zero; the core's next boot advances it normally without changing identity.
        if fresh {
            rows.put("epoch", "0")?;
        }
        Ok(rows)
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn recovery_budget(&self) -> Budget {
        self.ceiling
    }

    pub fn control_bound(&self, budget: Budget) -> Result<usize, Stop> {
        // Retained owners and newly admitted runners (or the reverse) can coexist.
        // Taking a maximum of the totals would omit that cross-kind population.
        record_bound(Budget {
            owned: budget
                .owned
                .max(self.held.keys().filter(|n| n.starts_with("own-")).count()),
            ran: budget
                .ran
                .max(self.held.keys().filter(|n| n.starts_with("ran-")).count()),
        })
    }

    /// Commit a larger read ceiling before the core can create a row under it.
    /// A restart compacts it to current admission plus the actual retained rows.
    pub fn grow_budget(&mut self, budget: Budget) -> Result<(), Stop> {
        let next = Budget {
            owned: self.ceiling.owned.max(budget.owned),
            ran: self.ceiling.ran.max(budget.ran),
        };
        if next.owned == self.ceiling.owned && next.ran == self.ceiling.ran {
            return Ok(());
        }
        let bound = record_bound(next)?;
        let epoch = self
            .held
            .get("epoch")
            .ok_or_else(|| bad("placement epoch absent while growing read ceiling"))?
            .row
            .clone();
        let old = self.ceiling;
        let old_bound = self.bound;
        self.ceiling = next;
        self.bound = bound;
        if let Err(why) = self.put("epoch", &epoch) {
            self.ceiling = old;
            self.bound = old_bound;
            return Err(why);
        }
        Ok(())
    }

    pub fn payload(&self, kind: &str, key: Key) -> Result<Payload, Stop> {
        self.held
            .get(&name(kind, key))
            .and_then(|s| s.payload.clone())
            .ok_or_else(|| bad("placement payload missing"))
    }

    pub fn row(&self, key: Key) -> Result<&str, Stop> {
        self.held
            .get(&name("ran", key))
            .map(|s| s.row.as_str())
            .ok_or_else(|| bad("ran record missing"))
    }

    pub fn running(&self) -> Result<Vec<(Key, Payload)>, Stop> {
        self.held
            .iter()
            .filter(|(n, _)| n.starts_with("ran-"))
            .map(|(n, s)| {
                let mut words = n.split('-').skip(1);
                let mut number = || words.next().ok_or_else(|| bad("record key"));
                let owner = number()?.parse().map_err(|_| bad("record owner"))?;
                let epoch = number()?.parse().map_err(|_| bad("record epoch"))?;
                let seq = number()?.parse().map_err(|_| bad("record sequence"))?;
                if words.next().is_some() {
                    return Err(bad("record key suffix"));
                }
                let key = Key { owner, epoch, seq };
                // Recovery must validate the immutable local payload before any runtime callback.
                if self
                    .held
                    .get(&name("own", key))
                    .is_some_and(|own| own.payload != s.payload)
                {
                    return Err(bad(format!(
                        "owned and runner rows disagree on payload for {key:?}"
                    )));
                }
                Ok((key, s.payload.clone().ok_or_else(|| bad("ran payload"))?))
            })
            .collect()
    }
}

impl Records for Rows {
    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        Ok(self
            .held
            .iter()
            .map(|(n, s)| (n.clone(), s.row.clone()))
            .collect())
    }

    fn put(&mut self, key: &str, row: &str) -> Result<(), Stop> {
        let old = self.held.get(key);
        if old.is_none() && self.held.len() >= self.bound {
            return Err(Stop::Full {
                what: "placement records",
                bound: self.bound,
            });
        }
        let payload = if let Some(old) = old {
            old.payload.clone()
        } else {
            match &self.context {
                Context::NextOwned(p) if key.starts_with("own-") => Some(p.clone()),
                Context::Start(k, p) if key == name("ran", *k) => Some(p.clone()),
                _ if key == "epoch" => None,
                _ => return Err(bad("new placement row without atomic payload context")),
            }
        };
        let payload_value = payload.as_ref().map(payload_value).transpose()?;
        let value = if key == "epoch" {
            json!({"row":row,"payload":payload_value,"namespace":self.namespace,
                "read_budget":{"owned":self.ceiling.owned,"ran":self.ceiling.ran}})
        } else {
            json!({"row":row,"payload":payload_value})
        };
        let mut bytes = Bounded(Vec::new());
        serde_json::to_writer(&mut bytes, &value).map_err(|e| bad(e.to_string()))?;
        let text = String::from_utf8(bytes.0).map_err(|e| bad(e.to_string()))?;
        self.files.put(key, &text)?;
        self.held.insert(
            key.to_owned(),
            Stored {
                row: row.to_owned(),
                payload,
            },
        );
        if matches!(self.context, Context::NextOwned(_)) && key.starts_with("own-") {
            self.context = Context::None;
        }
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<(), Stop> {
        self.files.remove(key)?;
        self.held.remove(key);
        Ok(())
    }
}
