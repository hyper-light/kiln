//! A topic's state as the generic core keeps it, without knowing its record types: the one
//! record's fields, the items of a collection by key, and a text or byte stream. The server's
//! topics keep it to answer snapshots (`crate::topic`), and a client mirrors it from what comes
//! (`crate::client`), so the two can be compared by digest. Each field is its own encoding
//! (docs/browser-protocol.md, "Changes"); an empty one is its type's default.

use std::collections::BTreeMap;

use crate::frames::{Delta, Key};
use crate::schema::fnv64;
use crate::wire::{Bits, Bytes, Reader, Writer};

/// An item's key, as the server names it (never a `Ref`, which is a subscription's own).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ItemKey {
    Num(u64),
    Str(String),
}

impl ItemKey {
    pub fn key(&self) -> Key {
        match self {
            ItemKey::Num(n) => Key::Num { n: *n },
            ItemKey::Str(s) => Key::Str { s: s.clone() },
        }
    }

    /// The key a delta names, `None` for a `Ref` (which only a subscription's table resolves).
    pub fn of(k: &Key) -> Option<ItemKey> {
        match k {
            Key::Num { n } => Some(ItemKey::Num(*n)),
            Key::Str { s } => Some(ItemKey::Str(s.clone())),
            Key::Ref { .. } => None,
        }
    }
}

/// Why a delta didn't apply: the state isn't where the delta expects it, so the subscription
/// starts again from its cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misfit {
    /// A patch or extension of an item that isn't there.
    NoItem,
    /// Text at an offset that isn't the stream's end.
    Offset,
    /// A key the delta can't name here (an unresolved `Ref`).
    Key,
    /// An extension of a field that isn't text.
    NotText,
}

/// The fields a mask has bits for, set into `into`.
pub fn set_fields(into: &mut Vec<Bytes>, mask: Bits, fields: &[Bytes]) {
    let mut it = fields.iter();
    for bit in 0..64usize {
        if mask.0 & (1u64 << bit) == 0 {
            continue;
        }
        let Some(f) = it.next() else { return };
        if into.len() <= bit {
            into.resize(bit.saturating_add(1), Bytes::default());
        }
        if let Some(slot) = into.get_mut(bit) {
            *slot = f.clone();
        }
    }
}

/// Merges `b`'s fields over `a`'s: the union of the masks, `b`'s value where both have one.
pub fn merge_fields(a_mask: Bits, a: &[Bytes], b_mask: Bits, b: &[Bytes]) -> (Bits, Vec<Bytes>) {
    let mut full: Vec<Option<Bytes>> = vec![None; 64];
    for (mask, fields) in [(a_mask, a), (b_mask, b)] {
        let mut it = fields.iter();
        for bit in 0..64usize {
            if mask.0 & (1u64 << bit) != 0
                && let (Some(f), Some(slot)) = (it.next(), full.get_mut(bit))
            {
                *slot = Some(f.clone());
            }
        }
    }
    let mask = Bits(a_mask.0 | b_mask.0);
    (mask, full.into_iter().flatten().collect())
}

/// Appends `data` to a field encoded as `str` or `bytes` (a varint length, then the bytes), at
/// offset `at` of what it holds.
pub fn extend_field(field: &mut Bytes, at: u64, data: &[u8]) -> Result<(), Misfit> {
    let mut held: Vec<u8> = if field.0.is_empty() {
        Vec::new()
    } else {
        let mut r = Reader::new(&field.0);
        let b = r.bytes().map_err(|_| Misfit::NotText)?;
        if !r.is_empty() {
            return Err(Misfit::NotText);
        }
        b.to_vec()
    };
    if u64::try_from(held.len()).ok() != Some(at) {
        return Err(Misfit::Offset);
    }
    held.extend_from_slice(data);
    let mut w = Writer::with_capacity(held.len().saturating_add(4));
    w.bytes(&held);
    field.0 = w.buf;
    Ok(())
}

/// How long the text a field holds is (0 when it holds none).
pub fn text_len(field: &Bytes) -> u64 {
    let mut r = Reader::new(&field.0);
    r.bytes().map(|b| u64::try_from(b.len()).unwrap_or(0)).unwrap_or(0)
}

/// A topic's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TopicState {
    /// The one record's fields (a `Set` topic).
    pub record: Vec<Bytes>,
    /// The items, by key (a collection topic).
    pub items: BTreeMap<ItemKey, Vec<Bytes>>,
    /// The stream's bytes held, and the offset of the first (a text topic).
    pub text: Vec<u8>,
    pub text_from: Option<u64>,
}

impl TopicState {
    /// Where the stream ends.
    pub fn text_end(&self) -> u64 {
        self.text_from.unwrap_or(0).saturating_add(u64::try_from(self.text.len()).unwrap_or(0))
    }

    /// Applies a delta whose keys are resolved (no `Ref`).
    pub fn apply(&mut self, d: &Delta) -> Result<(), Misfit> {
        match d {
            Delta::Set { mask, fields } => set_fields(&mut self.record, *mask, fields),
            Delta::Put { key, fields } => {
                self.items.insert(ItemKey::of(key).ok_or(Misfit::Key)?, fields.clone());
            }
            Delta::Patch { key, mask, fields } => {
                let item = self.items.get_mut(&ItemKey::of(key).ok_or(Misfit::Key)?).ok_or(Misfit::NoItem)?;
                set_fields(item, *mask, fields);
            }
            Delta::Remove { key } => {
                self.items.remove(&ItemKey::of(key).ok_or(Misfit::Key)?);
            }
            Delta::Append { at, data } => {
                if self.text_from.is_none() {
                    self.text_from = Some(*at);
                }
                if *at != self.text_end() {
                    return Err(Misfit::Offset);
                }
                self.text.extend_from_slice(&data.0);
            }
            Delta::Extend { key, field, at, data } => {
                let item = self.items.get_mut(&ItemKey::of(key).ok_or(Misfit::Key)?).ok_or(Misfit::NoItem)?;
                let i = usize::try_from(*field).map_err(|_| Misfit::NotText)?;
                if item.len() <= i {
                    item.resize(i.saturating_add(1), Bytes::default());
                }
                let f = item.get_mut(i).ok_or(Misfit::NotText)?;
                extend_field(f, *at, &data.0)?;
            }
        }
        Ok(())
    }

    /// Lets go of the stream's oldest bytes past `keep`.
    pub fn trim_text(&mut self, keep: usize) {
        if self.text.len() > keep {
            let cut = self.text.len().saturating_sub(keep);
            self.text.drain(..cut);
            self.text_from = Some(self.text_from.unwrap_or(0).saturating_add(u64::try_from(cut).unwrap_or(0)));
        }
    }

    /// The state as the deltas that make it from nothing: what a snapshot sends.
    pub fn snapshot(&self) -> Vec<Delta> {
        let mut out = Vec::new();
        if !self.record.is_empty() {
            let n = self.record.len().min(64);
            let mask = if n == 64 { u64::MAX } else { (1u64 << n).saturating_sub(1) };
            out.push(Delta::Set { mask: Bits(mask), fields: self.record.iter().take(64).cloned().collect() });
        }
        for (k, fields) in &self.items {
            out.push(Delta::Put { key: k.key(), fields: fields.clone() });
        }
        if let Some(from) = self.text_from {
            out.push(Delta::Append { at: from, data: Bytes(self.text.clone()) });
        }
        out
    }

    /// A digest of the state, for comparing a mirror with its source.
    pub fn digest(&self) -> u64 {
        let mut w = Writer::new();
        w.len(self.record.len());
        for f in &self.record {
            w.bytes(&f.0);
        }
        w.len(self.items.len());
        for (k, fields) in &self.items {
            match k {
                ItemKey::Num(n) => {
                    w.byte(0);
                    w.varint(*n);
                }
                ItemKey::Str(s) => {
                    w.byte(1);
                    w.bytes(s.as_bytes());
                }
            }
            w.len(fields.len());
            for f in fields {
                w.bytes(&f.0);
            }
        }
        w.varint(self.text_from.unwrap_or(0));
        w.bytes(&self.text);
        fnv64(&w.buf)
    }
}
