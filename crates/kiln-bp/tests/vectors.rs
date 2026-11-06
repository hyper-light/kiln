//! bp-g2 step 4: the shared test vectors (`ui/tests/bp-vectors.json`) are what this build reads
//! and writes, every kind and every codec refusal among them; `KILN_WRITE_BP=1` writes the file.
//! The TypeScript suite reads the same file (`ui/tests/bp-vectors.test.ts`), in Node, Chromium and
//! WebKit.

use kiln_bp::frames::KINDS;
use kiln_bp::vectors;
use kiln_bp::wire::{WHYS, Why};

#[test]
fn bp_g2_s4_the_vectors_are_current() -> Result<(), String> {
    let want = vectors::render();
    let path = vectors::path();
    if std::env::var("KILN_WRITE_BP").is_ok() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, &want).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(());
    }
    let have = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e} (run KILN_WRITE_BP=1 cargo test -p kiln-bp --test vectors)", path.display()))?;
    if have != want {
        let line = have.lines().zip(want.lines()).position(|(a, b)| a != b).unwrap_or(0);
        let name = want.lines().nth(line).unwrap_or("").chars().take(120).collect::<String>();
        return Err(format!("{} differs from what this build reads at line {} ({name}): run KILN_WRITE_BP=1 cargo test -p kiln-bp --test vectors", path.display(), line + 1));
    }
    Ok(())
}

#[test]
fn bp_g2_s4_every_kind_and_every_codec_refusal_has_a_vector() {
    let text = vectors::render();
    for k in KINDS {
        assert!(text.contains(&format!("\"kind\":\"{}\"", k.name())), "no vector of {k:?}");
    }
    let held = vectors::refusals_held();
    for w in WHYS {
        if w == Why::OutOfOrder {
            continue; // the connection's, held by tests/conn.rs
        }
        assert!(held.contains(&w), "no vector refused {}", w.name());
    }
}

/// Replays inputs of the fuzz run, printing each one and what reading it came to:
/// `KILN_BP_FUZZ_FROM=i KILN_BP_FUZZ_TO=j cargo test -p kiln-bp --test vectors replay -- --nocapture`
/// (what the TypeScript side prints when a block's digest differs).
#[test]
fn bp_g2_s4_replay() {
    let from: u64 = std::env::var("KILN_BP_FUZZ_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let to: u64 = std::env::var("KILN_BP_FUZZ_TO").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
    for i in from..to {
        let (made, msg) = kiln_bp::fuzz::input(vectors::FUZZ_SEED, i);
        let mut h = String::new();
        kiln_bp::wire::hex(&msg, &mut h);
        let mut o = String::new();
        kiln_bp::wire::hex(&kiln_bp::fuzz::outcome(&msg), &mut o);
        println!("input {i} ({made:?}): {h} -> {o}");
    }
}
