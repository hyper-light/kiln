#!/usr/bin/env bash
# remote.sh SCRIPT OUT PAYLOAD [kiln run flags…]: runs SCRIPT on a runner through `kiln run --no-sync
# -i`, PAYLOAD (a file, or /dev/null) as its stdin, its stdout and stderr into OUT, then "exit N".
# One stream reaches the runner's `bash -s`: SCRIPT written out by one heredoc, then run with PAYLOAD
# as another. Retried 20 s apart, up to 30 times, while the script never started: kiln's server
# answered nothing ("did not answer in time"), or the placement went away before the run started.
set -u
script=$1 out=$2 payload=$3
shift 3
name=$(basename "$script")
wrapped=$(mktemp "${TMPDIR:-/tmp}/remote.XXXXXX")
{
  echo 'echo "s25-step3: started on $(hostname)"'
  echo 'mkdir -p "$HOME/.cache/s25-step3"'
  echo "cat > \"\$HOME/.cache/s25-step3/$name\" <<'SCRIPT_EOF'"
  cat "$script"
  echo 'SCRIPT_EOF'
  echo "bash \"\$HOME/.cache/s25-step3/$name\" <<'PAYLOAD_EOF'"
  cat "$payload"
  echo 'PAYLOAD_EOF'
} > "$wrapped"
for try in $(seq 1 30); do
  kiln run --no-sync -i "$@" -- 'bash -s' < "$wrapped" > "$out" 2>&1
  code=$?
  if [ "$code" -ne 0 ] && ! grep -q '^s25-step3: started on ' "$out" && grep -q -E 'did not answer|restarted|interrupted|placement went away' "$out"; then
    sleep 20
    continue
  fi
  echo "exit $code (try $try)" >> "$out"
  rm -f "$wrapped"
  exit 0
done
echo "exit $code (gave up after 30 tries)" >> "$out"
rm -f "$wrapped"
