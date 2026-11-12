#!/usr/bin/env bash
# Guard-removal mutations of .kiln/images/harness-cursor/pin.sh (kiln plan
# 01a1ce705e5c7504b88732f443d99288, step 1; Shards R10): each mutant takes one of pin.sh's checks
# out, or gets one of its numbers wrong, and pin_test.sh must then fail, at the case that check
# or number is for.
#
#   mutate.sh [PIN.SH]   PIN.SH: the pin.sh to mutate, by default this checkout's
#
# Needs what pin_test.sh needs.
set -euo pipefail
export LC_ALL=C

here=$(cd "$(dirname "$0")" && pwd)
pin=${1:-$here/../../../../.kiln/images/harness-cursor/pin.sh}
[ -f "$pin" ] || { echo "mutate.sh: no pin.sh at $pin" >&2; exit 64; }
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
T=$(mktemp -d "$cache/cursor-pin-mutate.XXXXXX")
trap 'rm -rf "$T"' EXIT

IFS= read -r -d '' MUTATE << 'EOF' || true
import os, sys

pin, out = sys.argv[1:3]
with open(pin) as f:
    text = f.read()
# Its name, the case of pin_test.sh it must fail, the text it replaces (once in pin.sh), and with what.
MUTATIONS = [
    ("https-only", "good, over plain HTTP", "curl --proto '=https' ", "curl "),
    ("https-only-redirect", "installer-redirect-http", "curl --proto '=https' ", "curl "),
    ("status-200", "sidecar-404", '[ "$code" = 200 ] ||', "true ||"),
    ("version-charset", "good (argument 2026.11.11/x)", "^[0-9A-Za-z._-]+$", "^.*$"),
    ("version-dot-segments", "installer-dotdot", " && $1 != . && $1 != .. ]]", " ]]"),
    ("one-download-url", "two-download-urls", "if len(lines) > 1:", "if False:"),
    ("download-url-host", "other-host", r"downloads\.cursor\.com", "[^/]+"),
    ("sidecar-lowercase", "sidecar-uppercase", 'rb"([0-9a-f]{64})  "', 'rb"([0-9a-fA-F]{64})  "'),
    ("sidecar-names-package", "sidecar-other-file", 'rb"([0-9a-f]{64})  " + re.escape(PACKAGE.encode())', 'rb"([0-9a-f]{64})  " + rb"\\S+"'),
    ("sidecar-one-line", "sidecar-two-lines", 'rb"\\n", data)', 'rb"\\n", data[:LINE])'),
    ("digest", "sidecar-digit", "if got != want:", "if False:"),
    ("kinds", "symlink", "if m.type not in (tarfile.REGTYPE, tarfile.AREGTYPE, tarfile.DIRTYPE):", "if False:"),
    ("absolute", "absolute", 'if name.startswith("/"):', "if False:"),
    ("dotdot", "dotdot-inside", 'if ".." in parts:', "if False:"),
    ("empty-or-dot", "dot-component", 'if "" in parts or "." in parts:', "if False:"),
    ("under-top", "outside", "if parts[0] != TOP:", "if False:"),
    ("top-is-directory", "top-file", "if len(parts) == 1 and not is_dir:", "if False:"),
    ("harness-json", "root-harness-json", "if name == HARNESS_JSON:", "if False:"),
    ("set-id-sticky", "setuid", "if m.mode & (stat.S_ISUID | stat.S_ISGID | stat.S_ISVTX):", "if False:"),
    ("parent-is-file", "parent-file", 'if made == "file":', "if False:"),
    ("twice", "duplicate", 'if made in ("file", "dir"):', "if False:"),
    ("folded-names", "case-fold", "        claim(name, folded)\n", "        pass\n"),
    ("agent-present", "no-agent", "if agent is None:", "if False:"),
    ("agent-executable", "agent-not-exec", "agent = m.isreg() and bool(m.mode & stat.S_IXUSR)", "agent = True"),
    ("past-the-end", "garbage-header", 'if chunk.strip(b"\\0"):', "if False:"),
    ("entries", "good", "print(size, files + dirs, unpacked, files, dirs)", "print(size, files, unpacked, files, dirs)"),
    ("unpacked-bytes", "good", "unpacked += m.size", "unpacked += 0"),
    ("platforms", "good", "PLATFORMS=(linux/amd64:x64 linux/arm64:arm64)", "PLATFORMS=(linux/amd64:arm64 linux/arm64:x64)"),
    ("temporary-removed", "good", """trap 'rm -rf "$T" ${tmp:+"$tmp"}' EXIT""", """trap 'rm -rf ${tmp:+"$tmp"}' EXIT"""),
]
for name, case, old, new in MUTATIONS:
    if text.count(old) != 1:
        sys.exit(f"mutate.sh: {name}'s text is in pin.sh {text.count(old)} times, not once: {old!r}")
    with open(os.path.join(out, name), "w") as f:
        f.write(text.replace(old, new))
    print(f"{name}\t{case}")
EOF

python3 -I -c "$MUTATE" "$pin" "$T" > "$T/list"
killed=0 survived=0
while IFS=$'\t' read -r name case; do
  status=0
  bash "$here/pin_test.sh" "$T/$name" > "$T/$name.log" 2>&1 || status=$?
  line=$(grep -F -m 1 -- "not ok - $case: " "$T/$name.log" || true)
  if [ "$status" -ne 0 ] && [ -n "$line" ]; then
    killed=$((killed + 1))
    printf 'killed - %s (%s of its cases failed): %s\n' "$name" "$(grep -c '^not ok - ' "$T/$name.log")" "$line"
  else
    survived=$((survived + 1))
    printf 'SURVIVED - %s: pin_test.sh exited %s, its case %s %s\n' "$name" "$status" "$case" "$([ -n "$line" ] && echo failed || echo passed)"
  fi
done < "$T/list"

if [ "$survived" -eq 0 ]; then
  echo "all $killed mutants killed"
else
  echo "$survived of $((killed + survived)) mutants survived"
  exit 1
fi
