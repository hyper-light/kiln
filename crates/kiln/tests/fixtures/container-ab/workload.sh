#!/bin/sh
# W9's A/B (crates/kiln/src/workspace/container_ab_tests.rs): one workload run in a container at $2 (a fresh folder on
# the route under test: the container's own disk, a bind of the machine's Slates mount, or the engine's NFS volume of
# Slates' export), its inputs read-only at /in. It says `SECS <wall seconds of the workload alone>` and `DIGEST <sha256>`
# of what it made, path by path (each file's bytes, each link's target, each directory's name), so the same workload's
# digests agree across routes exactly when they made the same tree. Then it removes what it made.
#   workload.sh git|cargo|npm|fs <folder>
set -eu
what=$1
dir=$2
# The bind and the export are two routes to one volume, and a run that failed left its folder: each run starts clean.
rm -rf "$dir" 2>/dev/null || true
mkdir -p "$dir"
cd "$dir"
export HOME=/tmp/ab-home npm_config_cache=/tmp/ab-npm GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
mkdir -p "$HOME"
now() { date +%s.%N; }
digest() {
  ( find "$@" \( -type f -o -type l -o -type d \) -print | LC_ALL=C sort | while IFS= read -r p; do
      if [ -L "$p" ]; then printf 'L %s -> %s\n' "$p" "$(readlink "$p")"
      elif [ -d "$p" ]; then printf 'D %s\n' "$p"
      else printf 'F %s %s\n' "$p" "$(sha256sum < "$p" | cut -d' ' -f1)"; fi
    done ) | sha256sum | cut -d' ' -f1
}
start=$(now)
case "$what" in
  git)
    git clone -q --no-local /in/repo.git repo
    git -C repo -c pack.threads=1 gc -q --aggressive
    git -C repo fsck --full --no-progress
    git -C repo rev-list --all > revs
    git -C repo cat-file --batch-all-objects --batch-check > objects
    end=$(now)
    sum=$(digest repo/src repo/README.md revs objects)
    ;;
  cargo)
    cp -R /in/crate crate
    cargo build -q --release --offline --manifest-path crate/Cargo.toml
    crate/target/release/ab > ran
    end=$(now)
    sum=$(digest crate/src crate/Cargo.toml crate/Cargo.lock crate/target/release/ab ran)
    ;;
  npm)
    cp -R /in/npm npm
    (cd npm/app && npm install -s --no-audit --no-fund --offline)
    node npm/app/index.js > ran
    end=$(now)
    sum=$(digest npm/app/node_modules npm/app/package-lock.json ran)
    ;;
  fs)
    # Hard links, renames, a delete while open: what a share or a client could get wrong.
    i=0; while [ $i -lt 200 ]; do
      printf 'file %s\n' $i > f$i; ln f$i l$i; rm f$i; mv l$i m$i; i=$((i+1)); done
    printf 'held\n' > held; exec 3< held; rm held; read -r back <&3; exec 3<&-
    printf '%s\n' "$back" > back
    silly=$(ls -a | grep -c '^\.nfs' || true)
    end=$(now)
    sum=$(digest $(ls | LC_ALL=C sort))
    echo "SILLY $silly"
    ;;
esac
echo "SECS $(echo "$end $start" | awk '{printf "%.3f", $1-$2}')"
echo "DIGEST $sum"
cd /
rm -rf "$dir" 2>/dev/null || true
