#!/usr/bin/env bash
# Shards' own OSI reader on Devin's harness.json (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 3):
# shards-image's `osi::Config::parse` at Shards main ac87190d3, the reader `shards build` uses for a
# HARNESS directory's harness.json, which `--call=check` never opens (shards-check.sh's mutants).
# Run where shards-build.sh left its checkout; a scratch example there, removed after, calls it on
# the real file and on four mutants, each of which it must refuse.
# stdin: shards-check.sh's payload (`name base64` lines: Agentfile, harness.json, pin.json).
set -euo pipefail
ROOT="$HOME/.cache/s25-step3"
SRC="$ROOT/shards"
export CARGO_TARGET_DIR="$ROOT/target"
mkdir -p "$ROOT/in" "$ROOT/osi"
while read -r name b64; do
  [ -n "$name" ] || continue
  printf '%s' "$b64" | base64 -d > "$ROOT/in/$name"
done
git -C "$SRC" log -1 --format='shards %H'
sha256sum "$ROOT/in/harness.json"
H="$ROOT/in/harness.json"
cp "$H" "$ROOT/osi/harness.json"
jq '. + {"update": false}' "$H" > "$ROOT/osi/mutant-unknown-field.json"
jq '.run.env += ["SHARDS_DOMAIN=harness devin"]' "$H" > "$ROOT/osi/mutant-shards-domain.json"
jq '.schemaVersion = 2' "$H" > "$ROOT/osi/mutant-schema-2.json"
jq '.run.command = []' "$H" > "$ROOT/osi/mutant-empty-command.json"
mkdir -p "$SRC/crates/image/examples"
cat > "$SRC/crates/image/examples/s25_osi.rs" <<'EOF'
//! Scratch, not Shards' (kiln plan 01a1ce705e4d7008993f5c7291ef4476 step 3): each file named, read by
//! Shards' OSI config reader, accepted or refused in its words.
use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut out = std::io::stdout();
    let mut refused = 0;
    for path in std::env::args_os().skip(1) {
        let said = match std::fs::read(&path) {
            Ok(bytes) => match shards_image::osi::Config::parse(&bytes) {
                Ok(c) => format!("accepted: name {:?}, version {:?}, run {:?}", c.name, c.version, c.run),
                Err(e) => {
                    refused += 1;
                    format!("refused: {e}")
                }
            },
            Err(e) => format!("unreadable: {e}"),
        };
        let _ = writeln!(out, "{}: {said}", std::path::Path::new(&path).display());
    }
    let _ = writeln!(out, "refused {refused}");
    ExitCode::SUCCESS
}
EOF
cd "$SRC"
nice -n 10 cargo run -q -j 16 --locked -p shards-image --example s25_osi -- \
  "$ROOT/osi/harness.json" "$ROOT/osi/mutant-unknown-field.json" "$ROOT/osi/mutant-shards-domain.json" \
  "$ROOT/osi/mutant-schema-2.json" "$ROOT/osi/mutant-empty-command.json" 2>&1 | grep -v '^warning' || true
rm -f "$SRC/crates/image/examples/s25_osi.rs"
rmdir "$SRC/crates/image/examples" 2>/dev/null || true
git -C "$SRC" status --short | head -5
echo "shards checkout clean: $([ -z "$(git -C "$SRC" status --short)" ] && echo yes || echo no)"
