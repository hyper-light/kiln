#!/bin/bash
# What each `harness` frame of w21_r15_events_ship_and_api_budgets carries (crates/kiln/tests/wire_budgets.rs,
# EVENTS_HARNESS_*): the test's events reader made to write each harness frame's payload to a file (put back on exit),
# built, run once, and the frames read by harness-frames.py (each field's size, and which fields changed between one
# agent's consecutive frames). Run from a kiln checkout's root; CARGO_TARGET_DIR honoured. Usage: harness-frames.sh
set -u
T=${CARGO_TARGET_DIR:-$PWD/target}
f=crates/kiln/tests/wire_budgets.rs
[ -z "$(git status --porcelain -- "$f")" ] || { echo "$f has changes: stop"; exit 2; }
trap 'git checkout -q -- "$f"' EXIT
python3 - "$f" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
s = p.read_text()
old = "            if tx.send((t, m.wire, m.payload.len(), stream.push(&m.payload), Instant::now(), ship)).is_err() {"
new = """            if t == "harness" && let Some(p) = std::env::var_os("WIRE_DUMP") && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = std::io::Write::write_all(&mut f, &m.payload);
                let _ = std::io::Write::write_all(&mut f, format!("\\n#wire {}\\n", m.wire).as_bytes());
            }
""" + old
assert s.count(old) == 1, "the events reader moved: update this harness"
p.write_text(s.replace(old, new))
PY
cargo test --locked -p kiln --test wire_budgets --no-run >/dev/null 2>&1 || { echo "build failed"; exit 1; }
bin=$(ls -t "$T"/debug/deps/wire_budgets-* | grep -v '\.d$' | head -1)
out=$(mktemp -d)
echo "kiln $(git rev-parse --short=9 HEAD), $(uname -sm), $(date -u +%FT%TZ)"
WIRE_DUMP=$out/frames "$bin" w21_r15_events_ship_and_api_budgets --nocapture --test-threads 1 2>&1 | grep -aE '^events: harness|over [0-9.]+ s$'
python3 -I "$(dirname "$0")/harness-frames.py" "$out/frames"
rm -rf "$out"
