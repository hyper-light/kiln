"""opencode's reachable provider hosts: every catalog API whose @opencode/ai package the pinned build ships,
at a fixed public base_url (no template, no package that builds its own URL, no loopback), grouped by host.
Args: catalog.json builtin-packages.txt out.tsv"""
import json, sys, re, collections
from urllib.parse import urlparse
cat = json.load(open(sys.argv[1]))
builtin = {l.strip().strip('"') for l in open(sys.argv[2]) if l.strip()}
builds_own = {"@opencode/ai/providers/amazon-bedrock", "@opencode/ai/providers/azure/chat", "@opencode/ai/providers/azure/images",
  "@opencode/ai/providers/azure/messages", "@opencode/ai/providers/azure/responses", "@opencode/ai/providers/cloudflare-ai-gateway",
  "@opencode/ai/providers/cloudflare-ai-gateway/decisions", "@opencode/ai/providers/cloudflare-workers-ai",
  "@opencode/ai/providers/cloudflare-workers-ai/decisions", "@opencode/ai/providers/cloudflare-workers-ai/images",
  "@opencode/ai/providers/google-vertex/gemini", "@opencode/ai/providers/google-vertex/images", "@opencode/ai/providers/google-vertex/messages"}
label = re.compile(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$")
rows = collections.defaultdict(lambda: {"providers": set(), "names": set(), "docs": set(), "packages": set(), "urls": set()})
skipped = collections.Counter()
for pid, p in cat.items():
    for m in (p.get("models") or {}).values():
        if m.get("type") not in ("chat", "decision", "image"):
            continue
        for a in (m.get("api") or {}).values():
            pkg = ((a.get("sdk") or {}).get("opencodeai") or "")
            bu = a.get("base_url") or ""
            if pkg not in builtin: skipped["package not in this build"] += 1; continue
            if pkg in builds_own: skipped["package builds its own URL"] += 1; continue
            if not bu.startswith("https://"): skipped["no https base_url"] += 1; continue
            h = (urlparse(bu).hostname or "").lower()
            labels = h.split(".")
            if not h or "{" in bu.split("/")[2] or "$" in bu or len(labels) < 2 or not all(label.match(x) for x in labels) or labels[-1].isdigit() or h in ("localhost",):
                skipped["templated or not a public name"] += 1; continue
            r = rows[h]; r["providers"].add(pid); r["names"].add(p.get("name") or pid); r["packages"].add(pkg); r["urls"].add(bu)
            if p.get("doc"): r["docs"].add(p["doc"])
with open(sys.argv[3], "w") as o:
    o.write("host\tproviders\tnames\tdocs\tbase_urls\n")
    for h in sorted(rows):
        r = rows[h]
        o.write("\t".join([h, ",".join(sorted(r["providers"])), " | ".join(sorted(r["names"])), " ".join(sorted(r["docs"])), " ".join(sorted(r["urls"]))]) + "\n")
print(len(rows), "hosts;", dict(skipped))
