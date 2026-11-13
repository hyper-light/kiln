> ## AI Agent Context
> Get the complete documentation index at: https://aistudio.google.com/llms.txt
> Use this file to discover all available pages.
> For the full documentation in a single file, fetch: https://aistudio.google.com/llms-full.txt

# Gemini API reference

The Gemini API provides unary REST, streaming, and real-time WebSocket endpoints
for Gemini models and managed agents. Start new projects with the
[Interactions API](/api-reference/interactions-api), or follow the
[Get started guide](/docs/get-started) to send your first request.

For client libraries and editor tooling, see
[SDK references](/api-reference#sdk-references),
[Gemini API libraries](/docs/libraries), or
[Coding agent setup](/docs/coding-agents).

{% callout type="note" %}
  The latest Gemini API SDKs default to API version `v1`. Some features, such as
  the Live API and speech generation, are only available in `v1beta`. See
  [API versions](/api-reference/api-versions).

{% /callout %}

## Base URL and authentication  {% id="authentication" %}

{% two-column-layout %}
  {% left-column %}
  **To authenticate**, pass your [API key](/docs/api-key) in the `x-goog-api-key`
    header (or set `GEMINI_API_KEY` when using an SDK).

  **To call an endpoint**, send HTTPS requests to
    `https://generativelanguage.googleapis.com/{version}/{endpoint}` using
    [`v1beta` or `v1`](/api-reference/api-versions).

  {% table %}
  * Setting {% align="left" %}
  * Value {% align="left" %}

  ---

  * Base URL {% align="left" %}
  * `https://generativelanguage.googleapis.com` {% align="left" %}

  ---

  * WebSocket {% align="left" %}
  * `wss://generativelanguage.googleapis.com/ws/...` {% align="left" %}

  ---

  * Authentication {% align="left" %}
  * `x-goog-api-key: $GEMINI_API_KEY` {% align="left" %}

  ---

  * Versions {% align="left" %}
  * `v1beta` (new features and agents), `v1` (stable) {% align="left" %}
  {% /table %}

  {% button href="https://aistudio.google.com/apikey" variant="ais" %}Get API key{% /button %}

  {% /left-column %}

  {% right-column %}
  {% tabs %}
  {% tab label="REST" %}
  ```bash
  curl -X POST "https://generativelanguage.googleapis.com/v1beta/interactions" \
    -H "x-goog-api-key: $GEMINI_API_KEY" \
    -H "Content-Type: application/json" \
    -d '{
      "model": "gemini-3.8-flash",
      "input": "Explain how TCP congestion control works in two sentences."
    }'
  ```

  {% /tab %}

  {% tab label="Agents" %}
  ```bash
  # Set up your coding agent with these tools
  
  # Search live docs with the MCP server
  npx add-mcp "https://gemini-api-docs-mcp.dev"
  
  # Add skills for current SDK syntax and rules
  npx skills add google-gemini/gemini-skills --skill gemini-api-dev
  
  # In Antigravity, install both with one plugin
  agy plugin install https://github.com/google-gemini/gemini-skills
  
  # Fetch any reference page as clean Markdown
  curl -H "Accept: text/markdown" \
    "https://aistudio.google.com/api-reference/interactions-api"
  ```

  {% /tab %}

  {% /tabs %}

  {% /right-column %}

{% /two-column-layout %}

## API endpoints  {% id="primary-endpoints" %}

Start with a core surface below, browse endpoints by category, or view every
REST operation on one page in [All methods](/api-reference/all-methods).

{% card-grid columns="4" %}
  {% card description="Default endpoint for text, multimodal prompts, thinking, tools, stateful turns, and agents." href="/api-reference/interactions-api" icon="hub" subtitle="POST /v1beta/interactions" title="Interactions API" variant="stacked-icon" /%}

  {% card description="Low-latency WebSocket sessions for real-time voice, video, and interactive Lyria music." href="/api-reference/live" icon="graphic_eq" subtitle="WSS BidiGenerate*" title="Live and Live Music" variant="stacked-icon" /%}

  {% card description="Configure reusable agents, provision sandboxes, schedule cron runs, and register webhooks." href="/api-reference/agents" icon="smart_toy" subtitle="POST /v1beta/agents" title="Agents and automation" variant="stacked-icon" /%}

  {% card description="Single-page index of every REST operation across all Gemini API services." href="/api-reference/all-methods" icon="quick_reference_all" subtitle="/api-reference/all-methods" title="All REST methods" variant="stacked-icon" /%}

{% /card-grid %}

{% tabs %}
  {% tab label="Core and real-time" %}
  {% table %}
  * API
  * Endpoint
  * Description

  ---

  * [Interactions API](/api-reference/interactions-api)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/interactions`
  * Text, multimodal prompts, thinking, tools, stateful turns, background jobs, and SSE streaming.

  ---

  * [Live API](/api-reference/live)
  *   {% mark class="http-wss" %}
  {% icon name="sync_alt" /%}

`WSS`  {% /mark %}

 `BidiGenerateContent`
  * Low-latency WebSocket session for bidirectional voice, video, and tool calling.

  ---

  * [Live Music API](/api-reference/live-music)
  *   {% mark class="http-wss" %}
  {% icon name="sync_alt" /%}

`WSS`  {% /mark %}

 `BidiGenerateMusic`
  * Real-time WebSocket session for steering interactive instrumental music with Lyria.

  ---

  * [Models API](/api-reference/models)
  *   {% mark class="http-get" %}
  {% icon name="south_west" /%}

`GET`  {% /mark %}

 `/v1beta/models`
  * List models, inspect token limits, or run media predictions ([`predict`](/api-reference/models/predict), [`predictLongRunning`](/api-reference/models/predict-long-running)).
  {% /table %}

  {% /tab %}

  {% tab label="Agents and automation" %}
  {% table %}
  * API
  * Endpoint
  * Description

  ---

  * [Agents](/api-reference/agents)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/agents`
  * Create, retrieve, list, and delete reusable agent configurations.

  ---

  * [Environments](/api-reference/environments)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/environments`
  * Provision isolated remote sandboxes and manage workspace files for agent runs.

  ---

  * [Triggers](/api-reference/triggers)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/triggers`
  * Schedule recurring cron runs or invoke agent triggers on demand.

  ---

  * [Webhooks](/api-reference/webhooks)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/webhooks`
  * Register callback URLs and rotate signing secrets for background events.

  ---

  * [Credentials](/api-reference/credentials)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/credentials`
  * Store and manage external service credentials used by agents and tools.
  {% /table %}

  {% /tab %}

  {% tab label="Data, audio, and scale" %}
  {% table %}
  * API
  * Endpoint
  * Description

  ---

  * [Files API](/api-reference/files)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/files`
  * Upload and manage large audio, image, video, and document files for prompts.

  ---

  * [File Search](/api-reference/file-search/file-search-stores)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/fileSearchStores`
  * Chunk, index, and query [documents](/api-reference/file-search/documents) for managed retrieval grounding.

  ---

  * [Caching](/api-reference/caching)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/cachedContents`
  * Pin shared prompt context on supported models to reduce repeated input cost.

  ---

  * [Voices](/api-reference/voices)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/voices`
  * Create and manage custom prompted or replicated voices for text-to-speech.

  ---

  * [Batch API](/api-reference/batch-api)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/batches`
  * Submit asynchronous content generation and embedding jobs at lower cost.

  ---

  * [Embeddings](/api-reference/embeddings)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/models/{model}:embedContent`
  * Generate vector embeddings for semantic search, classification, and clustering.

  ---

  * [Tokens](/api-reference/tokens)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/models/{model}:countTokens`
  * Count input tokens before sending a request.
  {% /table %}

  {% /tab %}

  {% tab label=Legacy (generateContent) %}
  Start new projects on the [Interactions API](/api-reference/interactions-api) or
    follow the [Migration guide](/docs/migrate-to-interactions). Existing
    integrations remain supported on the endpoints below.

  {% table %}
  * API
  * Endpoint
  * Description

  ---

  * [`generateContent`](/api-reference/generate-content/generate-content)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/models/{model}:generateContent`
  * Unary text and multimodal generation for existing `generateContent` integrations.

  ---

  * [`streamGenerateContent`](/api-reference/generate-content/stream-generate-content)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1beta/models/{model}:streamGenerateContent`
  * Server-sent event streaming for existing `generateContent` integrations.

  ---

  * [`createToken`](/api-reference/generate-content/create-token)
  *   {% mark class="http-post" %}
  {% icon name="north_east" /%}

`POST`  {% /mark %}

 `/v1alpha/auth_tokens`
  * Provision short-lived ephemeral authentication tokens for client-side Live API sessions.
  {% /table %}

  {% /tab %}

{% /tabs %}

## SDK references  {% id="sdk-references" %}

Generated client, method, and type documentation for the official Google GenAI
SDKs.

{% card-grid columns="3" %}
  {% card description="Reference for the google-genai Python SDK, including sync and async clients." href="https://googleapis.github.io/python-genai/" subtitle="pip install google-genai" title="Python" variant="stacked-icon" /%}

  {% card description="Reference for the @google/genai SDK for Node.js and TypeScript." href="https://googleapis.github.io/js-genai/" subtitle="npm install @google/genai" title="TypeScript and JavaScript" variant="stacked-icon" /%}

  {% card description="Package reference for google.golang.org/genai on pkg.go.dev." href="https://pkg.go.dev/google.golang.org/genai" subtitle="go get google.golang.org/genai" title="Go" variant="stacked-icon" /%}

  {% card description="Javadoc reference for the com.google.genai Java SDK." href="https://googleapis.github.io/java-genai/javadoc/" subtitle="com.google.genai:google-genai" title="Java" variant="stacked-icon" /%}

  {% card description="Reference for the Google.GenAI .NET SDK." href="https://googleapis.github.io/dotnet-genai/" subtitle="dotnet add package Google.GenAI" title=C# (.NET) variant="stacked-icon" /%}

  {% card description="Install SDK packages, configure environment variables, or migrate from legacy libraries." href="/docs/libraries" subtitle="/docs/libraries" title="Installation and migration" variant="stacked-icon" /%}

{% /card-grid %}

