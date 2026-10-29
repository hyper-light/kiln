//! Every string the store keeps, once (`docs/observe.md` §4.1): metric names, tag keys and values,
//! in one arena, each a dense `u32` id.

use hashbrown::HashTable;

use crate::hash::Keyed;

/// A string's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sym(pub u32);

/// The table is full: its arena addresses bytes with `u32` offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the symbol table is full ({0} bytes)")]
pub struct SymbolsFull(pub usize);

#[derive(Debug, Default)]
pub struct Symbols {
    text: String,
    /// Where each symbol ends in `text`; it starts where the one before ends.
    ends: Vec<u32>,
    table: HashTable<u32>,
    hash: Keyed,
}

impl Symbols {
    pub fn new(hash: Keyed) -> Self {
        Self { text: String::new(), ends: Vec::new(), table: HashTable::new(), hash }
    }

    /// The id of `s`, interning it if new.
    pub fn intern(&mut self, s: &str) -> Result<Sym, SymbolsFull> {
        let h = self.hash.bytes(s.as_bytes());
        if let Some(&id) = self.table.find(h, |&id| self.str_of(id) == Some(s)) {
            return Ok(Sym(id));
        }
        let end = self.text.len().checked_add(s.len()).filter(|&e| u32::try_from(e).is_ok()).ok_or(SymbolsFull(self.text.len()))?;
        let id = u32::try_from(self.ends.len()).map_err(|_| SymbolsFull(self.text.len()))?;
        self.text.push_str(s);
        self.ends.push(end as u32);
        let (text, ends, hash) = (&self.text, &self.ends, &self.hash);
        self.table.insert_unique(h, id, |&id| str_in(text, ends, id).map_or(0, |s| hash.bytes(s.as_bytes())));
        Ok(Sym(id))
    }

    /// The id of `s`, if interned.
    pub fn find(&self, s: &str) -> Option<Sym> {
        let h = self.hash.bytes(s.as_bytes());
        self.table.find(h, |&id| self.str_of(id) == Some(s)).map(|&id| Sym(id))
    }

    pub fn get(&self, sym: Sym) -> &str {
        self.str_of(sym.0).unwrap_or("")
    }

    fn str_of(&self, id: u32) -> Option<&str> {
        str_in(&self.text, &self.ends, id)
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Bytes held: the arena, the ends and the table.
    pub fn heap_bytes(&self) -> usize {
        self.text.capacity() + self.ends.capacity() * 4 + self.table.capacity() * 4
    }
}

fn str_in<'a>(text: &'a str, ends: &[u32], id: u32) -> Option<&'a str> {
    let i = id as usize;
    let end = *ends.get(i)? as usize;
    let start = if i == 0 { 0 } else { *ends.get(i - 1)? as usize };
    text.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_is_interned_once_and_reads_back() {
        let mut s = Symbols::new(Keyed::with_keys(1, 2));
        let a = s.intern("host").unwrap();
        let b = s.intern("web-1").unwrap();
        assert_eq!(s.intern("host").unwrap(), a);
        assert_ne!(a, b);
        assert_eq!(s.get(a), "host");
        assert_eq!(s.get(b), "web-1");
        assert_eq!(s.find("web-1"), Some(b));
        assert_eq!(s.find("web-2"), None);
        assert_eq!(s.len(), 2);
        let e = s.intern("").unwrap();
        assert_eq!(s.get(e), "");
        assert_eq!(s.intern("").unwrap(), e);
    }

    #[test]
    fn many_strings_survive_the_table_growing() {
        let mut s = Symbols::new(Keyed::with_keys(3, 4));
        let ids: Vec<Sym> = (0..50_000).map(|i| s.intern(&format!("value-{i}")).unwrap()).collect();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(s.get(*id), format!("value-{i}"));
            assert_eq!(s.find(&format!("value-{i}")), Some(*id));
        }
    }
}
