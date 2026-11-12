#!/usr/bin/env bash
# Step 5 of kiln plan 01a1ce705e62732b83f16295d74ee263 (R09 and R10 evidence for research f17 and
# f18): does pi fetch ripgrep and fd from GitHub at run time when neither is on its PATH, and do the
# pinned bin/rg and bin/fd of harness-pi's context stop it? Runs outside a VM, on a Linux runner.
#
#   ab.sh PIN_JSON WORK_DIR
#
# Builds harness-pi's linux/amd64 harness directory from pin.json as kiln's fetcher will (each file
# checked by SHA-256 and size; pi's tree with pi/ stripped; rg and fd at bin/). It then sends one RPC
# prompt through a local mock provider whose first answer asks for grep and find, in five cases:
#   A-offline   PATH without rg or fd, no network: pi's own words for the download it can't make
#   A-online    the same, with the host's network: pi downloads rg and fd into its config's bin/
#   B-offline   PATH with the harness's bin/ first, no network: both tools run, nothing downloaded
#   B-online    the same, with the network: still nothing downloaded
#   B-norg      mutation: B-online with bin/rg removed: the download comes back, for rg alone
# PATH otherwise holds only tar and gzip (pi unpacks its downloads with tar), so no rg or fd of the
# host's can stand in. "No network" is a user and network namespace (unshare -rn) with loopback up.
# Exits non-zero unless every case's expectation holds.
set -u
here=$(cd "$(dirname "$0")" && pwd)
self=$here/$(basename "$0")

if [[ ${1:-} == __case ]]; then # one case, in its namespace: the mock, models.json, one prompt
    d=$2 path=$3 pi=$4 proj=$5
    if [[ ${OFFLINE:-} == 1 ]]; then
        ip link set lo up || exit 1
    fi
    python3 -I "$here/mock_provider.py" "$d/port" "$d/mock.log" &
    mock=$!
    for _ in $(seq 1 100); do
        [[ -s $d/port ]] && break
        sleep 0.1
    done
    mkdir -p "$d/agent"
    printf '{"providers":{"mock":{"baseUrl":"http://127.0.0.1:%s/v1","api":"openai-completions","apiKey":"mock-key-not-a-secret","models":[{"id":"mock-1"}]}}}\n' "$(cat "$d/port")" >"$d/agent/models.json"
    python3 -I "$here/drive.py" "$d" "$proj" "$pi" 120 -- PATH="$path" HOME="$d/home" PI_CODING_AGENT_DIR="$d/agent" PI_SKIP_VERSION_CHECK=1
    rc=$?
    kill "$mock" 2>/dev/null
    exit $rc
fi

pin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
W=$2
mkdir -p "$W" && cd "$W" || exit 1
exec > >(tee "$W/ab.log") 2>&1
echo "# step 5 A/B on $(hostname) ($(uname -srm)), $(ldd --version | head -1), $(date -u +%FT%TZ)"
echo "# pin.json sha256 $(sha256sum "$pin" | cut -d' ' -f1); ab.sh $(sha256sum "$self" | cut -d' ' -f1); drive.py $(sha256sum "$here/drive.py" | cut -d' ' -f1); mock_provider.py $(sha256sum "$here/mock_provider.py" | cut -d' ' -f1)"
for c in curl tar gzip python3 unshare ip sha256sum; do
    command -v "$c" >/dev/null || { echo "needs $c"; exit 1; }
done

echo "## harness-pi's linux/amd64 directory, from pin.json"
python3 -I -c '
import json, sys
p = json.load(open(sys.argv[1]))
for b in p["builds"]:
    if b["platform"] == "linux/amd64":
        for f in b["files"]:
            u = f.get("unpack") or {}
            print("|".join([f["url"], f["sha256"], str(f["size"]), f["path"], str(u.get("strip", "")), u.get("member", ""), f.get("mode", "")]))
' "$pin" >files.txt
mkdir -p dl ctx
# '|' rather than a tab: read collapses runs of whitespace separators, so an empty field would vanish.
while IFS='|' read -r url sha size path strip member mode; do
    f=dl/$(basename "$url")
    curl -fsSL -o "$f" "$url" || { echo "could not fetch $url"; exit 1; }
    [[ $(sha256sum "$f" | cut -d' ' -f1) == "$sha" && $(wc -c <"$f" | tr -d ' ') == "$size" ]] || { echo "MISMATCH: $url"; exit 1; }
    if [[ -n $strip ]]; then
        mkdir -p "ctx/$path" && tar -xzf "$f" --strip-components="$strip" -C "ctx/$path" || exit 1
    else
        mkdir -p "ctx/$(dirname "$path")" && tar -xzOf "$f" "$member" >"ctx/$path" && chmod "$mode" "ctx/$path" || exit 1
    fi
    echo "checked and laid out: $(basename "$url") -> $path"
done <files.txt
echo "pi --version: $(ctx/pi --version); bin: $(ls ctx/bin | tr '\n' ' ')"
mkdir -p proj sysbin
echo "the needle is here" >proj/notes.txt
ln -sf "$(command -v tar)" sysbin/tar
ln -sf "$(command -v gzip)" sysbin/gzip
cp -a ctx ctx-norg && rm ctx-norg/bin/rg

run_case() { # NAME NET(on|off) PATH PI_DIR
    local d=$W/$1
    mkdir -p "$d/home"
    echo "## $1: network $2, PATH=$3"
    if [[ $2 == off ]]; then
        OFFLINE=1 unshare -rn bash "$self" __case "$d" "$3" "$4/pi" "$W/proj"
    else
        bash "$self" __case "$d" "$3" "$4/pi" "$W/proj"
    fi
    echo "driver exit: $?"
    if [[ -d $d/agent/bin ]]; then
        echo "config bin/: $(ls "$d/agent/bin" | tr '\n' ' ')"
    else
        echo "config bin/: absent"
    fi
    for t in rg fd; do
        [[ -x $d/agent/bin/$t ]] && echo "downloaded $t: $("$d/agent/bin/$t" --version | head -1)"
    done
    echo "pi's own words: $(grep -h -o -i -E '[^"]*(could not be downloaded|not found\. [^"]*|Downloading[^"]*|installed to [^"]*)' "$d/events.jsonl" "$d/stderr.txt" 2>/dev/null | sort -u | head -6 | tr '\n' '|')"
    echo "mock requests: $(wc -l <"$d/mock.log")"
}
run_case A-offline off "$W/sysbin" "$W/ctx"
run_case A-online on "$W/sysbin" "$W/ctx"
run_case B-offline off "$W/ctx/bin:$W/sysbin" "$W/ctx"
run_case B-online on "$W/ctx/bin:$W/sysbin" "$W/ctx"
run_case B-norg on "$W/ctx-norg/bin:$W/sysbin" "$W/ctx-norg"

echo "## verdicts"
python3 -I - "$W" <<'EOF'
import json, os, sys
W = sys.argv[1]
def tools(case):
    try:
        return {t["tool"]: t for t in json.load(open(f"{W}/{case}/tools.json"))}
    except (OSError, ValueError):
        return {}
def downloaded(case):
    d = f"{W}/{case}/agent/bin"
    return sorted(n for n in os.listdir(d) if n in ("rg", "fd")) if os.path.isdir(d) else []
checks = []
t = tools("A-offline")
checks.append(("A-offline: grep fails as rg can't be had", t.get("grep", {}).get("error") is True and "ripgrep (rg) is not available and could not be downloaded" in t.get("grep", {}).get("first_line", "")))
checks.append(("A-offline: find fails as fd can't be had", t.get("find", {}).get("error") is True and "fd is not available and could not be downloaded" in t.get("find", {}).get("first_line", "")))
checks.append(("A-offline: nothing downloaded", downloaded("A-offline") == []))
t = tools("A-online")
checks.append(("A-online: pi downloads rg and fd into its config's bin/", downloaded("A-online") == ["fd", "rg"]))
checks.append(("A-online: grep and find then run", t.get("grep", {}).get("error") is False and t.get("find", {}).get("error") is False))
for case in ("B-offline", "B-online"):
    t = tools(case)
    checks.append((f"{case}: grep finds the needle with the pinned rg", t.get("grep", {}).get("error") is False and "needle" in t.get("grep", {}).get("first_line", "")))
    checks.append((f"{case}: find lists notes.txt with the pinned fd", t.get("find", {}).get("error") is False and "notes.txt" in t.get("find", {}).get("first_line", "")))
    checks.append((f"{case}: nothing downloaded", downloaded(case) == []))
checks.append(("B-norg (mutation): rg is downloaded again, fd is not", downloaded("B-norg") == ["rg"]))
bad = 0
for what, ok in checks:
    print(("PASS " if ok else "FAIL ") + what)
    bad += not ok
print(f"{len(checks) - bad} of {len(checks)} expectations hold")
sys.exit(1 if bad else 0)
EOF
