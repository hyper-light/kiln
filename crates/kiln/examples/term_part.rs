//! What kiln's server sends ahead of a shell's replay (W21-R02, `kiln::termpart`), for output kept in files:
//! for each `<name>.in` in a directory (a terminal's output from its start), the replay (`<name>.replay`) and
//! the part sent ahead of it (`<name>.part`), where one goes. `ci/ui/terminal_part.mjs` draws both in the
//! page's terminal and compares them.
//!
//! `cargo run --release -p kiln --example term_part -- <rows> <cols> <dir> [--unchecked]`
//!
//! With `--unchecked` a part kept back because its replay isn't twice its size is written too, and
//! `<name>.kept` says so (`gain`).

use kiln::termpart::{self, RESET};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, rows, cols, dir, rest @ ..] = args.as_slice() else { anyhow::bail!("term_part <rows> <cols> <dir> [--unchecked]") };
    let (rows, cols): (u16, u16) = (rows.parse()?, cols.parse()?);
    let unchecked = rest.iter().any(|a| a == "--unchecked");
    let mut inputs: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "in")).collect();
    inputs.sort();
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for path in inputs {
        let input = std::fs::read(&path)?;
        // As a session makes its replay (`kiln::termsess`): the reset, its output, the modes its input goes by.
        let mut session = vt100::Parser::new(rows, cols, 0);
        session.process(&input);
        let replay = [RESET, &input, &session.screen().input_mode_formatted()].concat();
        std::fs::write(path.with_extension("replay"), &replay)?;
        let output = RESET.len()..RESET.len().saturating_add(input.len());
        let why = if session.screen().alternate_screen() {
            "a full-screen program"
        } else if let Some(part) = termpart::newest_part(&replay, output.clone(), rows) {
            let sent = termpart::ahead(&replay, output, rows).is_some();
            if sent || unchecked {
                std::fs::write(path.with_extension("part"), &part)?;
            }
            if !sent && unchecked {
                std::fs::write(path.with_extension("kept"), "gain")?;
            }
            if sent { "sent" } else { "a cut, but no gain" }
        } else {
            "no cut"
        };
        *counts.entry(why).or_default() += 1;
    }
    println!("{}", counts.iter().map(|(why, n)| format!("{n} {why}")).collect::<Vec<_>>().join(", "));
    Ok(())
}
