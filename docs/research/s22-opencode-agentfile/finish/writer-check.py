"""pin.sh's pin.json writer, run alone on the pin's own record (step 22's finish, kiln plan
01a1cd8e862e7169a074db3d5fa33f3d): it must write the committed pin.json again, smoke in main's
shape included, but for `read.at`, the time of a run.

    python3 -I writer-check.py PIN_SH PIN_JSON DOWNLOADS

DOWNLOADS is a new empty directory: the release's SHA256SUMS and the tag's LICENSE are downloaded
into it, as pin.sh downloads them, and held to the digests the pin records for them. The writer gets
the archives' rows from the pin itself (pin.sh measures them; here they are what's checked against).
"""
import hashlib
import json
import os
import subprocess
import sys
import urllib.request

pin_sh, pin_json, downloads = sys.argv[1:4]
pin = json.load(open(pin_json))
if os.listdir(downloads):
    sys.exit(f"{downloads} isn't empty")

# The writer's program, as pin.sh hands it to python3 -I -c.
text = open(pin_sh).read()
start = text.index("python3 -I -c 'import hashlib, json, sys, time")
body = text[start + len("python3 -I -c '"):]
program = body[: body.index("' \\\n")]

work = os.path.join(downloads, "work")
os.mkdir(work)


def fetch(url, name):
    with urllib.request.urlopen(url, timeout=60) as r:
        data = r.read()
    open(os.path.join(work, name), "wb").write(data)
    return data


repo = pin["verified"]["provenance"]["repo"]
version = pin["version"]
sums = fetch(pin["verified"]["sha256sums"]["url"], "SHA256SUMS")
lic_url = pin["builds"][0]["files"][1]["url"]
lic = fetch(lic_url, "LICENSE")
print("SHA256SUMS", hashlib.sha256(sums).hexdigest(), "pinned", pin["verified"]["sha256sums"]["sha256"])
print("LICENSE", hashlib.sha256(lic).hexdigest(), len(lic), "pinned", pin["builds"][0]["files"][1]["sha256"])
json.dump(pin["read"]["answer"], open(os.path.join(work, "endpoint.json"), "w"))
with open(os.path.join(work, "builds.tsv"), "w") as t:
    for b in pin["builds"]:
        a = b["files"][0]
        t.write("\t".join([b["platform"], b["variant"], a["url"], a["sha256"], str(a["size"]), a["unpack"]["sha256"], str(a["unpack"]["size"])]) + "\n")

v = pin["verified"]["provenance"]
argv = [work, version, pin["tag"]["commit"], v["signer_workflow"], repo, pin["base"], lic_url, v["san"], v["source_commit"],
        v["source_ref"], v["run"], pin["read"]["from"], pin["smoke"]["program"], " ".join(pin["smoke"]["args"]), "opencode v"]
subprocess.run([sys.executable, "-I", "-c", program, *argv], check=True)
wrote = json.load(open(os.path.join(work, "pin.json")))
for p in (wrote, pin):
    p["read"]["at"] = "-"
same = wrote == pin
print("SMOKE", json.dumps(wrote.get("smoke")))
print("WRITES_THE_PIN_AGAIN", same)
if not same:
    for k in sorted(set(wrote) | set(pin)):
        if wrote.get(k) != pin.get(k):
            print("DIFFERS", k, json.dumps(wrote.get(k))[:300], json.dumps(pin.get(k))[:300])
    sys.exit(1)
