//! `amphora-corpus run <pool dir> [--engine <name>] [--budget-s <seconds>] [--times <file>]`: a pool
//! of corpus files run on one engine (Amphora by default; every engine of G2 step 4 through
//! amphora_corpus::engines), each on a fresh database, in the pool's order (`order.tsv`: a line a
//! file, `required` or `fill`, then its path; every `.test` file in path order when there is none).
//! Required files run whole; fill files run until the budget, measured on this machine as it runs,
//! is spent, so the subset's size is what this machine runs in the budget (G2 step 7). No record
//! runs past the budget itself: the engine stops it there and it's counted past the budget, which
//! fails the run for a required file (a required record can't run in the gate's budget). `--times`
//! writes each record's outcome and time, a JSON line each, then a line for the file whole
//! (`file_ns`, its fresh database and fixture included; `unsettled`, its failed settles), for re-ranking the pool from the machines
//! that run it (bench/corpus/subset.py). A file `<source>/base.test` is the fixture of every file under `<source>/`
//! (bench/corpus/build_corpus.py's rule). Exits 1 on any disagreement, 2 when the run itself fails,
//! and 3 when the engine can't run on this machine (its program missing, its server not reached).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use amphora_corpus::amphora::AmphoraRender;
use amphora_corpus::dialect::{Translated, Translation};
use amphora_corpus::run::{Counts, Limits, Outcome, run_file};

struct Args {
    pool: PathBuf,
    engine: String,
    budget: Option<Duration>,
    times: Option<PathBuf>,
}

fn args() -> Result<Args, String> {
    let mut it = std::env::args_os().skip(1).map(|a| {
        a.into_string()
            .map_err(|a| format!("an argument that isn't Unicode: {a:?}"))
    });
    match it.next().transpose()?.as_deref() {
        Some("run") => {}
        other => {
            return Err(format!(
                "usage: amphora-corpus run <pool dir> [--engine <name>] [--budget-s <seconds>] [--times <file>] (got {other:?})"
            ));
        }
    }
    let pool = PathBuf::from(it.next().transpose()?.ok_or("run takes a pool directory")?);
    let (mut budget, mut times, mut engine) = (None, None, "amphora".to_owned());
    while let Some(a) = it.next().transpose()? {
        let v = it
            .next()
            .transpose()?
            .ok_or_else(|| format!("{a} takes a value"))?;
        match a.as_str() {
            "--budget-s" => {
                budget = Some(
                    Duration::try_from_secs_f64(
                        v.parse::<f64>()
                            .map_err(|e| format!("--budget-s {v}: {e}"))?,
                    )
                    .map_err(|e| format!("--budget-s {v}: {e}"))?,
                )
            }
            "--times" => times = Some(PathBuf::from(v)),
            "--engine" => engine = v,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        pool,
        engine,
        budget,
        times,
    })
}

/// The pool's files in order, each with whether it's required.
fn order(pool: &Path) -> Result<Vec<(bool, PathBuf)>, String> {
    let manifest = pool.join("order.tsv");
    if manifest.exists() {
        let text = std::fs::read_to_string(&manifest)
            .map_err(|e| format!("{}: {e}", manifest.display()))?;
        return text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match l.split_once('\t') {
                Some(("required", p)) => Ok((true, pool.join(p))),
                Some(("fill", p)) => Ok((false, pool.join(p))),
                _ => Err(format!(
                    "{}: a line that isn't `required|fill<TAB>path`: {l}",
                    manifest.display()
                )),
            })
            .collect();
    }
    // A flat walk with an explicit stack (no recursion, CLAUDE.md §11).
    let mut files = Vec::new();
    let mut dirs = vec![pool.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let p = entry.map_err(|e| e.to_string())?.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "test")
                && p.file_name().is_some_and(|n| n != "base.test")
            {
                files.push(p);
            }
        }
    }
    files.sort();
    Ok(files.into_iter().map(|p| (true, p)).collect())
}

/// `<pool>/<source>/base.test` for a file under `<pool>/<source>/`.
fn fixture(pool: &Path, file: &Path) -> Option<PathBuf> {
    let source = file.strip_prefix(pool).ok()?.components().next()?;
    let f = pool.join(source).join("base.test");
    if f.exists() && f != file {
        Some(f)
    } else {
        None
    }
}

/// What starts the error of an engine that can't run here (exit 3).
const UNAVAILABLE: &str = "the engine can't run here: ";

fn main() -> ExitCode {
    let mut out = std::io::stdout().lock();
    match go(&mut out) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) if e.starts_with(UNAVAILABLE) => {
            let _ = writeln!(out, "amphora-corpus: {e}");
            ExitCode::from(3)
        }
        Err(e) => {
            let _ = writeln!(out, "amphora-corpus: {e}");
            ExitCode::from(2)
        }
    }
}

fn go(out: &mut dyn Write) -> Result<bool, String> {
    let a = args()?;
    let began = Instant::now();
    let deadline = a.budget.and_then(|b| began.checked_add(b));
    let mut times = match &a.times {
        Some(p) => Some(std::io::BufWriter::new(
            std::fs::File::create(p).map_err(|e| format!("{}: {e}", p.display()))?,
        )),
        None => None,
    };
    let mut render = AmphoraRender::new()?;
    let mut total = Counts::default();
    let (mut files, mut write_error, mut required_past) = (0u64, None, 0u64);
    for (required, file) in order(&a.pool)? {
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let fix = match fixture(&a.pool, &file) {
            Some(f) => {
                Some(std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?)
            }
            None => None,
        };
        let rel = file
            .strip_prefix(&a.pool)
            .unwrap_or(&file)
            .display()
            .to_string();
        // The file's records in the engine's dialect, made when the corpus was built (G2 step 4).
        let dialect = amphora_corpus::dialect::of(&a.engine);
        let translation = match dialect {
            Some(d) => Some(Translation::read(&amphora_corpus::dialect::path(
                &a.pool,
                d,
                Path::new(&rel),
            ))?),
            None => None,
        };
        let fix_translation = match (dialect, fixture(&a.pool, &file)) {
            (Some(d), Some(f)) => {
                let frel = f.strip_prefix(&a.pool).unwrap_or(&f).to_path_buf();
                Some(Translation::read(&amphora_corpus::dialect::path(
                    &a.pool, d, &frel,
                ))?)
            }
            _ => None,
        };
        let translated = translation.as_ref().map(|t| Translated {
            file: t,
            fixture: fix_translation.as_ref(),
        });
        let file_began = Instant::now();
        // Its opening, settling and closing statements are bounded as a record is: a server that
        // doesn't answer within the budget is unreachable, never a hung run.
        let mut engine = amphora_corpus::engines::open(&a.engine, a.budget)
            .map_err(|e| format!("{UNAVAILABLE}{e}"))?;
        // Fill files stop at the deadline; every record, required ones too, is stopped once it has
        // run as long as the whole budget.
        let limits = Limits {
            deadline: if required { None } else { deadline },
            record: a.budget,
        };
        let name = a.engine.as_str();
        let counts = run_file(engine.as_mut(), &mut render, &text, fix.as_deref(), translated, limits, &mut |ran| {
            let outcome = match &ran.outcome {
                Outcome::Agree => "agree".to_owned(),
                Outcome::Unreached => "unreached".to_owned(),
                Outcome::Untranslatable(_) => "untranslatable".to_owned(),
                Outcome::Unreadable(_) => "unreadable".to_owned(),
                other => {
                    let (shown, said) = match other {
                        Outcome::Answer { want, got } => (format!("SQLite answers {want}\n    {name} answers {got}"), "disagree"),
                        Outcome::Refused(e) => (format!("{name} refused it: {e}"), "disagree"),
                        Outcome::Accepted => (format!("{name} ran what SQLite refuses"), "disagree"),
                        _ => (format!("{name} was stopped at the budget"), "past_budget"),
                    };
                    if let Err(e) = writeln!(out, "{rel}:{}: {}\n    {shown}", ran.record.line, ran.record.sql.replace('\n', " ")) {
                        write_error = Some(e.to_string());
                    }
                    said.to_owned()
                }
            };
            if let Some(t) = times.as_mut() {
                let line = serde_json::json!({ "file": rel, "line": ran.record.line, "outcome": outcome, "ns": ran.nanos });
                if let Err(e) = writeln!(t, "{line}") {
                    write_error = Some(e.to_string());
                }
            }
        })
        .map_err(|e| format!("{rel}: {e}"))?;
        if let Some(t) = times.as_mut() {
            // The file whole, its fresh database and fixture included: what it costs a run's budget;
            // and its failed settles, which fail a run as a disagreement does.
            let ns = u64::try_from(file_began.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let line = serde_json::json!({ "file": rel, "file_ns": ns, "required": required, "unsettled": counts.unsettled, "past_budget": counts.past_budget });
            writeln!(t, "{line}").map_err(|e| e.to_string())?;
        }
        files = files.saturating_add(1);
        total.agree = total.agree.saturating_add(counts.agree);
        total.disagree = total.disagree.saturating_add(counts.disagree);
        total.skipped = total.skipped.saturating_add(counts.skipped);
        total.unreached = total.unreached.saturating_add(counts.unreached);
        total.past_budget = total.past_budget.saturating_add(counts.past_budget);
        if required {
            required_past = required_past.saturating_add(counts.past_budget);
        }
        total.unsettled = total.unsettled.saturating_add(counts.unsettled);
        total.untranslatable = total.untranslatable.saturating_add(counts.untranslatable);
        total.unreadable = total.unreadable.saturating_add(counts.unreadable);
    }
    if let Some(mut t) = times {
        t.flush().map_err(|e| e.to_string())?;
    }
    if let Some(e) = write_error {
        return Err(e);
    }
    writeln!(
        out,
        "{}: {files} files in {:.1} s: {} records agree, {} disagree, {} untranslatable into its dialect, {} unreadable in its output, {} skipped (not SQLite's), {} not reached in the budget, {} stopped at the budget ({required_past} of them required), {} settles failed",
        a.engine,
        began.elapsed().as_secs_f64(),
        total.agree,
        total.disagree,
        total.untranslatable,
        total.unreadable,
        total.skipped,
        total.unreached,
        total.past_budget,
        total.unsettled
    )
    .map_err(|e| e.to_string())?;
    Ok(total.disagree == 0 && total.unsettled == 0 && required_past == 0)
}
