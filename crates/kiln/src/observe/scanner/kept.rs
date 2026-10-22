//! The scanning groups kept and applied (`docs/observe.md` §8.9): the server keeps them
//! (`<observe>/scanner.json`, in their order, each rule's id given here) and the key hashes are
//! made with (`<observe>/scanner.key`, made once, never shown), and tells each runner both; every
//! node compiles the groups and runs them on each record it takes, where it takes it, after the
//! pipelines.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};

use super::{GROUPS, GroupDef, Scanner, Scratch, check};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A scanning group as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub id: String,
    #[serde(flatten)]
    pub def: GroupDef,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    groups: Vec<Kept>,
}

/// The server's scanning groups.
pub struct Groups {
    path: PathBuf,
    saved: Mutex<Saved>,
    key: String,
}

impl Groups {
    /// The groups kept in `dir`, and its key (made if it is not there yet: a key that could not be
    /// written is said, and the groups hash with one of this process's alone).
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("scanner.json");
        let saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let key = crate::util::load_or_create_token(&dir.join("scanner.key")).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "the sensitive data scanner's key could not be kept: hashes are this process's own");
            crate::util::random_token()
        });
        Self { path, saved: Mutex::new(saved), key }
    }

    pub fn list(&self) -> Vec<Kept> {
        lock(&self.saved).groups.clone()
    }

    pub fn get(&self, id: &str) -> Option<Kept> {
        lock(&self.saved).groups.iter().find(|g| g.id == id).cloned()
    }

    /// The groups as every node runs them, in order, and the key their hashes are made with.
    pub fn defs(&self) -> (Vec<GroupDef>, String) {
        (lock(&self.saved).groups.iter().map(|g| g.def.clone()).collect(), self.key.clone())
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// A new group (checked: it compiles), last.
    pub fn create(&self, mut def: GroupDef) -> Result<Kept, String> {
        check(&def).map_err(|e| e.to_string())?;
        let mut saved = lock(&self.saved);
        if saved.groups.len() >= GROUPS {
            return Err(format!("{GROUPS} scanning groups at the most"));
        }
        if saved.groups.iter().any(|g| g.def.name.trim() == def.name.trim()) {
            return Err(format!("a group named {:?} already", def.name.trim()));
        }
        name_rules(&mut def, None);
        let now = crate::util::now_ms();
        let id = super::super::fresh_id("s", |id| saved.groups.iter().any(|g| g.id == id));
        let kept = Kept { id, def, created: now, modified: now };
        saved.groups.push(kept.clone());
        self.save(&saved)?;
        Ok(kept)
    }

    /// A group changed whole (its rules' ids kept where given): none if it is not there.
    pub fn put(&self, id: &str, mut def: GroupDef) -> Result<Option<Kept>, String> {
        check(&def).map_err(|e| e.to_string())?;
        let mut saved = lock(&self.saved);
        if saved.groups.iter().any(|g| g.id != id && g.def.name.trim() == def.name.trim()) {
            return Err(format!("a group named {:?} already", def.name.trim()));
        }
        let Some(g) = saved.groups.iter_mut().find(|g| g.id == id) else { return Ok(None) };
        name_rules(&mut def, Some(&g.def));
        g.def = def;
        g.modified = crate::util::now_ms();
        let out = g.clone();
        self.save(&saved)?;
        Ok(Some(out))
    }

    /// A group removed: whether it was there.
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut saved = lock(&self.saved);
        let before = saved.groups.len();
        saved.groups.retain(|g| g.id != id);
        let had = saved.groups.len() != before;
        self.save(&saved)?;
        Ok(had)
    }

    /// The groups put in the order given: every one, each once.
    pub fn order(&self, ids: &[String]) -> Result<(), String> {
        let mut saved = lock(&self.saved);
        let mut have: Vec<&str> = saved.groups.iter().map(|g| g.id.as_str()).collect();
        let mut given: Vec<&str> = ids.iter().map(String::as_str).collect();
        have.sort_unstable();
        given.sort_unstable();
        if have != given {
            return Err("an order names every scanning group, each once".into());
        }
        let mut by_id: std::collections::HashMap<String, Kept> = saved.groups.drain(..).map(|g| (g.id.clone(), g)).collect();
        saved.groups = ids.iter().filter_map(|id| by_id.remove(id)).collect();
        self.save(&saved)
    }
}

/// Each rule of `def` an id: its own where it gives one this group's rules had (or that no other
/// of its rules takes), a fresh one else.
fn name_rules(def: &mut GroupDef, was: Option<&GroupDef>) {
    let had: Vec<&str> = was.map(|w| w.rules.iter().map(|r| r.id.as_str()).collect()).unwrap_or_default();
    let mut taken: Vec<String> = Vec::new();
    for r in &mut def.rules {
        let keep = !r.id.is_empty() && (had.contains(&r.id.as_str()) || was.is_none()) && !taken.contains(&r.id) && r.id.len() <= 64;
        if !keep {
            r.id = super::super::fresh_id("r", |id| taken.iter().any(|t| t == id) || had.contains(&id));
        }
        taken.push(r.id.clone());
    }
}

/// The scanner this node runs.
fn active() -> &'static RwLock<Arc<Scanner>> {
    static ACTIVE: std::sync::OnceLock<RwLock<Arc<Scanner>>> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(Arc::new(Scanner::default())))
}

/// `defs` run from now, hashing with `key`.
pub fn activate(defs: &[GroupDef], key: &str) {
    *active().write().unwrap_or_else(PoisonError::into_inner) = Arc::new(Scanner::compile(defs, key.as_bytes()));
}

/// The scanner now: what a reader asks of a line before it makes a record of it.
pub fn snapshot() -> Arc<Scanner> {
    active().read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The scanner run on `records`.
pub fn process(records: &mut [Record]) {
    let scanner = snapshot();
    if scanner.is_empty() || records.is_empty() {
        return;
    }
    let mut s = Scratch::default();
    for r in records {
        scanner.scan(r, &mut s);
    }
}

/// What `def` (or, none given, the groups that run) makes of `records`: a test, nothing kept.
pub fn try_on(def: Option<&GroupDef>, key: &str, records: &mut [Record]) -> Result<(), String> {
    let scanner = match def {
        Some(d) => {
            check(d).map_err(|e| e.to_string())?;
            Arc::new(Scanner::compile(std::slice::from_ref(d), key.as_bytes()))
        }
        None => snapshot(),
    };
    let mut s = Scratch::default();
    for r in records {
        scanner.scan(r, &mut s);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn def(name: &str) -> GroupDef {
        serde_json::from_value(serde_json::json!({ "name": name, "rules": [{ "name": "cards", "standard_pattern_id": "visa_card", "text_replacement": { "type": "replacement_string" } }] })).unwrap()
    }

    /// Groups kept in order across an opening, each rule given an id and keeping it as its group is
    /// changed; names unique; the key made once and kept; what they make of a record.
    #[test]
    fn groups_are_kept_in_order_and_their_rules_keep_their_ids() {
        let dir = tempfile::tempdir().unwrap();
        let g = Groups::open(dir.path());
        let a = g.create(def("a")).unwrap();
        let b = g.create(def("b")).unwrap();
        assert!(g.create(def("a")).unwrap_err().contains("already"));
        let rule_id = a.def.rules.first().map(|r| r.id.clone()).unwrap();
        assert!(rule_id.starts_with('r'));
        // Changed: the rule it named keeps its id; a new one gets its own.
        let mut changed = a.def.clone();
        changed.rules.push(serde_json::from_value(serde_json::json!({ "name": "emails", "standard_pattern_id": "email_address" })).unwrap());
        let put = g.put(&a.id, changed).unwrap().unwrap();
        let ids: Vec<&str> = put.def.rules.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.len() == 2 && ids[0] == rule_id && !ids[1].is_empty() && ids[1] != rule_id, "{ids:?}");
        g.order(&[b.id.clone(), a.id.clone()]).unwrap();
        assert!(g.order(std::slice::from_ref(&a.id)).is_err());
        let again = Groups::open(dir.path());
        assert_eq!(again.list().iter().map(|k| k.id.clone()).collect::<Vec<_>>(), vec![b.id.clone(), a.id.clone()]);
        let ((defs, key), (_, key2)) = (g.defs(), again.defs());
        assert_eq!(key, key2, "the key kept");
        activate(&defs, &key);
        let mut r = vec![Record { message: "4111111111111111".into(), ..Record::default() }];
        process(&mut r);
        assert_eq!(r.first().map(|r| r.message.as_str()), Some(super::super::REDACTED));
        activate(&[], &key);
        assert!(again.delete(&a.id).unwrap() && !again.delete(&a.id).unwrap());
    }
}
