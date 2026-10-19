//! Observe's cloud integrations, live (docs/observe.md §8.4, §13): a kiln server as a process of its
//! own, pulling from services that stand in for AWS (STS, CloudWatch, the instance metadata
//! service), Google (OAuth, Cloud Monitoring, the metadata server) and Azure (Entra, Resource
//! Manager, the VM's metadata service). Each checks what the real one checks (a signature over the
//! request, a signed assertion, a secret, a token) and answers with figures that are a function of
//! their time, so what kiln stores can be checked point by point; and their logs (CloudWatch Logs,
//! Cloud Logging, Log Analytics), each record of which must be stored once. And Prometheus's
//! scrapes: an exporter served here, read by the server and a runner as its targets say.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Fleet, at, raw};
use serde_json::{Value, json};

// ------------------------------------------------------------------------------------ services

#[derive(Debug, Clone)]
struct Req {
    method: String,
    /// The path without its query.
    path: String,
    query: BTreeMap<String, String>,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn form(&self) -> BTreeMap<String, String> {
        pairs(&String::from_utf8_lossy(&self.body))
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn reply(status: u16, v: &Value) -> Resp {
    Resp { status, headers: vec![("content-type".into(), "application/json".into())], body: serde_json::to_vec(v).unwrap() }
}

fn text(status: u16, t: &str) -> Resp {
    Resp { status, headers: vec![("content-type".into(), "text/plain".into())], body: t.as_bytes().to_vec() }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00"), 16).unwrap_or(0));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pairs(s: &str) -> BTreeMap<String, String> {
    s.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((decode(p), String::new()), |(k, v)| (decode(k), decode(v)))).collect()
}

/// A service on a port of its own: each request to `handle`, each kept.
struct Service {
    port: u16,
    seen: Arc<Mutex<Vec<Req>>>,
}

impl Service {
    fn start(handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        Self::start_with(std::net::TcpListener::bind("127.0.0.1:0")?, handle)
    }

    /// A service on `port` (one just found free: a service whose address its configuration names
    /// before it starts).
    fn start_on(port: u16, handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        Self::start_with(std::net::TcpListener::bind(("127.0.0.1", port))?, handle)
    }

    fn start_with(listener: std::net::TcpListener, handle: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> anyhow::Result<Self> {
        let port = listener.local_addr()?.port();
        let seen: Arc<Mutex<Vec<Req>>> = Arc::default();
        let handle = Arc::new(handle);
        let kept = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let (handle, kept) = (handle.clone(), kept.clone());
                std::thread::spawn(move || {
                    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16384];
                    let head_end = loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let mut lines = head.lines();
                    let first = lines.next().unwrap_or("");
                    let mut words = first.split_whitespace();
                    let method = words.next().unwrap_or("").to_owned();
                    let target = words.next().unwrap_or("").to_owned();
                    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
                    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                    let mut body = buf[head_end + 4..].to_vec();
                    while body.len() < len {
                        let Ok(n) = s.read(&mut chunk) else { break };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }
                    let (path, query) = target.split_once('?').map_or((target.clone(), BTreeMap::new()), |(p, q)| (p.to_owned(), pairs(q)));
                    let req = Req { method, path, query, headers, body };
                    let resp = handle(&req);
                    kept.lock().unwrap_or_else(PoisonError::into_inner).push(req);
                    let reason = match resp.status {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        403 => "Forbidden",
                        404 => "Not Found",
                        429 => "Too Many Requests",
                        _ => "Status",
                    };
                    let mut out = format!("HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n", resp.status, resp.body.len());
                    for (k, v) in &resp.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    let _ = s.write_all(out.as_bytes()).and_then(|()| s.write_all(&resp.body));
                });
            }
        });
        Ok(Self { port, seen })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn count(&self, f: impl Fn(&Req) -> bool) -> usize {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|r| f(r)).count()
    }
}

/// Unix ms now.
fn now_ms() -> i64 {
    common::now_ms()
}

fn minute(t: i64) -> i64 {
    t.div_euclid(60_000)
}

/// Each minute's start in `[start, end)` (ms).
fn minutes(start: i64, end: i64) -> impl Iterator<Item = i64> {
    let first = (start + 59_999).div_euclid(60_000) * 60_000;
    (0..).map(move |i| first + i * 60_000).take_while(move |&t| t < end)
}

fn rfc3339_ms(s: &str) -> i64 {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).map(|t| (t.unix_timestamp_nanos() / 1_000_000) as i64).unwrap_or(0)
}

fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap()
}

// ------------------------------------------------------------------------------------------ AWS

/// AWS's keys as the mock knows them: an access key's secret and its session's token.
struct AwsKey {
    secret: &'static str,
    token: Option<&'static str>,
}

#[derive(Default)]
struct AwsState {
    /// GetMetricData calls to refuse as throttled, and whether CloudWatch is down.
    throttle: usize,
    down: bool,
    bad_signatures: Vec<String>,
    /// CloudWatch Logs' events: group, stream, time, id, message.
    events: Vec<(String, String, i64, String, String)>,
}

/// The log groups CloudWatch Logs has.
const AWS_GROUPS: [&str; 3] = ["/aws/lambda/checkout", "/aws/lambda/refunds", "/aws/rds/instance/db-1/postgresql"];

/// CloudWatch Logs, JSON 1.1: `DescribeLogGroups` by prefix, two pages; `FilterLogEvents` over a
/// group's events in `[startTime, endTime]`, ten a page after a first that holds none (CloudWatch
/// Logs searches in parts), a pattern of one word matched within the message.
fn aws_logs(r: &Req, st: &mut AwsState) -> Resp {
    match aws_signed(r, "logs") {
        Ok((id, _)) if id == AWS_BASE.0 => return aws_error(403, "AccessDeniedException", "the base keys may not read logs: assume the role"),
        Ok(_) => {}
        Err(e) => {
            st.bad_signatures.push(format!("logs: {e}"));
            return aws_error(403, "InvalidSignatureException", &e);
        }
    }
    if r.header("content-type") != Some("application/x-amz-json-1.1") {
        return aws_error(400, "SerializationException", "not CloudWatch Logs' JSON protocol");
    }
    let body = r.json();
    match r.header("x-amz-target") {
        Some("Logs_20140328.DescribeLogGroups") => {
            let prefix = body["logGroupNamePrefix"].as_str().unwrap_or("");
            let mine: Vec<Value> = AWS_GROUPS.iter().filter(|g| g.starts_with(prefix)).map(|g| json!({ "logGroupName": g, "arn": format!("arn:aws:logs:us-east-1:123456789012:log-group:{g}") })).collect();
            match body["nextToken"].as_str() {
                None if mine.len() > 1 => reply(200, &json!({ "logGroups": [mine[0]], "nextToken": "groups-2" })),
                None => reply(200, &json!({ "logGroups": mine })),
                Some(_) => reply(200, &json!({ "logGroups": mine[1..] })),
            }
        }
        Some("Logs_20140328.FilterLogEvents") => {
            let group = body["logGroupName"].as_str().unwrap_or("");
            let (start, end) = (body["startTime"].as_i64().unwrap_or(0), body["endTime"].as_i64().unwrap_or(0));
            let word = body["filterPattern"].as_str().unwrap_or("");
            let mut found: Vec<&(String, String, i64, String, String)> = st.events.iter().filter(|e| e.0 == group && e.2 >= start && e.2 <= end && (word.is_empty() || e.4.contains(word))).collect();
            found.sort_by_key(|e| e.2);
            let page = |from: usize| -> Resp {
                let events: Vec<Value> = found.iter().skip(from).take(10).map(|e| json!({ "logStreamName": e.1, "timestamp": e.2, "message": e.4, "ingestionTime": e.2 + 400, "eventId": e.3 })).collect();
                let next = (from + 10 < found.len()).then(|| format!("events-{}", from + 10));
                reply(200, &json!({ "events": events, "nextToken": next, "searchedLogStreams": [] }))
            };
            match body["nextToken"].as_str() {
                None => reply(200, &json!({ "events": [], "nextToken": "events-0", "searchedLogStreams": [] })),
                Some(t) => page(t.trim_start_matches("events-").parse().unwrap_or(0)),
            }
        }
        _ => aws_error(400, "UnknownOperationException", "no such operation"),
    }
}

/// The figures CloudWatch gives: an instance's CPU each minute (five samples of it), a function's
/// invocations.
fn cpu(instance: &str, t: i64) -> f64 {
    (minute(t) % 7) as f64 * 10.0 + instance.trim_start_matches("i-").parse::<f64>().unwrap_or(0.0)
}

fn invocations(t: i64) -> f64 {
    (minute(t) % 13) as f64
}

const AWS_BASE: (&str, &str) = ("AKIDBASEEXAMPLE", "base-secret");
const AWS_ROLE: &str = "arn:aws:iam::123456789012:role/kiln-observe";

fn aws_keys(id: &str) -> Option<AwsKey> {
    match id {
        "AKIDBASEEXAMPLE" => Some(AwsKey { secret: "base-secret", token: None }),
        "ASIASESSIONEXAMPLE" => Some(AwsKey { secret: "session-secret", token: Some("session-token-1") }),
        "ASIAIMDSEXAMPLE" => Some(AwsKey { secret: "imds-secret", token: Some("imds-token-1") }),
        _ => None,
    }
}

/// Whether the request is signed as AWS would check it: the access key's secret over the method,
/// path, signed headers and body, within 5 min of now; the service and region its scope names.
fn aws_signed(r: &Req, service: &str) -> Result<(String, String), String> {
    let auth = r.header("authorization").ok_or("no authorization")?;
    let rest = auth.strip_prefix("AWS4-HMAC-SHA256 ").ok_or("not SigV4")?;
    let mut parts = BTreeMap::new();
    for p in rest.split(", ") {
        if let Some((k, v)) = p.split_once('=') {
            parts.insert(k, v);
        }
    }
    let cred = parts.get("Credential").ok_or("no credential")?;
    let fields: Vec<&str> = cred.split('/').collect();
    let [id, _date, region, svc, "aws4_request"] = fields.as_slice() else { return Err(format!("credential {cred}")) };
    if *svc != service {
        return Err(format!("signed for {svc}, not {service}"));
    }
    let key = aws_keys(id).ok_or_else(|| format!("unknown key {id}"))?;
    let amz_date = r.header("x-amz-date").ok_or("no x-amz-date")?;
    let at = time::PrimitiveDateTime::parse(amz_date, time::macros::format_description!("[year][month][day]T[hour][minute][second]Z")).map_err(|e| e.to_string())?.assume_utc();
    if (time::OffsetDateTime::now_utc() - at).whole_seconds().abs() > 300 {
        return Err("the signature's time is more than 5 min from now".into());
    }
    if key.token != r.header("x-amz-security-token") {
        return Err("the session's token is not the key's".into());
    }
    let signed: Vec<&str> = parts.get("SignedHeaders").ok_or("no signed headers")?.split(';').collect();
    for must in ["host", "x-amz-date"] {
        if !signed.contains(&must) {
            return Err(format!("{must} is not signed"));
        }
    }
    let headers: Vec<(&str, &str)> = signed.iter().filter(|h| !matches!(**h, "x-amz-date" | "x-amz-security-token")).map(|h| (*h, r.header(h).unwrap_or(""))).collect();
    let keys = kiln::observe::cloud::sigv4::Keys { access_key: (*id).to_owned(), secret: key.secret.to_owned(), token: key.token.map(str::to_owned) };
    let req = kiln::observe::cloud::sigv4::Request { method: &r.method, path: &r.path, query: &[], headers: &headers, body: &r.body };
    let want = kiln::observe::cloud::sigv4::sign(&keys, region, service, &req, at);
    let want = want.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.as_str()).unwrap_or("");
    if want != auth {
        return Err(format!("SignatureDoesNotMatch: {auth} is not {want}"));
    }
    Ok(((*id).to_owned(), (*region).to_owned()))
}

fn aws_error(status: u16, code: &str, message: &str) -> Resp {
    let mut r = reply(status, &json!({ "__type": format!("com.amazonaws.cloudwatch#{code}"), "message": message }));
    r.headers.push(("x-amzn-query-error".into(), format!("{code};Sender")));
    r
}

fn sts_xml(id: &str, secret: &str, token: &str) -> Resp {
    let until = rfc3339(now_ms() + 3_600_000);
    let body = format!("<AssumeRoleResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><AssumeRoleResult><Credentials><AccessKeyId>{id}</AccessKeyId><SecretAccessKey>{secret}</SecretAccessKey><SessionToken>{token}</SessionToken><Expiration>{until}</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>");
    Resp { status: 200, headers: vec![("content-type".into(), "text/xml".into())], body: body.into_bytes() }
}

fn aws(state: Arc<Mutex<AwsState>>) -> impl Fn(&Req) -> Resp {
    move |r: &Req| {
        let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
        // STS: AssumeRole, signed with the base keys, the role and its external id.
        if r.path.starts_with("/sts/") {
            let form = r.form();
            match aws_signed(r, "sts") {
                Ok((id, _)) if id == AWS_BASE.0 => {}
                Ok((id, _)) => return text(403, &format!("<ErrorResponse><Error><Code>AccessDenied</Code><Message>{id} may not assume roles</Message></Error></ErrorResponse>")),
                Err(e) => {
                    st.bad_signatures.push(format!("sts: {e}"));
                    return text(403, &format!("<ErrorResponse><Error><Type>Sender</Type><Code>SignatureDoesNotMatch</Code><Message>{e}</Message></Error></ErrorResponse>"));
                }
            }
            if form.get("Action").map(String::as_str) != Some("AssumeRole") || form.get("RoleArn").map(String::as_str) != Some(AWS_ROLE) || form.get("ExternalId").map(String::as_str) != Some("e2e-external") {
                return text(403, "<ErrorResponse><Error><Code>AccessDenied</Code><Message>not this role, or not its external id</Message></Error></ErrorResponse>");
            }
            return sts_xml("ASIASESSIONEXAMPLE", "session-secret", "session-token-1");
        }
        // The instance metadata service (IMDSv2): a session token first, then the role's keys.
        if r.path.starts_with("/latest/") {
            if r.path == "/latest/api/token" {
                return if r.method == "PUT" && r.header("x-aws-ec2-metadata-token-ttl-seconds").is_some() { text(200, "imds-session") } else { text(400, "") };
            }
            if r.header("x-aws-ec2-metadata-token") != Some("imds-session") {
                return text(401, "");
            }
            return match r.path.as_str() {
                "/latest/meta-data/iam/security-credentials/" => text(200, "kiln-instance-role"),
                "/latest/meta-data/iam/security-credentials/kiln-instance-role" => reply(200, &json!({ "Code": "Success", "Type": "AWS-HMAC", "AccessKeyId": "ASIAIMDSEXAMPLE", "SecretAccessKey": "imds-secret", "Token": "imds-token-1", "Expiration": rfc3339(now_ms() + 3_600_000) })),
                "/latest/meta-data/placement/region" => text(200, "eu-west-1"),
                _ => text(404, ""),
            };
        }
        // CloudWatch Logs, JSON 1.1.
        if r.path.starts_with("/logs/") {
            return aws_logs(r, &mut st);
        }
        // CloudWatch, JSON 1.0.
        if st.down {
            return text(503, "Service Unavailable");
        }
        let (id, region) = match aws_signed(r, "monitoring") {
            Ok(x) => x,
            Err(e) => {
                st.bad_signatures.push(format!("monitoring: {e}"));
                return aws_error(403, "SignatureDoesNotMatch", &e);
            }
        };
        if id == AWS_BASE.0 {
            return aws_error(403, "AccessDenied", "the base keys may not read metrics: assume the role");
        }
        if r.path != format!("/cw/{region}/") || r.header("content-type") != Some("application/x-amz-json-1.0") {
            return aws_error(400, "InvalidAction", "not CloudWatch's JSON protocol");
        }
        let body = r.json();
        match r.header("x-amz-target") {
            Some("GraniteServiceVersion20100801.ListMetrics") => {
                if body["RecentlyActive"] != "PT3H" {
                    return aws_error(400, "InvalidParameterValue", "RecentlyActive");
                }
                let all = [
                    json!({ "Namespace": "AWS/EC2", "MetricName": "CPUUtilization", "Dimensions": [{ "Name": "InstanceId", "Value": "i-1" }] }),
                    json!({ "Namespace": "AWS/EC2", "MetricName": "CPUUtilization", "Dimensions": [{ "Name": "InstanceId", "Value": "i-2" }] }),
                    json!({ "Namespace": "AWS/Lambda", "MetricName": "Invocations", "Dimensions": [{ "Name": "FunctionName", "Value": "checkout" }] }),
                ];
                let ns = body["Namespace"].as_str().unwrap_or("");
                let mine: Vec<&Value> = all.iter().filter(|m| m["Namespace"] == ns).collect();
                // Two pages: the first metric, then the rest.
                match body["NextToken"].as_str() {
                    None if mine.len() > 1 => reply(200, &json!({ "Metrics": [mine[0]], "NextToken": format!("{ns}-2") })),
                    None => reply(200, &json!({ "Metrics": mine })),
                    Some(_) => reply(200, &json!({ "Metrics": mine[1..] })),
                }
            }
            Some("GraniteServiceVersion20100801.GetMetricData") => {
                if st.throttle > 0 {
                    st.throttle -= 1;
                    return aws_error(400, "Throttling", "Rate exceeded");
                }
                let (start, end) = ((body["StartTime"].as_f64().unwrap_or(0.0) * 1000.0) as i64, (body["EndTime"].as_f64().unwrap_or(0.0) * 1000.0) as i64);
                let queries = body["MetricDataQueries"].as_array().cloned().unwrap_or_default();
                let mut results = Vec::new();
                for q in &queries {
                    let m = &q["MetricStat"]["Metric"];
                    let stat = q["MetricStat"]["Stat"].as_str().unwrap_or("");
                    let dim = m["Dimensions"][0]["Value"].as_str().unwrap_or("");
                    let (ts, vs): (Vec<f64>, Vec<f64>) = minutes(start, end)
                        .map(|t| {
                            let v = match (m["MetricName"].as_str(), stat) {
                                (Some("CPUUtilization"), "Sum") => cpu(dim, t) * 5.0,
                                (Some("CPUUtilization"), "SampleCount") => 5.0,
                                (Some("Invocations"), "Sum") => invocations(t),
                                (Some("Invocations"), "SampleCount") => invocations(t).max(1.0),
                                _ => -1.0,
                            };
                            ((t / 1000) as f64, v)
                        })
                        .unzip();
                    results.push(json!({ "Id": q["Id"], "Label": stat, "Timestamps": ts, "Values": vs, "StatusCode": "Complete" }));
                }
                // Paged as CloudWatch pages: half of the results, then the rest.
                match body["NextToken"].as_str() {
                    None if results.len() > 1 => {
                        let half = results.len() / 2;
                        reply(200, &json!({ "MetricDataResults": results[..half], "NextToken": "page-2", "Messages": [] }))
                    }
                    None => reply(200, &json!({ "MetricDataResults": results, "Messages": [] })),
                    Some(_) => reply(200, &json!({ "MetricDataResults": results[results.len() / 2..], "Messages": [] })),
                }
            }
            _ => aws_error(400, "UnknownOperation", "no such operation"),
        }
    }
}

// --------------------------------------------------------------------------------------- Google

#[derive(Default)]
struct GcpState {
    /// Cloud Logging's entries, and the filters `entries.list` was asked.
    entries: Vec<Value>,
    filters: Vec<String>,
    bad_assertions: Vec<String>,
}

fn utilization(t: i64) -> f64 {
    (minute(t) % 5) as f64 / 10.0
}

fn received(t: i64) -> f64 {
    (minute(t) % 11) as f64 * 1000.0
}

const GCP_TYPES: [(&str, &str, &str); 5] = [
    ("compute.googleapis.com/instance/cpu/utilization", "GAUGE", "DOUBLE"),
    ("compute.googleapis.com/instance/network/received_bytes_count", "DELTA", "INT64"),
    ("compute.googleapis.com/instance/quiet", "GAUGE", "DOUBLE"),
    ("compute.googleapis.com/instance/os_version", "GAUGE", "STRING"),
    ("loadbalancing.googleapis.com/https/total_latencies", "DELTA", "DISTRIBUTION"),
];

fn gcp(state: Arc<Mutex<GcpState>>, public_key: Vec<u8>, token_uri: String) -> impl Fn(&Req) -> Resp {
    move |r: &Req| {
        let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
        if r.path == "/token" {
            // A JWT bearer assertion (RFC 7523), signed by the service account's key.
            let form = r.form();
            if form.get("grant_type").map(String::as_str) != Some("urn:ietf:params:oauth:grant-type:jwt-bearer") {
                return reply(400, &json!({ "error": "unsupported_grant_type" }));
            }
            let jwt = form.get("assertion").cloned().unwrap_or_default();
            let parts: Vec<&str> = jwt.split('.').collect();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let verified = parts.len() == 3 && {
                let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, &public_key);
                key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &b64.decode(parts[2]).unwrap_or_default()).is_ok()
            };
            let claims: Value = parts.get(1).and_then(|c| b64.decode(c).ok()).and_then(|c| serde_json::from_slice(&c).ok()).unwrap_or(Value::Null);
            let now_s = now_ms() / 1000;
            let fresh = claims["iat"].as_i64().is_some_and(|iat| (iat - now_s).abs() < 300) && claims["exp"].as_i64() == claims["iat"].as_i64().map(|i| i + 3600);
            // Monitoring's scope, and Logging's beside it when logs are read: a token of each.
            let token = match claims["scope"].as_str() {
                Some("https://www.googleapis.com/auth/monitoring.read") => "gcp-token-1",
                Some("https://www.googleapis.com/auth/monitoring.read https://www.googleapis.com/auth/logging.read") => "gcp-token-logs",
                _ => "",
            };
            if !verified || claims["aud"] != token_uri.as_str() || claims["iss"] != "kiln-e2e@proj-e2e.iam.gserviceaccount.com" || token.is_empty() || !fresh {
                st.bad_assertions.push(format!("verified {verified}, claims {claims}"));
                return reply(400, &json!({ "error": "invalid_grant", "error_description": "Invalid JWT Signature." }));
            }
            return reply(200, &json!({ "access_token": token, "expires_in": 3599, "token_type": "Bearer" }));
        }
        // The metadata server: Compute Engine's service account and project.
        if r.path.starts_with("/computeMetadata/v1/") {
            if r.header("metadata-flavor") != Some("Google") {
                return text(403, "Missing Metadata-Flavor:Google header.");
            }
            return match r.path.as_str() {
                "/computeMetadata/v1/instance/service-accounts/default/token" => reply(200, &json!({ "access_token": "gce-token", "expires_in": 3599, "token_type": "Bearer" })),
                "/computeMetadata/v1/project/project-id" => text(200, "proj-gce"),
                _ => text(404, ""),
            };
        }
        let token = r.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
        // Cloud Logging: a project's entries in the filter's time, oldest first, eight a page.
        if r.path == "/v2/entries:list" {
            let body = r.json();
            if token != "gcp-token-logs" || body["resourceNames"] != json!(["projects/proj-e2e"]) {
                return reply(403, &json!({ "error": { "code": 403, "message": "Permission 'logging.logEntries.list' denied (or the token's scopes lack logging.read)", "status": "PERMISSION_DENIED" } }));
            }
            let filter = body["filter"].as_str().unwrap_or("").to_owned();
            let bound = |key: &str| filter.split(key).nth(1).and_then(|r| r.split('"').next()).map(rfc3339_ms);
            let (Some(from), Some(to)) = (bound("timestamp>=\""), bound("timestamp<=\"")) else {
                return reply(400, &json!({ "error": { "code": 400, "message": "a window, please", "status": "INVALID_ARGUMENT" } }));
            };
            st.filters.push(filter.clone());
            let typed = filter.contains("resource.type=\"cloud_run_revision\"");
            let mut found: Vec<&Value> = st.entries.iter().filter(|e| (from..=to).contains(&rfc3339_ms(e["timestamp"].as_str().unwrap_or(""))) && (!typed || e["resource"]["type"] == "cloud_run_revision")).collect();
            found.sort_by_key(|e| rfc3339_ms(e["timestamp"].as_str().unwrap_or("")));
            let skip: usize = body["pageToken"].as_str().and_then(|t| t.parse().ok()).unwrap_or(0);
            let page: Vec<&Value> = found.iter().skip(skip).take(8).copied().collect();
            let next = (skip + 8 < found.len()).then(|| (skip + 8).to_string());
            return reply(200, &json!({ "entries": page, "nextPageToken": next }));
        }
        let project = match (token, r.path.strip_prefix("/v3/projects/").and_then(|p| p.split('/').next())) {
            ("gcp-token-1" | "gcp-token-logs", Some(p @ "proj-e2e")) | ("gce-token", Some(p @ "proj-gce")) => p.to_owned(),
            _ => return reply(403, &json!({ "error": { "code": 403, "message": "Permission monitoring.timeSeries.list denied (or the token is not this project's)", "status": "PERMISSION_DENIED" } })),
        };
        if r.path.ends_with("/metricDescriptors") {
            let filter = r.query.get("filter").cloned().unwrap_or_default();
            let prefix = filter.strip_prefix("metric.type = starts_with(\"").and_then(|f| f.strip_suffix("\")")).unwrap_or("");
            let mine: Vec<Value> = GCP_TYPES.iter().filter(|(t, _, _)| t.starts_with(prefix)).map(|(t, k, v)| json!({ "name": format!("projects/{project}/metricDescriptors/{t}"), "type": t, "metricKind": k, "valueType": v })).collect();
            // Two pages.
            return match r.query.get("pageToken") {
                None if mine.len() > 1 => reply(200, &json!({ "metricDescriptors": mine[..1], "nextPageToken": "d2" })),
                None => reply(200, &json!({ "metricDescriptors": mine })),
                Some(_) => reply(200, &json!({ "metricDescriptors": mine[1..] })),
            };
        }
        if r.path.ends_with("/timeSeries") {
            let filter = r.query.get("filter").cloned().unwrap_or_default();
            let t = filter.strip_prefix("metric.type = \"").and_then(|f| f.strip_suffix('"')).unwrap_or("");
            let aligner = r.query.get("aggregation.perSeriesAligner").map(String::as_str).unwrap_or("");
            let want = match t {
                "compute.googleapis.com/instance/cpu/utilization" => "ALIGN_MEAN",
                "compute.googleapis.com/instance/network/received_bytes_count" | "loadbalancing.googleapis.com/https/total_latencies" => "ALIGN_DELTA",
                "compute.googleapis.com/instance/quiet" => "ALIGN_MEAN",
                _ => return reply(400, &json!({ "error": { "code": 400, "message": format!("{t} is not asked for"), "status": "INVALID_ARGUMENT" } })),
            };
            if aligner != want || r.query.get("aggregation.alignmentPeriod").map(String::as_str) != Some("60s") {
                return reply(400, &json!({ "error": { "code": 400, "message": format!("{t}: the aligner {aligner} is not {want}"), "status": "INVALID_ARGUMENT" } }));
            }
            let (start, end) = (rfc3339_ms(r.query.get("interval.startTime").map(String::as_str).unwrap_or("")), rfc3339_ms(r.query.get("interval.endTime").map(String::as_str).unwrap_or("")));
            // Aligned points: each period's end, in (start, end].
            let ends: Vec<i64> = minutes(start + 1, end + 1).collect();
            let resource = json!({ "type": "gce_instance", "labels": { "project_id": project, "zone": "us-central1-a", "instance_id": "4711" } });
            let series = match t {
                "compute.googleapis.com/instance/cpu/utilization" => vec![json!({
                    "metric": { "type": t, "labels": { "instance_name": "web-1" } }, "resource": resource,
                    "metadata": { "systemLabels": { "name": "web-1", "machine_type": "e2-medium" }, "userLabels": { "env": "prod" } },
                    "metricKind": "GAUGE", "valueType": "DOUBLE",
                    "points": ends.iter().rev().map(|&e| json!({ "interval": { "startTime": rfc3339(e), "endTime": rfc3339(e) }, "value": { "doubleValue": utilization(e) } })).collect::<Vec<_>>(),
                })],
                "compute.googleapis.com/instance/network/received_bytes_count" => vec![json!({
                    "metric": { "type": t, "labels": { "instance_name": "web-1", "loadbalanced": "false" } }, "resource": resource,
                    "metricKind": "DELTA", "valueType": "INT64",
                    "points": ends.iter().rev().map(|&e| json!({ "interval": { "startTime": rfc3339(e - 60_000), "endTime": rfc3339(e) }, "value": { "int64Value": format!("{}", received(e) as i64) } })).collect::<Vec<_>>(),
                })],
                "loadbalancing.googleapis.com/https/total_latencies" => vec![json!({
                    "metric": { "type": t, "labels": { "response_code": "200" } }, "resource": { "type": "https_lb_rule", "labels": { "project_id": project, "url_map_name": "web" } },
                    "metricKind": "DELTA", "valueType": "DISTRIBUTION",
                    "points": ends.iter().rev().map(|&e| json!({ "interval": { "startTime": rfc3339(e - 60_000), "endTime": rfc3339(e) }, "value": { "distributionValue": {
                        "count": "20", "mean": 15.0, "bucketOptions": { "explicitBuckets": { "bounds": [0, 10, 20, 40] } }, "bucketCounts": ["0", "10", "10"]
                    } } })).collect::<Vec<_>>(),
                })],
                _ => Vec::new(),
            };
            return reply(200, &json!({ "timeSeries": series, "unit": "" }));
        }
        reply(404, &json!({ "error": { "code": 404, "message": "no such route", "status": "NOT_FOUND" } }))
    }
}

// ---------------------------------------------------------------------------------------- Azure

fn vm_cpu(vm: &str, t: i64) -> f64 {
    (minute(t) % 9) as f64 + if vm == "vm-2" { 0.5 } else { 0.0 }
}

fn network_in(t: i64) -> f64 {
    (minute(t) % 4) as f64 * 1_000_000.0
}

const SUB: &str = "00000000-0000-0000-0000-00000000e2e0";

/// Log Analytics' workspace in the scenario, and its rows: each a time, an id and a message.
const WS: &str = "00000000-0000-0000-0000-0000000000a1";

fn azure(arm: String) -> impl Fn(&Req) -> Resp {
    azure_with(arm, Arc::default())
}

/// Azure, its Log Analytics workspace holding `rows` (each its `TimeGenerated`, `_ItemId`,
/// `Message` and `SeverityLevel`), its API under `/la` with a token of its own audience.
fn azure_with(arm: String, rows: Arc<Mutex<Vec<(i64, String, String, i64)>>>) -> impl Fn(&Req) -> Resp {
    move |r: &Req| {
        if r.path == "/tenant-e2e/oauth2/v2.0/token" {
            let form = r.form();
            let token = match form.get("scope") {
                Some(s) if *s == format!("{arm}/.default") => "az-token-1",
                Some(s) if *s == format!("{arm}/la/.default") => "az-logs-token",
                _ => "",
            };
            if form.get("grant_type").map(String::as_str) != Some("client_credentials") || form.get("client_id").map(String::as_str) != Some("app-e2e") || token.is_empty() {
                return reply(400, &json!({ "error": "invalid_request", "error_description": "AADSTS900144: The request body must contain the following parameter: 'scope'." }));
            }
            if form.get("client_secret").map(String::as_str) != Some("s3cret-az") {
                return reply(401, &json!({ "error": "invalid_client", "error_description": "AADSTS7000215: Invalid client secret provided. Ensure the secret being sent in the request is the client secret value." }));
            }
            return reply(200, &json!({ "token_type": "Bearer", "expires_in": 3599, "ext_expires_in": 3599, "access_token": token }));
        }
        // Log Analytics: a workspace's rows of the query's table within its timespan, oldest first.
        if let Some(ws) = r.path.strip_prefix("/la/v1/workspaces/").and_then(|p| p.strip_suffix("/query")) {
            let bearer = r.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
            if bearer != "az-logs-token" {
                return reply(403, &json!({ "error": { "code": "InsufficientAccessError", "message": "The provided credentials have insufficient access to perform the requested operation (a token for Log Analytics' audience)" } }));
            }
            let body = r.json();
            let query = body["query"].as_str().unwrap_or("");
            let Some((from, to)) = body["timespan"].as_str().and_then(|t| t.split_once('/')).map(|(a, b)| (rfc3339_ms(a), rfc3339_ms(b))) else {
                return reply(400, &json!({ "error": { "code": "BadArgumentError", "message": "timespan" } }));
            };
            if ws != WS || !query.starts_with("AppTraces") || !query.contains("| where TimeGenerated between (datetime(") || !query.contains("| order by TimeGenerated asc") {
                return reply(400, &json!({ "error": { "code": "BadArgumentError", "message": format!("not the scenario's query: {query}") } }));
            }
            let mut kept: Vec<(i64, String, String, i64)> = rows.lock().unwrap_or_else(PoisonError::into_inner).iter().filter(|r| r.0 >= from && r.0 <= to).cloned().collect();
            kept.sort_by_key(|r| r.0);
            let rows: Vec<Value> = kept.iter().map(|(t, id, m, l)| json!([rfc3339(*t), m, l, format!("/subscriptions/{SUB}/resourceGroups/prod-rg/providers/Microsoft.Web/sites/checkout"), "api", id, "AppTraces"])).collect();
            let columns = ["TimeGenerated", "Message", "SeverityLevel", "_ResourceId", "AppRoleName", "_ItemId", "Type"].map(|c| json!({ "name": c, "type": "string" }));
            return reply(200, &json!({ "tables": [{ "name": "PrimaryResult", "columns": columns, "rows": rows }] }));
        }
        if r.path == "/metadata/identity/oauth2/token" {
            if r.header("metadata") != Some("true") || r.query.get("resource") != Some(&format!("{arm}/")) {
                return reply(400, &json!({ "error": "invalid_request", "error_description": "Required metadata header not specified" }));
            }
            return reply(200, &json!({ "access_token": "az-mi-token", "expires_in": "3599", "expires_on": format!("{}", now_ms() / 1000 + 3599), "resource": format!("{arm}/"), "token_type": "Bearer" }));
        }
        let token = r.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
        if token != "az-token-1" && token != "az-mi-token" {
            return reply(401, &json!({ "error": { "code": "InvalidAuthenticationToken", "message": "The access token is invalid." } }));
        }
        let rg = format!("/subscriptions/{SUB}/resourceGroups/Prod-RG/providers");
        if r.path == "/subscriptions" {
            return reply(200, &json!({ "value": [{ "subscriptionId": SUB, "displayName": "e2e", "state": "Enabled" }, { "subscriptionId": "00000000-0000-0000-0000-0000000dead0", "state": "Disabled" }] }));
        }
        if r.path == format!("/subscriptions/{SUB}/resources") {
            // `$filter=resourceType eq 'A' or resourceType eq 'B'`, as Resource Manager reads it.
            let types: Vec<String> = r.query.get("$filter").map(|f| f.split('\'').skip(1).step_by(2).map(str::to_ascii_lowercase).collect()).unwrap_or_default();
            let keep = |v: Value| -> Value {
                let mut v = v;
                if !types.is_empty()
                    && let Some(list) = v["value"].as_array_mut()
                {
                    list.retain(|x| types.contains(&x["type"].as_str().unwrap_or("").to_ascii_lowercase()));
                }
                v
            };
            // Two pages, the second by `nextLink`.
            return match r.query.get("$skiptoken") {
                None => reply(200, &keep(json!({ "value": [
                    { "id": format!("{rg}/Microsoft.Compute/virtualMachines/vm-1"), "name": "vm-1", "type": "Microsoft.Compute/virtualMachines", "location": "eastus", "tags": { "Cost Center": "42" } },
                    { "id": format!("{rg}/Microsoft.Network/networkSecurityGroups/nsg-1"), "name": "nsg-1", "type": "Microsoft.Network/networkSecurityGroups", "location": "eastus" },
                ], "nextLink": format!("{arm}/subscriptions/{SUB}/resources?api-version=2021-04-01&$skiptoken=2") }))),
                Some(_) => reply(200, &keep(json!({ "value": [{ "id": format!("{rg}/Microsoft.Compute/virtualMachines/vm-2"), "name": "vm-2", "type": "Microsoft.Compute/virtualMachines", "location": "eastus" }] }))),
            };
        }
        if let Some(id) = r.path.strip_suffix("/providers/Microsoft.Insights/metricDefinitions") {
            return if id.contains("/virtualMachines/") {
                reply(200, &json!({ "value": [
                    { "name": { "value": "Percentage CPU", "localizedValue": "Percentage CPU" }, "primaryAggregationType": "Average", "supportedAggregationTypes": ["None", "Average", "Minimum", "Maximum", "Total", "Count"], "metricAvailabilities": [{ "timeGrain": "PT1M", "retention": "P93D" }, { "timeGrain": "PT1H", "retention": "P93D" }], "isDimensionRequired": false },
                    { "name": { "value": "Network In Total" }, "primaryAggregationType": "Total", "metricAvailabilities": [{ "timeGrain": "PT1M" }] },
                ] }))
            } else {
                reply(400, &json!({ "error": { "code": "ResourceTypeNotSupported", "message": "The resource type is not supported for metrics." } }))
            };
        }
        if let Some(id) = r.path.strip_suffix("/providers/Microsoft.Insights/metrics") {
            let vm = id.rsplit('/').next().unwrap_or("");
            let names: Vec<String> = r.query.get("metricnames").map(|n| n.split(',').map(str::to_owned).collect()).unwrap_or_default();
            let aggs = r.query.get("aggregation").cloned().unwrap_or_default();
            let span = r.query.get("timespan").cloned().unwrap_or_default();
            let (start, end) = span.split_once('/').map(|(a, b)| (rfc3339_ms(a), rfc3339_ms(b))).unwrap_or((0, 0));
            if r.query.get("interval").map(String::as_str) != Some("PT1M") || !aggs.contains("Average") || !aggs.contains("Total") {
                return reply(400, &json!({ "error": { "code": "BadRequest", "message": format!("interval {:?}, aggregation {aggs}", r.query.get("interval")) } }));
            }
            let value: Vec<Value> = names
                .iter()
                .map(|n| {
                    let data: Vec<Value> = minutes(start, end + 60_000)
                        .map(|t| {
                            // Each aggregation asked: the CPU's average and maximum, the network's
                            // total and maximum.
                            let (avg, total, max) = match n.as_str() {
                                "Percentage CPU" => (Some(vm_cpu(vm, t)), None, vm_cpu(vm, t) + 10.0),
                                _ => (None, Some(network_in(t)), network_in(t) / 60.0),
                            };
                            let mut p = json!({ "timeStamp": rfc3339(t) });
                            if let (Some(a), true) = (avg, aggs.contains("Average")) {
                                p["average"] = a.into();
                            }
                            if let (Some(a), true) = (total, aggs.contains("Total")) {
                                p["total"] = a.into();
                            }
                            if aggs.contains("Maximum") {
                                p["maximum"] = max.into();
                            }
                            p
                        })
                        .collect();
                    json!({ "id": format!("{id}/providers/Microsoft.Insights/metrics/{n}"), "type": "Microsoft.Insights/metrics", "name": { "value": n, "localizedValue": n }, "unit": "Percent", "timeseries": [{ "metadatavalues": [], "data": data }], "errorCode": "Success" })
                })
                .collect();
            return reply(200, &json!({ "cost": 0, "timespan": span, "interval": "PT1M", "value": value, "namespace": "Microsoft.Compute/virtualMachines", "resourceregion": "eastus" }));
        }
        reply(404, &json!({ "error": { "code": "NotFound", "message": format!("{} {}", r.method, r.path) } }))
    }
}

// ---------------------------------------------------------------------------------------- kiln's

fn clouds(f: &Fleet) -> anyhow::Result<Vec<Value>> {
    Ok(at(&f.call("GET", "/api/observe/v1/clouds", None)?, &["clouds"]).as_array().cloned().unwrap_or_default())
}

fn cloud(f: &Fleet, name: &str) -> anyhow::Result<Value> {
    clouds(f)?.into_iter().find(|c| c["name"] == name).ok_or_else(|| anyhow::anyhow!("no cloud integration {name}"))
}

/// Waits until `name`'s status holds `done`.
fn until_cloud(f: &Fleet, name: &str, within: Duration, done: impl Fn(&Value) -> bool) -> anyhow::Result<Value> {
    let start = Instant::now();
    let mut last = Value::Null;
    while start.elapsed() < within {
        if let Ok(c) = cloud(f, name) {
            if done(&c["status"]) {
                return Ok(c);
            }
            last = c;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    anyhow::bail!("{name}: not within {within:?}; it was {last:#}\n{}", f.logs())
}

/// The query's series over `[from, to)` on a minute's grid: each its tags and its values by time.
fn read(f: &Fleet, q: &str, from: i64, to: i64) -> anyhow::Result<Vec<(BTreeMap<String, String>, BTreeMap<i64, f64>)>> {
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": q, "from": from, "to": to - 1, "step": 60_000 })))?;
    let (g0, step) = (at(&a, &["grid", "from"]).as_i64().unwrap_or(0), at(&a, &["grid", "step"]).as_i64().unwrap_or(60_000));
    Ok(at(&a, &["series"])
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| {
            let tags = at(s, &["tags"]).as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned())).collect()).unwrap_or_default();
            let values = at(s, &["values"]).as_array().into_iter().flatten().enumerate().filter_map(|(i, v)| Some((g0 + i as i64 * step, v.as_f64()?))).filter(|(t, _)| (from..to).contains(t)).collect();
            (tags, values)
        })
        .collect())
}

/// The query's one series' values must be `want` at each minute of `[from, to)`, once what was
/// taken is applied (within 30 s).
fn expect(f: &Fleet, q: &str, from: i64, to: i64, want: impl Fn(i64) -> f64) -> anyhow::Result<BTreeMap<String, String>> {
    let start = Instant::now();
    loop {
        let got = read(f, q, from, to)?;
        let checked = match got.as_slice() {
            [(tags, values)] => minutes(from, to)
                .try_for_each(|t| {
                    let (w, v) = (want(t), values.get(&t).copied());
                    if v.is_some_and(|v| (v - w).abs() < 1e-9 * w.abs().max(1.0)) { Ok(()) } else { Err(format!("{q} at {}: {v:?}, not {w} (all: {values:?})", rfc3339(t))) }
                })
                .map(|()| tags.clone()),
            _ => Err(format!("{q}: {} series, not one: {got:?}", got.len())),
        };
        match checked {
            Ok(tags) => return Ok(tags),
            Err(e) if start.elapsed() > Duration::from_secs(30) => anyhow::bail!("{e}\n{}", f.logs()),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// The events `q` finds since `from`, once there are `n` (within 30 s), or those there are then.
fn events(f: &Fleet, q: &str, from: i64, n: usize) -> anyhow::Result<Vec<String>> {
    let start = Instant::now();
    loop {
        let found = f.call("POST", "/api/observe/v1/events/search", Some(&json!({ "query": q, "from": from, "to": now_ms() + 60_000, "limit": 50 })))?;
        let said: Vec<String> = at(&found, &["records"]).as_array().into_iter().flatten().map(|r| at(r, &["message"]).as_str().unwrap_or("").to_owned()).collect();
        if said.len() >= n || start.elapsed() > Duration::from_secs(30) {
            return Ok(said);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn service_account(token_uri: &str) -> anyhow::Result<(String, Vec<u8>)> {
    use aws_lc_rs::encoding::AsDer as _;
    use aws_lc_rs::signature::KeyPair as _;
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|e| anyhow::anyhow!("{e}"))?;
    let der = kp.as_der().map_err(|e| anyhow::anyhow!("{e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
    let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap_or("")).collect();
    let pem = format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", lines.join("\n"));
    let key = json!({ "type": "service_account", "project_id": "proj-e2e", "private_key_id": "e2e-key-1", "private_key": pem, "client_email": "kiln-e2e@proj-e2e.iam.gserviceaccount.com", "client_id": "1234", "token_uri": token_uri });
    Ok((key.to_string(), kp.public_key().as_ref().to_vec()))
}

/// Ten minutes read back each pull, every 10 s (a period is a minute: each pull reads ten).
fn often() -> Value {
    json!({ "interval_s": 10, "lookback_s": 600 })
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}

/// The last pull's time and the five minutes before it: what every pull since has read.
fn window(c: &Value) -> (i64, i64) {
    let end = c["status"]["last_ok"].as_i64().unwrap_or(0).div_euclid(60_000) * 60_000;
    (end - 300_000, end)
}

#[test]
#[ignore = "starts a kiln server as a process, and services that stand in for AWS, Google and Azure"]
fn each_cloud_s_metrics_are_read_with_the_credentials_given_and_stored_as_datadog_names_them() -> anyhow::Result<()> {
    let aws_state = Arc::new(Mutex::new(AwsState { throttle: 1, ..AwsState::default() }));
    let aws_mock = Service::start(aws(aws_state.clone()))?;
    let gcp_state = Arc::new(Mutex::new(GcpState::default()));
    // The token's address is the service account's, known before the service is: the service is
    // started on a port taken first.
    let gcp_port = common::free_port()?;
    let token_uri = format!("http://127.0.0.1:{gcp_port}/token");
    let (key, public) = service_account(&token_uri)?;
    let gcp_mock = Service::start_on(gcp_port, gcp(gcp_state.clone(), public, token_uri.clone()))?;
    let azure_port = common::free_port()?;
    let arm = format!("http://127.0.0.1:{azure_port}");
    let azure_mock = Service::start_on(azure_port, azure(arm.clone()))?;
    let mut f = Fleet::new()?;
    let t0 = now_ms();

    // AWS: base keys, a role assumed in another account with its external id, two namespaces.
    let aws_cfg = merge(
        json!({
            "kind": "aws", "regions": ["us-east-1"], "namespaces": ["AWS/EC2", "AWS/Lambda"],
            "access_key_id": AWS_BASE.0, "secret_access_key": AWS_BASE.1, "role_arn": AWS_ROLE, "external_id": "e2e-external",
            "endpoint": format!("{}/cw/{{region}}/", aws_mock.url()), "sts_endpoint": format!("{}/sts/{{region}}/", aws_mock.url()),
            "tags": ["account:prod"],
        }),
        often(),
    );
    // Tested before it is kept: what it would read.
    let tested = f.call("POST", "/api/observe/v1/clouds/prod/test", Some(&aws_cfg))?;
    anyhow::ensure!(tested["ok"] == true && tested["series"] == 9 && tested["sample"].as_array().is_some_and(|s| s.iter().any(|m| m == "aws.ec2.cpuutilization")), "{tested:#}");
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/prod", Some(&aws_cfg))?.0 == 204);
    // Google: a service account's key, two services.
    let gcp_cfg = merge(json!({ "kind": "gcp", "services": ["compute.googleapis.com", "loadbalancing.googleapis.com"], "service_account": key, "endpoint": gcp_mock.url() }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/gcp-main", Some(&gcp_cfg))?.0 == 204);
    // Azure: an app's secret, every subscription it sees, the maximum besides each primary.
    let azure_cfg = merge(json!({ "kind": "azure", "tenant_id": "tenant-e2e", "client_id": "app-e2e", "client_secret": "s3cret-az", "aggregations": ["Maximum"], "authority": arm, "endpoint": arm }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/azure-main", Some(&azure_cfg))?.0 == 204);

    // AWS: each instance's CPU as the average of its sum and count; the function's invocations a
    // count; the series tagged by region, dimension and the integration's tags.
    let c = until_cloud(&f, "prod", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    anyhow::ensure!(c["status"]["series"] == 9 && c["secret_access_key"] == "…" && c["status"]["error"].is_null(), "{c:#}");
    let (from, to) = window(&c);
    let tags = expect(&f, "avg:aws.ec2.cpuutilization{instanceid:i-1}", from, to, |t| cpu("i-1", t))?;
    anyhow::ensure!(tags.is_empty() || tags.values().all(|v| !v.is_empty()), "{tags:?}");
    expect(&f, "avg:aws.ec2.cpuutilization{instanceid:i-2,region:us-east-1,account:prod}", from, to, |t| cpu("i-2", t))?;
    expect(&f, "sum:aws.ec2.cpuutilization.samplecount{instanceid:i-2}.as_count()", from, to, |_| 5.0)?;
    expect(&f, "sum:aws.lambda.invocations.sum{functionname:checkout}.as_count()", from, to, invocations)?;
    let st = aws_state.lock().unwrap_or_else(PoisonError::into_inner);
    anyhow::ensure!(st.bad_signatures.is_empty(), "every call is signed as AWS checks it: {:?}", st.bad_signatures);
    anyhow::ensure!(st.throttle == 0, "the throttled call was made");
    drop(st);
    // The test's pull and the kept one's: AssumeRole once each, its keys kept from pull to pull.
    let assumed = aws_mock.count(|r| r.path.starts_with("/sts/"));
    let pulls = c["status"]["pulls"].as_u64().unwrap_or(0);
    anyhow::ensure!(assumed == 2, "AssumeRole {assumed} times over {pulls} pulls of the kept integration and one test");

    // Google: the gauge by its mean, the delta a count, the distribution its six; the quiet type
    // asked once, the string type never.
    let c = until_cloud(&f, "gcp-main", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    let (from, to) = window(&c);
    let tags = expect(&f, "avg:gcp.compute.instance.cpu.utilization{instance_id:4711}", from, to, utilization)?;
    for (k, v) in [("project_id", "proj-e2e"), ("zone", "us-central1-a"), ("name", "web-1"), ("env", "prod"), ("resource_type", "gce_instance"), ("instance_name", "web-1")] {
        anyhow::ensure!(read(&f, &format!("avg:gcp.compute.instance.cpu.utilization{{{k}:{v}}}"), from, to)?.len() == 1, "tagged {k}:{v} ({tags:?})");
    }
    expect(&f, "sum:gcp.compute.instance.network.received_bytes_count{loadbalanced:false}.as_count()", from, to, received)?;
    expect(&f, "sum:gcp.loadbalancing.https.total_latencies.count{response_code:200}.as_count()", from, to, |_| 20.0)?;
    expect(&f, "sum:gcp.loadbalancing.https.total_latencies.sum{*}.as_count()", from, to, |_| 300.0)?;
    expect(&f, "avg:gcp.loadbalancing.https.total_latencies.avg{*}", from, to, |_| 15.0)?;
    expect(&f, "avg:gcp.loadbalancing.https.total_latencies.p50{*}", from, to, |_| 10.0)?;
    expect(&f, "avg:gcp.loadbalancing.https.total_latencies.p99{*}", from, to, |_| 19.8)?;
    anyhow::ensure!(gcp_state.lock().unwrap_or_else(PoisonError::into_inner).bad_assertions.is_empty());
    // Several pulls later, the quiet type was still asked once; the string type never.
    until_cloud(&f, "gcp-main", Duration::from_secs(60), |s| s["pulls"].as_u64().unwrap_or(0) >= 3)?;
    let quiet = gcp_mock.count(|r| r.query.get("filter").is_some_and(|q| q.contains("instance/quiet")));
    let strings = gcp_mock.count(|r| r.query.get("filter").is_some_and(|q| q.contains("os_version")));
    anyhow::ensure!(quiet == 1 && strings == 0, "the quiet type asked {quiet} times, the string type {strings}");

    // Azure: both pages of resources; each VM's CPU its primary (the average) and its maximum, the
    // network its total (a count); the security group, without metrics, asked once and no more.
    let c = until_cloud(&f, "azure-main", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    let (from, to) = window(&c);
    let tags = expect(&f, "avg:azure.compute_virtualmachines.percentage_cpu{name:vm-2}", from, to, |t| vm_cpu("vm-2", t))?;
    expect(&f, "avg:azure.compute_virtualmachines.percentage_cpu{name:vm-1,resource_group:prod-rg,region:eastus,cost_center:42}", from, to, |t| vm_cpu("vm-1", t))?;
    expect(&f, "sum:azure.compute_virtualmachines.network_in_total{name:vm-1}.as_count()", from, to, network_in)?;
    expect(&f, "avg:azure.compute_virtualmachines.percentage_cpu.maximum{name:vm-2}", from, to, |t| vm_cpu("vm-2", t) + 10.0)?;
    expect(&f, "max:azure.compute_virtualmachines.network_in_total.maximum{name:vm-1}", from, to, |t| network_in(t) / 60.0)?;
    let by_vm = read(&f, "avg:azure.compute_virtualmachines.percentage_cpu{*} by {name}", from, to)?;
    anyhow::ensure!(by_vm.iter().map(|(t, _)| t.get("name").cloned().unwrap_or_default()).collect::<Vec<_>>() == ["vm-1", "vm-2"], "both pages' VMs: {by_vm:?} ({tags:?})");
    let nsg = azure_mock.count(|r| r.path.contains("networkSecurityGroups"));
    anyhow::ensure!(nsg == 1, "the security group asked {nsg} times");
    anyhow::ensure!(azure_mock.count(|r| r.path.contains("dead0")) == 0, "a disabled subscription is not read");

    // Every integration said it began, as an event.
    let said = events(&f, "service:kiln.cloud @event.type:cloud.pull @result:pulling", t0, 3)?;
    for name in ["prod", "gcp-main", "azure-main"] {
        anyhow::ensure!(said.iter().any(|m| m.contains(&format!(" {name} pulling"))), "{name}: {said:?}");
    }
    // kiln's own series of the pulls.
    let pulled = read(&f, "sum:kiln.cloud.points{*} by {integration}.as_count()", t0 - 60_000, now_ms() + 60_000)?;
    anyhow::ensure!(pulled.len() == 3 && pulled.iter().all(|(_, v)| v.values().sum::<f64>() > 0.0), "{pulled:?}");

    // Secrets: never shown, never at rest in the clear, and kept across a restart.
    let shown = serde_json::to_string(&clouds(&f)?)?;
    for secret in [AWS_BASE.1, "s3cret-az", "PRIVATE KEY"] {
        anyhow::ensure!(!shown.contains(secret), "{secret} is shown");
    }
    let kept = std::fs::read_to_string(f.root.path().join("server").join("observe").join("clouds.json"))?;
    for secret in [AWS_BASE.1, "s3cret-az", "PRIVATE KEY"] {
        anyhow::ensure!(!kept.contains(secret), "{secret} is at rest in the clear");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(f.root.path().join("server").join("observe").join("clouds.json"))?.permissions().mode();
        anyhow::ensure!(mode & 0o077 == 0, "clouds.json is the owner's alone: {mode:o}");
    }
    f.kill_server();
    let before = aws_mock.count(|r| r.path.starts_with("/sts/"));
    f.start_server()?;
    let restarted = now_ms();
    for name in ["prod", "gcp-main", "azure-main"] {
        until_cloud(&f, name, Duration::from_secs(60), |s| s["state"] == "ok" && s["last_ok"].as_i64().unwrap_or(0) >= restarted)?;
    }
    anyhow::ensure!(aws_mock.count(|r| r.path.starts_with("/sts/")) == before + 1, "after the restart the role is assumed again, with the kept keys");
    Ok(())
}


/// The records `q` finds over `[from, now]`, once there are `n` (within 60 s), or those there are.
fn records(f: &Fleet, q: &str, from: i64, n: usize) -> anyhow::Result<Vec<Value>> {
    let start = Instant::now();
    loop {
        let found = f.call("POST", "/api/observe/v1/logs/search", Some(&json!({ "query": q, "from": from, "to": now_ms() + 60_000, "limit": 1000 })))?;
        let got = at(&found, &["records"]).as_array().cloned().unwrap_or_default();
        if got.len() >= n || start.elapsed() > Duration::from_secs(60) {
            return Ok(got);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[ignore = "starts a kiln server as a process, and services that stand in for AWS, Google and Azure"]
fn each_cloud_s_logs_are_stored_once_each_though_every_pull_reads_its_lookback() -> anyhow::Result<()> {
    let t0 = now_ms();
    // CloudWatch Logs: thirty events of the checkout function, every 7th an error; a few of the
    // refunds function's; an RDS group's, which the prefix asked does not take.
    let mut aws_state = AwsState::default();
    for i in 0..30i64 {
        let msg = if i % 7 == 0 { format!("[ERROR]\t{i}\tpayment {i} failed") } else { format!("[INFO]\t{i}\tpayment {i}") };
        aws_state.events.push(("/aws/lambda/checkout".into(), format!("2026/10/18/[$LATEST]s{}", i % 3), t0 - 290_000 + i * 10_000, format!("e{i}"), msg));
    }
    for i in 0..4i64 {
        aws_state.events.push(("/aws/lambda/refunds".into(), "s".into(), t0 - 200_000 + i * 1_000, format!("r{i}"), format!("refund {i}")));
    }
    aws_state.events.push(("/aws/rds/instance/db-1/postgresql".into(), "db".into(), t0 - 100_000, "d0".into(), "LOG: checkpoint".into()));
    let aws_state = Arc::new(Mutex::new(aws_state));
    let aws_mock = Service::start(aws(aws_state.clone()))?;
    // Cloud Logging: twenty Cloud Run entries (text and JSON, every 5th an error, one traced) and a
    // GCE one the filter leaves out.
    let mut gcp_state = GcpState::default();
    for i in 0..20i64 {
        let mut e = json!({
            "logName": "projects/proj-e2e/logs/run.googleapis.com%2Fstdout", "insertId": format!("g{i}"),
            "timestamp": rfc3339(t0 - 280_000 + i * 12_000), "severity": if i % 5 == 0 { "ERROR" } else { "INFO" },
            "resource": { "type": "cloud_run_revision", "labels": { "service_name": "checkout", "revision_name": "checkout-00042", "project_id": "proj-e2e" } },
        });
        if i % 2 == 0 {
            e["textPayload"] = json!(format!("served {i}"));
        } else {
            e["jsonPayload"] = json!({ "message": format!("charged {i}"), "amount": i * 10 });
        }
        if i == 3 {
            e["trace"] = json!("projects/proj-e2e/traces/0af7651916cd43dd8448eb211c80319c");
            e["spanId"] = json!("b7ad6b7169203331");
        }
        gcp_state.entries.push(e);
    }
    gcp_state.entries.push(json!({ "logName": "projects/proj-e2e/logs/syslog", "insertId": "vm0", "timestamp": rfc3339(t0 - 100_000), "textPayload": "kernel", "resource": { "type": "gce_instance", "labels": { "instance_id": "1" } } }));
    let gcp_state = Arc::new(Mutex::new(gcp_state));
    let gcp_port = common::free_port()?;
    let token_uri = format!("http://127.0.0.1:{gcp_port}/token");
    let (key, public) = service_account(&token_uri)?;
    let gcp_mock = Service::start_on(gcp_port, gcp(gcp_state.clone(), public, token_uri.clone()))?;
    // Log Analytics: fifteen traces of the checkout app, their levels 0 to 3 in turn.
    let rows: Arc<Mutex<Vec<(i64, String, String, i64)>>> = Arc::new(Mutex::new((0..15i64).map(|i| (t0 - 270_000 + i * 15_000, format!("a{i}"), format!("trace {i}"), i % 4)).collect()));
    let azure_port = common::free_port()?;
    let arm = format!("http://127.0.0.1:{azure_port}");
    let azure_mock = Service::start_on(azure_port, azure_with(arm.clone(), rows.clone()))?;
    let f = Fleet::new()?;

    let aws_cfg = merge(
        json!({
            "kind": "aws", "regions": ["us-east-1"], "namespaces": ["AWS/Lambda"],
            "access_key_id": AWS_BASE.0, "secret_access_key": AWS_BASE.1, "role_arn": AWS_ROLE, "external_id": "e2e-external",
            "endpoint": format!("{}/cw/{{region}}/", aws_mock.url()), "sts_endpoint": format!("{}/sts/{{region}}/", aws_mock.url()),
            "logs_endpoint": format!("{}/logs/{{region}}/", aws_mock.url()), "log_groups": ["/aws/lambda/*"], "tags": ["account:prod"],
        }),
        often(),
    );
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/prod", Some(&aws_cfg))?.0 == 204);
    let gcp_cfg = merge(json!({ "kind": "gcp", "services": ["compute.googleapis.com"], "service_account": key, "endpoint": gcp_mock.url(), "logs": "resource.type=\"cloud_run_revision\"", "logs_endpoint": gcp_mock.url() }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/gcp-main", Some(&gcp_cfg))?.0 == 204);
    let azure_cfg = merge(json!({ "kind": "azure", "tenant_id": "tenant-e2e", "client_id": "app-e2e", "client_secret": "s3cret-az", "authority": arm, "endpoint": arm, "workspaces": [WS], "log_query": "AppTraces", "logs_endpoint": format!("{arm}/la") }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/azure-main", Some(&azure_cfg))?.0 == 204);

    // The first pulls take every record of the lookback; three more, none again.
    for (name, n) in [("prod", 34u64), ("gcp-main", 20), ("azure-main", 15)] {
        let c = until_cloud(&f, name, Duration::from_secs(60), |s| s["state"] == "ok" && s["records_total"].as_u64().unwrap_or(0) >= n)?;
        anyhow::ensure!(c["status"]["records_total"] == n && c["status"]["records_deferred"] == 0, "{name}: {:#}", c["status"]);
        let pulls = c["status"]["pulls"].as_u64().unwrap_or(0);
        let c = until_cloud(&f, name, Duration::from_secs(60), |s| s["pulls"].as_u64().unwrap_or(0) >= pulls + 3)?;
        anyhow::ensure!(c["status"]["records"] == 0 && c["status"]["records_total"] == n, "{name}: later pulls take none again: {:#}", c["status"]);
    }
    // Records come late to a cloud: one each, inside the lookback, taken by the next pull, once.
    aws_state.lock().unwrap_or_else(PoisonError::into_inner).events.push(("/aws/lambda/checkout".into(), "s9".into(), t0 - 250_000, "late".into(), "[WARN]\tlate\tretried".into()));
    let mut late = json!({ "logName": "projects/proj-e2e/logs/run.googleapis.com%2Fstdout", "insertId": "glate", "timestamp": rfc3339(t0 - 250_000), "severity": "WARNING", "textPayload": "late", "resource": { "type": "cloud_run_revision", "labels": { "service_name": "checkout" } } });
    gcp_state.lock().unwrap_or_else(PoisonError::into_inner).entries.push(late.take());
    rows.lock().unwrap_or_else(PoisonError::into_inner).push((t0 - 250_000, "alate".into(), "late".into(), 2));
    for name in ["prod", "gcp-main", "azure-main"] {
        let pulls = cloud(&f, name)?["status"]["pulls"].as_u64().unwrap_or(0);
        until_cloud(&f, name, Duration::from_secs(60), |s| s["pulls"].as_u64().unwrap_or(0) >= pulls + 3)?;
    }

    // CloudWatch Logs: Datadog's forwarder's names, each record once; the RDS group not asked.
    let from = t0 - 600_000;
    let checkout = records(&f, "source:lambda service:checkout", from, 31)?;
    anyhow::ensure!(checkout.len() == 31, "{} of 31 records: {checkout:#?}", checkout.len());
    let errors = records(&f, "source:lambda service:checkout status:error", from, 5)?;
    anyhow::ensure!(errors.len() == 5 && errors.iter().all(|r| r["message"].as_str().is_some_and(|m| m.contains("failed"))), "{errors:#?}");
    let one = checkout.iter().find(|r| r["message"].as_str().is_some_and(|m| m.contains("payment 3\t") || m.ends_with("payment 3"))).ok_or_else(|| anyhow::anyhow!("payment 3"))?;
    anyhow::ensure!(one["attributes"]["aws.awslogs.logGroup"] == "/aws/lambda/checkout" && one["attributes"]["region"] == "us-east-1" && one["attributes"]["account"] == "prod" && one["ts_ns"] == (t0 - 290_000 + 30_000) * 1_000_000, "{one:#}");
    anyhow::ensure!(records(&f, "source:lambda service:checkout status:warn", from, 1)?.len() == 1, "the late event, once");
    anyhow::ensure!(records(&f, "source:lambda service:refunds", from, 4)?.len() == 4);
    anyhow::ensure!(records(&f, "source:rds", from, 0)?.is_empty(), "a group the prefix does not name is not read");
    // Cloud Logging: the filter asked within each pull's window; JSON payloads' values attributes.
    let run = records(&f, "source:gcp.cloud_run_revision service:checkout", from, 21)?;
    anyhow::ensure!(run.len() == 21, "{} of 21 entries", run.len());
    let charged = run.iter().find(|r| r["message"] == "charged 3").ok_or_else(|| anyhow::anyhow!("charged 3"))?;
    anyhow::ensure!(charged["attributes"]["amount"] == "30" && charged["trace_id"] == "0af7651916cd43dd8448eb211c80319c" && charged["span_id"] == "b7ad6b7169203331" && charged["status"] == "info", "{charged:#}");
    anyhow::ensure!(records(&f, "source:gcp.cloud_run_revision status:error", from, 4)?.len() == 4);
    anyhow::ensure!(records(&f, "source:gcp.gce_instance", from, 0)?.is_empty(), "the filter's resource type alone");
    let filters = gcp_state.lock().unwrap_or_else(PoisonError::into_inner).filters.clone();
    anyhow::ensure!(!filters.is_empty() && filters.iter().all(|q| q.starts_with("(resource.type=\"cloud_run_revision\") AND timestamp>=\"")), "{filters:?}");
    // Log Analytics: Application Insights' levels; the resource's name the service.
    let traces = records(&f, "source:azure.apptraces service:checkout", from, 16)?;
    anyhow::ensure!(traces.len() == 16, "{} of 16 rows", traces.len());
    let t3 = traces.iter().find(|r| r["message"] == "trace 3").ok_or_else(|| anyhow::anyhow!("trace 3"))?;
    anyhow::ensure!(t3["status"] == "error" && t3["attributes"]["AppRoleName"] == "api" && t3["attributes"]["azure.workspace"] == WS, "{t3:#}");
    anyhow::ensure!(records(&f, "source:azure.apptraces status:warn", from, 5)?.len() == 5, "levels 2: four and the late one");
    anyhow::ensure!(azure_mock.count(|r| r.path.starts_with("/la/")) > 0 && azure_mock.count(|r| r.path.starts_with("/la/") && r.header("authorization") != Some("Bearer az-logs-token")) == 0, "Log Analytics with its own token alone");

    // Every call signed or authorized as the cloud checks it; kiln's own count of what it took.
    anyhow::ensure!(aws_state.lock().unwrap_or_else(PoisonError::into_inner).bad_signatures.is_empty());
    anyhow::ensure!(gcp_state.lock().unwrap_or_else(PoisonError::into_inner).bad_assertions.is_empty());
    let taken = read(&f, "sum:kiln.cloud.records{*} by {integration}.as_count()", t0 - 60_000, now_ms() + 60_000)?;
    let total = |name: &str| taken.iter().find(|(t, _)| t.get("integration").map(String::as_str) == Some(name)).map_or(0.0, |(_, v)| v.values().sum::<f64>());
    anyhow::ensure!(total("prod") == 35.0 && total("gcp-main") == 21.0 && total("azure-main") == 16.0, "{taken:?}");
    Ok(())
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

/// A Firehose delivery as Kinesis Data Firehose sends one: its records base64, the body gzip'd,
/// its id, its stream's ARN and its common attributes in its headers.
fn firehose(f: &Fleet, key: &str, request: &str, records: &[Vec<u8>]) -> anyhow::Result<(u16, Value)> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let body = json!({ "requestId": request, "timestamp": now_ms(), "records": records.iter().map(|r| json!({ "data": b64.encode(r) })).collect::<Vec<_>>() });
    let headers = format!(
        "X-Amz-Firehose-Request-Id: {request}\r\nX-Amz-Firehose-Source-Arn: arn:aws:firehose:eu-west-1:123456789012:deliverystream/kiln\r\nX-Amz-Firehose-Access-Key: {key}\r\nX-Amz-Firehose-Common-Attributes: {{\"commonAttributes\":{{\"team\":\"pay\"}}}}\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\n"
    );
    let (code, answer) = raw(f.http, "POST", "/v1/firehose", &headers, &gz(body.to_string().as_bytes()))?;
    Ok((code, serde_json::from_slice(&answer).unwrap_or(Value::Null)))
}

#[test]
#[ignore = "starts a kiln server as a process"]
fn the_clouds_pushes_are_taken_once_each_as_their_pulls_name_them() -> anyhow::Result<()> {
    let f = Fleet::new()?;
    let ingest = std::fs::read_to_string(f.root.path().join("server").join("observe").join("ingest.token"))?.trim().to_owned();
    let t = now_ms().div_euclid(60_000) * 60_000 - 600_000;
    // A metric stream's JSON: two instances' CPU, five minutes, the sum and count of each minute.
    let mut lines = String::new();
    for m in 0..5i64 {
        for (i, base) in [("i-1", 10.0), ("i-2", 40.0)] {
            let count = 5.0;
            let sum = (base + m as f64) * count;
            lines.push_str(&json!({ "metric_stream_name": "kiln", "account_id": "123456789012", "region": "eu-west-1", "namespace": "AWS/EC2", "metric_name": "CPUUtilization", "dimensions": { "InstanceId": i }, "timestamp": t + m * 60_000, "value": { "max": base + m as f64 + 5.0, "min": base, "sum": sum, "count": count }, "unit": "Percent" }).to_string());
            lines.push('\n');
        }
    }
    let events: Vec<Value> = (0..6i64).map(|i| json!({ "id": format!("ev{i}"), "timestamp": t + i * 1000, "message": if i == 2 { "[ERROR] card declined".to_owned() } else { format!("[INFO] charge {i}") } })).collect();
    let logs = json!({ "messageType": "DATA_MESSAGE", "owner": "123456789012", "logGroup": "/aws/lambda/checkout", "logStream": "2026/10/18/[$LATEST]a", "subscriptionFilters": ["kiln"], "logEvents": events });
    let control = json!({ "messageType": "CONTROL_MESSAGE", "owner": "CloudwatchLogs", "logGroup": "", "logStream": "", "subscriptionFilters": [], "logEvents": [{ "id": "", "timestamp": t, "message": "CWL CONTROL MESSAGE: Checking health of destination Firehose." }] });
    let delivery = [lines.into_bytes(), gz(logs.to_string().as_bytes()), gz(control.to_string().as_bytes())];

    // Its key must be the ingest token; the answer is Firehose's, its id echoed.
    let (code, answer) = firehose(&f, "not-the-token", "req-0", &delivery)?;
    anyhow::ensure!(code == 401 && answer["requestId"] == "req-0" && answer["errorMessage"].as_str().is_some_and(|m| m.contains("ingest token")), "{code} {answer}");
    let (code, answer) = firehose(&f, &ingest, "req-1", &delivery)?;
    anyhow::ensure!(code == 200 && answer["requestId"] == "req-1" && answer["timestamp"].as_i64().is_some() && answer.get("errorMessage").is_none(), "{code} {answer}");
    // Sent again (Firehose retries what it did not hear back of): taken once.
    let (code, answer) = firehose(&f, &ingest, "req-1", &delivery)?;
    anyhow::ensure!(code == 200 && answer["requestId"] == "req-1", "{code} {answer}");
    // Another delivery with an event already taken (CloudWatch Logs delivers at least once): once.
    let again = json!({ "messageType": "DATA_MESSAGE", "owner": "123456789012", "logGroup": "/aws/lambda/checkout", "logStream": "2026/10/18/[$LATEST]a", "subscriptionFilters": ["kiln"], "logEvents": [events[0].clone(), { "id": "ev9", "timestamp": t + 9_000, "message": "[INFO] charge 9" }] });
    let (code, _) = firehose(&f, &ingest, "req-2", &[gz(again.to_string().as_bytes())])?;
    anyhow::ensure!(code == 200);

    // The pulls' names: the average the name itself, the sums and counts counts, the extremes gauges.
    let (from, to) = (t, t + 300_000);
    expect(&f, "avg:aws.ec2.cpuutilization{instanceid:i-1,region:eu-west-1,aws_account:123456789012,team:pay}", from, to, |m| 10.0 + ((m - t) / 60_000) as f64)?;
    expect(&f, "avg:aws.ec2.cpuutilization{instanceid:i-2}", from, to, |m| 40.0 + ((m - t) / 60_000) as f64)?;
    expect(&f, "sum:aws.ec2.cpuutilization.samplecount{instanceid:i-2}.as_count()", from, to, |_| 5.0)?;
    expect(&f, "max:aws.ec2.cpuutilization.maximum{instanceid:i-1}", from, to, |m| 15.0 + ((m - t) / 60_000) as f64)?;
    let checkout = records(&f, "source:lambda service:checkout", t - 60_000, 7)?;
    anyhow::ensure!(checkout.len() == 7, "six events and the new one, each once: {} {checkout:#?}", checkout.len());
    anyhow::ensure!(checkout.iter().all(|r| r["attributes"]["region"] == "eu-west-1" && r["attributes"]["aws_account"] == "123456789012" && r["attributes"]["team"] == "pay"), "{checkout:#?}");
    anyhow::ensure!(records(&f, "source:lambda status:error", t - 60_000, 1)?.len() == 1);

    // Pub/Sub: a Cloud Logging sink's entry, pushed twice (at least once); a wrong token refused.
    let b64 = base64::engine::general_purpose::STANDARD;
    let entry = json!({ "logName": "projects/proj-e2e/logs/run.googleapis.com%2Fstdout", "insertId": "p1", "timestamp": rfc3339(t + 30_000), "severity": "ERROR", "textPayload": "pushed", "resource": { "type": "cloud_run_revision", "labels": { "service_name": "web" } } });
    let push = json!({ "message": { "data": b64.encode(entry.to_string()), "messageId": "m-1", "publishTime": rfc3339(t + 31_000) }, "subscription": "projects/proj-e2e/subscriptions/kiln" });
    let send = |token: &str| raw(f.http, "POST", &format!("/v1/pubsub?token={token}"), "Content-Type: application/json\r\n", push.to_string().as_bytes()).map(|(c, _)| c);
    anyhow::ensure!(send("wrong")? == 401);
    anyhow::ensure!(send(&ingest)? == 204 && send(&ingest)? == 204, "acknowledged, both times");
    let web = records(&f, "source:gcp.cloud_run_revision service:web", t - 60_000, 1)?;
    std::thread::sleep(Duration::from_secs(2));
    let web_after = records(&f, "source:gcp.cloud_run_revision service:web", t - 60_000, 2)?;
    anyhow::ensure!(web.len() == 1 && web_after.len() == 1 && web_after[0]["status"] == "error" && web_after[0]["message"] == "pushed", "{web_after:#?}");

    // kiln's own count of the pushes: what was taken, sent again and refused (added up, written
    // every 15 s).
    std::thread::sleep(Duration::from_secs(16));
    let counted = read(&f, "sum:kiln.cloud.push.records{*} by {via}.as_count()", t - 60_000, now_ms() + 60_000)?;
    let total = |via: &str| counted.iter().find(|(tags, _)| tags.get("via").map(String::as_str) == Some(via)).map_or(0.0, |(_, v)| v.values().sum::<f64>());
    anyhow::ensure!(total("firehose") == 7.0 && total("pubsub") == 1.0, "{counted:?}");
    let again = read(&f, "sum:kiln.cloud.push.again{*} by {via}.as_count()", t - 60_000, now_ms() + 60_000)?;
    let total = |via: &str| again.iter().find(|(tags, _)| tags.get("via").map(String::as_str) == Some(via)).map_or(0.0, |(_, v)| v.values().sum::<f64>());
    anyhow::ensure!(total("firehose") == 1.0 && total("pubsub") == 1.0, "{again:?}");
    Ok(())
}

#[test]
#[ignore = "starts a kiln server as a process, and services that stand in for AWS and Azure"]
fn a_cloud_that_fails_is_said_to_and_its_recovery_too_and_a_wrong_secret_says_why() -> anyhow::Result<()> {
    let aws_state = Arc::new(Mutex::new(AwsState::default()));
    let aws_mock = Service::start(aws(aws_state.clone()))?;
    let azure_port = common::free_port()?;
    let arm = format!("http://127.0.0.1:{azure_port}");
    let azure_mock = Service::start_on(azure_port, azure(arm.clone()))?;
    let f = Fleet::new()?;
    let t0 = now_ms();
    let aws_cfg = merge(
        json!({
            "kind": "aws", "regions": ["us-east-1"], "namespaces": ["AWS/Lambda"],
            "access_key_id": AWS_BASE.0, "secret_access_key": AWS_BASE.1, "role_arn": AWS_ROLE, "external_id": "e2e-external",
            "endpoint": format!("{}/cw/{{region}}/", aws_mock.url()), "sts_endpoint": format!("{}/sts/{{region}}/", aws_mock.url()),
        }),
        often(),
    );
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/prod", Some(&aws_cfg))?.0 == 204);
    until_cloud(&f, "prod", Duration::from_secs(60), |s| s["state"] == "ok")?;

    // CloudWatch goes away: each call tried four times, then the pull fails, said once as an event.
    aws_state.lock().unwrap_or_else(PoisonError::into_inner).down = true;
    let downed = now_ms();
    f.call("POST", "/api/observe/v1/clouds/prod/pull", Some(&json!({})))?;
    let c = until_cloud(&f, "prod", Duration::from_secs(60), |s| s["state"] == "failing")?;
    anyhow::ensure!(c["status"]["error"].as_str().is_some_and(|e| e.contains("503")), "{c:#}");
    let tries = aws_mock.count(|r| r.path.starts_with("/cw/"));
    std::thread::sleep(Duration::from_secs(2));
    anyhow::ensure!(aws_mock.count(|r| r.path.starts_with("/cw/")) == tries, "a failed pull waits a minute before the next, not its interval");
    let failing = events(&f, "service:kiln.cloud @integration:prod @result:failing", downed - 1000, 1)?;
    anyhow::ensure!(failing.len() == 1 && failing.iter().all(|m| m.contains("503")), "{failing:?}");
    // It comes back: the next pull (asked now) recovers, and says so.
    aws_state.lock().unwrap_or_else(PoisonError::into_inner).down = false;
    f.call("POST", "/api/observe/v1/clouds/prod/pull", Some(&json!({})))?;
    until_cloud(&f, "prod", Duration::from_secs(60), |s| s["state"] == "ok" && s["last_ok"].as_i64().unwrap_or(0) > downed)?;
    let recovered = events(&f, "service:kiln.cloud @integration:prod @result:recovered", downed - 1000, 1)?;
    anyhow::ensure!(recovered.len() == 1, "{recovered:?}");

    // A wrong secret: the integration fails, and says Entra's words why.
    let azure_cfg = merge(json!({ "kind": "azure", "tenant_id": "tenant-e2e", "client_id": "app-e2e", "client_secret": "wrong", "authority": arm, "endpoint": arm }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/az", Some(&azure_cfg))?.0 == 204);
    let c = until_cloud(&f, "az", Duration::from_secs(60), |s| s["state"] == "failing")?;
    anyhow::ensure!(c["status"]["error"].as_str().is_some_and(|e| e.contains("AADSTS7000215") && e.contains("invalid_client")), "{c:#}");
    // Settings changed without the secret keep the one kept (still wrong); the right one recovers;
    // changed again without it, the right one stays.
    let hidden = merge(azure_cfg.clone(), json!({ "client_secret": "…", "aggregations": ["Maximum"] }));
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/az", Some(&hidden))?.0 == 204);
    let c = until_cloud(&f, "az", Duration::from_secs(60), |s| s["state"] == "failing")?;
    anyhow::ensure!(c["aggregations"] == json!(["Maximum"]));
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/az", Some(&merge(azure_cfg.clone(), json!({ "client_secret": "s3cret-az" }))))?.0 == 204);
    until_cloud(&f, "az", Duration::from_secs(60), |s| s["state"] == "ok")?;
    let mut without = merge(azure_cfg.clone(), json!({ "aggregations": ["Minimum"] }));
    without.as_object_mut().map(|o| o.remove("client_secret"));
    let changed = now_ms();
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/az", Some(&without))?.0 == 204);
    until_cloud(&f, "az", Duration::from_secs(60), |s| s["state"] == "ok" && s["last_ok"].as_i64().unwrap_or(0) >= changed)?;
    anyhow::ensure!(azure_mock.count(|r| r.form().get("client_secret").map(String::as_str) == Some("s3cret-az")) >= 2);

    // Settings that cannot be are refused, saying which.
    let (code, body) = f.request("PUT", "/api/observe/v1/clouds/bad", Some(&merge(aws_cfg.clone(), json!({ "period_s": 90 }))))?;
    anyhow::ensure!(code == 400 && String::from_utf8_lossy(&body).contains("period_s"), "{code} {}", String::from_utf8_lossy(&body));
    let (code, _) = f.request("PUT", "/api/observe/v1/clouds/bad name", Some(&aws_cfg))?;
    anyhow::ensure!(code == 400 || code == 404, "a name with a space: {code}");

    // Deleted, an integration is pulled no more.
    anyhow::ensure!(f.request("DELETE", "/api/observe/v1/clouds/prod", None)?.0 == 204);
    anyhow::ensure!(clouds(&f)?.iter().all(|c| c["name"] != "prod"));
    let calls = aws_mock.count(|_| true);
    std::thread::sleep(Duration::from_secs(12));
    anyhow::ensure!(aws_mock.count(|_| true) == calls, "a deleted integration's task stopped");
    anyhow::ensure!(f.request("DELETE", "/api/observe/v1/clouds/prod", None)?.0 == 404);
    let _ = t0;
    Ok(())
}

#[test]
#[ignore = "starts a kiln server as a process, and services that stand in for each cloud's metadata service"]
fn the_server_s_own_credentials_are_found_where_it_runs() -> anyhow::Result<()> {
    let aws_mock = Service::start(aws(Arc::new(Mutex::new(AwsState::default()))))?;
    let gcp_port = common::free_port()?;
    let gcp_mock = Service::start_on(gcp_port, gcp(Arc::new(Mutex::new(GcpState::default())), Vec::new(), String::new()))?;
    let azure_port = common::free_port()?;
    let arm = format!("http://127.0.0.1:{azure_port}");
    let azure_mock = Service::start_on(azure_port, azure(arm.clone()))?;
    // An EC2 instance's, a Compute Engine VM's and an Azure VM's metadata services, as their SDKs
    // are told where they are.
    let (aws_url, gcp_host) = (aws_mock.url(), format!("127.0.0.1:{gcp_port}"));
    let f = Fleet::with_env(|_| String::new(), &[("AWS_EC2_METADATA_SERVICE_ENDPOINT", &aws_url), ("GCE_METADATA_HOST", &gcp_host), ("AZURE_POD_IDENTITY_AUTHORITY_HOST", &arm)])?;
    // No keys, no region, no project, no tenant: the server's own.
    let aws_cfg = merge(json!({ "kind": "aws", "namespaces": ["AWS/EC2"], "endpoint": format!("{aws_url}/cw/{{region}}/") }), often());
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/ec2", Some(&aws_cfg))?.0 == 204);
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/gce", Some(&merge(json!({ "kind": "gcp", "services": ["compute.googleapis.com/instance/cpu"], "endpoint": gcp_mock.url() }), often())))?.0 == 204);
    anyhow::ensure!(f.request("PUT", "/api/observe/v1/clouds/azvm", Some(&merge(json!({ "kind": "azure", "resource_types": ["Microsoft.Compute/virtualMachines"], "endpoint": arm }), often())))?.0 == 204);
    let c = until_cloud(&f, "ec2", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    let (from, to) = window(&c);
    expect(&f, "avg:aws.ec2.cpuutilization{instanceid:i-1,region:eu-west-1}", from, to, |t| cpu("i-1", t))?;
    anyhow::ensure!(aws_mock.count(|r| r.path == "/latest/api/token" && r.method == "PUT") >= 1);
    let c = until_cloud(&f, "gce", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    let (from, to) = window(&c);
    expect(&f, "avg:gcp.compute.instance.cpu.utilization{project_id:proj-gce}", from, to, utilization)?;
    let c = until_cloud(&f, "azvm", Duration::from_secs(60), |s| s["state"] == "ok" && s["points"].as_u64().unwrap_or(0) > 0)?;
    let (from, to) = window(&c);
    expect(&f, "avg:azure.compute_virtualmachines.percentage_cpu{name:vm-1}", from, to, |t| vm_cpu("vm-1", t))?;
    anyhow::ensure!(azure_mock.count(|r| r.path == "/metadata/identity/oauth2/token") >= 1);
    anyhow::ensure!(azure_mock.count(|r| r.path.contains("networkSecurityGroups")) == 0, "only the types asked are listed, by the list's filter");
    Ok(())
}

/// Prometheus's scrapes (docs/observe.md §8, §13): an exporter served here, a counter climbing a
/// hundred a second and a gauge; a target the server reads, one the runner reads, one every
/// machine reads and one that asks for a token. The samples arrive tagged `job` and `instance` (and
/// the machine's `host` where every machine reads its own), a sample's own `job` kept as
/// `exported_job`, each target's `up` 1, read where its target says; the counter as a rate is the
/// hundred it climbs by; the exporter failing makes `up` 0 and is said once, and its recovery too;
/// a target deleted is read no more; the token is sent, and never shown.
#[test]
#[ignore = "starts a kiln server and a runner as processes, and an exporter for them"]
fn a_prometheus_exporter_is_read_where_its_target_says_and_tagged_as_prometheus_tags_it() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let failing = Arc::new(AtomicBool::new(false));
    let born = Instant::now();
    let exporter = {
        let failing = failing.clone();
        Service::start(move |r: &Req| {
            if failing.load(Ordering::Relaxed) {
                return text(500, "down");
            }
            if r.path == "/secure" && r.header("authorization") != Some("Bearer t0ken") {
                return text(401, "no token");
            }
            let climbed = (born.elapsed().as_secs_f64() * 100.0).floor();
            let body = format!("# HELP e2e_requests_total Requests.\n# TYPE e2e_requests_total counter\ne2e_requests_total{{code=\"200\",job=\"theirs\"}} {climbed}\n# TYPE e2e_temperature gauge\ne2e_temperature 21.5\n");
            Resp { status: 200, headers: vec![("content-type".into(), "text/plain; version=0.0.4; charset=utf-8".into())], body: body.into_bytes() }
        })?
    };
    let mut f = Fleet::new()?;
    f.start_runner("r1")?;
    f.until(Duration::from_secs(90), "r1 a learner, caught up", |s| common::caught_up(s, &["r1"]))?;
    let url = |path: &str| format!("{}{path}", exporter.url());
    // Tested first: what it would read.
    let tested = f.call("POST", "/api/observe/v1/scrapes/srv/test", Some(&json!({ "url": url("/metrics"), "every_s": 1, "timeout_s": 1 })))?;
    anyhow::ensure!(tested["ok"] == true && tested["samples"] == 2 && tested["sample"].as_array().is_some_and(|m| m.iter().any(|n| n == "e2e_requests_total")), "{tested:#}");
    for (name, target) in [
        ("srv", json!({ "url": url("/metrics"), "every_s": 1, "timeout_s": 1, "tags": ["env:e2e"] })),
        ("run", json!({ "url": url("/metrics?r=1"), "every_s": 1, "timeout_s": 1, "on": "r1" })),
        ("all", json!({ "url": url("/metrics?a=1"), "every_s": 1, "timeout_s": 1, "on": "*" })),
        ("sec", json!({ "url": url("/secure"), "every_s": 1, "timeout_s": 1, "bearer": "t0ken" })),
    ] {
        let (code, body) = f.request("PUT", &format!("/api/observe/v1/scrapes/{name}"), Some(&target))?;
        anyhow::ensure!(code == 204, "{name}: {code} {}", String::from_utf8_lossy(&body));
    }
    let t0 = now_ms();
    // Each target up, read where it says: by job; every machine's by host; the runner's own by r1.
    let start = Instant::now();
    loop {
        let up = read(&f, "max:up{*} by {job,host}", t0 - 60_000, now_ms() + 60_000)?;
        let ones: Vec<(String, String)> = up.iter().filter(|(_, v)| v.values().last().copied() == Some(1.0)).map(|(t, _)| (t.get("job").cloned().unwrap_or_default(), t.get("host").cloned().unwrap_or_default())).collect();
        let jobs = |j: &str| ones.iter().filter(|(job, _)| job == j).count();
        if jobs("srv") == 1 && jobs("run") == 1 && jobs("sec") == 1 && jobs("all") == 2 && ones.iter().any(|(j, h)| j == "all" && h == "r1") {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(60), "up by job and host: {up:?}\n{}", f.logs());
        std::thread::sleep(Duration::from_millis(500));
    }
    // Who reads what, as each machine reports it (a runner's every 10 s, with its status).
    let start = Instant::now();
    loop {
        let listed = f.call("GET", "/api/observe/v1/scrapes", None)?;
        anyhow::ensure!(!listed.to_string().contains("t0ken"), "a token shown: {listed}");
        let entry = |name: &str| listed["scrapes"].as_array().and_then(|l| l.iter().find(|t| t["name"] == name).cloned()).unwrap_or(Value::Null);
        anyhow::ensure!(entry("sec")["bearer"] == true, "{listed:#}");
        let machines = |name: &str| -> Vec<String> {
            let mut m: Vec<String> = entry(name)["machines"].as_array().into_iter().flatten().filter(|m| m["state"]["up"] == true).filter_map(|m| m["machine"].as_str().map(str::to_owned)).collect();
            m.sort();
            m
        };
        if machines("run") == ["r1"] && machines("srv") == ["server"] && machines("all") == ["r1", "server"] {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(40), "who reads what: {listed:#}");
        std::thread::sleep(Duration::from_millis(500));
    }
    // Tagged as Prometheus tags them: the target's `job` over the sample's, kept as `exported_job`.
    let t1 = now_ms();
    std::thread::sleep(Duration::from_secs(25));
    let rows = read(&f, "sum:e2e_requests_total{job:srv,env:e2e} by {exported_job,instance}", t1 - 60_000, now_ms() + 60_000)?;
    let [(tags, _)] = rows.as_slice() else { anyhow::bail!("one series: {rows:?}") };
    anyhow::ensure!(tags.get("exported_job").map(String::as_str) == Some("theirs") && tags.get("instance").is_some_and(|i| i.starts_with("127.0.0.1:")), "{tags:?}");
    // A counter read as a rate: the hundred a second it climbs.
    let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "sum:e2e_requests_total{job:srv}", "from": t1, "to": now_ms() - 2_000, "step": 5_000 })))?;
    let rates: Vec<f64> = a["series"][0]["values"].as_array().into_iter().flatten().filter_map(Value::as_f64).collect();
    // The whole buckets: not the first nor the last, which the range cuts.
    let whole = rates.get(1..rates.len().saturating_sub(1)).unwrap_or(&[]);
    anyhow::ensure!(whole.len() >= 2 && whole.iter().all(|r| (90.0..=110.0).contains(r)), "the counter's rate: {rates:?}");
    // The exporter down: `up` 0, said once; up again, said too.
    failing.store(true, Ordering::Relaxed);
    let start = Instant::now();
    loop {
        // A second's grid: the newest scrape's `up`.
        let a = f.call("POST", "/api/observe/v1/query", Some(&json!({ "query": "max:up{job:srv}", "from": now_ms() - 10_000, "to": now_ms(), "step": 1_000 })))?;
        let newest = a["series"][0]["values"].as_array().into_iter().flatten().rev().find_map(Value::as_f64);
        if newest == Some(0.0) {
            break;
        }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(30), "up never 0: {a}");
        std::thread::sleep(Duration::from_millis(250));
    }
    let down = events(&f, "source:kiln.event @event.type:scrape @target:srv @result:failing", t0, 1)?;
    anyhow::ensure!(down.len() == 1 && down[0].contains("scrape target srv is down"), "{down:?}");
    failing.store(false, Ordering::Relaxed);
    let back = events(&f, "source:kiln.event @event.type:scrape @target:srv @result:recovered", t0, 1)?;
    anyhow::ensure!(back.len() == 1 && back[0].contains("is up again"), "{back:?}");
    // Deleted: read no more, by the runner either.
    anyhow::ensure!(f.request("DELETE", "/api/observe/v1/scrapes/run", None)?.0 == 204);
    std::thread::sleep(Duration::from_secs(3));
    let before = exporter.count(|r| r.query.contains_key("r"));
    std::thread::sleep(Duration::from_secs(4));
    anyhow::ensure!(exporter.count(|r| r.query.contains_key("r")) == before, "the runner reads a target gone");
    anyhow::ensure!(f.request("DELETE", "/api/observe/v1/scrapes/run", None)?.0 == 404);
    Ok(())
}
