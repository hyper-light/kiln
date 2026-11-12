#!/usr/bin/env bash
# Tests pin.sh against fixtures standing in for Google's updater and storage (kiln plan
# 01a1cd8e862e7169a074db3d5fa33f3d, step 26): the right archives pin, with their digests and
# sizes; each broken input is refused with the failed check named, and pin.json is left as it
# was. Needs what pin.sh needs. Prints one line per case and exits non-zero if any case failed.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
pin=$here/pin.sh
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
W=$(mktemp -d "$cache/agy-pin-test.XXXXXX")
trap 'rm -rf "$W"' EXIT

# Fixture writer: DIR CASE lays out DIR/updater and DIR/storage as pin.sh's PIN_FIXTURE reads
# them, broken as CASE says, and prints the expected pin's facts as JSON on stdout.
cat > "$W/fixture.py" <<'PY'
import hashlib, io, json, os, sys, tarfile

root, case = sys.argv[1], sys.argv[2]
version, eid = "1.5.6", "6212266538958848"
storage = "https://storage.googleapis.com/antigravity-public/antigravity-cli/"
builds = [("linux/amd64", "linux_amd64_musl", "linux-x64-musl/cli_linux_x64_musl.tar.gz"),
          ("linux/arm64", "linux_arm64_musl", "linux-arm-musl/cli_linux_arm64_musl.tar.gz")]

def archive(entries):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for name, data, kind in entries:
            info = tarfile.TarInfo(name)
            info.mode = 0o755
            if kind == "symlink":
                info.type, info.linkname = tarfile.SYMTYPE, "/bin/sh"
                tf.addfile(info)
            else:
                info.size = len(data)
                tf.addfile(info, io.BytesIO(data))
    return buf.getvalue()

def put(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)

releases = [{"version": version, "execution_id": eid}, {"version": "1.5.5", "execution_id": "6740358537805824"}]
if case == "releases-newer":
    releases.insert(0, {"version": "1.5.7", "execution_id": "7000000000000000"})
put(f"{root}/updater/releases", json.dumps(releases).encode())

expect = []
for i, (platform, name, rel) in enumerate(builds):
    member = f"#!/bin/sh\necho {version} {platform}\n".encode()
    entries = [("antigravity", member, "file")]
    first = i == 0
    if first and case == "second-entry":
        entries.append(("README", b"extra\n", "file"))
    if first and case == "symlink":
        entries = [("antigravity", b"", "symlink")]
    if first and case == "renamed":
        entries = [("agy", member, "file")]
    data = archive(entries)
    sha512 = hashlib.sha512(data).hexdigest()
    url = f"{storage}{version}-{eid}/{rel}"
    if first and case == "other-eid":
        url = f"{storage}{version}-1/{rel}"
    if first and case == "http-url":
        url = url.replace("https://", "http://")
    stored = data
    if first and case == "byte-flipped":
        stored = bytearray(data)
        stored[len(stored) // 2] ^= 0x01
        stored = bytes(stored)
    manifest = {"version": version, "url": url, "sha512": sha512}
    if i == 1 and case == "versions-differ":
        manifest["version"] = "1.5.5"
    if first and case == "bad-sha512":
        manifest["sha512"] = "z" * 128
    put(f"{root}/updater/manifests/{name}.json", json.dumps(manifest, indent=4).encode())
    put(f"{root}/storage/{url.split('/antigravity-cli/', 1)[1]}", stored)
    expect.append({"platform": platform, "url": url, "sha256": hashlib.sha256(data).hexdigest(), "sha512": sha512,
                   "size": len(data), "member_sha256": hashlib.sha256(member).hexdigest(), "member_size": len(member)})
print(json.dumps({"version": version, "execution_id": eid, "builds": expect}))
PY

# The pin a fixture's good case must give: every field the fixture knows, and the shape.
cat > "$W/check.py" <<'PY'
import json, sys
pin, want = json.load(open(sys.argv[1])), json.loads(sys.argv[2])
problems = []
def eq(what, got, exp):
    if got != exp:
        problems.append(f"{what}: {got!r} != {exp!r}")
eq("harness", pin.get("harness"), "antigravity")
eq("version", pin.get("version"), want["version"])
eq("execution_id", pin.get("execution_id"), want["execution_id"])
eq("base", pin.get("base"), "buildpack-deps:trixie-scm@sha256:4f34e1d673fdb65371de9f88bdd32c4e30152fe692b35604aef5c0b3c0e6da10")
eq("smoke", pin.get("smoke"), {"program": "bin/agy", "args": ["--version"], "stdout": want["version"]})
v = pin.get("verified", {})
eq("manifests", [m.get("url", "").rsplit("/", 1)[-1] for m in v.get("manifests", [])], ["linux_amd64_musl.json", "linux_arm64_musl.json"])
eq("read_at present", bool(v.get("read_at")), True)
builds = pin.get("builds", [])
eq("platforms", [b.get("platform") for b in builds], [b["platform"] for b in want["builds"]])
for b, w in zip(builds, want["builds"]):
    files = b.get("files", [])
    eq(f"{w['platform']} files", len(files), 1)
    if not files:
        continue
    f = files[0]
    for key in ("url", "sha256", "sha512", "size"):
        eq(f"{w['platform']} {key}", f.get(key), w[key])
    eq(f"{w['platform']} path", f.get("path"), "bin/agy")
    eq(f"{w['platform']} mode", f.get("mode"), "0755")
    eq(f"{w['platform']} unpack", f.get("unpack"), {"format": "tar.gz", "member": "antigravity", "sha256": w["member_sha256"], "size": w["member_size"]})
if problems:
    sys.exit("; ".join(problems))
PY

failed=0
# run NAME EXPECTED-STATUS EXPECTED-WORDS-ON-STDERR [ARGS...]
run() {
  local name=$1 want=$2 words=$3; shift 3
  local d=$W/$name
  mkdir -p "$d"
  local facts
  facts=$(python3 -I "$W/fixture.py" "$d/fx" "$name")
  printf 'sentinel\n' > "$d/pin.json"
  local status=0
  PIN_FIXTURE=$d/fx PIN_OUT=$d/pin.json XDG_CACHE_HOME=$d/cache bash "$pin" "$@" > "$d/out" 2> "$d/err" || status=$?
  local why=""
  [ "$status" = "$want" ] || why="exit $status, not $want ($(tr '\n' ' ' < "$d/err"))"
  if [ -z "$why" ] && [ -n "$words" ] && ! grep -q -F -- "$words" "$d/err"; then
    why="stderr does not say \"$words\": $(tr '\n' ' ' < "$d/err")"
  fi
  if [ -z "$why" ] && [ "$want" = 0 ]; then
    why=$(python3 -I "$W/check.py" "$d/pin.json" "$facts" 2>&1) || true
  elif [ -z "$why" ] && [ "$(cat "$d/pin.json")" != sentinel ]; then
    why="pin.json changed on a refusal"
  fi
  if [ -z "$why" ] && [ -n "$(find "$d/cache" -mindepth 1 2>/dev/null)" ]; then
    why="left files in its cache: $(find "$d/cache" -mindepth 1 | head -3 | tr '\n' ' ')"
  fi
  if [ -z "$why" ]; then
    printf 'ok - %s\n' "$name"
  else
    printf 'not ok - %s: %s\n' "$name" "$why"
    failed=$((failed + 1))
  fi
}

run good 0 ""
run good-asked-current 0 "" 1.5.6
run asked-other-version 2 "agy 1.5.5 has no publisher checksum" 1.5.5
run byte-flipped 3 "is not the manifest's"
run versions-differ 3 "manifest linux_arm64_musl names 1.5.5, but /releases' newest is 1.5.6"
run releases-newer 3 "names 1.5.6, but /releases' newest is 1.5.7"
run other-eid 3 "is not under https://storage.googleapis.com/antigravity-public/antigravity-cli/1.5.6-6212266538958848/"
run http-url 3 "is not under"
run bad-sha512 3 "sha512 is not 128 hex digits"
run second-entry 3 "it holds more than one entry"
run symlink 3 "its entry antigravity is not a regular file"
run renamed 3 "its entry is 'agy', not antigravity"

if [ "$failed" -ne 0 ]; then
  printf '%s case(s) failed\n' "$failed"
  exit 1
fi
printf 'all cases passed\n'
