//! The audit log (CP-T1.7, AC-T1.7.4; AL.1, docs/design/audit.md): events appended in order, kept
//! as RFC 6962's Merkle tree ([`tree`]) in one family of a [`Records`] store, and verified on every
//! read, so a changed byte anywhere in its rows is refused as [`AuditError::Tampered`].
//!
//! It reaches the store only through [`Records::get`] and [`Records::commit`], so it runs on any
//! implementation of Amphora's Records API, and it keeps three kinds of rows in its family:
//!
//! - the head, key `h`: a format byte, the tree's size (u64, big-endian) and its root (32 bytes);
//! - each event, key `e` and its index (u64, big-endian): the event's bytes as given;
//! - each perfect subtree, key `n`, its level (one byte) and its index (u64, big-endian): its
//!   32-byte hash, level 0 being an event's leaf hash, written once by the append that completes it.
//!
//! Opening the log checks the head's root against the subtrees it names, and that no event lies
//! past its end; given a [`Checkpoint`] read before, it checks RFC 6962's consistency proof from it
//! to the head, so history rewritten whole, hashes and all, is caught too. Reading an event checks
//! its leaf hash and its inclusion proof against the head.

pub mod tree;

use std::time::Instant;

use amphora_api::{self as api, Cond, Consistency, Records, Refusal, Rev, Tx, Write};

use tree::{Frontier, Hash, Node, TreeError};

/// A tree a reader has seen: its size and root. Kept by an auditor, it makes any later head that
/// doesn't extend it a detected tampering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub size: u64,
    pub root: Hash,
}

/// A row of the log, by what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Head,
    Event(u64),
    Node { level: u8, index: u64 },
}

/// How a read found the log changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Tamper {
    #[error("{0:?} is missing")]
    Missing(Place),
    #[error("{0:?} isn't in the log's format")]
    Malformed(Place),
    #[error("{0:?} lies past the head's end")]
    Beyond(Place),
    #[error("the head's root isn't the root of the subtrees it names")]
    HeadRoot,
    #[error("event {0} isn't the event its leaf hash was made from")]
    Leaf(u64),
    #[error("event {0}'s inclusion proof doesn't reach the head's root")]
    Inclusion(u64),
    #[error("the head holds {stored} events, fewer than the {trusted} a reader saw")]
    RolledBack { trusted: u64, stored: u64 },
    #[error("the head of {stored} events doesn't extend the tree of {trusted} a reader saw")]
    Inconsistent { trusted: u64, stored: u64 },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AuditError {
    #[error("the audit log was changed: {0}")]
    Tampered(Tamper),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

impl From<Tamper> for AuditError {
    fn from(t: Tamper) -> Self {
        tracing::error!(target: "amphora.audit", event = "audit.tampered", tamper = %t, "the audit log was changed outside its appends");
        Self::Tampered(t)
    }
}

impl From<TreeError> for AuditError {
    fn from(e: TreeError) -> Self {
        match e {
            TreeError::OutOfRange { .. } | TreeError::NotEarlier { .. } => {
                Self::Refused(api::invalid("audit log index", &e.to_string()))
            }
            TreeError::Shape { .. } => {
                Self::Refused(Refusal::Unavailable(format!("audit tree: {e}")))
            }
        }
    }
}

/// The head's format byte: the layout this module writes, the only one it reads.
const FORMAT: u8 = 1;
/// The head's length: the format byte, the size's 8 bytes and the root's 32.
const HEAD_LEN: usize = 41;
const HEAD_KEY: &[u8] = b"h";
const EVENT_TAG: u8 = b'e';
const NODE_TAG: u8 = b'n';

fn event_key(index: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(9);
    k.push(EVENT_TAG);
    k.extend_from_slice(&index.to_be_bytes());
    k
}

fn node_key(node: Node) -> Vec<u8> {
    let mut k = Vec::with_capacity(10);
    k.push(NODE_TAG);
    k.push(node.level);
    k.extend_from_slice(&node.index.to_be_bytes());
    k
}

fn encode_head(head: &Checkpoint) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEAD_LEN);
    v.push(FORMAT);
    v.extend_from_slice(&head.size.to_be_bytes());
    v.extend_from_slice(&head.root);
    v
}

fn decode_head(bytes: &[u8]) -> Result<Checkpoint, Tamper> {
    let malformed = Tamper::Malformed(Place::Head);
    let (&format, rest) = bytes.split_first().ok_or(malformed)?;
    let (size, root) = rest.split_first_chunk::<8>().ok_or(malformed)?;
    let root: Hash = root.try_into().map_err(|_| malformed)?;
    if format != FORMAT {
        return Err(malformed);
    }
    Ok(Checkpoint {
        size: u64::from_be_bytes(*size),
        root,
    })
}

/// An open audit log: its head as verified when opened or last appended to.
pub struct AuditLog<'r, R: Records + ?Sized> {
    records: &'r R,
    family: String,
    head: Checkpoint,
    /// The head row's revision, the condition the next append commits on; `None` before the first.
    head_rev: Option<Rev>,
    frontier: Frontier,
}

impl<'r, R: Records + ?Sized> AuditLog<'r, R> {
    /// Opens the log in `family`, checking its head against its subtrees and that nothing lies past
    /// its end; with `trusted`, checking that the head extends that earlier tree.
    pub fn open(
        records: &'r R,
        family: &str,
        trusted: Option<&Checkpoint>,
        until: Instant,
    ) -> Result<Self, AuditError> {
        api::family(family)?;
        let (head, head_rev) =
            match records.get(family, HEAD_KEY, Consistency::Linearizable, until)? {
                Some(row) => (decode_head(&row.value)?, Some(row.rev)),
                None => (
                    Checkpoint {
                        size: 0,
                        root: tree::empty_root(),
                    },
                    None,
                ),
            };
        let mut log = Self {
            records,
            family: family.to_owned(),
            head,
            head_rev,
            frontier: Frontier::new(0, &[])?,
        };
        let parts = tree::subtrees(0, head.size)?;
        let mut hashes = Vec::with_capacity(parts.len());
        for part in parts {
            hashes.push(log.node(part, until)?);
        }
        log.frontier = Frontier::new(head.size, &hashes)?;
        if log.frontier.root() != head.root {
            return Err(Tamper::HeadRoot.into());
        }
        // A head moved back over appended events would still match its own subtrees.
        if log.row(&event_key(head.size), until)?.is_some() {
            return Err(Tamper::Beyond(Place::Event(head.size)).into());
        }
        if let Some(trusted) = trusted {
            log.extends(trusted, until)?;
        }
        Ok(log)
    }

    /// The head as this handle last verified or wrote it.
    #[must_use]
    pub fn head(&self) -> Checkpoint {
        self.head
    }

    /// The event at `index`, once its leaf hash and its inclusion proof check against the head.
    pub fn read(&self, index: u64, until: Instant) -> Result<Vec<u8>, AuditError> {
        let size = self.head.size;
        if index >= size {
            return Err(TreeError::OutOfRange { index, size }.into());
        }
        let event = self
            .row(&event_key(index), until)?
            .ok_or(Tamper::Missing(Place::Event(index)))?;
        let leaf = tree::leaf_hash(&event);
        if self.node(Node { level: 0, index }, until)? != leaf {
            return Err(Tamper::Leaf(index).into());
        }
        let proof = tree::inclusion_proof(index, size, |n| self.node(n, until))?;
        if !tree::verify_inclusion(index, size, &leaf, &proof, &self.head.root) {
            return Err(Tamper::Inclusion(index).into());
        }
        Ok(event)
    }

    /// Appends `events` in one transaction, conditioned on the head this handle verified: a writer
    /// that raced it is refused with [`Refusal::Conflict`], and the caller opens the log again.
    /// After a timeout or an unknown outcome, too, the caller opens it again to learn the head.
    pub fn append<E: AsRef<[u8]>>(
        &mut self,
        events: &[E],
        until: Instant,
    ) -> Result<Checkpoint, AuditError> {
        if events.is_empty() {
            return Ok(self.head);
        }
        let mut frontier = self.frontier.clone();
        let mut writes = Vec::new();
        let mut completed = Vec::new();
        for event in events {
            let event = event.as_ref();
            writes.push(Write::Put {
                family: self.family.clone(),
                key: event_key(frontier.size()),
                value: event.to_vec(),
            });
            completed.clear();
            frontier.push(tree::leaf_hash(event), &mut completed)?;
            for (node, hash) in &completed {
                writes.push(Write::Put {
                    family: self.family.clone(),
                    key: node_key(*node),
                    value: hash.to_vec(),
                });
            }
        }
        let head = Checkpoint {
            size: frontier.size(),
            root: frontier.root(),
        };
        writes.push(Write::Put {
            family: self.family.clone(),
            key: HEAD_KEY.to_vec(),
            value: encode_head(&head),
        });
        let cond = match self.head_rev {
            Some(rev) => Cond::At {
                family: self.family.clone(),
                key: HEAD_KEY.to_vec(),
                rev,
            },
            None => Cond::Absent {
                family: self.family.clone(),
                key: HEAD_KEY.to_vec(),
            },
        };
        let rev = self.records.commit(
            Tx {
                conds: vec![cond],
                writes,
            },
            until,
        )?;
        self.head = head;
        self.head_rev = Some(rev);
        self.frontier = frontier;
        Ok(head)
    }

    /// Whether the head extends `trusted`: RFC 6962's consistency proof, built from the stored
    /// subtrees and checked against both roots.
    fn extends(&self, trusted: &Checkpoint, until: Instant) -> Result<(), AuditError> {
        let stored = self.head.size;
        let inconsistent = Tamper::Inconsistent {
            trusted: trusted.size,
            stored,
        };
        if trusted.size > stored {
            return Err(Tamper::RolledBack {
                trusted: trusted.size,
                stored,
            }
            .into());
        }
        if trusted.size == 0 {
            return if trusted.root == tree::empty_root() {
                Ok(())
            } else {
                Err(inconsistent.into())
            };
        }
        let proof = tree::consistency_proof(trusted.size, stored, |n| self.node(n, until))?;
        if tree::verify_consistency(trusted.size, stored, &trusted.root, &self.head.root, &proof) {
            Ok(())
        } else {
            Err(inconsistent.into())
        }
    }

    fn row(&self, key: &[u8], until: Instant) -> Result<Option<Vec<u8>>, AuditError> {
        Ok(self
            .records
            .get(&self.family, key, Consistency::Linearizable, until)?
            .map(|row| row.value))
    }

    /// A perfect subtree's stored hash. Rows are never rewritten once an append completes them, so
    /// a read after the head's needs no snapshot to see the head's tree.
    fn node(&self, node: Node, until: Instant) -> Result<Hash, AuditError> {
        let place = Place::Node {
            level: node.level,
            index: node.index,
        };
        let value = self
            .row(&node_key(node), until)?
            .ok_or(Tamper::Missing(place))?;
        Hash::try_from(value.as_slice()).map_err(|_| AuditError::from(Tamper::Malformed(place)))
    }
}
