# Plan step 23: Goose's built-in Agentfile (the record)

Step 23's runs are in `step1-run.sh`, `step2-run.sh`, `step2b-pin-run.sh`, `step4/` and `evidence/`. This file holds what
later steps add to the record.

## Hosts by name

Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18, part 4 (plan 01a1d2bd5014705d918ad1f2ce1154f2, step 4).
`.kiln/images/harness-goose/Agentfile` grants these 67 hosts by name (Shards D236), on TCP 443, HTTPS's port (RFC 9110
§4.2.2), and nothing else.

**The rule** is ROOT's call of 2026-11-13, made on the person's bar of zero breaks against today
(`docs/research/s18-host-in-vm/evidence/part4/root-2026-11-13-providers.txt`). The image names every provider that
Goose's pinned build (1.59.0) reaches at a fixed public host. Each host is cited to that provider's own API docs and
to Goose's own source. A host neither backs is dropped. Each run is narrowed at launch to the provider it's
configured for; that narrowing is the launch's (goal 60, plan step 17).

The plan had said "each provider their docs list". Goose's docs (`evidence/hosts/providers.md`, v1.59.0) list 33 of
these providers. Its build reaches the rest too: DeepSeek, Fireworks, Together, Hugging Face, Kimi Code, MiniMax,
Moonshot, NVIDIA, Qwen, Zhipu, Vercel AI Gateway, OpenCode Zen and Go, and routers. People who use those through kiln
on the host today would lose them in a microVM, so ROOT's call keeps them.

**Each host's citations** are in `evidence/hosts/hosts.tsv`, one row per host and source file:
- its provider ids (what `GOOSE_PROVIDER` takes) and what it's for;
- Goose's file at tag v1.59.0 with its SHA-256, with the numbered lines naming the host in
  `goose-source-excerpts.txt`;
- its provider's own page: URL, fetch date, the page's SHA-256 and the words naming the host.

56 hosts are named on their provider's own page. One, `api.githubcopilot.com`, falls under GitHub's documented
wildcard `*.githubcopilot.com`. For 10, no page of the provider's own docs that kiln could fetch names the host, so
those rest on Goose's source alone, as said below. Provider pages are cited by quote and hash rather than saved whole:
they're the providers' documents, not Goose's. Goose's own page is saved whole in `evidence/hosts/`
(`SOURCES.tsv`).

| Host | Provider ids | For | Provider's own page | Harness source |
|---|---|---|---|---|
| `ai-gateway.vercel.sh` | vercel_ai_gateway | Vercel AI Gateway | names it: https://vercel.com/docs/ai-gateway/openai-compat.md | `crates/goose-providers/src/declarative/definitions/vercel_ai_gateway.json` |
| `ai.salad.cloud` | saladcloud | SaladCloud AI Gateway | names it: https://docs.salad.com/ai-gateway/explanation/overview.md | `crates/goose-providers/src/declarative/definitions/saladcloud.json` |
| `aihubmix.com` | aihubmix | AIHubMix | names it: https://docs.aihubmix.com/cn/api-reference/openai-compatible/create-a-model-response.md | `crates/goose-providers/src/declarative/definitions/aihubmix.json` |
| `api.aigateway.sh` | aigateway | AIgateway | names it: https://aigateway.sh/docs.md | `crates/goose-providers/src/declarative/definitions/aigateway.json` |
| `api.aimlapi.com` | aimlapi | AI/ML API | names it: https://docs.aimlapi.com/quickstart/setting-up.md | `crates/goose-providers/src/declarative/definitions/aimlapi.json` |
| `api.anthropic.com` | anthropic | Anthropic | names it: https://platform.claude.com/docs/en/api/overview.md | `crates/goose-providers/src/anthropic.rs`, `crates/goose/src/providers/anthropic_def.rs` |
| `api.avian.io` | avian | Avian | names it: https://avian.io/docs | `crates/goose/src/providers/avian.rs` |
| `api.cerebras.ai` | cerebras | Cerebras | names it: https://inference-docs.cerebras.ai/resources/openai.md | `crates/goose-providers/src/declarative/definitions/cerebras.json` |
| `api.deepseek.com` | custom_deepseek | DeepSeek | names it: https://api-docs.deepseek.com/ | `crates/goose-providers/src/declarative/definitions/deepseek.json` |
| `api.empiriolabs.ai` | empiriolabs | EmpirioLabs AI | names it: https://docs.empiriolabs.ai/api-reference/api-reference/models/list-models.md | `crates/goose-providers/src/declarative/definitions/empiriolabs.json` |
| `api.eurouter.ai` | eurouter | EUrouter | names it: https://www.eurouter.ai/api/openapi.json | `crates/goose-providers/src/declarative/definitions/eurouter.json` |
| `api.fireworks.ai` | fireworks-ai | Fireworks AI | names it: https://docs.fireworks.ai/tools-sdks/openai-compatibility.md | `crates/goose-providers/src/declarative/definitions/fireworks.json` |
| `api.friendli.ai` | friendli | Friendli AI | names it: https://friendli.ai/docs/openapi/model-apis/chat-completions.md | `crates/goose-providers/src/declarative/definitions/friendli.json` |
| `api.github.com` | github_copilot | GitHub Copilot token | names it: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `crates/goose/src/providers/githubcopilot.rs` |
| `api.githubcopilot.com` | github_copilot | GitHub Copilot | a wildcard it documents: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `crates/goose/src/providers/githubcopilot.rs` |
| `api.gondola-ai.com` | gondola | Gondola | names it: https://gondola-ai.com/guides | `crates/goose/src/providers/gondola.rs` |
| `api.groq.com` | groq | Groq | names it: https://console.groq.com/docs/openai.md | `crates/goose-providers/src/declarative/definitions/groq.json` |
| `api.inceptionlabs.ai` | inception | Inception | names it: https://docs.inceptionlabs.ai/get-started/models.md | `crates/goose-providers/src/declarative/definitions/inception.json` |
| `api.intelligence.io.solutions` | ionet | IO Intelligence | names it: https://io.net/docs/reference/ai-models/create-chat-completion.md | `crates/goose-providers/src/declarative/definitions/ionet.json` |
| `api.kimi.com` | kimi_code | Kimi Code | names it: https://www.kimi.com/code/docs/en/ | `crates/goose/src/providers/kimicode.rs` |
| `api.meta.ai` | meta muse_code | Meta \| Meta (Muse Code) | names it: https://dev.meta.ai/docs | `crates/goose-providers/src/declarative/definitions/meta.json`, `crates/goose/src/providers/muse_code_def.rs` |
| `api.minimax.io` | minimax | MiniMax | names it: https://platform.minimax.io/docs/guides/quickstart-preparation.md | `crates/goose-providers/src/declarative/definitions/minimax.json` |
| `api.mistral.ai` | mistral | Mistral AI | names it: https://docs.mistral.ai/api | `crates/goose-providers/src/declarative/definitions/mistral.json` |
| `api.moonshot.cn` | moonshot | Moonshot | names it: https://platform.moonshot.cn/docs/guide/start-using-kimi-api.md | `crates/goose-providers/src/declarative/definitions/moonshot.json` |
| `api.novita.ai` | novita | Novita AI | names it: https://novita.ai/docs/guides/introduction | `crates/goose-providers/src/declarative/definitions/novita.json` |
| `api.openai.com` | openai | OpenAI | names it: https://developers.openai.com/api/reference/overview.md | `crates/goose-providers/src/openai.rs`, `crates/goose/src/providers/openai_def.rs` |
| `api.opper.ai` | opper | Opper | names it: https://opper.ai/models.md | `crates/goose-providers/src/declarative/definitions/opper.json` |
| `api.orcarouter.ai` | orcarouter | OrcaRouter | names it: https://docs.orcarouter.ai | `crates/goose-providers/src/declarative/definitions/orcarouter.json` |
| `api.perplexity.ai` | perplexity | Perplexity | names it: https://docs.perplexity.ai/api-reference/models-get.md | `crates/goose-providers/src/declarative/definitions/perplexity.json` |
| `api.router.tetrate.ai` | tetrate | Tetrate Agent Router Service | none found: the harness's source alone | `crates/goose/src/providers/tetrate.rs` |
| `api.routerplex.com` | routerplex | RouterPlex | names it: https://docs.routerplex.com/models | `crates/goose-providers/src/declarative/definitions/routerplex.json` |
| `api.routstr.com` | routstr | Routstr | names it: https://raw.githubusercontent.com/Routstr/docs/main/README.md | `crates/goose-providers/src/declarative/definitions/routstr.json` |
| `api.sakana.ai` | sakana | Sakana AI | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/sakana.json` |
| `api.saygm.com` | saygm | SayGM | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/saygm.json` |
| `api.scaleway.ai` | scaleway | Scaleway | names it: https://www.scaleway.com/en/docs/generative-apis/api-cli/using-models-api.md | `crates/goose-providers/src/declarative/definitions/scaleway.json` |
| `api.tensorix.ai` | custom_tensorix | Tensorix | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/tensorix.json` |
| `api.together.xyz` | together | Together AI | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/together.json` |
| `api.trustedrouter.com` | trustedrouter | TrustedRouter | names it: https://trustedrouter.com/docs | `crates/goose-providers/src/declarative/definitions/trustedrouter.json` |
| `api.typesafe.ai` | auto-effort(typesafe) | TypeSafe, for auto effort (GOOSE_AUTO_EFFORT_ENABLED and TYPESAFE_API_KEY) | names it: https://api.typesafe.ai/openapi.json | `crates/goose-providers/src/typesafe.rs`, `crates/goose/src/agents/services.rs` |
| `api.venice.ai` | venice | Venice.ai | names it: https://docs.venice.ai | `crates/goose-providers/src/declarative/definitions/venice.json` |
| `api.x.ai` | xai | xAI | names it: https://docs.x.ai/docs/api-reference | `crates/goose/src/providers/xai.rs` |
| `api.z.ai` | zai zai_coding_plan | Z.AI \| Z.AI Coding Plan | names it: https://docs.z.ai/guides/overview/quick-start.md | `crates/goose-providers/src/declarative/definitions/zai.json`, `crates/goose-providers/src/declarative/definitions/zai_coding_plan.json` |
| `auth.kimi.com` | kimi_code | Kimi Code sign-in | none found: the harness's source alone | `crates/goose/src/providers/kimicode.rs` |
| `auth.meta.com` | muse_code | Meta (Muse Code) sign-in | none found: the harness's source alone | `crates/goose/src/providers/muse_code_def.rs` |
| `auth.openai.com` | chatgpt_codex | ChatGPT Codex sign-in | names it: https://learn.chatgpt.com/docs/app-server.md | `crates/goose/src/providers/chatgpt_codex.rs` |
| `auth.x.ai` | xai_oauth | xAI sign-in | none found: the harness's source alone | `crates/goose/src/providers/xai_oauth.rs` |
| `chatgpt.com` | chatgpt_codex | ChatGPT Codex | names it: https://learn.chatgpt.com/docs/enterprise/connect-to-a-gateway.md | `crates/goose/src/providers/chatgpt_codex.rs` |
| `cloud-api.near.ai` | nearai | NEAR AI Cloud | names it: https://docs.near.ai/api-reference/models/get-model-details.md | `crates/goose-providers/src/declarative/definitions/nearai.json` |
| `cloudcode-pa.googleapis.com` | gemini_oauth | Gemini (Google sign-in, Code Assist) | names it: https://developers.google.com/gemini-code-assist/docs/network-access.md | `crates/goose/src/providers/gemini_oauth.rs` |
| `dashscope-intl.aliyuncs.com` | alibaba | Alibaba (Qwen) | names it: https://www.alibabacloud.com/help/en/model-studio/compatibility-of-openai-with-dashscope.md | `crates/goose-providers/src/declarative/definitions/alibaba.json` |
| `futurmix.ai` | futurmix | FuturMix | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/futurmix.json` |
| `generativelanguage.googleapis.com` | google | Gemini | names it: https://ai.google.dev/api.md | `crates/goose-providers/src/google.rs` |
| `github.com` | github_copilot | GitHub Copilot device sign-in | names it: https://docs.github.com/en/copilot/reference/copilot-allowlist-reference.md | `crates/goose/src/providers/githubcopilot.rs` |
| `huggingface.co` | huggingface | Hugging Face sign-in | names it: https://huggingface.co/docs/hub/oauth.md | `crates/goose/src/providers/huggingface_auth.rs` |
| `inference.celeris.ai` | celeris | Celeris | names it: https://docs.celeris.ai/api-reference.md | `crates/goose-providers/src/declarative/definitions/celeris.json` |
| `integrate.api.nvidia.com` | nvidia | NVIDIA | names it: https://docs.api.nvidia.com/nim/reference/llm-apis.md | `crates/goose-providers/src/declarative/definitions/nvidia.json` |
| `maas-token-api.cn-huabei-1.xf-yun.com` | iflytek_astron | iFlytek Astron MaaS | names it: https://www.xfyun.cn/doc/spark/TokenPlan.html | `crates/goose-providers/src/declarative/definitions/iflytek_astron.json` |
| `nano-gpt.com` | nano-gpt | NanoGPT | names it: https://docs.nano-gpt.com | `crates/goose/src/providers/nanogpt.rs` |
| `oai.endpoints.kepler.ai.cloud.ovh.net` | ovhcloud | OVHcloud | names it: https://www.ovhcloud.com/en/public-cloud/ai-endpoints/catalog// | `crates/goose-providers/src/declarative/definitions/ovhcloud.json` |
| `oauth2.googleapis.com` | gemini_oauth | Gemini (Google sign-in) token endpoint | names it: https://developers.google.com/identity/protocols/oauth2/native-app.md | `crates/goose/src/providers/gemini_oauth.rs` |
| `ollama.com` | ollama_cloud | Ollama Cloud | names it: https://docs.ollama.com/cloud.md | `crates/goose-providers/src/declarative/definitions/ollama_cloud.json`, `crates/goose/src/providers/ollama_cloud.rs` |
| `open.bigmodel.cn` | zhipu | Zhipu AI | names it: https://docs.bigmodel.cn/cn/guide/start/quick-start.md | `crates/goose-providers/src/declarative/definitions/zhipu.json` |
| `opencode.ai` | opencode_go opencode_zen | OpenCode Go \| OpenCode Zen \| OpenCode Zen and Go | names it: https://opencode.ai/docs/console/api/inference.md | `crates/goose-providers/src/declarative/definitions/opencode_go.json`, `crates/goose-providers/src/declarative/definitions/opencode_zen.json`, `crates/goose-providers/src/openai.rs` |
| `openrouter.ai` | openrouter | OpenRouter | names it: https://openrouter.ai/docs/quickstart.md | `crates/goose-providers/src/openrouter.rs`, `crates/goose/src/providers/openrouter_def.rs` |
| `router.huggingface.co` | huggingface | Hugging Face | names it: https://huggingface.co/docs/inference-providers/index.md | `crates/goose/src/providers/huggingface.rs` |
| `router.pleum.ai` | pleumrouter | PleumRouter | none found: the harness's source alone | `crates/goose-providers/src/declarative/definitions/pleumrouter.json` |
| `spark-api-open.xf-yun.com` | iflytek | iFlytek Spark | names it: https://www.xfyun.cn/doc/spark/HTTP%E8%B0%83%E7%94%A8%E6%96%87%E6%A1%A3.html | `crates/goose-providers/src/declarative/definitions/iflytek.json` |

Not granted:
- **A provider whose host the person's account, resource or region places:** Amazon Bedrock and SageMaker, Azure
  OpenAI and Azure AI Foundry, Databricks, Snowflake, GCP Vertex AI and VMware Tanzu.
- **A local or self-hosted server:** Ollama, LM Studio, Lemonade, Docker Model Runner, Ramalama, Atomic Chat, llama-swap,
  Lynkr, oMLX and LiteLLM.
- **A host the person gives:** any `*_HOST`, `*_BASE_URL` or custom provider. The Coding Plan URL that Astron's and
  Zhipu's descriptions offer through `ASTRON_BASE_URL` and `ZHIPU_BASE_URL` is one of these.
- **The CLI and ACP providers** (Claude Code, Codex, Cursor Agent, Gemini CLI, pi, Amp, Copilot ACP): each runs another
  agent, which reaches its own hosts in its own image.

Each of those needs an Agentfile of the person's own.

**Changed from part 2** (64 hosts):
- `www.googleapis.com` is dropped: Goose names it only in OAuth scope names (`gemini_oauth.rs`), never as a host it
  asks.
- Added, each the default of a provider part 2's scan missed because its address sits behind a variable:
  - `api.routstr.com` (Routstr, `ROUTSTR_HOST`);
  - `api.moonshot.cn` (Moonshot, `MOONSHOT_BASE_URL`);
  - `open.bigmodel.cn` (Zhipu, `ZHIPU_BASE_URL`);
  - `maas-token-api.cn-huabei-1.xf-yun.com` (iFlytek Astron, `ASTRON_BASE_URL`).
- `api.typesafe.ai` stays, now cited: Goose asks it for auto effort when `GOOSE_AUTO_EFFORT_ENABLED` is set with a
  `TYPESAFE_API_KEY` (`crates/goose/src/agents/services.rs`).

**Which provider a run uses** is for the launch (goal 60) to narrow to. Goose documents it as `GOOSE_PROVIDER` (and
`GOOSE_MODEL`), in the environment or in its `config.yaml` (`evidence/hosts/providers.md`, "Configure Provider and
Model"). Auto effort adds `api.typesafe.ai` when it's on.
