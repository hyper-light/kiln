#!/usr/bin/env bash
# Trial only: what's in RESULTS so far, committed whole to g11-memory-results2c (each part's push replaces the
# last with all there is).   push.sh RESULTS PART
R=$1 PART=$2
D=$RUNNER_TEMP/push-$PART; rm -rf "$D"; mkdir -p "$D" && cd "$D" || exit 0
git init -q . && cp -r "$R" results
git config user.name "kiln g11 lab" && git config user.email "g11-lab@users.noreply.github.com"
git add results && git commit -q -m "g11 lab results ($PART) for $GITHUB_SHA"
git push -q -f "https://x-access-token:${GH_TOKEN}@github.com/${GITHUB_REPOSITORY}.git" HEAD:refs/heads/g11-memory-results2c && echo "pushed $PART"
