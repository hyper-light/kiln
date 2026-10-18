//! What a rollout's checks measure (docs/cd-targets.md §5): an HTTP probe or a Prometheus query
//! from the server, judged by the check's criteria (`pass:` and `fail:`, `${{ }}` expressions in
//! the language workflows use, over `result` and `rollout`), or by its kind's default without
//! them. A person's approval (`approve:`) is answered through the API, not here. Strings in a
//! check read `${{ rollout.* }}` (the preview's address, the revision) when it measures.

use std::time::{Duration, Instant};

use kiln_cd::rollout::{CheckSpec, Measurement, Verdict};
use kiln_ci::expr::{self, Budget, Condition, Dialect, MapContexts, ParseOptions, Template};
use serde_json::{Map, Value, json};

use crate::http::client::{Client, Method};

/// The most of a response kept as `result.body` (and read for `result.json`).
const BODY: usize = 1 << 20;

/// Whether kiln measures a kind of check yet: a rollout using another is refused before it moves
/// any traffic.
pub fn measurable(spec: &CheckSpec) -> Result<(), String> {
    match spec.kind()? {
        "http" | "approve" => Ok(()),
        "metric" if spec.metric.as_ref().and_then(|m| m.get("prometheus")).is_some() => Ok(()),
        "metric" => Err("a metric check measures Prometheus for now (CloudWatch, Cloud Monitoring, Azure Monitor, Cloudflare and Datadog are not built yet)".into()),
        k => Err(format!("{k} checks are not built yet: use http, metric (Prometheus) or approve")),
    }
}

/// Measures a check once (`n`-th time) with the rollout's context (`rollout.*`).
pub async fn measure(spec: &CheckSpec, rollout: &Value, timeout: Duration) -> Measurement {
    let at_ms = super::now_ms();
    let measured = match spec.kind() {
        Ok("http") => http(spec, rollout, timeout).await,
        Ok("metric") => prometheus(spec, rollout, timeout).await,
        Ok(k) => Err(format!("{k} checks are not measured here")),
        Err(e) => Err(e),
    };
    let (result, default) = match measured {
        Ok(x) => x,
        // It couldn't measure: an error, tried again, not a failure.
        Err(e) => return Measurement { at_ms, verdict: Verdict::Error, value: Value::Null, message: e },
    };
    let (verdict, message) = judge(spec, &result, rollout, default);
    Measurement { at_ms, verdict, value: result, message }
}

/// A verdict by the check's criteria: `fail:` holding fails; `pass:` holding passes; with
/// criteria and neither holding, it can't tell. Without criteria, the kind's default.
fn judge(spec: &CheckSpec, result: &Value, rollout: &Value, default: (Verdict, String)) -> (Verdict, String) {
    if spec.pass.is_none() && spec.fail.is_none() {
        return default;
    }
    let cx = MapContexts::default().with("result", expr::Value::from_json(result)).with("rollout", expr::Value::from_json(rollout));
    let holds = |src: &str| -> Result<bool, String> {
        let c = Condition::parse(src, &ParseOptions::permissive(Dialect::Kiln)).map_err(|e| e.located(src))?;
        expr::eval_condition(&c, &cx, &mut Budget::default()).map_err(|e| e.located(src))
    };
    if let Some(f) = &spec.fail {
        match holds(f) {
            Ok(true) => return (Verdict::Fail, format!("fail: {f} holds")),
            Ok(false) => {}
            Err(e) => return (Verdict::Error, format!("fail: {e}")),
        }
    }
    match &spec.pass {
        Some(p) => match holds(p) {
            Ok(true) => (Verdict::Pass, String::new()),
            Ok(false) if spec.fail.is_some() => (Verdict::Inconclusive, "neither pass: nor fail: holds".into()),
            Ok(false) => (Verdict::Fail, format!("pass: {p} doesn't hold")),
            Err(e) => (Verdict::Error, format!("pass: {e}")),
        },
        None => (Verdict::Pass, String::new()),
    }
}

/// A check's strings with `${{ rollout.* }}` filled in.
fn fill(v: &Value, rollout: &Value) -> Result<Value, String> {
    let cx = MapContexts::default().with("rollout", expr::Value::from_json(rollout));
    fn go(v: &Value, cx: &MapContexts) -> Result<Value, String> {
        Ok(match v {
            Value::String(s) if s.contains("${{") => {
                let t = Template::parse_with(s, &ParseOptions::permissive(Dialect::Kiln)).map_err(|e| e.located(s))?;
                Value::String(expr::render(&t, cx, &mut Budget::default()).map_err(|e| e.located(s))?)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| go(x, cx)).collect::<Result<_, _>>()?),
            Value::Object(m) => Value::Object(m.iter().map(|(k, x)| Ok((k.clone(), go(x, cx)?))).collect::<Result<Map<_, _>, String>>()?),
            other => other.clone(),
        })
    }
    go(v, &cx)
}

fn method(m: &str) -> Result<Method, String> {
    Ok(match m.to_ascii_uppercase().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        "HEAD" => Method::Head,
        other => return Err(format!("method {other} isn't one kiln sends")),
    })
}

/// `http: { url, method, headers, body }`: its status, headers, body (and JSON, when it is) and
/// how long it took. Without criteria a 2xx passes.
async fn http(spec: &CheckSpec, rollout: &Value, timeout: Duration) -> Result<(Value, (Verdict, String)), String> {
    let h = fill(spec.http.as_ref().unwrap_or(&Value::Null), rollout)?;
    let url = h.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()).ok_or("an http check needs a url")?;
    let client = Client::public().map_err(|e| format!("{e:#}"))?;
    let mut req = client.request(method(h.get("method").and_then(Value::as_str).unwrap_or("GET"))?, url).timeout(timeout);
    for (k, v) in h.get("headers").and_then(Value::as_object).into_iter().flatten() {
        req = req.header(k, &v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    }
    match h.get("body") {
        Some(Value::String(s)) => req = req.body(s.clone().into_bytes()),
        Some(v @ (Value::Object(_) | Value::Array(_))) => req = req.json(v),
        _ => {}
    }
    let started = Instant::now();
    let resp = req.send().await.map_err(|e| format!("{url}: {e:#}"))?;
    let status = resp.status();
    let headers: Map<String, Value> = resp.headers().into_iter().map(|(k, v)| (k, Value::String(v))).collect();
    let bytes = resp.bytes(BODY).await.map_err(|e| format!("{url}: reading the body: {e:#}"))?;
    let latency_ms = started.elapsed().as_millis() as u64;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let result = json!({ "status": status, "headers": headers, "body": body, "json": json, "latency_ms": latency_ms });
    let default = if (200..300).contains(&status) { (Verdict::Pass, String::new()) } else { (Verdict::Fail, format!("HTTP {status}")) };
    Ok((result, default))
}

/// `metric: { prometheus: { query, address } }`: one sample's value as a number, several as a
/// list of `{ metric, value }`, none as null. A query has no default: without criteria it
/// can't tell.
async fn prometheus(spec: &CheckSpec, rollout: &Value, timeout: Duration) -> Result<(Value, (Verdict, String)), String> {
    let m = fill(spec.metric.as_ref().and_then(|m| m.get("prometheus")).unwrap_or(&Value::Null), rollout)?;
    let query = m.get("query").and_then(Value::as_str).filter(|q| !q.is_empty()).ok_or("a Prometheus check needs a query")?;
    let address = m.get("address").and_then(Value::as_str).ok_or("a Prometheus check needs its address (http://prometheus:9090)")?;
    let url = format!("{}/api/v1/query?query={}", address.trim_end_matches('/'), kiln_cd::kube::enc(query));
    let client = Client::public().map_err(|e| format!("{e:#}"))?;
    let resp = client.request(Method::Get, &url).timeout(timeout).send().await.map_err(|e| format!("{address}: {e:#}"))?;
    let status = resp.status();
    let bytes = resp.bytes(BODY).await.map_err(|e| format!("{address}: reading the answer: {e:#}"))?;
    let answer: Value = serde_json::from_slice(&bytes).map_err(|_| format!("{address} answered HTTP {status}, not Prometheus's JSON"))?;
    if answer.get("status").and_then(Value::as_str) != Some("success") {
        return Err(format!("Prometheus: {}", answer.get("error").and_then(Value::as_str).unwrap_or("the query failed")));
    }
    let num = |v: &Value| v.get(1).and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()).map_or(Value::Null, |f| json!(f));
    let data = answer.get("data").cloned().unwrap_or(Value::Null);
    let result = match data.get("resultType").and_then(Value::as_str) {
        Some("scalar") => num(data.get("result").unwrap_or(&Value::Null)),
        Some("vector") => {
            let samples = data.get("result").and_then(Value::as_array).cloned().unwrap_or_default();
            match samples.as_slice() {
                [] => Value::Null,
                [one] => num(one.get("value").unwrap_or(&Value::Null)),
                many => Value::Array(many.iter().map(|s| json!({ "metric": s.get("metric"), "value": num(s.get("value").unwrap_or(&Value::Null)) })).collect()),
            }
        }
        _ => data.get("result").cloned().unwrap_or(Value::Null),
    };
    Ok((result, (Verdict::Inconclusive, "a metric check needs pass: or fail: to judge it".into())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(yaml: &str) -> CheckSpec {
        serde_yaml::from_str(yaml).unwrap_or_default()
    }

    #[test]
    fn criteria_judge_and_default_without_them() {
        let r = json!({ "weight": 20, "preview_url": "https://green.example" });
        let result = json!({ "status": 200, "json": { "db": "ok" }, "latency_ms": 120 });
        let ok = (Verdict::Pass, String::new());
        assert_eq!(judge(&spec("http: {}"), &result, &r, ok.clone()).0, Verdict::Pass, "no criteria: the kind's default");
        let s = spec("http: {}\npass: \"${{ result.status == 200 && result.json.db == 'ok' && result.latency_ms < 300 }}\"");
        assert_eq!(judge(&s, &result, &r, ok.clone()).0, Verdict::Pass);
        let s = spec("http: {}\npass: \"${{ result.latency_ms < 100 }}\"");
        assert_eq!(judge(&s, &result, &r, ok.clone()).0, Verdict::Fail, "pass: alone that doesn't hold fails");
        let s = spec("http: {}\npass: \"${{ result.latency_ms < 100 }}\"\nfail: \"${{ result.status >= 500 }}\"");
        assert_eq!(judge(&s, &result, &r, ok.clone()).0, Verdict::Inconclusive, "neither holds");
        let s = spec("http: {}\nfail: \"${{ rollout.weight > 10 && result.latency_ms > 100 }}\"");
        assert_eq!(judge(&s, &result, &r, ok.clone()).0, Verdict::Fail, "rollout.* in criteria");
        let s = spec("http: {}\nfail: \"${{ result.status == \"");
        assert_eq!(judge(&s, &result, &r, ok).0, Verdict::Error, "a broken expression is an error");
    }

    #[test]
    fn strings_read_the_rollout() -> Result<(), String> {
        let r = json!({ "preview_url": "https://green.example", "revision": "abc123" });
        let h = fill(&json!({ "url": "${{ rollout.preview_url }}/healthz", "headers": { "x-rev": "${{ rollout.revision }}" }, "n": 3 }), &r)?;
        assert_eq!(h, json!({ "url": "https://green.example/healthz", "headers": { "x-rev": "abc123" }, "n": 3 }));
        assert!(measurable(&spec("script: { run: ./e2e.sh }")).is_err_and(|e| e.contains("not built yet")));
        assert!(measurable(&spec("metric: { cloudwatch: {} }")).is_err());
        assert!(measurable(&spec("metric: { prometheus: { query: up } }")).is_ok());
        Ok(())
    }
}
