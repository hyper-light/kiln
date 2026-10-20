//! The pipelines kept and applied (`docs/observe.md` §8.2): the server keeps them
//! (`<observe>/pipelines.json`: yours, their order, and which of kiln's own are off) and tells each
//! runner the list in order; every node compiles it and applies it to each record it takes, where it
//! takes it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kiln_obs::logs::record::Record;
use serde::{Deserialize, Serialize};

use super::{Pipeline, PipelineDef, Scratch, builtin};

/// Pipelines kept at the most (Datadog's 100, nested ones apart).
pub const PIPELINES: usize = 100;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A pipeline as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    pub id: String,
    /// kiln's own: read-only, but for whether it is on.
    #[serde(default)]
    pub builtin: bool,
    #[serde(flatten)]
    pub def: PipelineDef,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    /// Yours, in their order (kiln's own come first, in theirs).
    #[serde(default)]
    pipelines: Vec<Kept>,
    #[serde(default)]
    builtins_off: BTreeSet<String>,
}

/// The server's pipelines.
pub struct Pipelines {
    path: PathBuf,
    saved: Mutex<Saved>,
}

impl Pipelines {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("pipelines.json");
        let saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, saved: Mutex::new(saved) }
    }

    /// Every pipeline in the order they apply: kiln's own, then yours.
    pub fn list(&self) -> Vec<Kept> {
        let saved = lock(&self.saved);
        let mut out: Vec<Kept> = builtin::all()
            .into_iter()
            .map(|(id, mut def)| {
                def.is_enabled = !saved.builtins_off.contains(id);
                Kept { id: id.to_owned(), builtin: true, def, created: 0, modified: 0 }
            })
            .collect();
        out.extend(saved.pipelines.iter().cloned());
        out
    }

    pub fn get(&self, id: &str) -> Option<Kept> {
        self.list().into_iter().find(|p| p.id == id)
    }

    /// The definitions as every node applies them, in order.
    pub fn defs(&self) -> Vec<PipelineDef> {
        self.list().into_iter().map(|p| p.def).collect()
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// A new pipeline (checked: it compiles), last: its id.
    pub fn create(&self, def: PipelineDef) -> Result<Kept, String> {
        check(&def)?;
        let mut saved = lock(&self.saved);
        if saved.pipelines.len() >= PIPELINES {
            return Err(format!("{PIPELINES} pipelines at the most"));
        }
        let now = crate::util::now_ms();
        let id = super::super::fresh_id("p", |id| saved.pipelines.iter().any(|p| p.id == id));
        let kept = Kept { id, builtin: false, def, created: now, modified: now };
        saved.pipelines.push(kept.clone());
        self.save(&saved)?;
        Ok(kept)
    }

    /// A pipeline changed: yours whole; kiln's own, whether it is on (nothing else of it changes).
    pub fn put(&self, id: &str, def: PipelineDef) -> Result<Option<Kept>, String> {
        let mut saved = lock(&self.saved);
        if builtin::all().iter().any(|(b, _)| *b == id) {
            if def.is_enabled {
                saved.builtins_off.remove(id);
            } else {
                saved.builtins_off.insert(id.to_owned());
            }
            self.save(&saved)?;
            drop(saved);
            return Ok(self.get(id));
        }
        check(&def)?;
        let Some(p) = saved.pipelines.iter_mut().find(|p| p.id == id) else { return Ok(None) };
        p.def = def;
        p.modified = crate::util::now_ms();
        let out = p.clone();
        self.save(&saved)?;
        Ok(Some(out))
    }

    /// Yours removed (kiln's own are turned off instead): whether it was there.
    pub fn delete(&self, id: &str) -> Result<bool, String> {
        if builtin::all().iter().any(|(b, _)| *b == id) {
            return Err("kiln's own pipelines are turned off, not removed".into());
        }
        let mut saved = lock(&self.saved);
        let before = saved.pipelines.len();
        saved.pipelines.retain(|p| p.id != id);
        let had = saved.pipelines.len() != before;
        self.save(&saved)?;
        Ok(had)
    }

    /// Yours put in the order given: every one of them, each once.
    pub fn order(&self, ids: &[String]) -> Result<(), String> {
        let mut saved = lock(&self.saved);
        let mine: Vec<&str> = ids.iter().map(String::as_str).filter(|id| !id.starts_with("builtin:")).collect();
        let have: BTreeSet<&str> = saved.pipelines.iter().map(|p| p.id.as_str()).collect();
        let given: BTreeSet<&str> = mine.iter().copied().collect();
        if given != have || given.len() != mine.len() {
            return Err("an order names every pipeline of yours, each once".into());
        }
        let mut by_id: std::collections::HashMap<String, Kept> = saved.pipelines.drain(..).map(|p| (p.id.clone(), p)).collect();
        saved.pipelines = mine.iter().filter_map(|id| by_id.remove(*id)).collect();
        self.save(&saved)
    }
}

/// A pipeline checked: named, and compiling (its filter, rules, expressions and tables).
pub fn check(def: &PipelineDef) -> Result<(), String> {
    if def.name.trim().is_empty() || def.name.len() > 200 {
        return Err("a pipeline's name: 1 to 200 characters".into());
    }
    def.compile().map(drop).map_err(|e| e.to_string())
}

/// The pipelines this node applies, compiled.
fn active() -> &'static RwLock<Arc<Vec<Pipeline>>> {
    static ACTIVE: std::sync::OnceLock<RwLock<Arc<Vec<Pipeline>>>> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(Arc::new(Vec::new())))
}

/// `defs` applied from now, those that are on, compiled (one that does not compile, a version's
/// before, is skipped and said).
pub fn activate(defs: &[PipelineDef]) {
    let mut compiled = Vec::new();
    for d in defs.iter().filter(|d| d.is_enabled) {
        match d.compile() {
            Ok(p) => compiled.push(p),
            Err(e) => tracing::warn!(pipeline = %d.name, error = %e, "a log pipeline does not compile here; it is skipped"),
        }
    }
    *active().write().unwrap_or_else(PoisonError::into_inner) = Arc::new(compiled);
}

/// The pipelines active now: what a reader asks of a line before it makes a record of it.
pub fn snapshot() -> Arc<Vec<Pipeline>> {
    active().read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Every active pipeline applied to `records`, in order.
pub fn process(records: &mut [Record]) {
    let pipelines = active().read().unwrap_or_else(PoisonError::into_inner).clone();
    if pipelines.is_empty() || records.is_empty() {
        return;
    }
    let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
    let mut s = Scratch::default();
    for r in records {
        super::apply_all(&pipelines, r, now_ns, &mut s);
    }
}

/// What `def` (or, none given, every active pipeline) makes of `records`: a test, nothing kept.
pub fn try_on(def: Option<&PipelineDef>, records: &mut [Record]) -> Result<(), String> {
    let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
    let mut s = Scratch::default();
    match def {
        Some(d) => {
            let p = d.compile().map_err(|e| e.to_string())?;
            for r in records {
                p.apply(r, now_ns, &mut s);
            }
        }
        None => {
            let pipelines = active().read().unwrap_or_else(PoisonError::into_inner).clone();
            for r in records {
                super::apply_all(&pipelines, r, now_ns, &mut s);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn def(name: &str) -> PipelineDef {
        serde_json::from_value(serde_json::json!({ "name": name, "filter": { "query": "service:web" }, "processors": [{ "type": "string-builder-processor", "template": name, "target": "by" }] })).unwrap()
    }

    #[test]
    fn pipelines_are_kept_in_order_with_kiln_s_own_first() {
        let dir = tempfile::tempdir().unwrap();
        let p = Pipelines::open(dir.path());
        let a = p.create(def("a")).unwrap();
        let b = p.create(def("b")).unwrap();
        assert!(p.create(PipelineDef { name: String::new(), ..def("c") }).is_err());
        let ids: Vec<String> = p.list().into_iter().map(|k| k.id).collect();
        assert_eq!(ids, vec!["builtin:nginx".to_owned(), "builtin:apache".to_owned(), a.id.clone(), b.id.clone()]);
        p.order(&[b.id.clone(), a.id.clone()]).unwrap();
        assert!(p.order(std::slice::from_ref(&a.id)).is_err(), "every one of yours");
        // kiln's own: turned off, not removed nor changed.
        assert!(p.delete("builtin:nginx").is_err());
        let off = PipelineDef { is_enabled: false, ..def("ignored") };
        let k = p.put("builtin:nginx", off).unwrap().unwrap();
        assert!(k.builtin && !k.def.is_enabled && k.def.name == "nginx");
        let again = Pipelines::open(dir.path());
        let listed: Vec<(String, bool)> = again.list().into_iter().map(|k| (k.id, k.def.is_enabled)).collect();
        assert_eq!(listed, vec![("builtin:nginx".to_owned(), false), ("builtin:apache".to_owned(), true), (b.id.clone(), true), (a.id.clone(), true)]);
        // Applied in order: the last that sets an attribute wins.
        activate(&again.defs());
        let mut r = vec![Record { service: "web".into(), message: "m".into(), ..Record::default() }];
        process(&mut r);
        assert_eq!(r.first().map(|r| r.attrs.clone()), Some(vec![("by".to_owned(), "a".to_owned())]));
        activate(&[]);
        assert!(again.delete(&a.id).unwrap() && !again.delete(&a.id).unwrap());
    }
}

