# Plan step 28: pi's built-in Agentfile (the record)

Step 28's runs are in `pin-run-*.log`, `port/`, `step2/` and `step5/`. This file holds what later steps add to the
record.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-pi/Agentfile` grants these 45 hosts by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else.

**The rule** is ROOT's call of 2026-11-13, made on the person's bar of zero breaks against today
(`docs/research/s18-host-in-vm/evidence/part4/root-2026-11-13-providers.txt`). The image names every provider that
pi's pinned build (1.5.2) reaches at a fixed public host. Each host is cited to that provider's own API docs and to
pi's own source. A host neither backs is dropped. Each run is narrowed at launch to the provider it's configured for;
that narrowing is the launch's (goal 60, plan step 17). For pi, its docs and its build agree: `evidence/hosts/providers.md`
lists each provider here. The exception is the sign-ins, which `/login` offers per provider.

**Each host's citations** are in `evidence/hosts/hosts.tsv`, one row per host and source file:
- its provider ids (what `defaultProvider` and `--provider` take) and what it's for;
- pi's file at tag v1.5.2 with its SHA-256, with the numbered lines naming the host in `pi-source-excerpts.txt`;
- its provider's own page: URL, fetch date, the page's SHA-256 and the words naming the host.

38 hosts are named on their provider's own page. One, `api.individual.githubcopilot.com`, falls under GitHub's
documented wildcard `*.individual.githubcopilot.com`. For 6, no page of the provider's own docs that kiln could fetch
names the host, so those rest on pi's source alone, as said below. MiniMax's China docs now name `api.minimax.cn`,
where pi 1.5.2 asks `api.minimaxi.com`. Provider pages are cited by quote and hash rather than saved whole. pi's own
pages are saved whole in `evidence/hosts/` (`SOURCES.tsv`).

| Host | Provider ids | For | Provider's own page | Harness source |
|---|---|---|---|---|
| `ai-gateway.vercel.sh` | vercel-ai-gateway | Vercel AI Gateway | names it: https://vercel.com/docs/ai-gateway/openai-compat.md | `packages/ai/src/providers/vercel-ai-gateway.ts` |
| `api.ant-ling.com` | ant-ling | Ant Ling | none found: the harness's source alone | `packages/ai/catalog/providers/ant-ling.ts` |
| `api.anthropic.com` | anthropic | Anthropic | names it: https://platform.claude.com/docs/en/api/overview.md | `packages/ai/catalog/providers/anthropic.ts` |
| `api.cerebras.ai` | cerebras | Cerebras | names it: https://inference-docs.cerebras.ai/resources/openai.md | `packages/ai/catalog/providers/cerebras.ts` |
| `api.cloudflare.com` | cloudflare-workers-ai | Cloudflare Workers AI | names it: https://developers.cloudflare.com/workers-ai/configuration/open-ai-compatibility/ | `packages/ai/src/api/cloudflare.ts` |
| `api.deepseek.com` | deepseek | DeepSeek | names it: https://api-docs.deepseek.com/ | `packages/ai/catalog/providers/deepseek.ts` |
| `api.fireworks.ai` | fireworks | Fireworks | names it: https://docs.fireworks.ai/tools-sdks/openai-compatibility.md | `packages/ai/catalog/providers/fireworks.ts` |
| `api.github.com` | github-copilot | GitHub Copilot token | names it: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `packages/ai/src/auth/oauth/github-copilot.ts` |
| `api.groq.com` | groq | Groq | names it: https://console.groq.com/docs/openai.md | `packages/ai/catalog/providers/groq.ts` |
| `api.individual.githubcopilot.com` | github-copilot | GitHub Copilot | a wildcard it documents: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `packages/ai/catalog/providers/github-copilot.ts` |
| `api.kimi.com` | kimi-coding | Kimi For Coding | names it: https://www.kimi.com/code/docs/en/ | `packages/ai/catalog/providers/kimi-coding.ts` |
| `api.meta.ai` | meta | Meta | names it: https://dev.meta.ai/docs | `packages/ai/catalog/providers/meta.ts` |
| `api.minimax.io` | minimax | MiniMax | names it: https://platform.minimax.io/docs/guides/quickstart-preparation.md | `packages/ai/catalog/providers/minimax.ts` |
| `api.minimaxi.com` | minimax-cn | MiniMax (China) | none found: the harness's source alone | `packages/ai/catalog/providers/minimax-cn.ts` |
| `api.mistral.ai` | mistral | Mistral | names it: https://docs.mistral.ai/api | `packages/ai/catalog/providers/mistral.ts` |
| `api.moonshot.ai` | moonshotai | Moonshot AI (Global) | names it: https://platform.moonshot.ai/docs/guide/start-using-kimi-api.md | `packages/ai/catalog/providers/moonshotai.ts` |
| `api.moonshot.cn` | moonshotai-cn | Moonshot AI (China) | names it: https://platform.moonshot.cn/docs/guide/start-using-kimi-api.md | `packages/ai/catalog/providers/moonshotai-cn.ts` |
| `api.openai.com` | openai | OpenAI | names it: https://developers.openai.com/api/reference/overview.md | `packages/ai/catalog/providers/openai.ts` |
| `api.together.ai` | together | Together AI | names it: https://docs.together.ai/docs/openai-api-compatibility.md | `packages/ai/catalog/providers/together.ts` |
| `api.typesafe.ai` | typesafe | TypeSafe | names it: https://api.typesafe.ai/openapi.json | `packages/ai/catalog/providers/typesafe.ts` |
| `api.x.ai` | xai | xAI | names it: https://docs.x.ai/docs/api-reference | `packages/ai/catalog/providers/xai.ts` |
| `api.xiaomimimo.com` | xiaomi | Xiaomi MiMo | names it: https://mimo.mi.com/static/docs/quick-start/summary/first-api-call.md | `packages/ai/catalog/providers/xiaomi.ts` |
| `api.z.ai` | zai | ZAI Coding Plan (Global) | names it: https://docs.z.ai/guides/overview/quick-start.md | `packages/ai/catalog/providers/zai.ts` |
| `auth.kimi.com` | kimi-coding | Kimi For Coding sign-in | none found: the harness's source alone | `packages/ai/src/auth/oauth/kimi-coding.ts` |
| `auth.meta.com` | meta | Meta sign-in | none found: the harness's source alone | `packages/ai/src/auth/oauth/meta.ts` |
| `auth.openai.com` | openai-codex | OpenAI ChatGPT sign-in | names it: https://learn.chatgpt.com/docs/app-server.md | `packages/ai/src/auth/oauth/openai-chatgpt.ts`, `packages/ai/src/auth/oauth/openai-codex.ts` |
| `auth.x.ai` | xai | xAI sign-in | none found: the harness's source alone | `packages/ai/src/auth/oauth/xai.ts` |
| `chatgpt.com` | openai-codex | OpenAI ChatGPT (Codex) | names it: https://learn.chatgpt.com/docs/enterprise/connect-to-a-gateway.md | `packages/ai/catalog/providers/openai-codex.ts` |
| `gateway.ai.cloudflare.com` | cloudflare-ai-gateway | Cloudflare AI Gateway | names it: https://developers.cloudflare.com/ai-gateway/get-started/ | `packages/ai/src/api/cloudflare.ts` |
| `generativelanguage.googleapis.com` | google | Google Gemini | names it: https://ai.google.dev/api.md | `packages/ai/catalog/providers/google.ts` |
| `github.com` | github-copilot | GitHub Copilot device sign-in | names it: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `packages/ai/src/auth/oauth/github-copilot.ts` |
| `inference.baseten.co` | baseten | Baseten | names it: https://docs.baseten.co/development/model-apis/overview.md | `packages/ai/catalog/providers/baseten.ts` |
| `integrate.api.nvidia.com` | nvidia | NVIDIA NIM | names it: https://docs.api.nvidia.com/nim/reference/llm-apis.md | `packages/ai/catalog/sources/index.ts` |
| `open.bigmodel.cn` | zai-coding-cn | ZAI Coding Plan (China) | names it: https://docs.bigmodel.cn/cn/guide/start/quick-start.md | `packages/ai/catalog/providers/zai-coding-cn.ts` |
| `opencode.ai` | opencode opencode-go | OpenCode Zen and Go | names it: https://opencode.ai/docs/console/api/inference.md | `packages/ai/catalog/providers/opencode.ts` |
| `openrouter.ai` | openrouter | OpenRouter | names it: https://openrouter.ai/docs/quickstart.md | `packages/ai/catalog/providers/openrouter.ts`, `packages/ai/src/auth/oauth/openrouter.ts` |
| `pi.dev` | every-provider(pi-catalog-overlays) | pi's catalog overlays (models.md) | names it: https://raw.githubusercontent.com/earendil-works/pi/v1.5.2/packages/coding-agent/docs/models.md | `packages/coding-agent/docs/models.md` |
| `platform.claude.com` | anthropic | Anthropic sign-in (token exchange and refresh) | names it: https://code.claude.com/docs/en/network-config.md | `packages/ai/src/auth/oauth/anthropic.ts` |
| `radius.pi.dev` | radius | Radius | names it: https://raw.githubusercontent.com/earendil-works/pi/v1.5.2/packages/coding-agent/docs/providers.md | `packages/ai/src/providers/radius-config.ts` |
| `router.huggingface.co` | huggingface | Hugging Face | names it: https://huggingface.co/docs/inference-providers/index.md | `packages/ai/catalog/providers/huggingface.ts` |
| `token-plan-ams.xiaomimimo.com` | xiaomi-token-plan-ams | Xiaomi MiMo Token Plan (Amsterdam) | names it: https://mimo.mi.com/static/docs/tokenplan/integration/opencode.md | `packages/ai/catalog/providers/xiaomi-token-plan-ams.ts` |
| `token-plan-cn.xiaomimimo.com` | xiaomi-token-plan-cn | Xiaomi MiMo Token Plan (China) | names it: https://mimo.mi.com/static/docs/quick-start/summary/first-api-call.md | `packages/ai/catalog/providers/xiaomi-token-plan-cn.ts` |
| `token-plan-sgp.xiaomimimo.com` | xiaomi-token-plan-sgp | Xiaomi MiMo Token Plan (Singapore) | names it: https://mimo.mi.com/static/docs/tokenplan/integration/opencode.md | `packages/ai/catalog/providers/xiaomi-token-plan-sgp.ts` |
| `token-plan.ap-southeast-1.maas.aliyuncs.com` | qwen-token-plan qwen-token-plan-individual | Qwen Token Plan and Individual | names it: https://www.alibabacloud.com/help/en/model-studio/token-plan-overview.md | `packages/ai/catalog/providers/qwen-token-plan.ts` |
| `token-plan.cn-beijing.maas.aliyuncs.com` | qwen-token-plan-cn qwen-token-plan-individual-cn | Qwen Token Plan (China) and Individual (China) | none found: the harness's source alone | `packages/ai/catalog/providers/qwen-token-plan.ts` |

Not granted:
- **A provider whose host the person's resource or region places:** Azure OpenAI, Amazon Bedrock and Google
  Vertex AI.
- **A local server, or an endpoint the person gives in `models.json`:** for example a regional OpenRouter
  (`us.openrouter.ai`), Ollama or llama.cpp.

Each of those needs an Agentfile of the person's own.

**Changed from part 2** (44 hosts):
- `claude.ai` is dropped: pi only opens it in the person's browser, as the sign-in's authorize page
  (`packages/ai/src/auth/oauth/anthropic.ts`). The token exchange and refresh go to `platform.claude.com`, which stays.
- Added: `gateway.ai.cloudflare.com` (Cloudflare AI Gateway) and `api.cloudflare.com` (Cloudflare Workers AI). Their
  hosts are fixed, with the account only in the path (`packages/ai/src/api/cloudflare.ts`). pi's docs list both
  providers, and Cloudflare's docs name both hosts.

**Which provider a run uses** is for the launch (goal 60) to narrow to. pi documents it as `defaultProvider` and
`defaultModel` in its settings (`evidence/hosts/settings.md`), or `--provider` / `--model` on its command line
(`evidence/hosts/cli.md`). `pi.dev` serves the model catalog's refreshes for every provider (`evidence/hosts/models.md`:
"Pi starts with its bundled catalog and can overlay newer catalog data from pi.dev").
