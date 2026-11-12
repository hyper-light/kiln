#!/usr/bin/env bash
# Shards' own check of Devin's Agentfile and its context (kiln plan 01a1ce705e4d7008993f5c7291ef4476,
# step 3), on a Linux runner, with the shards shards-build.sh built from Shards main ac87190d3.
# Reads the Agentfile, harness.json and pin.json from stdin (one `name base64` line each), makes the
# context as `kiln agentfiles context devin` will (each pinned bundle checked against pin.json's size
# and SHA-256, unpacked into devin-<arch>/ beside harness.json, nothing added), then asks:
#  1. kiln's static check, as crates/kiln/src/agentfiles/check.rs asks it (`--target
#     kiln:static-check`: parsed, directives and grants checked, no base resolved);
#  2. the whole check, per platform: the base resolved by digest (it opens no harness directory: the
#     last two mutants pass it; osi-check.sh holds harness.json to Shards' reader instead);
#  3. four mutants, each of which must be refused or warned: a CONNECT on a network never declared;
#     `ENV WINDSURF_API_KEY=x` (SecretsUsedInArgOrEnv); and in a hard-linked copy of the context, a
#     harness.json field Shards' OSI reader doesn't know, and devin-amd64/ missing.
# Shards' home is kept between runs (the base is pinned by digest), so a rerun pulls nothing again.
set -euo pipefail
ROOT="$HOME/.cache/s25-step3"
SHARDS="$ROOT/target/debug/shards"
CTX="$ROOT/ctx"
DL="$ROOT/dl"
rm -rf "$CTX" "$ROOT/docker-config" "$ROOT/mutants" "$ROOT/mctx-field" "$ROOT/mctx-nodir"
mkdir -p "$CTX" "$DL" "$ROOT/home" "$ROOT/docker-config" "$ROOT/mutants" "$ROOT/in"
while read -r name b64; do
  [ -n "$name" ] || continue
  printf '%s' "$b64" | base64 -d > "$ROOT/in/$name"
done
(cd "$ROOT/in" && sha256sum Agentfile harness.json pin.json)
"$SHARDS" --version
echo "host $(hostname) $(uname -srm) at $(date -u +%FT%TZ)"

cp "$ROOT/in/Agentfile" "$CTX/Agentfile"
for arch in amd64 arm64; do
  sel=".builds[] | select(.platform == \"linux/$arch\") | .files[0]"
  url=$(jq -r "$sel.url" "$ROOT/in/pin.json")
  sha=$(jq -r "$sel.sha256" "$ROOT/in/pin.json")
  size=$(jq -r "$sel.size" "$ROOT/in/pin.json")
  f="$DL/${url##*/}"
  if [ ! -f "$f" ]; then
    curl -fsSL --proto '=https' --tlsv1.2 -o "$f.part" "$url"
    mv "$f.part" "$f"
  fi
  echo "$sha  $f" | sha256sum -c -
  [ "$(stat -c %s "$f")" = "$size" ] && echo "size $size ok"
  mkdir "$CTX/devin-$arch"
  tar -xzf "$f" -C "$CTX/devin-$arch" --no-same-owner
  cp "$ROOT/in/harness.json" "$CTX/devin-$arch/harness.json"
  echo "devin-$arch: $(find "$CTX/devin-$arch" -type f | wc -l) files, $(find "$CTX/devin-$arch" -type d | wc -l) dirs, $(find "$CTX/devin-$arch" ! -type f ! -type d | wc -l) others"
  ls -l "$CTX/devin-$arch" "$CTX/devin-$arch/bin"
done

export SHARDS_HOME="$ROOT/home" DOCKER_CONFIG="$ROOT/docker-config"
check() { # check NAME ARGS…: one check, its answer and exit code
  local name=$1; shift
  echo "== $name: shards build --call=check,format=json --progress=quiet $*"
  set +e
  "$SHARDS" build --call=check,format=json --progress=quiet "$@" > "$ROOT/$name.json" 2> "$ROOT/$name.err"
  local code=$?
  set -e
  echo "exit $code"
  jq -c 'del(.sources[]?.data)' "$ROOT/$name.json" 2>/dev/null || cat "$ROOT/$name.json"
  sed 's/^/stderr: /' "$ROOT/$name.err" | tail -5
}
cd "$CTX"
check static --target kiln:static-check -f "$CTX/Agentfile" "$CTX"
check whole-amd64 --platform linux/amd64 -f "$CTX/Agentfile" "$CTX"
check whole-arm64 --platform linux/arm64 -f "$CTX/Agentfile" "$CTX"

sed 's/^CONNECT devin WITH devin ON provider$/CONNECT devin WITH devin ON nowhere/' "$CTX/Agentfile" > "$ROOT/mutants/Agentfile.connect"
printf 'ENV WINDSURF_API_KEY=x\n' | cat "$CTX/Agentfile" - > "$ROOT/mutants/Agentfile.secret"
diff "$CTX/Agentfile" "$ROOT/mutants/Agentfile.connect" || true
check mutant-connect --target kiln:static-check -f "$ROOT/mutants/Agentfile.connect" "$CTX"
check mutant-secret --platform linux/amd64 -f "$ROOT/mutants/Agentfile.secret" "$CTX"
cp -al "$CTX" "$ROOT/mctx-field"
jq '. + {"update": false}' "$CTX/devin-amd64/harness.json" > "$ROOT/mutants/harness.field.json"
mv "$ROOT/mutants/harness.field.json" "$ROOT/mctx-field/devin-amd64/harness.json"
cmp -s "$CTX/devin-amd64/harness.json" "$ROOT/mctx-field/devin-amd64/harness.json" && echo "the field mutant changed nothing"
check mutant-harness-field --platform linux/amd64 -f "$ROOT/mctx-field/Agentfile" "$ROOT/mctx-field"
cp -al "$CTX" "$ROOT/mctx-nodir"
rm -rf "$ROOT/mctx-nodir/devin-amd64"
[ -d "$CTX/devin-amd64/bin" ] && echo "the real context keeps devin-amd64/"
check mutant-no-dir --platform linux/amd64 -f "$ROOT/mctx-nodir/Agentfile" "$ROOT/mctx-nodir"
echo "context: $(du -sh "$CTX" | cut -f1); store: $(du -sh "$ROOT/home" | cut -f1)"
