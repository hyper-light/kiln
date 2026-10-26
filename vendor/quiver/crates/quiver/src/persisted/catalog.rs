//! The catalog: which persisted topics a cluster has, and where each node serves each one's part.
//!
//! The catalog is a part of its own, replicated by every node like a topic's (`super`), its records
//! the cluster's decisions about topics: a topic created, a topic deleted, a node serving a
//! topic's part at a port. Every node applies the records in the order the catalog's group
//! committed them, so every node comes to the same catalog: what a create or a delete did is
//! decided by what came before it, never by the node that asked. A topic's incarnation is the
//! offset of the record that created it: a topic deleted and created again is a new incarnation,
//! a new part, its offsets from 0 (QUIVER-17).
//!
//! This is the pure core (CLAUDE.md §9): records and the state they make, no clock, disk or network.

use std::collections::BTreeMap;

use quiver_api::{Class, Topic};

/// Format: a record's first byte, what it is.
const CREATE: u8 = 1;
const DELETE: u8 = 2;
const SERVES: u8 = 3;
const START: u8 = 4;

/// Format: a class as a record holds it.
const fn class_code(class: Class) -> u8 {
    match class {
        Class::Ephemeral => 0,
        Class::Replicated => 1,
        Class::Persisted => 2,
        Class::Windowed => 3,
    }
}

const fn class_of(code: u8) -> Option<Class> {
    match code {
        0 => Some(Class::Ephemeral),
        1 => Some(Class::Replicated),
        2 => Some(Class::Persisted),
        3 => Some(Class::Windowed),
        _ => None,
    }
}

/// One of the catalog's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    /// Create `name` with `topic`'s settings, unless it exists.
    Create { name: String, topic: Topic },
    /// Delete `name`, if it exists.
    Delete { name: String },
    /// Node `node` serves the part of the topic of `incarnation` at `port`, on the address the
    /// cluster's configuration gives the node.
    Serves {
        incarnation: u64,
        node: u64,
        port: u16,
    },
    /// Node `node` started its run `run`: once it has applied this, it has applied every record
    /// committed before it started.
    Start { node: u64, run: u64 },
}

impl Record {
    /// The record's bytes, into `out` (emptied first).
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        match self {
            Self::Create { name, topic } => {
                out.push(CREATE);
                put_name(out, name);
                out.push(class_code(topic.class));
                out.extend_from_slice(&topic.retain_messages.to_le_bytes());
                out.extend_from_slice(&topic.retain_bytes.to_le_bytes());
            }
            Self::Delete { name } => {
                out.push(DELETE);
                put_name(out, name);
            }
            Self::Serves {
                incarnation,
                node,
                port,
            } => {
                out.push(SERVES);
                out.extend_from_slice(&incarnation.to_le_bytes());
                out.extend_from_slice(&node.to_le_bytes());
                out.extend_from_slice(&port.to_le_bytes());
            }
            Self::Start { node, run } => {
                out.push(START);
                out.extend_from_slice(&node.to_le_bytes());
                out.extend_from_slice(&run.to_le_bytes());
            }
        }
    }

    /// The record `bytes` hold; `None` for bytes that are none (every node refuses them alike).
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (tag, rest) = bytes.split_first()?;
        match *tag {
            CREATE => {
                let (name, rest) = take_name(rest)?;
                let (class, rest) = rest.split_first()?;
                let (messages, rest) = take_u64(rest)?;
                let (bytes, rest) = take_u64(rest)?;
                if !rest.is_empty() {
                    return None;
                }
                Some(Self::Create {
                    name,
                    topic: Topic {
                        class: class_of(*class)?,
                        retain_messages: messages,
                        retain_bytes: bytes,
                    },
                })
            }
            DELETE => {
                let (name, rest) = take_name(rest)?;
                if rest.is_empty() {
                    Some(Self::Delete { name })
                } else {
                    None
                }
            }
            SERVES => {
                let (incarnation, rest) = take_u64(rest)?;
                let (node, rest) = take_u64(rest)?;
                let port = u16::from_le_bytes(rest.try_into().ok()?);
                Some(Self::Serves {
                    incarnation,
                    node,
                    port,
                })
            }
            START => {
                let (node, rest) = take_u64(rest)?;
                let run = u64::from_le_bytes(rest.try_into().ok()?);
                Some(Self::Start { node, run })
            }
            _ => None,
        }
    }
}

/// A name, as a record holds it: its length (one byte: names are at most 128 bytes, QUIVER-8),
/// then its bytes.
fn put_name(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();
    let len = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
    out.push(len);
    out.extend_from_slice(bytes.get(..usize::from(len)).unwrap_or_default());
}

fn take_name(bytes: &[u8]) -> Option<(String, &[u8])> {
    let (len, rest) = bytes.split_first()?;
    let name = rest.get(..usize::from(*len))?;
    let rest = rest.get(usize::from(*len)..)?;
    Some((String::from_utf8(name.to_vec()).ok()?, rest))
}

fn take_u64(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let head = bytes.get(..8)?;
    Some((u64::from_le_bytes(head.try_into().ok()?), bytes.get(8..)?))
}

/// A topic the catalog has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The offset of the record that created it.
    pub(crate) incarnation: u64,
    pub(crate) topic: Topic,
}

/// What applying a record did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    /// A topic made, its incarnation the record's offset.
    Created,
    /// The name exists with the same settings: nothing changed.
    Same,
    /// The name exists with other settings: nothing changed.
    Exists,
    /// The cluster holds as many topics as it may: nothing changed.
    Full,
    /// A topic deleted, of this incarnation.
    Deleted(u64),
    /// No topic of the name: nothing changed.
    NoTopic,
    /// A node's port for a topic's part, recorded.
    Served,
    /// A node's start: nothing changed.
    Started,
    /// A record no node takes (bytes that are none, or a port for an incarnation not live):
    /// nothing changed, alike at every node.
    Ignored,
}

/// The catalog as of the records applied, at most `limit` topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Catalog {
    live: BTreeMap<String, Entry>,
    /// Each live incarnation's ports, by node.
    serves: BTreeMap<u64, BTreeMap<u64, u16>>,
    limit: usize,
}

impl Catalog {
    pub(crate) const fn new(limit: usize) -> Self {
        Self {
            live: BTreeMap::new(),
            serves: BTreeMap::new(),
            limit,
        }
    }

    /// Applies the record at `offset` (`None`: bytes that are no record).
    pub(crate) fn apply(&mut self, offset: u64, record: Option<&Record>) -> Applied {
        match record {
            None => Applied::Ignored,
            Some(Record::Create { name, topic }) => match self.live.get(name) {
                Some(entry) if entry.topic == *topic => Applied::Same,
                Some(_) => Applied::Exists,
                None if self.live.len() >= self.limit => Applied::Full,
                None => {
                    self.live.insert(
                        name.clone(),
                        Entry {
                            incarnation: offset,
                            topic: *topic,
                        },
                    );
                    self.serves.insert(offset, BTreeMap::new());
                    Applied::Created
                }
            },
            Some(Record::Delete { name }) => match self.live.remove(name) {
                Some(entry) => {
                    self.serves.remove(&entry.incarnation);
                    Applied::Deleted(entry.incarnation)
                }
                None => Applied::NoTopic,
            },
            Some(Record::Serves {
                incarnation,
                node,
                port,
            }) => match self.serves.get_mut(incarnation) {
                Some(ports) => {
                    ports.insert(*node, *port);
                    Applied::Served
                }
                None => Applied::Ignored,
            },
            Some(Record::Start { .. }) => Applied::Started,
        }
    }

    /// The live topic of `name`.
    pub(crate) fn get(&self, name: &str) -> Option<Entry> {
        self.live.get(name).copied()
    }

    /// The ports each node serves `incarnation`'s part at, as recorded so far.
    pub(crate) fn ports(&self, incarnation: u64) -> Option<&BTreeMap<u64, u16>> {
        self.serves.get(&incarnation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persisted(retain_messages: u64) -> Topic {
        Topic {
            class: Class::Persisted,
            retain_messages,
            retain_bytes: 1 << 20,
        }
    }

    /// Do encode each record and decode it, expect it back; do decode bytes that are none, expect
    /// none, and a record cut short, expect none.
    #[test]
    fn records_round_trip_and_bytes_that_are_none_are_refused() {
        let records = [
            Record::Create {
                name: "ci/events".into(),
                topic: persisted(10),
            },
            Record::Delete {
                name: "x".repeat(128),
            },
            Record::Serves {
                incarnation: 7,
                node: 3,
                port: 40_001,
            },
            Record::Start { node: 2, run: 9 },
        ];
        let mut out = Vec::new();
        for record in &records {
            record.encode(&mut out);
            assert_eq!(Record::decode(&out).as_ref(), Some(record));
            for cut in 0..out.len() {
                assert_eq!(Record::decode(&out[..cut]), None, "{record:?} cut at {cut}");
            }
            out.push(0);
            assert_eq!(
                Record::decode(&out),
                None,
                "{record:?} with a byte after it"
            );
        }
        assert_eq!(Record::decode(&[9, 1, 2]), None);
    }

    /// QUIVER-8, QUIVER-17 on the catalog: do create, create again alike and otherwise, delete,
    /// create again, expect the first create to make the topic, the same one to change nothing,
    /// the other refused, the delete to name the incarnation, and the new create a new
    /// incarnation.
    #[test]
    fn creates_and_deletes_decide_by_what_came_before() {
        let mut c = Catalog::new(2);
        let create = |n: &str, t| Record::Create {
            name: n.into(),
            topic: t,
        };
        assert_eq!(
            c.apply(0, Some(&create("a", persisted(10)))),
            Applied::Created
        );
        assert_eq!(c.apply(1, Some(&create("a", persisted(10)))), Applied::Same);
        assert_eq!(
            c.apply(2, Some(&create("a", persisted(11)))),
            Applied::Exists
        );
        assert_eq!(c.get("a").map(|e| e.incarnation), Some(0));
        assert_eq!(
            c.apply(3, Some(&create("b", persisted(1)))),
            Applied::Created
        );
        assert_eq!(c.apply(4, Some(&create("c", persisted(1)))), Applied::Full);
        let delete = Record::Delete { name: "a".into() };
        assert_eq!(c.apply(5, Some(&delete)), Applied::Deleted(0));
        assert_eq!(c.apply(6, Some(&delete)), Applied::NoTopic);
        assert_eq!(
            c.apply(7, Some(&create("a", persisted(11)))),
            Applied::Created
        );
        assert_eq!(c.get("a").map(|e| e.incarnation), Some(7));
        assert_eq!(c.apply(8, None), Applied::Ignored);
    }

    /// Do record ports for a live incarnation and a gone one, expect the live one's kept and the
    /// gone one's ignored, and a delete to take its ports with it.
    #[test]
    fn ports_are_kept_for_live_incarnations_only() {
        let mut c = Catalog::new(4);
        c.apply(
            0,
            Some(&Record::Create {
                name: "t".into(),
                topic: persisted(1),
            }),
        );
        let serves = |incarnation, node, port| Record::Serves {
            incarnation,
            node,
            port,
        };
        assert_eq!(c.apply(1, Some(&serves(0, 1, 5000))), Applied::Served);
        assert_eq!(c.apply(2, Some(&serves(0, 2, 5001))), Applied::Served);
        assert_eq!(c.apply(3, Some(&serves(0, 2, 5002))), Applied::Served);
        assert_eq!(c.apply(4, Some(&serves(9, 1, 5003))), Applied::Ignored);
        let ports: Vec<(u64, u16)> = c.ports(0).unwrap().iter().map(|(n, p)| (*n, *p)).collect();
        assert_eq!(ports, [(1, 5000), (2, 5002)]);
        c.apply(5, Some(&Record::Delete { name: "t".into() }));
        assert!(c.ports(0).is_none());
    }
}
