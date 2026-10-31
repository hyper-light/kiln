//! A headroom pacer's guard (docs/headroom.md, "Failing safe"), as the kiln binary runs it: it says
//! it runs, keeps the last whole list its pacer sends, and once its input closes (its pacer ended,
//! however) starts everything on that list again and removes the pacer's record.

#![cfg(target_os = "linux")]

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::process::Stdio;

#[test]
fn the_guard_starts_again_what_its_pacer_stopped_once_the_pacer_is_gone() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // Groups that are only folders: the guard thaws one by writing its cgroup.freeze.
    let (listed, dropped) = (dir.path().join("listed"), dir.path().join("dropped"));
    for g in [&listed, &dropped] {
        std::fs::create_dir(g)?;
        std::fs::write(g.join("cgroup.freeze"), "1")?;
    }
    let record = dir.path().join("pacer.json");
    std::fs::write(&record, "{}")?;
    let mut guard = common::kiln().arg("headroom-guard").arg(&record).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
    let mut out = BufReader::new(guard.stdout.take().ok_or_else(|| anyhow::anyhow!("no output"))?);
    let mut line = String::new();
    out.read_line(&mut line)?;
    anyhow::ensure!(line.trim() == "ready", "it said {line:?}");
    let mut input = guard.stdin.take().ok_or_else(|| anyhow::anyhow!("no input"))?;
    // What the pacer may stop, as it grows and shrinks: only the last whole list counts, and a line
    // cut short (the pacer ended while telling it) doesn't.
    writeln!(input, "{}", serde_json::to_string(&[dropped.display().to_string()])?)?;
    writeln!(input, "{}", serde_json::to_string(&[listed.display().to_string()])?)?;
    write!(input, "[\"{}", dropped.display())?;
    drop(input);
    let status = guard.wait()?;
    anyhow::ensure!(status.success(), "{status}");
    anyhow::ensure!(std::fs::read_to_string(listed.join("cgroup.freeze"))? == "0", "the last list's group is still frozen");
    anyhow::ensure!(std::fs::read_to_string(dropped.join("cgroup.freeze"))? == "1", "a group off the last list was thawed");
    anyhow::ensure!(!record.exists(), "the record is left");
    Ok(())
}
