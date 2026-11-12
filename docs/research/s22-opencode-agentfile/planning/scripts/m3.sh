#!/bin/bash
# m3: what each opencode 2.2.7 Linux binary needs from its base: ELF class, interpreter, NEEDED, newest GLIBC/GLIBCXX symbol versions.
set -u
D=$HOME/.cache/s22-opencode-research/dl; mkdir -p "$D"
R=/opt/homebrew/opt/llvm/bin/llvm-readelf
cd "$D" || exit 1
echo "M3 run $(date -u +%Y-%m-%dT%H:%M:%SZ) $($R --version | grep -m1 -i version)"
for a in x64 arm64 x64-musl arm64-musl; do
  f=opencode-linux-$a.tar.gz; x=ex-$a
  rm -rf "$x" && mkdir "$x" && tar -xzf "$f" -C "$x"
  echo "== $f members: $(tar -tzf "$f" | tr '\n' ' ')"
  b=$x/opencode
  echo "BINARY $a sha256=$(shasum -a 256 "$b" | cut -d' ' -f1) size=$(stat -f %z "$b") mode=$(stat -f %Lp "$b")"
  echo "MACHINE $a $($R -h "$b" | awk -F: '/Machine:/{gsub(/^ +/,"",$2); print $2}')"
  echo "INTERP $a $($R -l "$b" | grep -o 'Requesting program interpreter: [^]]*' | sed 's/Requesting program interpreter: //')"
  echo "NEEDED $a $($R -d "$b" | grep NEEDED | sed 's/.*\[\(.*\)\]/\1/' | tr '\n' ' ')"
  V=$($R -V "$b" 2>/dev/null)
  echo "MAX_GLIBC $a $(printf '%s\n' "$V" | grep -o 'GLIBC_[0-9.]*' | sort -t_ -k2 -V | uniq | tail -1)"
  echo "MAX_GLIBCXX $a $(printf '%s\n' "$V" | grep -o 'GLIBCXX_[0-9.]*' | sort -t_ -k2 -V | uniq | tail -1)"
  echo "MAX_CXXABI $a $(printf '%s\n' "$V" | grep -o 'CXXABI_[0-9.]*' | sort -t_ -k2 -V | uniq | tail -1)"
  echo "MAX_GCC $a $(printf '%s\n' "$V" | grep -o 'GCC_[0-9.]*' | sort -t_ -k2 -V | uniq | tail -1)"
done
