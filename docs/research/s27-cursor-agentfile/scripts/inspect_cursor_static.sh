#!/bin/bash
# Step 27 research, static half (any host): Cursor's installer, both Linux packages, their
# hashes, any published checksum or signature, what each package holds and what its ELF files
# need. Downloads go to a new directory under ~/.cache; this script lives elsewhere.
set -u
LLVM=/opt/homebrew/opt/llvm/bin
D="$HOME/.cache/kiln-step27-research/pkg-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$D" && cd "$D" || exit 1
echo "DIR $D"; date -u
UA='research-fetch'

echo "== installer"
mkdir -p installer
curl -fsSL -A "$UA" -D installer/headers -o installer/install.sh https://cursor.com/install || { echo "install fetch failed"; exit 1; }
shasum -a 256 installer/install.sh; wc -c < installer/install.sh
grep -n -E 'DOWNLOAD_URL=|FINAL_DIR=|ln -s|TEMP_EXTRACT_DIR=' installer/install.sh
VER=$(sed -n 's#.*downloads.cursor.com/lab/\([^/]*\)/.*#\1#p' installer/install.sh | head -1)
echo "VERSION $VER"
[ -n "$VER" ] || exit 1

echo "== integrity files at the version root"
for f in manifest.json checksums.txt SHA256SUMS sha256sums.txt SHA256SUMS.asc linux/x64/manifest.json; do
  echo "probe lab/$VER/$f -> $(curl -s -A "$UA" -o /dev/null -w '%{http_code}' -I "https://downloads.cursor.com/lab/$VER/$f")"
done

for A in ${ARCHS:-x64 arm64}; do
  U="https://downloads.cursor.com/lab/$VER/linux/$A/agent-cli-package.tar.gz"
  echo "== $A $U"
  mkdir -p "dl-$A"
  curl -fsSL -A "$UA" -D "dl-$A/headers" -o "dl-$A/agent-cli-package.tar.gz" "$U" || { echo "fetch $A failed"; continue; }
  tr -d '\r' < "dl-$A/headers" | grep -i -E '^(http|etag|content-length|last-modified|x-amz|x-goog|content-md5|content-type|server|via|x-cache|cf-cache|age|cache-control|digest|repr-digest)'
  shasum -a 256 "dl-$A/agent-cli-package.tar.gz"; stat -f '%z bytes' "dl-$A/agent-cli-package.tar.gz"
  for s in .sha256 .sha256sum .sha512 .sig .asc .sigstore .sigstore.json .minisig .pem; do
    echo "probe $s -> $(curl -s -A "$UA" -o /dev/null -w '%{http_code}' -I "$U$s")"
  done
  echo "-- a second download: $(curl -fsSL -A "$UA" "$U" | shasum -a 256)"
  mkdir -p "x-$A" && tar -xzf "dl-$A/agent-cli-package.tar.gz" -C "x-$A"
  echo "-- entries: $(tar -tzf "dl-$A/agent-cli-package.tar.gz" | wc -l); top level:"
  tar -tzf "dl-$A/agent-cli-package.tar.gz" | awk -F/ '{print $1"/"$2}' | sort -u | head -40
  echo "-- unpacked: $(du -sk "x-$A" | cut -f1) KiB"
  echo "-- executables, native modules and ELF files"
  find "x-$A" -type f | sort | while read -r f; do
    magic=$(head -c 4 "$f" | od -An -c | tr -d ' ')
    if [ "$magic" = '177ELF' ] || [ -x "$f" ]; then
      printf '%s %s: ' "$(stat -f %z "$f")" "${f#x-$A/}"; file -b "$f" | cut -c1-170
    fi
    if [ "$magic" = '177ELF' ]; then
      interp=$($LLVM/llvm-readelf -l "$f" 2>/dev/null | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')
      needed=$($LLVM/llvm-readelf -d "$f" 2>/dev/null | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' | tr '\n' ' ')
      glibc=$($LLVM/llvm-objdump -T "$f" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -u -t_ -k2 -V | tail -1)
      glibcxx=$($LLVM/llvm-objdump -T "$f" 2>/dev/null | grep -o 'GLIBCXX_[0-9.]*' | sort -u -t_ -k2 -V | tail -1)
      echo "    interp=${interp:-none} needed=[$needed] max=${glibc:-none} ${glibcxx}"
    fi
  done
done
echo "DONE $D"
