#!/bin/bash
# m5: do npm's @opencode/cli-linux-x64@2.2.7 and GitHub's opencode-linux-x64.tar.gz carry the same binary?
set -u
D=$HOME/.cache/s22-opencode-research/dl; mkdir -p "$D"
cd "$D" || exit 1
echo "M5 run $(date -u +%Y-%m-%dT%H:%M:%SZ)"
for p in cli-linux-x64 cli-linux-arm64-musl; do
  meta=$(curl -fsSL "https://registry.npmjs.org/@opencode%2f$p/2.2.7")
  tb=$(printf '%s' "$meta" | python3 -I -c 'import json,sys; m=json.load(sys.stdin); print(m["dist"]["tarball"])')
  integ=$(printf '%s' "$meta" | python3 -I -c 'import json,sys; m=json.load(sys.stdin); print(m["dist"]["integrity"])')
  f=npm-$p-2.2.7.tgz
  [ -f "$f" ] || curl -fsSL -o "$f" "$tb"
  got="sha512-$(openssl dgst -sha512 -binary "$f" | base64 | tr -d '\n')"
  echo "NPM @opencode/$p@2.2.7 tarball=$tb integrity=$([ "$got" = "$integ" ] && echo match || echo MISMATCH)"
  echo "NPM_MEMBERS $p $(tar -tzf "$f" | tr '\n' ' ')"
  rm -rf "npmx-$p" && mkdir "npmx-$p" && tar -xzf "$f" -C "npmx-$p"
  bin=$(find "npmx-$p" -type f -name opencode | head -1)
  echo "NPM_BINARY $p $bin sha256=$(shasum -a 256 "$bin" | cut -d' ' -f1)"
done
echo "GITHUB_BINARY x64 sha256=$(shasum -a 256 ex-x64/opencode | cut -d' ' -f1)"
echo "GITHUB_BINARY arm64-musl sha256=$(shasum -a 256 ex-arm64-musl/opencode | cut -d' ' -f1)"
