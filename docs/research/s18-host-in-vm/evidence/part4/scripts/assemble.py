"""Assemble each harness's provider hosts from its pinned source (data only), with doc candidates.
Writes work/{goose,pi}-hosts.tsv: host, providers, source files, doc candidates."""
import json, os, re, glob, collections
from urllib.parse import urlparse
R = os.path.expanduser("~/.cache/g121-s18-hosts")
def host(u):
    return (urlparse(u).hostname or "").lower() if u.startswith("https://") else ""
rows = {"goose": collections.defaultdict(lambda: {"prov": set(), "src": set(), "docs": set(), "ids": set()}),
        "pi": collections.defaultdict(lambda: {"prov": set(), "src": set(), "docs": set(), "ids": set()})}
def add(h, harness, prov, src, docs=(), pid=""):
    if not h or "{" in h or "$" in h: return
    r = rows[harness][h]; r["prov"].add(prov); r["src"].add(src); r["docs"].update(d for d in docs if d); r["ids"].update(pid.split())
# Goose declarative definitions
for f in sorted(glob.glob(f"{R}/dl/goose-declarative/*.json")):
    d = json.load(open(f)); n = os.path.basename(f)[:-5]
    bu = d.get("base_url", "")
    m = re.match(r"^\$\{([A-Z0-9_]+)\}", bu)
    if m:
        dv = next((e.get("default") for e in d.get("env_vars", []) if e.get("name") == m.group(1)), None)
        if not dv: continue
        bu = dv
    docs = [d.get("model_doc_link"), (d.get("setup") or {}).get("docs_url")]
    add(host(bu), "goose", d.get("display_name") or n, f"crates/goose-providers/src/declarative/definitions/{n}.json", docs, d.get("name") or n)
# Goose coded providers (hosts it requests; scope URIs and browser pages left out)
coded = {
 "api.anthropic.com": ("Anthropic", ["crates/goose-providers/src/anthropic.rs", "crates/goose/src/providers/anthropic_def.rs"], "anthropic"),
 "generativelanguage.googleapis.com": ("Gemini", ["crates/goose-providers/src/google.rs"], "google"),
 "api.openai.com": ("OpenAI", ["crates/goose-providers/src/openai.rs", "crates/goose/src/providers/openai_def.rs"], "openai"),
 "opencode.ai": ("OpenCode Zen and Go", ["crates/goose-providers/src/openai.rs"], "opencode_zen opencode_go"),
 "openrouter.ai": ("OpenRouter", ["crates/goose-providers/src/openrouter.rs", "crates/goose/src/providers/openrouter_def.rs"], "openrouter"),
 "api.avian.io": ("Avian", ["crates/goose/src/providers/avian.rs"], "avian"),
 "chatgpt.com": ("ChatGPT Codex", ["crates/goose/src/providers/chatgpt_codex.rs"], "chatgpt_codex"),
 "auth.openai.com": ("ChatGPT Codex sign-in", ["crates/goose/src/providers/chatgpt_codex.rs"], "chatgpt_codex"),
 "cloudcode-pa.googleapis.com": ("Gemini (Google sign-in, Code Assist)", ["crates/goose/src/providers/gemini_oauth.rs"], "gemini_oauth"),
 "oauth2.googleapis.com": ("Gemini (Google sign-in) token endpoint", ["crates/goose/src/providers/gemini_oauth.rs"], "gemini_oauth"),
 "github.com": ("GitHub Copilot device sign-in", ["crates/goose/src/providers/githubcopilot.rs"], "github_copilot"),
 "api.github.com": ("GitHub Copilot token", ["crates/goose/src/providers/githubcopilot.rs"], "github_copilot"),
 "api.githubcopilot.com": ("GitHub Copilot", ["crates/goose/src/providers/githubcopilot.rs"], "github_copilot"),
 "api.gondola-ai.com": ("Gondola", ["crates/goose/src/providers/gondola.rs"], "gondola"),
 "router.huggingface.co": ("Hugging Face", ["crates/goose/src/providers/huggingface.rs"], "huggingface"),
 "huggingface.co": ("Hugging Face sign-in", ["crates/goose/src/providers/huggingface_auth.rs"], "huggingface"),
 "api.kimi.com": ("Kimi Code", ["crates/goose/src/providers/kimicode.rs"], "kimi_code"),
 "auth.kimi.com": ("Kimi Code sign-in", ["crates/goose/src/providers/kimicode.rs"], "kimi_code"),
 "api.meta.ai": ("Meta (Muse Code)", ["crates/goose/src/providers/muse_code_def.rs"], "muse_code"),
 "auth.meta.com": ("Meta (Muse Code) sign-in", ["crates/goose/src/providers/muse_code_def.rs"], "muse_code"),
 "nano-gpt.com": ("NanoGPT", ["crates/goose/src/providers/nanogpt.rs"], "nano-gpt"),
 "ollama.com": ("Ollama Cloud", ["crates/goose/src/providers/ollama_cloud.rs"], "ollama_cloud"),
 "api.router.tetrate.ai": ("Tetrate Agent Router Service", ["crates/goose/src/providers/tetrate.rs"], "tetrate"),
 "api.typesafe.ai": ("TypeSafe, for auto effort (GOOSE_AUTO_EFFORT_ENABLED and TYPESAFE_API_KEY)", ["crates/goose/src/agents/services.rs", "crates/goose-providers/src/typesafe.rs"], "auto-effort(typesafe)"),
 "api.x.ai": ("xAI", ["crates/goose/src/providers/xai.rs"], "xai"),
 "auth.x.ai": ("xAI sign-in", ["crates/goose/src/providers/xai_oauth.rs"], "xai_oauth"),
}
for h, (p, srcs, pid) in coded.items():
    for s in srcs: add(h, "goose", p, s, (), pid)
# pi: catalog providers (packages/ai/catalog/providers/*.ts) and its sign-ins (packages/ai/src/auth/oauth/*.ts)
pi_map = {
 "api.ant-ling.com": ("Ant Ling", ["packages/ai/catalog/providers/ant-ling.ts"]),
 "api.anthropic.com": ("Anthropic", ["packages/ai/catalog/providers/anthropic.ts"]),
 "platform.claude.com": ("Anthropic sign-in (token exchange and refresh)", ["packages/ai/src/auth/oauth/anthropic.ts"]),
 "inference.baseten.co": ("Baseten", ["packages/ai/catalog/providers/baseten.ts"]),
 "api.cerebras.ai": ("Cerebras", ["packages/ai/catalog/providers/cerebras.ts"]),
 "gateway.ai.cloudflare.com": ("Cloudflare AI Gateway", ["packages/ai/src/api/cloudflare.ts"]),
 "api.cloudflare.com": ("Cloudflare Workers AI", ["packages/ai/src/api/cloudflare.ts"]),
 "api.deepseek.com": ("DeepSeek", ["packages/ai/catalog/providers/deepseek.ts"]),
 "api.fireworks.ai": ("Fireworks", ["packages/ai/catalog/providers/fireworks.ts"]),
 "api.individual.githubcopilot.com": ("GitHub Copilot", ["packages/ai/catalog/providers/github-copilot.ts"]),
 "github.com": ("GitHub Copilot device sign-in", ["packages/ai/src/auth/oauth/github-copilot.ts"]),
 "api.github.com": ("GitHub Copilot token", ["packages/ai/src/auth/oauth/github-copilot.ts"]),
 "generativelanguage.googleapis.com": ("Google Gemini", ["packages/ai/catalog/providers/google.ts"]),
 "api.groq.com": ("Groq", ["packages/ai/catalog/providers/groq.ts"]),
 "router.huggingface.co": ("Hugging Face", ["packages/ai/catalog/providers/huggingface.ts"]),
 "api.kimi.com": ("Kimi For Coding", ["packages/ai/catalog/providers/kimi-coding.ts"]),
 "auth.kimi.com": ("Kimi For Coding sign-in", ["packages/ai/src/auth/oauth/kimi-coding.ts"]),
 "api.meta.ai": ("Meta", ["packages/ai/catalog/providers/meta.ts"]),
 "auth.meta.com": ("Meta sign-in", ["packages/ai/src/auth/oauth/meta.ts"]),
 "api.minimax.io": ("MiniMax", ["packages/ai/catalog/providers/minimax.ts"]),
 "api.minimaxi.com": ("MiniMax (China)", ["packages/ai/catalog/providers/minimax-cn.ts"]),
 "api.mistral.ai": ("Mistral", ["packages/ai/catalog/providers/mistral.ts"]),
 "api.moonshot.ai": ("Moonshot AI (Global)", ["packages/ai/catalog/providers/moonshotai.ts"]),
 "api.moonshot.cn": ("Moonshot AI (China)", ["packages/ai/catalog/providers/moonshotai-cn.ts"]),
 "integrate.api.nvidia.com": ("NVIDIA NIM", ["packages/ai/catalog/sources/index.ts"]),
 "api.openai.com": ("OpenAI", ["packages/ai/catalog/providers/openai.ts"]),
 "chatgpt.com": ("OpenAI ChatGPT (Codex)", ["packages/ai/catalog/providers/openai-codex.ts"]),
 "auth.openai.com": ("OpenAI ChatGPT sign-in", ["packages/ai/src/auth/oauth/openai-codex.ts", "packages/ai/src/auth/oauth/openai-chatgpt.ts"]),
 "opencode.ai": ("OpenCode Zen and Go", ["packages/ai/catalog/providers/opencode.ts"]),
 "openrouter.ai": ("OpenRouter", ["packages/ai/catalog/providers/openrouter.ts", "packages/ai/src/auth/oauth/openrouter.ts"]),
 "token-plan.ap-southeast-1.maas.aliyuncs.com": ("Qwen Token Plan and Individual", ["packages/ai/catalog/providers/qwen-token-plan.ts"]),
 "token-plan.cn-beijing.maas.aliyuncs.com": ("Qwen Token Plan (China) and Individual (China)", ["packages/ai/catalog/providers/qwen-token-plan.ts"]),
 "radius.pi.dev": ("Radius", ["packages/ai/src/providers/radius-config.ts"]),
 "api.together.ai": ("Together AI", ["packages/ai/catalog/providers/together.ts"]),
 "api.typesafe.ai": ("TypeSafe", ["packages/ai/catalog/providers/typesafe.ts"]),
 "ai-gateway.vercel.sh": ("Vercel AI Gateway", ["packages/ai/src/providers/vercel-ai-gateway.ts"]),
 "api.x.ai": ("xAI", ["packages/ai/catalog/providers/xai.ts"]),
 "auth.x.ai": ("xAI sign-in", ["packages/ai/src/auth/oauth/xai.ts"]),
 "api.xiaomimimo.com": ("Xiaomi MiMo", ["packages/ai/catalog/providers/xiaomi.ts"]),
 "token-plan-ams.xiaomimimo.com": ("Xiaomi MiMo Token Plan (Amsterdam)", ["packages/ai/catalog/providers/xiaomi-token-plan-ams.ts"]),
 "token-plan-cn.xiaomimimo.com": ("Xiaomi MiMo Token Plan (China)", ["packages/ai/catalog/providers/xiaomi-token-plan-cn.ts"]),
 "token-plan-sgp.xiaomimimo.com": ("Xiaomi MiMo Token Plan (Singapore)", ["packages/ai/catalog/providers/xiaomi-token-plan-sgp.ts"]),
 "api.z.ai": ("ZAI Coding Plan (Global)", ["packages/ai/catalog/providers/zai.ts"]),
 "open.bigmodel.cn": ("ZAI Coding Plan (China)", ["packages/ai/catalog/providers/zai-coding-cn.ts"]),
 "pi.dev": ("pi's catalog overlays (models.md)", ["packages/coding-agent/docs/models.md"]),
}
def pid(src):
    b = src.rsplit("/", 1)[-1].rsplit(".", 1)[0]
    return {"openai-chatgpt": "openai-codex", "radius-config": "radius", "cloudflare": "cloudflare-ai-gateway cloudflare-workers-ai", "index": "nvidia", "models": "(pi itself)"}.get(b, b)
PI_IDS = {"api.cloudflare.com": "cloudflare-workers-ai", "gateway.ai.cloudflare.com": "cloudflare-ai-gateway",
  "token-plan.ap-southeast-1.maas.aliyuncs.com": "qwen-token-plan qwen-token-plan-individual",
  "token-plan.cn-beijing.maas.aliyuncs.com": "qwen-token-plan-cn qwen-token-plan-individual-cn", "opencode.ai": "opencode opencode-go", "pi.dev": "every-provider(pi-catalog-overlays)"}
for h, (p, srcs) in pi_map.items():
    for s in srcs: add(h, "pi", p, s, (), PI_IDS.get(h) or pid(s))
# doc candidates from opencode's catalog and models.dev, by host
oc = {}
for line in open(f"{R}/work/opencode-hosts.tsv").read().splitlines()[1:]:
    c = line.split("\t"); oc[c[0]] = c[3].split()
md = json.load(open(f"{R}/dl/modelsdev-api/api.json"))
mdh = collections.defaultdict(set)
for k, v in md.items():
    h = host(v.get("api") or "")
    if h and v.get("doc"): mdh[h].add(v["doc"])
for harness, rs in rows.items():
    with open(f"{R}/work/{harness}-hosts.tsv", "w") as o:
        o.write("host\tproviders\tsources\tdocs\tids\n")
        for h in sorted(rs):
            r = rs[h]; docs = sorted(set(r["docs"]) | set(oc.get(h, [])) | mdh.get(h, set()))
            o.write("\t".join([h, " | ".join(sorted(r["prov"])), " ".join(sorted(r["src"])), " ".join(docs), " ".join(sorted(r["ids"]))]) + "\n")
    print(harness, len(rs), "hosts")
