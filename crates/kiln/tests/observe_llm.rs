//! LLM Observability, live (docs/observe.md §8.12, §13): an LLM app (a planner agent that asks
//! OpenAI's chat model, calls a weather tool, asks Anthropic's model and embeds its question, under
//! a workflow of a session, then judges its answer) against a fake model server on its container's
//! loopback (fixed answers: the chat 1,200 input tokens, 1,000 of them cached, and 40 out; Anthropic
//! 20 uncached, 1,000 read from its cache and 100 written to it, 40 of them for an hour), traced each
//! way an app is, each its own app:
//!
//! - Datadog's tracer (dd-trace-py 4.16, its LLM Observability on) sending to a kiln runner as to
//!   its Agent: with APM on, its LLM events ride its APM spans; with APM off, they go through the
//!   runner's event platform proxy, as its evaluation does;
//! - OpenTelemetry's GenAI instrumentation of the OpenAI client (openai-v2 2.4b0, opted in to the
//!   latest conventions) and Arize's OpenInference (0.1.66), exporting OTLP to the runner.
//!
//! What a user relies on: each call is in kiln as the LLM span it is (its kind, model and tokens),
//! priced as the catalog prices it, to the nanodollar (each as its instrumentation says its tokens:
//! openai-v2 says no cache reads); its trace's steps in their tree; its evaluation beside it; and
//! Datadog's `ml_obs` metrics count every call once.
//!
//! It needs Docker: `cargo test -p kiln --test observe_llm -- --ignored --nocapture`.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use common::{Fleet, at, docker, free_port, now_ms};
use serde_json::{Value, json};

const IMAGE: &str = "kiln-llm-apps:e2e";
/// Datadog's tracer's evaluation as it sends it (kiln-obs's fixture), of app `kiln-capture`.
const EVAL_PAYLOAD: &[u8] = include_bytes!("../../kiln-obs/src/fixtures/llmobs/dd-trace-py-4.16.0-eval-metric.json");
const DOCKERFILE: &str = "FROM python:3.12-slim\nRUN pip install --no-cache-dir ddtrace==4.16.0 openai==3.37.0 anthropic==2.3.0 httpx opentelemetry-sdk==1.45.1 \
opentelemetry-exporter-otlp-proto-http==1.45.1 opentelemetry-instrumentation-openai-v2==2.4b0 opentelemetry-util-genai==0.4b0 openinference-instrumentation-openai==0.1.66\n";

const APP: &str = r#"
import http.server, json, logging, socketserver, sys, threading, time

# What the tracer says of its sends (a payload refused, retried, given up on), where the test reads it.
logging.basicConfig(stream=sys.stdout, level=logging.WARNING, format="LOG %(name)s %(levelname)s %(message)s")

MODE = sys.argv[1]

class Model(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(n)
        if self.path.endswith("/chat/completions"):
            r = {"id": "chatcmpl-e2e", "object": "chat.completion", "created": 1792800000, "model": "gpt-4o-mini-2024-07-18", "service_tier": "default",
                 "choices": [{"index": 0, "message": {"role": "assistant", "content": "Paris: 18C and clear."}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 1200, "completion_tokens": 40, "total_tokens": 1240, "prompt_tokens_details": {"cached_tokens": 1000}, "completion_tokens_details": {"reasoning_tokens": 10}}}
        elif self.path.endswith("/embeddings"):
            r = {"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}], "model": "text-embedding-3-small", "usage": {"prompt_tokens": 8, "total_tokens": 8}}
        elif self.path.endswith("/messages"):
            r = {"id": "msg_e2e", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "Clear in Paris."}], "stop_reason": "end_turn", "stop_sequence": None,
                 "usage": {"input_tokens": 20, "output_tokens": 40, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 100, "cache_creation": {"ephemeral_5m_input_tokens": 60, "ephemeral_1h_input_tokens": 40}}}
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers(); return
        b = json.dumps(r).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)

s = socketserver.ThreadingTCPServer(("127.0.0.1", int(sys.argv[2])), Model)
s.daemon_threads = True
threading.Thread(target=s.serve_forever, daemon=True).start()
BASE = f"http://127.0.0.1:{sys.argv[2]}"

if MODE == "dd":
    from ddtrace.llmobs import LLMObs
    from ddtrace.llmobs.decorators import agent, tool, workflow
    LLMObs.enable(integrations_enabled=True)
    import anthropic, openai
    oc = openai.OpenAI(base_url=BASE + "/v1", api_key="sk-e2e")
    ac = anthropic.Anthropic(base_url=BASE, api_key="sk-ant-e2e")

    @tool(name="weather")
    def weather(city):
        LLMObs.annotate(input_data={"city": city}, output_data="18C clear")
        return "18C clear"

    @agent(name="planner")
    def plan(q):
        r = oc.chat.completions.create(model="gpt-4o-mini", messages=[{"role": "system", "content": "You are terse."}, {"role": "user", "content": q}], temperature=0.2)
        weather("Paris")
        ac.messages.create(model="claude-opus-5-5", max_tokens=100, messages=[{"role": "user", "content": q}])
        oc.embeddings.create(model="text-embedding-3-small", input="weather in Paris")
        return r.choices[0].message.content

    @workflow(name="answer", session_id="sess-e2e")
    def answer(q):
        out = plan(q)
        LLMObs.annotate(input_data=q, output_data=out)
        return LLMObs.export_span()

    exported = answer("Weather in Paris?")
    LLMObs.submit_evaluation(label="accuracy", metric_type="score", value=0.8, span=exported, assessment="pass", reasoning="matches the forecast")
    LLMObs.flush()
    try:
        from ddtrace.trace import tracer
    except ImportError:
        from ddtrace import tracer
    tracer.flush()
    time.sleep(2)
    LLMObs.disable()
    print("IDS " + json.dumps(exported), flush=True)
else:
    from opentelemetry import trace
    from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
    from opentelemetry.sdk.resources import Resource
    from opentelemetry.sdk.trace import TracerProvider
    from opentelemetry.sdk.trace.export import SimpleSpanProcessor
    p = TracerProvider(resource=Resource.create({"service.name": sys.argv[3]}))
    p.add_span_processor(SimpleSpanProcessor(OTLPSpanExporter(endpoint=sys.argv[4])))
    trace.set_tracer_provider(p)
    if MODE == "openinference":
        from openinference.instrumentation.openai import OpenAIInstrumentor
    else:
        from opentelemetry.instrumentation.openai_v2 import OpenAIInstrumentor
    OpenAIInstrumentor().instrument()
    import openai
    c = openai.OpenAI(base_url=BASE + "/v1", api_key="sk-e2e")
    c.chat.completions.create(model="gpt-4o-mini", messages=[{"role": "system", "content": "You are terse."}, {"role": "user", "content": "Weather in Paris?"}], temperature=0.2)
    c.embeddings.create(model="text-embedding-3-small", input="weather in Paris")
    p.shutdown()
    print("IDS {}", flush=True)
"#;

fn until(what: &str, within: Duration, mut done: impl FnMut() -> anyhow::Result<bool>, logs: &dyn Fn() -> String) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if done()? {
            return Ok(());
        }
        anyhow::ensure!(start.elapsed() < within, "{what}\n{}", logs());
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn spans(f: &Fleet, query: &str, from: i64) -> anyhow::Result<Vec<Value>> {
    let found = f.call("POST", "/api/observe/v1/spans/search", Some(&json!({ "query": query, "from": from, "to": now_ms() + 60_000, "limit": 1000 })))?;
    Ok(at(&found, &["spans"]).as_array().cloned().unwrap_or_default())
}

fn attr<'a>(span: &'a Value, key: &str) -> &'a str {
    at(span, &["attributes", key]).as_str().unwrap_or("")
}

/// Every value of every series `q` gives from `from` to `to`, summed.
fn metric_sum(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<f64> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to, "step": 10_000 })))?;
    Ok(at(&a, &["series"]).as_array().into_iter().flatten().flat_map(|s| at(s, &["values"]).as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_f64()).sum())
}

/// The app run once, in `mode`, as app `app`: its Datadog tracer's Agent the runner's receiver on
/// `apm`, its OTLP exporter the runner's on `otlp`; `env` besides. What it printed of its ids, and
/// what its libraries logged (warnings and errors: a send refused, given up on).
fn run_app(mode: &str, app: &str, apm: u16, otlp: u16, env: &[(&str, &str)]) -> anyhow::Result<(Value, String)> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("app.py"), APP)?;
    #[cfg(unix)]
    Command::new("chmod").args(["-R", "a+rX"]).arg(dir.path()).output()?;
    // The app in a container (its model a server of its own, at its own 127.0.0.1), the runner's
    // receivers where the container reaches this machine.
    let net = common::DockerNet::get();
    let mut args: Vec<String> = vec!["run".into(), "--rm".into()];
    args.extend(net.args(&[], &[]));
    args.extend(["--user", "1000:1000", "-e", "HOME=/tmp", "-v"].map(str::to_owned));
    args.push(format!("{}:/e2e:ro", dir.path().display()));
    let mut all = vec![
        ("DD_AGENT_HOST", net.host().to_owned()),
        ("DD_TRACE_AGENT_PORT", apm.to_string()),
        ("DD_SERVICE", app.to_owned()),
        ("DD_LLMOBS_ML_APP", app.to_owned()),
        ("DD_ENV", "e2e".to_owned()),
        ("DD_REMOTE_CONFIGURATION_ENABLED", "false".to_owned()),
        ("DD_INSTRUMENTATION_TELEMETRY_ENABLED", "false".to_owned()),
        ("OTEL_SEMCONV_STABILITY_OPT_IN", "gen_ai_latest_experimental".to_owned()),
        ("OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT", "SPAN_ONLY".to_owned()),
    ];
    all.extend(env.iter().map(|(k, v)| (*k, (*v).to_owned())));
    for (k, v) in all {
        args.push("-e".to_owned());
        args.push(format!("{k}={v}"));
    }
    let model = free_port()?.to_string();
    let endpoint = format!("http://{}:{otlp}/v1/traces", net.host());
    args.extend([IMAGE, "python", "-u", "/e2e/app.py", mode, &model, app, &endpoint].map(str::to_owned));
    let out = docker(&args.iter().map(String::as_str).collect::<Vec<_>>(), None)?;
    let ids = out.lines().find_map(|l| l.strip_prefix("IDS ")).map(serde_json::from_str).transpose()?.ok_or_else(|| anyhow::anyhow!("the app's ids:\n{out}"))?;
    Ok((ids, out.lines().filter(|l| l.starts_with("LOG ")).collect::<Vec<_>>().join("\n")))
}

/// Each call of `app`'s spans priced as the catalog prices it: `want` (each step's kind and name,
/// the nanodollars it cost, or none); the costs counted by `ml_obs` too, once.
fn priced(f: &Fleet, app: &str, t0: i64, want: &[(&str, &str, Option<&str>)]) -> anyhow::Result<Vec<Value>> {
    let q = format!("@ml_app:{app} @meta.span.kind:*");
    until(&format!("{app}'s {} LLM steps", want.len()), Duration::from_secs(90), || Ok(spans(f, &q, t0)?.len() >= want.len()), &|| f.logs())?;
    let got = spans(f, &q, t0)?;
    anyhow::ensure!(got.len() == want.len(), "{app}: {} steps, not {}: {got:#?}", got.len(), want.len());
    let name = |s: &Value| -> String {
        let own = attr(s, "llmobs.name");
        (if own.is_empty() { at(s, &["message"]).as_str().unwrap_or("") } else { own }).to_owned()
    };
    let mut total = 0.0;
    for (kind, step, cost) in want {
        let s = got.iter().find(|s| attr(s, "meta.span.kind") == *kind && name(s) == *step).ok_or_else(|| anyhow::anyhow!("{app}: no {kind} step {step}: {got:#?}"))?;
        anyhow::ensure!(attr(s, "metrics.estimated_total_cost") == cost.unwrap_or(""), "{app}'s {step}: cost {:?}, not {cost:?}: {s:#}", attr(s, "metrics.estimated_total_cost"));
        total += cost.map_or(0.0, |c| c.parse::<f64>().unwrap_or(0.0));
    }
    // Datadog's metrics: every call's cost, counted once its window is whole.
    let to = now_ms() + 60_000;
    let q = format!("sum:ml_obs.span.llm.total.cost.sum{{ml_app:{app}}}.as_count()");
    until(&format!("{app}'s cost in ml_obs"), Duration::from_secs(90), || Ok((metric_sum(f, &q, t0, to)? - total).abs() < 0.5), &|| {
        format!("{} of {total}\n{}", metric_sum(f, &q, t0, to).unwrap_or(-1.0), f.logs())
    })?;
    let calls = metric_sum(f, &format!("sum:ml_obs.span{{ml_app:{app}}}.as_count()"), t0, to)?;
    anyhow::ensure!((calls - want.len() as f64).abs() < 0.5, "{app}: ml_obs.span {calls}, not {}", want.len());
    Ok(got)
}

#[test]
#[ignore = "needs Docker: runs Datadog's Python tracer and OpenTelemetry's and OpenInference's instrumentations, and a kiln server and runner as processes"]
fn an_llm_app_s_calls_in_every_convention_are_kiln_s_llm_spans_priced_to_the_nanodollar() -> anyhow::Result<()> {
    let t0 = now_ms() - 60_000;
    docker(&["build", "-q", "-t", IMAGE, "-"], Some(DOCKERFILE))?;
    let versions = docker(
        &["run", "--rm", IMAGE, "sh", "-c", "pip list 2>/dev/null | grep -i -E '^(ddtrace|openai|anthropic|opentelemetry-(sdk|instrumentation-openai-v2)|openinference-instrumentation-openai) '"],
        None,
    )?;
    eprintln!("  apps       {}", versions.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut f = Fleet::new()?;
    let apm = free_port()?;
    f.start_runner_with("web-1", &[("KILN_DD_TRACE_ADDR", &format!("127.0.0.1:{apm}"))])?;
    let otlp = *f.otlp.get("web-1").ok_or_else(|| anyhow::anyhow!("the runner's OTLP port"))?;
    until("the runner's receiver", Duration::from_secs(60), || Ok(common::raw(apm, "GET", "/info", "", b"").is_ok_and(|(c, _)| c == 200)), &|| f.logs())?;
    until(
        "the runner replicating",
        Duration::from_secs(60),
        || Ok(at(&f.call("GET", "/api/observe/v1/status", None)?, &["machines"]).as_array().is_some_and(|m| m.iter().any(|m| at(m, &["up"]) == true))),
        &|| f.logs(),
    )?;
    // What a tracer reads to send its LLM events through this node.
    let (_, info) = common::raw(apm, "GET", "/info", "", b"")?;
    let info: Value = serde_json::from_slice(&info)?;
    anyhow::ensure!(at(&info, &["endpoints"]).as_array().is_some_and(|e| e.iter().any(|e| e == "/evp_proxy/v2/")) && at(&info, &["span_meta_structs"]) == true, "{info}");

    // Datadog's tracer, APM on: its events ride its APM spans, read in place (each step its own
    // APM span, its LLM trace said beside it); its evaluation through the proxy.
    let dd = [
        ("workflow", "answer", None),
        ("agent", "planner", None),
        ("llm", "OpenAI.createChatCompletion", Some("129000")),
        ("tool", "weather", None),
        ("llm", "anthropic.request", Some("1700000")),
        // Its provider named by none (`unknown`, a client of another address), its model two of the
        // catalog's providers': unpriced, and said so.
        ("embedding", "OpenAI.createEmbedding", None),
    ];
    let (exported, logged) = run_app("dd", "llm-dd-apm", apm, otlp, &[])?;
    let got = priced(&f, "llm-dd-apm", t0, &dd)?;
    anyhow::ensure!(got.iter().all(|s| !attr(s, "llmobs.trace_id").is_empty()), "every step its LLM trace's: {got:#?}");
    let chat = got.iter().find(|s| attr(s, "meta.span.kind") == "llm" && attr(s, "meta.model_name").starts_with("gpt")).ok_or_else(|| anyhow::anyhow!("the chat"))?;
    anyhow::ensure!(
        attr(chat, "metrics.cache_read_input_tokens") == "1000" && attr(chat, "matched_model_name") == "gpt-4o-mini" && attr(chat, "meta.input.messages").contains("You are terse."),
        "{chat:#}"
    );
    // Its evaluation: a record of the span it judges, through the runner's proxy.
    let search = |q: &str| f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": q, "from": t0, "to": now_ms() + 60_000, "limit": 20 })));
    let mut evals = Value::Null;
    until(
        "the evaluation, a record",
        Duration::from_secs(60),
        || {
            evals = search("source:llmobs.eval service:llm-dd-apm")?;
            Ok(at(&evals, &["records"]).as_array().is_some_and(|r| !r.is_empty()))
        },
        &|| {
            // Whether the runner takes one: the tracer's own payload, of now, posted as it posts it.
            let probe = String::from_utf8_lossy(EVAL_PAYLOAD).replace("1792817084564", &now_ms().to_string());
            let posted = common::raw(apm, "POST", "/evp_proxy/v2/api/intake/llm-obs/v2/eval-metric", "Content-Type: application/json\r\n", probe.as_bytes())
                .map_or_else(|e| e.to_string(), |(c, b)| format!("{c} {}", String::from_utf8_lossy(&b)));
            std::thread::sleep(Duration::from_secs(3));
            format!(
                "the app logged: {logged}\na probe posted to the runner: {posted}\nevaluations of any app: {}\nthe app's records: {}\n{}",
                search("source:llmobs.eval").map(|v| v.to_string()).unwrap_or_default(),
                search("service:llm-dd-apm").map(|v| v.to_string()).unwrap_or_default(),
                f.logs()
            )
        },
    )?;
    let eval = at(&evals, &["records"]).as_array().and_then(|r| r.first()).cloned().ok_or_else(|| anyhow::anyhow!("the evaluation: {evals}"))?;
    anyhow::ensure!(at(&eval, &["attributes", "label"]) == "accuracy" && at(&eval, &["attributes", "value"]) == "0.8" && at(&eval, &["attributes", "assessment"]) == "pass", "{eval:#}");
    anyhow::ensure!(at(&exported, &["trace_id"]).as_str().is_some_and(|t| got.iter().all(|s| attr(s, "llmobs.trace_id") == t)), "the evaluation's trace is its steps': {exported}");
    eprintln!("  dd apm     six steps riding their APM spans, each priced; the evaluation beside them");

    // APM off: every event through the proxy, each a span of its LLM trace.
    run_app("dd", "llm-dd-proxy", apm, otlp, &[("DD_APM_TRACING_ENABLED", "false")])?;
    let proxied = priced(&f, "llm-dd-proxy", t0, &dd)?;
    anyhow::ensure!(proxied.iter().all(|s| at(s, &["source"]) == "llmobs"), "{proxied:#?}");
    let trace = proxied.first().and_then(|s| at(s, &["trace_id"]).as_str()).unwrap_or("").to_owned();
    let t = f.call("GET", &format!("/api/observe/v1/traces/{trace}"), None)?;
    anyhow::ensure!(at(&t, &["spans"]).as_array().is_some_and(|s| s.len() == 6), "its LLM trace, its six steps: {t}");
    eprintln!("  dd proxy   six steps through the event platform proxy, one LLM trace of six");

    // OpenTelemetry's GenAI instrumentation: no cache reads said, every input token the input's.
    run_app("otel", "llm-otel", apm, otlp, &[])?;
    let got = priced(&f, "llm-otel", t0, &[("llm", "chat gpt-4o-mini", Some("204000")), ("embedding", "embeddings text-embedding-3-small", Some("160"))])?;
    anyhow::ensure!(got.iter().any(|s| attr(s, "gen_ai.input.messages").contains("You are terse.")), "{got:#?}");
    eprintln!("  openai-v2  its chat and embedding, priced as its spans say their tokens");

    // OpenInference: its cache reads said.
    run_app("openinference", "llm-openinference", apm, otlp, &[])?;
    priced(&f, "llm-openinference", t0, &[("llm", "ChatCompletion", Some("129000")), ("embedding", "CreateEmbeddings", Some("160"))])?;
    eprintln!("  openinference its chat and embedding, its cache reads priced as theirs");
    Ok(())
}
