#!/usr/bin/env bash
# kiln's server image's build context, by Docker itself (kiln plan 01a1ce705e4d7008993f5c7291ef4476
# step 3, research f25): a context of kiln's .dockerignore and .kiln/images/harness-devin/ (plus a
# decoy x.log there, which .dockerignore's `**/*.log` must drop), built with the build stage's own
# line, `COPY .kiln/images/ .kiln/images/`, onto scratch. What lands must be the five files, byte for
# byte, and no decoy. BuildKit's local output where the CLI has it; else a scratch image of this
# script's own name, exported, then removed (it creates and removes only its own names).
# stdin: `path base64` lines, each a file of the context.
set -euo pipefail
ROOT="$HOME/.cache/s25-step3"
CTX="$ROOT/docker-ctx"
OUT="$ROOT/docker-out"
NAME=s25-step3-context-check
rm -rf "$CTX" "$OUT"
mkdir -p "$CTX" "$OUT"
while read -r path b64; do
  [ -n "$path" ] || continue
  mkdir -p "$CTX/$(dirname "$path")"
  printf '%s' "$b64" | base64 -d > "$CTX/$path"
done
printf 'a decoy .dockerignore drops\n' > "$CTX/.kiln/images/harness-devin/x.log"
echo "context:"
(cd "$CTX" && find . -type f | sort && sha256sum .dockerignore)
docker version --format 'docker {{.Server.Version}}'
printf 'FROM scratch\nCOPY .kiln/images/ .kiln/images/\n' > "$ROOT/Dockerfile.context"
if DOCKER_BUILDKIT=1 docker build -q -f "$ROOT/Dockerfile.context" --output "type=local,dest=$OUT" "$CTX" > "$ROOT/docker-build.log" 2>&1; then
  echo "built with BuildKit's local output"
else
  sed 's/^/buildkit: /' "$ROOT/docker-build.log" | tail -3
  docker build -q -t "$NAME:latest" -f "$ROOT/Dockerfile.context" "$CTX" > /dev/null
  cid=$(docker create "$NAME:latest" /none)
  docker export "$cid" | tar -x -C "$OUT"
  docker rm "$cid" > /dev/null
  docker rmi "$NAME:latest" > /dev/null
  echo "built as $NAME, exported, removed"
fi
echo "landed:"
(cd "$OUT" && find . -type f | sort)
for f in Agentfile harness.json pin.json pin.sh pin-tests.sh; do
  if cmp -s "$CTX/.kiln/images/harness-devin/$f" "$OUT/.kiln/images/harness-devin/$f"; then echo "$f: as in the context"; else echo "$f: MISSING or different"; fi
done
[ -e "$OUT/.kiln/images/harness-devin/x.log" ] && echo "x.log: LANDED (.dockerignore not applied)" || echo "x.log: dropped by .dockerignore"
docker image ls -q "$NAME" | wc -l | sed 's/^/images of its name left: /'
rm -rf "$CTX" "$OUT"
