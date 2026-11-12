#!/usr/bin/env bash
# Plan step 26: pin.sh's record, as step 1 made it. harness.tar is a git archive of the kiln
# worktree's .kiln/images/harness-antigravity at a commit; its commit id is read back from the
# archive. Run there:
#   - pin_test.sh;
#   - pin.sh against Google's updater;
#   - pin.sh asked for OLD, a version the manifests no longer serve;
#   - each pinned URL downloaded again apart from pin.sh.
# The record is written to $W/pin.log and the pin to $W/.../pin.json.
#   pin-record.sh OLD
set -uo pipefail
old=$1
here=$(pwd)
W=$(mktemp -d "$HOME/.cache/s26-pin-record.XXXXXX") || exit 1
L="$W/pin.log"
c=$(git get-tar-commit-id < "$here/harness.tar") || exit 1
tar -xf "$here/harness.tar" -C "$W" || exit 1
H="$W/.kiln/images/harness-antigravity"
cd "$H" || exit 1
{
echo "# pin.sh's record (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 26), $(date -u +%FT%TZ), on a Linux runner ($(uname -sr) $(uname -m); $(grep -E '^PRETTY_NAME=' /etc/os-release))"
echo "# run from the kiln worktree s26-agy-agentfile's commit $c: git archive of .kiln/images/harness-antigravity at that commit, its commit id read back from the archive (git get-tar-commit-id)"
echo "# the scripts as run: $(sha256sum pin.sh | tr -s ' '), $(sha256sum pin_test.sh | tr -s ' ')"
echo '$ bash pin_test.sh'
bash pin_test.sh; echo "(exit $?)"
echo '$ bash pin.sh'
bash pin.sh; echo "(exit $?)"
echo "\$ PIN_OUT=~/.cache/s26-asked.json bash pin.sh $old    # a version the manifests do not serve"
PIN_OUT="$HOME/.cache/s26-asked.json" bash pin.sh "$old"; rc=$?
if [ -e "$HOME/.cache/s26-asked.json" ]; then echo "(exit $rc; a pin was written)"; rm -f "$HOME/.cache/s26-asked.json"; else echo "(exit $rc; no pin written)"; fi
echo '$ each pinned URL downloaded again, apart from pin.sh, against pin.json'
python3 -I -c 'import json, sys
for b in json.load(open(sys.argv[1]))["builds"]:
    f = b["files"][0]
    print(f["url"], f["sha256"], f["sha512"], f["size"])' pin.json > "$W/urls"
n=0; ok=0
while read -r url s256 s512 size; do
  n=$((n + 1)); a="$W/re.tar.gz"
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL -o "$a" "$url" || { echo "fetch failed: $url"; continue; }
  if [ "$(sha256sum "$a" | cut -d' ' -f1)" = "$s256" ] && [ "$(sha512sum "$a" | cut -d' ' -f1)" = "$s512" ] && [ "$(stat -c %s "$a")" = "$size" ]; then
    echo "match: $url ($size bytes)"; ok=$((ok + 1))
  else
    echo "DIFFERS: $url"
  fi
  rm -f "$a"
done < "$W/urls"
echo "archives whose SHA-256, SHA-512 and size equal pin.json's: $ok of $n"
echo "# pin.json as written: $(sha256sum pin.json | cut -d' ' -f1)"
} > "$L" 2>&1
cat "$L"
echo "RECORD_DIR $W"
