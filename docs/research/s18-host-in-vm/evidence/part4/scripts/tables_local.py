import os
R = os.path.expanduser("~/.cache/g121-s18-hosts")
def local(url):
    if "/aaif-goose/goose/v1.59.0/" in url:
        path = url.split("/v1.59.0/", 1)[1]
        if "/declarative/definitions/" in path: return f"{R}/dl/goose-declarative/" + os.path.basename(path)
        return f"{R}/dl/goose-coded/" + path.replace("/", "_")
    if "/earendil-works/pi/v1.5.2/" in url:
        path = url.split("/v1.5.2/", 1)[1]
        if path.startswith("packages/ai/catalog/providers/"): return f"{R}/dl/pi-catalog/" + os.path.basename(path)
        if path.startswith("packages/ai/src/auth/oauth/"): return f"{R}/dl/pi-src2/auth_oauth_" + os.path.basename(path)
        if path.startswith("packages/ai/src/providers/"): return f"{R}/dl/pi-src/" + os.path.basename(path)
        if path == "packages/coding-agent/docs/models.md": return f"{R}/dl/pi-docs/models.md"
        return f"{R}/dl/pi-src3/" + path.replace("/", "_")
    if "/anomalyco/opencode/v2.2.7/" in url:
        path = url.split("/v2.2.7/", 1)[1]
        if path.startswith("packages/core/src/plugin/provider/"): return f"{R}/dl/oc-plugins/plugin_provider_" + os.path.basename(path)
        if path == "packages/core/src/ripgrep/binary.ts": return f"{R}/dl/oc-plugins/ripgrep_binary.ts"
        if path == "packages/core/src/models-dev.ts": return f"{R}/dl/oc-core/models-dev.ts"
        if path.startswith("packages/ai/src/providers/"): return f"{R}/dl/oc-providers/" + os.path.basename(path)
    raise SystemExit("no local copy for " + url)
