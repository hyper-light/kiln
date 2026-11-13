"""tables.py OUTDIR: each of Goose, pi and opencode's provider hosts with its harness source (file at the pin and
that file's SHA-256) and its provider's own page (status, URL, fetch date, page SHA-256, quote)."""
import csv, os, sys, hashlib, collections
R = os.path.expanduser("~/.cache/g121-s18-hosts"); OUT = sys.argv[1]
docs = {r["host"]: r for r in csv.DictReader(open(f"{R}/work/doccheck-final.tsv"), delimiter="\t")}
def sha(p): return hashlib.sha256(open(p, "rb").read()).hexdigest()
def goose_local(path):
    if "/declarative/definitions/" in path: return f"{R}/dl/goose-declarative/" + os.path.basename(path)
    return f"{R}/dl/goose-coded/" + path.replace("/", "_")
def pi_local(path):
    if path.startswith("packages/ai/catalog/providers/"): return f"{R}/dl/pi-catalog/" + os.path.basename(path)
    if path.startswith("packages/ai/src/auth/oauth/"): return f"{R}/dl/pi-src2/auth_oauth_" + os.path.basename(path)
    if path.startswith("packages/ai/src/providers/"): return f"{R}/dl/pi-src/" + os.path.basename(path)
    if path == "packages/coding-agent/docs/models.md": return f"{R}/dl/pi-docs/models.md"
    return f"{R}/dl/pi-src3/" + path.replace("/", "_")
TAG = {"goose": ("https://raw.githubusercontent.com/aaif-goose/goose/v1.59.0/", goose_local),
       "pi": ("https://raw.githubusercontent.com/earendil-works/pi/v1.5.2/", pi_local)}
HDR = ["host", "provider_ids", "for", "harness_source", "harness_source_sha256", "provider_doc", "provider_doc_url", "provider_doc_fetched", "provider_doc_sha256", "provider_doc_quote"]
def doccols(h):
    d = docs.get(h)
    if not d or d["result"] not in ("found", "wildcard"):
        tried = (d or {}).get("url", "")
        return ["none found: harness source only", "", "", "", ("pages checked: " + tried)[:600] if tried else ""]
    return [d["result"] if d["result"] == "wildcard" else "names it", d["url"], d["fetched"], d["sha256"], d["line"]]
counts = {}
for harness in ("goose", "pi"):
    base, local = TAG[harness]
    rows = []
    for line in open(f"{R}/work/{harness}-hosts.tsv").read().splitlines()[1:]:
        h, prov, srcs, _, ids = line.split("\t")
        for s in srcs.split():
            rows.append([h, ids, prov, base + s, sha(local(s))] + doccols(h))
    with open(f"{OUT}/{harness}-hosts.tsv", "w") as o:
        w = csv.writer(o, delimiter="\t", lineterminator="\n"); w.writerow(HDR); w.writerows(rows)
    counts[harness] = len({r[0] for r in rows})
# opencode: its catalog's hosts, its sign-ins and its own hosts
cat_url = "https://models.opencode.ai/experimental/v2.0/api.json?type=chat,decision,image"
cat_sha = sha(f"{R}/dl/oc-catalog-v2/api.json")
B = "https://raw.githubusercontent.com/anomalyco/opencode/v2.2.7/"
rows = []
for line in open(f"{R}/work/opencode-hosts.tsv").read().splitlines()[1:]:
    h, provs, names, _, bases = line.split("\t")
    rows.append([h, provs.replace(",", " "), f"catalog: {names}; base URLs {bases}", cat_url + " (fetched 2026-11-13)", cat_sha] + doccols(h))
own = [
 ("github.com", "github-copilot", "GitHub Copilot device sign-in (login/device/code, login/oauth/access_token)", "packages/core/src/plugin/provider/github-copilot.ts", "oc-plugins/plugin_provider_github-copilot.ts"),
 ("github.com", "every-provider(ripgrep)", "ripgrep 15.1.0's release download, where its base has no rg", "packages/core/src/ripgrep/binary.ts", "oc-plugins/ripgrep_binary.ts"),
 ("api.github.com", "github-copilot", "GitHub Copilot entitlement (copilot_internal/user)", "packages/core/src/plugin/provider/github-copilot.ts", "oc-plugins/plugin_provider_github-copilot.ts"),
 ("auth.openai.com", "openai", "ChatGPT sign-in and its token refresh", "packages/core/src/plugin/provider/chatgpt.ts", "oc-plugins/plugin_provider_chatgpt.ts"),
 ("auth.openai.com", "openai", "ChatGPT sign-in and its token refresh", "packages/core/src/plugin/provider/openai.ts", "oc-plugins/plugin_provider_openai.ts"),
 ("chatgpt.com", "openai", "ChatGPT's Codex backend (backend-api/codex) for a ChatGPT sign-in", "packages/core/src/plugin/provider/openai.ts", "oc-plugins/plugin_provider_openai.ts"),
 ("gitlab.com", "gitlab", "GitLab sign-in, its token refresh and GitLab's API", "packages/core/src/plugin/provider/gitlab.ts", "oc-plugins/plugin_provider_gitlab.ts"),
 ("auth.x.ai", "xai", "xAI sign-in", "packages/core/src/plugin/provider/xai.ts", "oc-plugins/plugin_provider_xai.ts"),
 ("api.poe.com", "poe", "Poe sign-in's token exchange", "packages/core/src/plugin/provider/poe.ts", "oc-plugins/plugin_provider_poe.ts"),
 ("api.digitalocean.com", "digitalocean", "DigitalOcean's model routers, after its sign-in", "packages/core/src/plugin/provider/digitalocean.ts", "oc-plugins/plugin_provider_digitalocean.ts"),
 ("gateway.ai.cloudflare.com", "cloudflare-ai-gateway", "Cloudflare AI Gateway (its account and gateway are in the path)", "packages/ai/src/providers/cloudflare-ai-gateway.ts", "oc-providers/cloudflare-ai-gateway.ts"),
 ("api.cloudflare.com", "cloudflare-workers-ai", "Cloudflare Workers AI (its account is in the path)", "packages/ai/src/providers/cloudflare-shared.ts", "oc-providers/cloudflare-shared.ts"),
 ("models.opencode.ai", "every-provider(catalog)", "opencode's model catalog (its default source, refreshed live)", "packages/core/src/models-dev.ts", "oc-core/models-dev.ts"),
 ("release-assets.githubusercontent.com", "every-provider(ripgrep)", "where github.com sends ripgrep's release download (302, observed 2026-11-13)", "packages/core/src/ripgrep/binary.ts", "oc-plugins/ripgrep_binary.ts"),
]
for h, pid, why, src, loc in own:
    rows.append([h, pid, why, B + src, sha(f"{R}/dl/{loc}")] + doccols(h))
with open(f"{OUT}/opencode-hosts.tsv", "w") as o:
    w = csv.writer(o, delimiter="\t", lineterminator="\n"); w.writerow(HDR); w.writerows(rows)
counts["opencode"] = len({r[0] for r in rows})
for k in ("goose", "pi", "opencode"):
    rs = list(csv.DictReader(open(f"{OUT}/{k}-hosts.tsv"), delimiter="\t"))
    st = collections.Counter(next(r["provider_doc"] for r in rs if r["host"] == h) for h in {r["host"] for r in rs})
    print(k, counts[k], "hosts;", dict(st))
