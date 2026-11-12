#!/usr/bin/env bash
# Step 5's evidence, brought home byte for byte (plan 01a1cf48e8157494ac9a51f7a564f526). For each FILE, the runner
# prints its SHA-256 and its base64 through `kiln run`. This side decodes it into DEST/NAME and refuses a copy whose
# SHA-256 differs from the runner's.
#
#   fetch.sh RUNNER REMOTE_FILE DEST/NAME [REMOTE_FILE DEST/NAME]...
set -euo pipefail
runner=${1:?runner}
shift
while [ $# -ge 2 ]; do
    remote=$1 local=$2
    shift 2
    raw=$(mktemp "${TMPDIR:-/tmp}/s19-step5-fetch.XXXXXX")
    kiln run -q --no-sync --on "$runner" -- "f=$remote; if command -v sha256sum >/dev/null; then sha256sum \"\$f\"; else shasum -a 256 \"\$f\"; fi; base64 <\"\$f\" | tr -d '\n'; echo" >"$raw"
    python3 -I - "$raw" "$local" <<'PY'
import base64, hashlib, os, sys
raw, out = sys.argv[1:]
lines = open(raw).read().split("\n")
want = lines[0].split()[0]
data = base64.b64decode(lines[1])
got = hashlib.sha256(data).hexdigest()
if got != want:
    sys.exit(f"{out}: sha256 {got}, the runner's {want}")
os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
open(out, "wb").write(data)
print(f"{out}: {len(data)} bytes, sha256 {got}, equal to the runner's")
PY
    rm -f "$raw"
done
