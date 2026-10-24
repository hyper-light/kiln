What dd-trace-py 4.16.0 (with openai 3.37.0 and anthropic 2.3.0) sent of an LLM app, captured
2026-10-23 on a container's loopback (`docker run --network none`): capture.py.txt is the app, a
fake model server answering OpenAI's and Anthropic's APIs with fixed responses, and a stand-in for
the Datadog Agent that kept each request. Every value is the capture's own, nothing personal in it.

- dd-trace-py-4.16.0-llmobs.json: POST /evp_proxy/v2/api/v2/llmobs (X-Datadog-EVP-Subdomain:
  llmobs-intake), APM off (DD_APM_TRACING_ENABLED=false): the LLM events through the Agent's proxy.
- dd-trace-py-4.16.0-eval-metric.json: POST /evp_proxy/v2/api/intake/llm-obs/v2/eval-metric, the
  same run: the evaluation the app submitted.
- dd-trace-py-4.16.0-apm-traces.msgpack: POST /v0.4/traces, APM on and the Agent's /info listing
  /evp_proxy/v2/: the events riding their APM spans (meta_struct["_llmobs"], MessagePack).
