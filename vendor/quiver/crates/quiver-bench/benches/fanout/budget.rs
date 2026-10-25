//! The ephemeral class's budgets (`bench/budgets.tsv`; kiln's CP-T2.1, step 2): what one run of a
//! load may spend, figure by figure, a message or a delivery. A run checked against them
//! (`--budgets FILE`) fails where a figure is over its budget by more than a tenth (one of zero,
//! where it's over zero at all), and where the figure isn't there to check, refused or never
//! profiled: a budget nobody measured isn't met.
//!
//! The file is tab-separated, one budget a line, `#` beginning a comment: the load (system, how
//! its subscribers wait, rate, subscribers), the figure (its name in the runs file, as
//! `subscribers_allocations`), what it's counted by (`message` or `delivery`), the budget, and the
//! machine it holds on (part of the CPU's model name, or `any` for a figure no machine moves).

use std::path::Path;

use crate::report::Run;

/// One budget.
pub struct Budget {
    system: String,
    waits: String,
    rate: u64,
    subscribers: usize,
    figure: String,
    per: String,
    value: f64,
    machine: String,
}

/// The budgets in `path`.
pub fn read(path: &Path) -> Vec<Budget> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading the budgets {}: {e}", path.display()));
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            assert!(f.len() == 8, "a budget has 8 fields, tab-separated: {l:?}");
            assert!(
                f[5] == "message" || f[5] == "delivery",
                "a budget is a message's or a delivery's: {l:?}"
            );
            Budget {
                system: f[0].to_owned(),
                waits: f[1].to_owned(),
                rate: f[2].parse().unwrap(),
                subscribers: f[3].parse().unwrap(),
                figure: f[4].to_owned(),
                per: f[5].to_owned(),
                value: f[6].parse().unwrap(),
                machine: f[7].to_owned(),
            }
        })
        .collect()
}

/// `r` against the budgets for its load on `machine`: their table for the record, and whether
/// every one was met (true where none applies).
pub fn check(budgets: &[Budget], r: &Run, machine: &str) -> (String, bool) {
    let waits = if r.busy { "busy" } else { "wait" };
    let fields = r
        .profile
        .as_ref()
        .map(crate::profile::Profile::fields)
        .unwrap_or_default();
    let mine: Vec<&Budget> = budgets
        .iter()
        .filter(|b| {
            b.system == r.system.name()
                && b.waits == waits
                && b.rate == r.rate as u64
                && b.subscribers == r.subscribers
                && (b.machine == "any" || machine.contains(&b.machine))
        })
        .collect();
    if mine.is_empty() {
        return (String::new(), true);
    }
    let mut out =
        String::from("| budget | per | budget | this run | met |\n|---|---|---|---|---|\n");
    let mut met = true;
    for b in mine {
        let by = fields
            .get(if b.per == "message" {
                "profile_messages"
            } else {
                "profile_deliveries"
            })
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| *n > 0.0);
        let raw = fields.get(&b.figure);
        let (shown, ok) = match (raw.and_then(|v| v.parse::<f64>().ok()), by) {
            (Some(v), Some(n)) => {
                let per = v / n;
                let ok = if b.value == 0.0 {
                    per == 0.0
                } else {
                    per <= b.value * 1.1
                };
                (crate::profile::show(per), ok)
            }
            _ => (
                raw.map_or_else(|| "not profiled".to_owned(), |v| v.replace('|', "/")),
                false,
            ),
        };
        met &= ok;
        out.push_str(&format!(
            "| {} | {} | {} | {shown} | {} |\n",
            b.figure,
            b.per,
            crate::profile::show(b.value),
            if ok { "yes" } else { "NO" }
        ));
    }
    (out, met)
}
