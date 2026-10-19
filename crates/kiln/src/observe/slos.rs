//! Service level objectives, kept (`docs/observe.md` §9.2): each its indicator (`kiln_obs::slo`), a
//! target over a rolling timeframe, and its standing read from the store when asked; its alerts are
//! monitors of it (`slo("<id>").burn_rate("1h", "5m") > 14.4`, `slo("<id>").error_budget() < 25`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kiln_obs::monitor::Op;
use kiln_obs::slo::{Sli, Slo, TIMEFRAMES_MS};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// SLOs kept at the most.
pub const SLOS: usize = 1_000;

/// An SLO as the API takes and shows it (Datadog's SLO object, near enough).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SloDef {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// `ratio` (good events over total) or `slices` (time slices meeting a condition).
    #[serde(rename = "type")]
    pub kind: String,
    /// A ratio's queries, of counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub good: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<String>,
    /// A time slice SLO's query, its condition (`< 0.3`) and its slice (60 or 300 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice_s: Option<u64>,
    /// Percent: 99.9.
    pub target: f64,
    /// A warning above the target (99.95): the standing says WARN below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<f64>,
    /// `7d`, `30d` or `90d`.
    pub timeframe: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

/// A condition's comparison and threshold (`< 0.3`, `>= 99`).
fn condition(s: &str) -> Result<(Op, f64), String> {
    let s = s.trim();
    let (op, rest) = [(">=", Op::Ge), ("<=", Op::Le), ("==", Op::Eq), ("!=", Op::Ne), (">", Op::Gt), ("<", Op::Lt)]
        .into_iter()
        .find_map(|(t, op)| s.strip_prefix(t).map(|r| (op, r)))
        .ok_or_else(|| format!("a condition: a comparison and a number (< 0.3): {s}"))?;
    let threshold: f64 = rest.trim().parse().map_err(|_| format!("a condition's number: {rest}"))?;
    Ok((op, threshold))
}

impl SloDef {
    /// The engine's SLO of it, checked.
    pub fn slo(&self) -> Result<Slo, String> {
        let timeframe_ms = match self.timeframe.as_str() {
            "7d" => TIMEFRAMES_MS[0],
            "30d" => TIMEFRAMES_MS[1],
            "90d" => TIMEFRAMES_MS[2],
            other => return Err(format!("a timeframe: 7d, 30d or 90d, not {other:?}")),
        };
        let sli = match self.kind.as_str() {
            "ratio" => Sli::Ratio { good: self.good.clone().filter(|q| !q.trim().is_empty()).ok_or("a ratio's good events: a query of counts")?, total: self.total.clone().filter(|q| !q.trim().is_empty()).ok_or("a ratio's total events: a query of counts")? },
            "slices" => {
                let (op, threshold) = condition(self.condition.as_deref().unwrap_or(""))?;
                Sli::Slices { query: self.query.clone().filter(|q| !q.trim().is_empty()).ok_or("a time slice's query")?, op, threshold, slice_ms: i64::try_from(self.slice_s.unwrap_or(300).saturating_mul(1000)).unwrap_or(0) }
            }
            other => return Err(format!("an SLO's type: ratio or slices, not {other:?}")),
        };
        let slo = Slo { sli, target: self.target, timeframe_ms };
        slo.check().map_err(|e| e.to_string())?;
        if let Some(w) = self.warning
            && !(w > self.target && w < 100.0)
        {
            return Err(format!("a warning above the target and under 100: {w}"));
        }
        Ok(slo)
    }
}

/// An SLO's standing as the API shows it: its SLI, the budget left, the state; and the burn rates
/// of the workbook's windows and, with `days`, each day's.
pub fn standing(store: &kiln_obs::store::Store, def: &SloDef, now: i64, days: bool) -> Value {
    let slo = match def.slo() {
        Ok(s) => s,
        Err(e) => return json!({ "state": "INVALID", "error": e }),
    };
    let st = match kiln_obs::slo::status(store, &slo, now) {
        Ok(s) => s,
        Err(e) => return json!({ "state": "NO DATA", "error": e.to_string() }),
    };
    let state = match st.sli {
        None => "NO DATA",
        Some(s) if s < def.target => "BREACHED",
        Some(s) if def.warning.is_some_and(|w| s < w) => "WARN",
        Some(_) => "OK",
    };
    let mut burn = serde_json::Map::new();
    for (label, ms) in [("1h", 3_600_000), ("6h", 6 * 3_600_000), ("1d", 86_400_000), ("3d", 3 * 86_400_000)] {
        let b = kiln_obs::slo::burn_rate(store, &slo, ms, now).ok().flatten();
        burn.insert(label.into(), b.map_or(Value::Null, Value::from));
    }
    let mut out = json!({ "state": state, "sli": st.sli, "good": st.good, "total": st.total, "budget_left": st.budget_left, "burn_rates": burn });
    if days && let Some(o) = out.as_object_mut() {
        o.insert("days".into(), st.days.iter().map(|(d, sli, left)| json!({ "day": d, "sli": sli, "budget_left": left })).collect());
    }
    out
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The SLOs kept, in `<observe>/slos.json`.
pub struct Slos {
    path: PathBuf,
    all: Mutex<BTreeMap<String, SloDef>>,
}

impl Slos {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("slos.json");
        let all = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { path, all: Mutex::new(all) }
    }

    /// Every SLO, by name.
    pub fn list(&self) -> Vec<SloDef> {
        let mut out: Vec<SloDef> = lock(&self.all).values().cloned().collect();
        out.sort_by_cached_key(|s| s.name.to_lowercase());
        out
    }

    pub fn get(&self, id: &str) -> Option<SloDef> {
        lock(&self.all).get(id).cloned()
    }

    /// Creates or replaces an SLO, checked.
    pub fn put(&self, mut s: SloDef) -> Result<SloDef, String> {
        if s.name.trim().is_empty() {
            return Err("an SLO's name".into());
        }
        s.slo()?;
        let now = crate::util::now_ms();
        let mut all = lock(&self.all);
        match all.get(&s.id) {
            Some(old) => s.created = old.created,
            None => {
                if all.len() >= SLOS {
                    return Err(format!("{SLOS} SLOs at the most"));
                }
                if s.id.is_empty() {
                    s.id = super::fresh_id("s", |id| all.contains_key(id));
                }
                s.created = now;
            }
        }
        s.modified = now;
        all.insert(s.id.clone(), s.clone());
        let bytes = serde_json::to_vec_pretty(&*all).map_err(|e| e.to_string())?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        Ok(s)
    }

    pub fn delete(&self, id: &str) -> Result<bool, String> {
        let mut all = lock(&self.all);
        let had = all.remove(id).is_some();
        let bytes = serde_json::to_vec_pretty(&*all).map_err(|e| e.to_string())?;
        drop(all);
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        Ok(had)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def() -> SloDef {
        SloDef {
            id: String::new(),
            name: "checkout availability".into(),
            description: String::new(),
            kind: "ratio".into(),
            good: Some("sum:http.requests{status:ok}.as_count()".into()),
            total: Some("sum:http.requests{*}.as_count()".into()),
            query: None,
            condition: None,
            slice_s: None,
            target: 99.9,
            warning: Some(99.95),
            timeframe: "30d".into(),
            tags: vec!["team:pay".into()],
            created: 0,
            modified: 0,
        }
    }

    #[test]
    fn slos_are_checked_kept_and_reopened() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let slos = Slos::open(dir.path());
        let s = slos.put(def()).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(s.id.starts_with('s') && s.created > 0);
        let slices = SloDef { kind: "slices".into(), good: None, total: None, query: Some("p99:http.latency{*}".into()), condition: Some("< 0.3".into()), slice_s: Some(60), ..def() };
        slos.put(slices).map_err(anyhow::Error::msg)?;
        for bad in [
            SloDef { target: 100.0, ..def() },
            SloDef { timeframe: "14d".into(), ..def() },
            SloDef { kind: "uptime".into(), ..def() },
            SloDef { good: None, ..def() },
            SloDef { warning: Some(99.0), ..def() },
            SloDef { kind: "slices".into(), query: Some("p99:x{*}".into()), condition: Some("about 3".into()), ..def() },
            SloDef { kind: "slices".into(), query: Some("p99:x{*}".into()), condition: Some("< 1".into()), slice_s: Some(120), ..def() },
            SloDef { name: " ".into(), ..def() },
        ] {
            anyhow::ensure!(slos.put(bad.clone()).is_err(), "{bad:?}");
        }
        let again = Slos::open(dir.path());
        anyhow::ensure!(again.list().len() == 2 && again.get(&s.id).is_some_and(|x| x.target == 99.9));
        anyhow::ensure!(again.delete(&s.id).map_err(anyhow::Error::msg)? && !again.delete(&s.id).map_err(anyhow::Error::msg)?);
        Ok(())
    }
}
