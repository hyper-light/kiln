//! A table the scheduler looks up by key on every input, in constant time, and walks only in its
//! keys' order: a hash map's order never reaches a decision (docs/needs.md, E1).

use std::collections::HashMap;

/// Entries by key: looked up in constant time, walked in key order.
#[derive(Debug, Clone)]
pub struct Table<V> {
    map: HashMap<String, V>,
}

impl<V> Default for Table<V> {
    fn default() -> Self {
        Table { map: HashMap::new() }
    }
}

impl<V> Table<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn insert(&mut self, key: String, value: V) -> Option<V> {
        self.map.insert(key, value)
    }

    pub fn remove(&mut self, key: &str) -> Option<V> {
        self.map.remove(key)
    }

    /// Every entry, in its keys' order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        let mut all: Vec<(&String, &V)> = self.map.iter().collect();
        all.sort_unstable_by(|a, b| a.0.cmp(b.0));
        all.into_iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E1.3: tables holding the same entries walk them alike, whatever order they came in, and
    /// whatever their hash maps' seeds.
    #[test]
    fn e1_3_tables_walk_in_key_order() {
        let keys = ["l9", "l10", "a", "l1", "b2", "Z", "l100"];
        let (mut a, mut b) = (Table::default(), Table::default());
        for (i, k) in keys.iter().enumerate() {
            a.insert((*k).to_owned(), i);
        }
        for (i, k) in keys.iter().enumerate().rev() {
            b.insert((*k).to_owned(), i);
        }
        let walked: Vec<&str> = a.keys().map(String::as_str).collect();
        assert_eq!(walked, ["Z", "a", "b2", "l1", "l10", "l100", "l9"]);
        assert!(a.iter().eq(b.iter()));
        assert_eq!(a.values().copied().collect::<Vec<_>>(), [5, 2, 4, 3, 1, 6, 0]);
        assert_eq!(a.remove("l10"), Some(1));
        assert_eq!(a.get("l10"), None);
        if let Some(v) = a.get_mut("a") {
            *v = 7;
        }
        assert_eq!(a.get("a"), Some(&7));
        assert_eq!(a.insert("a".into(), 8), Some(7));
    }
}
