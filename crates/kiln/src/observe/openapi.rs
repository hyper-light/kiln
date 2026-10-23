//! Observe's API, described (`docs/observe.md` §7): an OpenAPI 3.1 document of every route under
//! `/api/observe/v1/` and of the routes that take what is sent in (OTLP, Firehose, Pub/Sub), each
//! with what it takes and answers, and the capability it needs. Served at
//! `GET /api/observe/v1/openapi.json`.
//!
//! Kept honest twice: a test reads the router's routes from its source and finds each here (and
//! nothing here it does not route), each with the capability the gate asks (`crate::authz::rule`);
//! the live scenarios check what the routes answer against the schemas here.

use serde_json::{Map, Value, json};

fn string() -> Value {
    json!({ "type": "string" })
}

fn integer() -> Value {
    json!({ "type": "integer" })
}

fn number() -> Value {
    json!({ "type": "number" })
}

fn boolean() -> Value {
    json!({ "type": "boolean" })
}

fn array(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}

/// A string of one of `values`.
fn one_of(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}

/// `x`, or null.
fn nullable(x: Value) -> Value {
    json!({ "anyOf": [x, { "type": "null" }] })
}

/// An object of `props` (its other keys allowed), `required` among them.
fn object(props: &[(&str, Value)], required: &[&str]) -> Value {
    let props: Map<String, Value> = props.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect();
    json!({ "type": "object", "properties": props, "required": required })
}

/// An object whose every value is `x`.
fn map_of(x: Value) -> Value {
    json!({ "type": "object", "additionalProperties": x })
}

fn describe(mut x: Value, what: &str) -> Value {
    if let Some(o) = x.as_object_mut() {
        o.insert("description".into(), what.into());
    }
    x
}

fn schema(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// Unix ms.
fn ms() -> Value {
    describe(integer(), "milliseconds since the epoch")
}

/// Every schema the routes name.
fn schemas() -> Map<String, Value> {
    let tags = describe(map_of(string()), "tag keys to values");
    let mut s = Map::new();
    let mut put = |k: &str, v: Value| {
        s.insert(k.to_owned(), v);
    };
    put("Error", object(&[("error", string())], &["error"]));
    put(
        "QueryRequest",
        object(
            &[
                ("query", describe(string(), "a metric query (docs/observe.md §6): `avg:system.cpu.user{env:prod} by {host}`")),
                ("from", ms()),
                ("to", ms()),
                ("step", describe(integer(), "each bucket's width in ms; else the nicest that fits `points`")),
                ("points", describe(integer(), "buckets a series at the most (1,200 unless said)")),
                ("max_series", integer()),
                ("heatmap", describe(integer(), "a heatmap of so many rows at the most instead of the series")),
            ],
            &["query", "from", "to"],
        ),
    );
    put(
        "CheckStatusRequest",
        object(
            &[
                ("check", describe(string(), "a service check's name: `datadog.agent.up`")),
                ("tags", describe(array(string()), "the runs it reads: `key:value` (a value with `*` a glob) or a bare tag, each; `*` or none, every run")),
                ("group_by", describe(array(string()), "what a group is (none: each of the check's series apart)")),
                ("group", describe(nullable(string()), "one group's tags, `key:value` comma-separated (Datadog's grouping by check)")),
                ("from", ms()),
                ("to", ms()),
            ],
            &["check", "from", "to"],
        ),
    );
    let check_status = one_of(&["OK", "WARNING", "CRITICAL", "UNKNOWN"]);
    put(
        "CheckStatus",
        object(
            &[
                ("check", string()),
                ("status", describe(nullable(check_status.clone()), "the worst group's (none: no run in the time)")),
                ("counts", describe(map_of(integer()), "groups in each status")),
                ("groups", describe(array(object(&[("tags", tags.clone()), ("status", check_status), ("at", ms())], &["tags", "status", "at"])), "each group's, the first 1,000")),
                ("total", describe(integer(), "groups")),
            ],
            &["check", "status", "counts", "groups", "total"],
        ),
    );
    put(
        "Grid",
        object(
            &[
                ("from", ms()),
                ("step", describe(integer(), "ms; a calendar's nominal period")),
                ("n", describe(integer(), "buckets")),
                ("times", describe(array(integer()), "a calendar rollup's buckets' starts (unix ms)")),
                ("calendar", one_of(&["daily", "weekly", "monthly"])),
            ],
            &["from", "step", "n"],
        ),
    );
    put(
        "Series",
        object(
            &[
                ("tags", tags.clone()),
                ("values", describe(array(nullable(number())), "a value a bucket; null where there is none")),
                (
                    "bounds",
                    describe(
                        object(
                            &[
                                ("lower", array(nullable(number()))),
                                ("upper", array(nullable(number()))),
                                ("score", array(nullable(number()))),
                                ("k", number()),
                                ("direction", one_of(&["both", "above", "below"])),
                                ("algorithm", string()),
                                ("forecast_from", nullable(integer())),
                            ],
                            &["lower", "upper"],
                        ),
                        "an anomalies() or forecast() series' band",
                    ),
                ),
                ("outlier", describe(boolean(), "an outliers() series': whether it is one")),
            ],
            &["tags", "values"],
        ),
    );
    put(
        "QueryAnswer",
        object(
            &[
                ("grid", schema("Grid")),
                ("series", array(schema("Series"))),
                ("scalar", describe(nullable(number()), "a query that is a number")),
                ("scanned", object(&[("series", integer()), ("samples", integer())], &["series", "samples"])),
                ("applied", describe(integer(), "the log index the answer was read at")),
                ("took_ms", number()),
                ("queued_ms", describe(number(), "waited for a thread to read on")),
                ("waited_ms", describe(number(), "waited for the store (a write, a cut)")),
            ],
            &["grid", "series", "scanned", "applied", "took_ms"],
        ),
    );
    put(
        "HeatmapAnswer",
        object(
            &[
                ("grid", schema("Grid")),
                (
                    "heatmap",
                    object(
                        &[
                            ("rows", describe(array(array(number())), "each row's [lo, hi], ascending")),
                            ("counts", describe(array(array(number())), "each bucket of time's count in each row")),
                            ("distribution", describe(boolean(), "rows a distribution's buckets")),
                        ],
                        &["rows", "counts", "distribution"],
                    ),
                ),
                ("applied", integer()),
                ("took_ms", number()),
                ("queued_ms", number()),
                ("waited_ms", number()),
            ],
            &["grid", "heatmap", "applied", "took_ms"],
        ),
    );
    let view = describe(string(), "a rehydration's view to read (its id): the logs it read back from an archive; none, the live logs (logs only)");
    put(
        "SearchRequest",
        object(
            &[
                ("query", describe(string(), "a log search (Datadog's syntax; empty, every record)")),
                ("from", ms()),
                ("to", ms()),
                ("limit", describe(integer(), "records at the most (100; 1,000)")),
                ("view", view.clone()),
            ],
            &["from", "to"],
        ),
    );
    put(
        "Record",
        object(
            &[
                ("ts_ns", describe(integer(), "nanoseconds since the epoch")),
                ("status", one_of(&["trace", "debug", "info", "warn", "error", "fatal", "unspecified"])),
                ("severity", describe(integer(), "OpenTelemetry's severity number, 0 to 24")),
                ("service", string()),
                ("host", string()),
                ("source", string()),
                ("message", string()),
                ("attributes", map_of(string())),
                ("trace_id", nullable(string())),
                ("span_id", nullable(string())),
            ],
            &["ts_ns", "status", "severity", "service", "host", "source", "message", "attributes"],
        ),
    );
    put(
        "SearchAnswer",
        object(
            &[
                ("records", describe(array(schema("Record")), "newest first")),
                ("blocks", object(&[("total", integer()), ("pruned", integer())], &["total", "pruned"])),
                ("rows_scanned", integer()),
                ("took_ms", number()),
                ("queued_ms", describe(number(), "waited for a thread to read on")),
                ("waited_ms", describe(number(), "waited for the store (a write, a cut)")),
            ],
            &["records", "blocks", "rows_scanned", "took_ms"],
        ),
    );
    put(
        "AggregateRequest",
        object(
            &[
                ("query", string()),
                ("from", ms()),
                ("to", ms()),
                ("by", describe(array(string()), "facets: service, host, source, status, @attribute")),
                ("measure", one_of(&["count", "cardinality", "sum", "avg", "min", "max"])),
                ("of", describe(string(), "the facet or attribute measured (@duration)")),
                ("step", describe(integer(), "buckets of so many ms: each group's value a bucket")),
                ("view", view.clone()),
            ],
            &["from", "to"],
        ),
    );
    put(
        "AggregateAnswer",
        object(
            &[
                ("groups", array(object(&[("group", map_of(string())), ("value", nullable(number()))], &["group"]))),
                ("grid", schema("Grid")),
                ("series", array(object(&[("group", map_of(string())), ("values", array(nullable(number())))], &["group", "values"]))),
                ("rows_scanned", integer()),
                ("blocks_pruned", integer()),
                ("took_ms", number()),
                ("queued_ms", describe(number(), "waited for a thread to read on")),
                ("waited_ms", describe(number(), "waited for the store (a write, a cut)")),
            ],
            &["rows_scanned", "took_ms"],
        ),
    );
    put(
        "FacetsRequest",
        object(
            &[("query", string()), ("from", ms()), ("to", ms()), ("facets", array(string())), ("top", describe(integer(), "values a facet shows at the most (10)")), ("view", view)],
            &["from", "to", "facets"],
        ),
    );
    put(
        "FacetsAnswer",
        object(
            &[
                ("facets", array(object(&[("facet", string()), ("distinct", integer()), ("values", array(object(&[("value", string()), ("count", integer())], &["value", "count"])))], &["facet", "distinct", "values"]))),
                ("matched", integer()),
                ("rows_scanned", integer()),
                ("blocks_pruned", integer()),
                ("took_ms", number()),
                ("queued_ms", describe(number(), "waited for a thread to read on")),
                ("waited_ms", describe(number(), "waited for the store (a write, a cut)")),
            ],
            &["facets", "matched", "rows_scanned", "took_ms"],
        ),
    );
    fn span_fields(extra: &[(&'static str, Value)]) -> Vec<(&'static str, Value)> {
        let mut f: Vec<(&'static str, Value)> = vec![
            ("ts_ns", describe(integer(), "the span's start, nanoseconds since the epoch")),
            ("status", describe(one_of(&["info", "error"]), "error for OpenTelemetry's status ERROR")),
            ("service", string()),
            ("host", string()),
            ("source", describe(string(), "its instrumentation scope")),
            ("attributes", describe(map_of(string()), "the span's and its resource's; kiln's own: span.kind, duration (ns), parent_id, otel.status_code, env, version, error.*, peer.service, http.status_code, events, links")),
            ("trace_id", string()),
            ("span_id", string()),
        ];
        f.extend(extra.iter().cloned());
        f
    }
    put("Span", object(&span_fields(&[("severity", integer()), ("message", describe(string(), "its name: Datadog's resource"))]), &["ts_ns", "status", "service", "message", "attributes", "trace_id", "span_id"]));
    put(
        "SpansAnswer",
        object(
            &[("spans", describe(array(schema("Span")), "newest first")), ("blocks", object(&[("total", integer()), ("pruned", integer())], &["total", "pruned"])), ("rows_scanned", integer()), ("took_ms", number())],
            &["spans", "blocks", "rows_scanned", "took_ms"],
        ),
    );
    put(
        "PlacedSpan",
        object(
            &span_fields(&[
                ("resource_name", describe(string(), "its name")),
                ("offset_ns", describe(integer(), "its start from the trace's")),
                ("duration_ns", integer()),
                ("depth", describe(integer(), "how many of its ancestors the trace holds")),
                ("parent", describe(nullable(integer()), "its parent's place in spans, if the trace holds it")),
            ]),
            &["ts_ns", "offset_ns", "duration_ns", "depth", "status", "service", "resource_name", "attributes", "span_id"],
        ),
    );
    put(
        "Trace",
        object(
            &[
                ("trace_id", string()),
                ("start_ns", integer()),
                ("end_ns", integer()),
                ("duration_ns", integer()),
                ("root", nullable(object(&[("service", string()), ("resource_name", string())], &["service", "resource_name"]))),
                ("spans_count", describe(integer(), "spans the trace holds; past 10,000 the earliest are given")),
                ("truncated", boolean()),
                ("errors", integer()),
                ("roots", describe(integer(), "spans whose parent the trace does not hold: one, held whole")),
                ("services", array(object(&[("service", string()), ("spans", integer())], &["service", "spans"]))),
                ("spans", describe(array(schema("PlacedSpan")), "each root, then its descendants depth first, siblings by their start")),
                ("took_ms", number()),
            ],
            &["trace_id", "start_ns", "end_ns", "duration_ns", "spans_count", "truncated", "errors", "roots", "services", "spans"],
        ),
    );
    let red = |key: &str| {
        object(
            &[
                (key, string()),
                ("hits", number()),
                ("errors", number()),
                ("error_rate", nullable(number())),
                ("avg_s", nullable(number())),
                ("p50_s", nullable(number())),
                ("p95_s", nullable(number())),
                ("p99_s", nullable(number())),
                ("max_s", nullable(number())),
            ],
            &[key, "hits", "errors"],
        )
    };
    put("ServicesAnswer", object(&[("from", ms()), ("to", ms()), ("env", nullable(string())), ("envs", array(string())), ("services", array(red("service")))], &["from", "to", "services"]));
    put(
        "ServiceAnswer",
        object(
            &[
                ("service", string()),
                ("from", ms()),
                ("to", ms()),
                ("env", nullable(string())),
                ("summary", nullable(red("service"))),
                ("kinds", array(red("span.kind"))),
                ("resources", array(red("resource_name"))),
                ("calls", describe(array(red("peer.service")), "the calls it makes, by what it calls")),
            ],
            &["service", "from", "to", "kinds", "resources", "calls"],
        ),
    );
    put(
        "ServiceMap",
        object(
            &[
                ("from", ms()),
                ("to", ms()),
                ("nodes", array(object(&[("service", string()), ("hits", number()), ("errors", number()), ("inferred", describe(boolean(), "only named, as a peer a service calls"))], &["service", "hits", "errors", "inferred"]))),
                (
                    "edges",
                    array(object(
                        &[
                            ("from", string()),
                            ("to", string()),
                            ("calls", number()),
                            ("errors", number()),
                            ("avg_s", nullable(number())),
                            ("inferred", boolean()),
                            ("counted", describe(boolean(), "its figures from the trace metrics (every call, whatever traces were kept), not the spans read alone")),
                        ],
                        &["from", "to", "calls", "errors", "inferred", "counted"],
                    )),
                ),
                ("spans_read", integer()),
                ("sampled_one_in", describe(integer(), "one trace in so many read (by its id): figures not from the trace metrics are the spans read's, so many times")),
                ("took_ms", number()),
            ],
            &["from", "to", "nodes", "edges", "spans_read", "sampled_one_in"],
        ),
    );
    put(
        "Issue",
        object(
            &[
                ("id", describe(string(), "sixteen hex digits: the `issue.id` its errors carry")),
                ("source", one_of(&["span", "log", ""])),
                ("service", string()),
                ("type", describe(string(), "its errors' type (error.type, a log's error.kind)")),
                ("message", describe(string(), "its first error's message")),
                ("file", describe(string(), "the file of the frame its errors are grouped by")),
                ("function", describe(string(), "that frame's function")),
                ("first_seen", ms()),
                ("last_seen", ms()),
                ("versions", describe(array(string()), "the versions its errors were of, in the order they came")),
                ("envs", array(string())),
                ("status", one_of(&["for_review", "reviewed", "resolved", "ignored"])),
                ("status_at", ms()),
                ("regression", describe(boolean(), "back for review: it erred again once resolved, in a version it had not been seen in")),
                ("activity", array(object(&[("at", ms()), ("status", string()), ("by", one_of(&["kiln", "user"])), ("why", string())], &["at", "status", "by", "why"]))),
                ("errors", describe(nullable(number()), "its errors over the range asked")),
            ],
            &["id", "service", "type", "message", "first_seen", "last_seen", "status", "regression", "errors"],
        ),
    );
    put("IssueStatus", object(&[("status", one_of(&["for_review", "reviewed", "resolved", "ignored"]))], &["status"]));
    put(
        "IssueList",
        object(
            &[("from", ms()), ("to", ms()), ("total", describe(integer(), "issues that match, before the limit")), ("issues", describe(array(schema("Issue")), "the most errors first"))],
            &["from", "to", "total", "issues"],
        ),
    );
    put(
        "IssueDetail",
        object(
            &[
                ("issue", schema("Issue")),
                ("from", ms()),
                ("to", ms()),
                ("timeline", object(&[("from", ms()), ("step", describe(integer(), "ms")), ("values", array(number()))], &["from", "step", "values"])),
                ("versions", array(object(&[("version", string()), ("errors", number())], &["version", "errors"]))),
                ("envs", array(object(&[("env", string()), ("errors", number())], &["env", "errors"]))),
                ("latest", describe(nullable(schema("Record")), "its latest error in the range, whole: its stack, its trace")),
            ],
            &["issue", "from", "to", "timeline", "versions", "envs"],
        ),
    );
    put(
        "PatternsAnswer",
        object(
            &[
                ("patterns", array(object(&[("template", string()), ("count", integer()), ("first_ns", integer()), ("last_ns", integer()), ("sample", string()), ("statuses", map_of(integer()))], &["template", "count", "first_ns", "last_ns", "sample", "statuses"]))),
                ("records", integer()),
                ("rows_scanned", integer()),
                ("took_ms", number()),
            ],
            &["patterns", "records", "rows_scanned", "took_ms"],
        ),
    );
    put(
        "EventIn",
        object(
            &[
                ("title", string()),
                ("text", string()),
                ("tags", describe(array(string()), "key:value")),
                ("alert_type", one_of(&["error", "warning", "info", "success"])),
                ("source_type_name", string()),
                ("host", string()),
                ("date_happened", describe(integer(), "seconds since the epoch")),
                ("aggregation_key", string()),
            ],
            &["title"],
        ),
    );
    let machine = object(&[("machine", string()), ("member", integer()), ("status", any())], &["machine"]);
    put(
        "Status",
        object(
            &[
                ("role", one_of(&["server", "runner"])),
                ("member", integer()),
                ("leader", integer()),
                ("term", integer()),
                ("applied", integer()),
                ("durable", integer()),
                ("series", integer()),
                ("blocks", integer()),
                ("disk_bytes", integer()),
                ("log_bytes", integer()),
                ("cutoff_ms", integer()),
                ("voters", array(integer())),
                ("learners", array(integer())),
                ("machines", array(machine)),
                ("digest", integer()),
                ("digest_hex", describe(string(), "the digest in hex: a u64 is more than JavaScript holds exactly")),
                ("digest_at", integer()),
                ("verified_hex", nullable(array(any()))),
                ("error", nullable(string())),
                ("disk_free", nullable(integer())),
                ("disk_low", boolean()),
            ],
            &["role", "member", "leader", "applied", "series", "blocks", "digest_hex", "disk_low"],
        ),
    );
    let thresholds = object(
        &[
            ("critical", nullable(number())),
            ("warning", nullable(number())),
            ("critical_recovery", nullable(number())),
            ("warning_recovery", nullable(number())),
            ("ok", describe(nullable(number()), "a service check monitor's: OK runs in a row to recover (its critical and warning: runs in a row too)")),
            ("unknown", describe(nullable(number()), "a service check monitor's: UNKNOWN runs in a row to NO DATA")),
        ],
        &[],
    );
    let options = object(
        &[
            ("thresholds", thresholds),
            ("notify_no_data", boolean()),
            ("no_data_timeframe", describe(nullable(integer()), "minutes")),
            ("renotify_interval", describe(nullable(integer()), "minutes")),
            ("evaluation_delay", describe(nullable(integer()), "seconds")),
            ("every", describe(nullable(integer()), "seconds between evaluations")),
        ],
        &[],
    );
    let monitor_fields = [
        ("name", string()),
        (
            "type",
            describe(
                string(),
                "`composite` for a composite (its query an expression of monitors' ids); else the query says (none, or Datadog's type: `metric alert`, `query alert`, `log alert`, `trace-analytics alert`, `slo alert`, `error-tracking alert`, `service check`)",
            ),
        ),
        (
            "query",
            describe(
                string(),
                "Datadog's monitor query: `avg(last_5m):avg:system.load.1{*} by {host} > 4`, `logs(\"status:error\").last(\"5m\") > 10`, `error-tracking(\"service:shop\").source(\"backend\").new().rollup(\"count\").by(\"issue.id\").last(\"1d\") > 0`, `\"datadog.agent.up\".over(\"*\").by(\"host\").last(2).count_by_status()`",
            ),
        ),
        ("message", describe(string(), "Datadog's template variables (an Error Tracking monitor's `{{issue.attributes.error.type}}`, `.message`, `.file`, `.stack`; a service check monitor's `{{check_message}}`, `{{check_status}}`) and @-mentions of integrations")),
        ("tags", array(string())),
        ("priority", nullable(integer())),
        ("options", options),
        ("muted_until", describe(nullable(integer()), "unix ms")),
    ];
    put("MonitorIn", object(&monitor_fields, &["name", "query"]));
    let mut shown: Vec<(&str, Value)> = monitor_fields.to_vec();
    shown.extend([
        ("id", string()),
        ("created", ms()),
        ("modified", ms()),
        ("state", one_of(&["OK", "WARN", "ALERT", "NO DATA"])),
        ("groups", array(schema("GroupState"))),
        ("composite", describe(object(&[("monitors", array(any())), ("expression", string()), ("named", string())], &[]), "a composite's monitors by letter")),
        ("downtimes", describe(array(schema("Downtime")), "the downtimes that hold it now, or will")),
    ]);
    put("Monitor", object(&shown, &["id", "name", "query", "state", "groups"]));
    put(
        "GroupState",
        object(
            &[
                ("group", describe(array(array(string())), "[tag, value] pairs")),
                ("state", one_of(&["OK", "WARN", "ALERT", "NO DATA"])),
                ("value", nullable(number())),
                ("since", ms()),
                ("last_data", ms()),
                ("notified", ms()),
                ("quiet_recovery", describe(boolean(), "a downtime that ended with it in trouble asked its first recovery to say nothing")),
            ],
            &["group", "state", "since"],
        ),
    );
    put(
        "Validated",
        object(
            &[
                ("ok", boolean()),
                ("error", string()),
                ("kind", one_of(&["metric", "change", "logs", "spans", "forecast", "slo", "error-tracking", "composite"])),
                ("op", string()),
                ("threshold", number()),
                ("watched", any()),
                ("window_ms", integer()),
                ("composite", any()),
            ],
            &["ok"],
        ),
    );
    put(
        "IntegrationIn",
        json!({ "oneOf": [
            object(&[("kind", one_of(&["slack", "discord", "teams"])), ("url", string())], &["kind", "url"]),
            object(&[("kind", one_of(&["pagerduty"])), ("routing_key", string()), ("url", string())], &["kind", "routing_key"]),
            object(&[("kind", one_of(&["webhook"])), ("url", string()), ("secret", describe(string(), "signs each delivery (HMAC-SHA256)"))], &["kind", "url"]),
            object(&[("kind", one_of(&["opsgenie"])), ("api_key", describe(string(), "an API integration's key (GenieKey)")), ("url", describe(string(), "https://api.opsgenie.com unless said (EU: https://api.eu.opsgenie.com)"))], &["kind", "api_key"]),
            object(
                &[
                    ("kind", one_of(&["email"])),
                    ("to", describe(array(string()), "1 to 50 addresses")),
                    ("from", string()),
                    ("host", describe(string(), "the SMTP relay")),
                    ("port", describe(integer(), "587 for starttls, 465 for tls, 25 for none, unless said")),
                    ("security", one_of(&["starttls", "tls", "none"])),
                    ("username", string()),
                    ("password", string()),
                    ("ca", describe(string(), "the CA (PEM) the relay's certificate is signed by, when not a public one")),
                ],
                &["kind", "to", "from", "host"],
            ),
        ] }),
    );
    put(
        "Integration",
        describe(
            object(
                &[
                    ("name", string()),
                    ("kind", one_of(&["slack", "discord", "teams", "pagerduty", "webhook", "opsgenie", "email"])),
                    ("url", string()),
                    ("routing_key", string()),
                    ("api_key", string()),
                    ("signed", boolean()),
                    ("to", array(string())),
                    ("from", string()),
                    ("host", string()),
                    ("port", integer()),
                    ("security", one_of(&["starttls", "tls", "none"])),
                    ("username", string()),
                    ("password", string()),
                    ("ca", string()),
                ],
                &["name", "kind"],
            ),
            "as shown: a webhook URL's host alone, keys and passwords \"…\", whether a webhook signs",
        ),
    );
    put(
        "ScrapeIn",
        object(
            &[
                ("url", describe(string(), "a Prometheus exposition's (or OpenMetrics') URL")),
                ("every_s", integer()),
                ("timeout_s", integer()),
                ("on", describe(string(), "`server`, a runner's name, or `*`: every machine, each its own")),
                ("tags", array(string())),
                ("bearer", describe(string(), "a token sent as Authorization: Bearer; left out, the one kept")),
            ],
            &["url"],
        ),
    );
    let scrape_state = object(
        &[("up", nullable(boolean())), ("last_ms", nullable(integer())), ("took_ms", integer()), ("samples", integer()), ("skipped", integer()), ("deferred", integer()), ("error", nullable(string())), ("scrapes", integer()), ("failures", integer())],
        &["up", "scrapes", "failures"],
    );
    put(
        "Scrape",
        object(
            &[
                ("name", string()),
                ("url", string()),
                ("every_s", integer()),
                ("timeout_s", integer()),
                ("on", string()),
                ("tags", array(string())),
                ("bearer", describe(boolean(), "whether it has a token (never shown)")),
                ("machines", array(object(&[("machine", string()), ("state", scrape_state)], &["machine", "state"]))),
            ],
            &["name", "url", "every_s", "timeout_s", "on", "tags", "bearer", "machines"],
        ),
    );
    put(
        "ScrapeTest",
        object(
            &[("ok", boolean()), ("error", string()), ("format", one_of(&["openmetrics", "text"])), ("samples", integer()), ("skipped", integer()), ("series", integer()), ("metrics", integer()), ("sample", array(string())), ("took_ms", integer())],
            &["ok", "took_ms"],
        ),
    );
    // What each metric is and costs.
    put(
        "MetricSummary",
        object(
            &[
                ("name", string()),
                ("series", describe(integer(), "its series: its cardinality")),
                ("kinds", array(object(&[("kind", one_of(&["gauge", "count", "counter"])), ("series", integer())], &["kind", "series"]))),
                (
                    "tags",
                    describe(
                        array(object(
                            &[
                                ("key", string()),
                                ("values", describe(integer(), "its values among the metric's series")),
                                ("series", describe(integer(), "the series carrying it")),
                                ("top", describe(array(object(&[("value", string()), ("series", integer())], &["value", "series"])), "a metric's own summary: the values of most series")),
                            ],
                            &["key", "values", "series"],
                        )),
                        "the tag of most values first",
                    ),
                ),
                ("samples", describe(integer(), "samples applied since the server started")),
                ("samples_per_s", describe(nullable(number()), "of late (against a count ten seconds to a minute before); none until there is one")),
                ("last_ms", nullable(integer())),
            ],
            &["name", "series", "kinds", "tags", "samples"],
        ),
    );
    // DogStatsD listeners (docs/observe.md §8.4).
    let statsd_fields = vec![
        ("listen", describe(array(string()), "udp://127.0.0.1:8125, unix:///var/run/kiln/dsd.socket (4 at the most)")),
        ("tags", describe(array(string()), "key:value, on every metric, event and check (a message's own win on its key)")),
        ("histogram_aggregates", describe(array(one_of(&["max", "median", "avg", "count", "min", "sum"])), "max, median, avg and count unless said")),
        ("histogram_percentiles", describe(array(number()), "0.01 to 0.99 in hundredths; 0.95 unless said")),
        ("on", describe(string(), "`server`, a runner's name, or `*`: every machine, each its own")),
    ];
    put("StatsdIn", object(&statsd_fields, &["listen"]));
    let statsd_state = object(
        &[
            ("listening", array(string())),
            ("rcvbuf", describe(integer(), "the least receive buffer the system granted its sockets, bytes")),
            ("datagrams", integer()),
            ("metrics", integer()),
            ("events", integer()),
            ("checks", integer()),
            ("invalid", describe(integer(), "messages that did not read, values that are not numbers")),
            ("refused", describe(integer(), "samples of contexts past the bound, or that no series can be")),
            ("conflicts", describe(integer(), "tags whose key came twice with another value (the first kept)")),
            ("contexts", integer()),
            ("points", integer()),
            ("dropped", describe(integer(), "points and records dropped after waiting a minute for room")),
            ("held", describe(integer(), "points and records waiting for room")),
            ("last_ms", nullable(integer())),
            ("error", nullable(string())),
        ],
        &["listening", "datagrams", "points"],
    );
    let mut statsd_listed = statsd_fields.clone();
    statsd_listed.extend([("name", string()), ("machines", array(object(&[("machine", string()), ("state", statsd_state)], &["machine", "state"])))]);
    put("Statsd", object(&statsd_listed, &["name", "listen", "on", "machines"]));
    // Synthetic tests (docs/observe.md §10.4).
    let assertion = object(
        &[
            ("type", one_of(&["status_code", "response_time", "header", "body", "certificate_days", "address"])),
            ("operator", one_of(&["is", "is_not", "less_than", "more_than", "contains", "does_not_contain", "matches", "does_not_match"])),
            ("property", describe(string(), "a header assertion's header")),
            ("target", describe(json!({}), "a number (status_code, response_time in ms, certificate_days) or text")),
        ],
        &["type", "operator", "target"],
    );
    let alerting = object(&[("message", describe(string(), "mentions and Datadog's template variables")), ("min_locations", integer()), ("for_s", integer()), ("priority", nullable(integer()))], &[]);
    let synthetic_fields = vec![
        ("type", describe(one_of(&["http", "tcp", "dns", "tls"]), "what it tries")),
        ("url", describe(string(), "http: the request's URL")),
        ("method", string()),
        ("headers", array(array(string()))),
        ("body", string()),
        ("follow_redirects", boolean()),
        ("bearer", describe(string(), "http: a token sent as Authorization: Bearer; sealed at rest, shown \"…\"")),
        ("ca", describe(string(), "http and tls: a CA (PEM) the certificate is signed by, when not a public one")),
        ("host", describe(string(), "tcp, dns and tls: the host")),
        ("port", integer()),
        ("assertions", array(assertion)),
        ("locations", describe(array(string()), "`server`, runners by name, or `*`: every machine")),
        ("every_s", integer()),
        ("timeout_s", integer()),
        ("retries", integer()),
        ("retry_ms", integer()),
        ("tags", array(string())),
        ("paused", boolean()),
        ("alert", describe(alerting, "a monitor of its own: at least min_locations failing for for_s")),
    ];
    put("SyntheticIn", object(&synthetic_fields, &["type"]));
    let synthetic_state = object(
        &[
            ("passed", nullable(boolean())),
            ("last_ms", nullable(integer())),
            ("took_us", integer()),
            ("status", nullable(integer())),
            ("certificate_minutes", nullable(integer())),
            ("attempts", integer()),
            ("failed", array(string())),
            ("runs", integer()),
            ("failures", integer()),
        ],
        &["passed", "runs", "failures"],
    );
    let mut listed = synthetic_fields.clone();
    listed.extend([("name", string()), ("monitor_id", describe(string(), "the monitor it owns, when it alerts")), ("machines", array(object(&[("machine", string()), ("state", synthetic_state)], &["machine", "state"])))]);
    put("Synthetic", object(&listed, &["name", "type", "locations", "every_s", "machines"]));
    put(
        "SyntheticOutcome",
        object(
            &[
                ("passed", boolean()),
                ("failures", array(string())),
                ("total_ms", number()),
                ("dns_ms", nullable(number())),
                ("connect_ms", nullable(number())),
                ("tls_ms", nullable(number())),
                ("firstbyte_ms", nullable(number())),
                ("download_ms", nullable(number())),
                ("redirect_ms", nullable(number())),
                ("status", nullable(integer())),
                ("size", nullable(integer())),
                ("certificate_days", nullable(number())),
                ("addresses", array(string())),
                ("attempts", integer()),
            ],
            &["passed", "failures", "total_ms", "attempts"],
        ),
    );
    let rule = object(
        &[
            ("type", one_of(&["exclude_at_match", "include_at_match", "mask_sequences", "multi_line"])),
            ("name", string()),
            ("pattern", describe(string(), "a regular expression (Rust's regex: no look-around)")),
            ("replace_placeholder", describe(string(), "what a mask_sequences rule puts where its pattern matched")),
        ],
        &["type", "pattern"],
    );
    let source_fields = [
        ("type", describe(one_of(&["file", "kubernetes", "docker", "journald", "syslog", "unified_log", "windows_event"]), "files by their globs; a Kubernetes node's pods' logs; Docker's json-file logs; the systemd journal; syslog received; macOS's unified log; the Windows Event Log's channels")),
        ("paths", describe(array(string()), "a file source's globs (/var/log/app/*.log); Kubernetes' and Docker's their own unless given")),
        ("exclude", describe(array(string()), "globs of paths not read")),
        ("format", one_of(&["auto", "text", "json", "cri", "docker"])),
        ("start", describe(one_of(&["end", "beginning"]), "where a file there when the source starts, with no place kept, is read from")),
        ("encoding", one_of(&["utf-8", "utf-16le", "utf-16be"])),
        ("units", describe(array(string()), "the journal's units read (every unit when none)")),
        ("listen", describe(array(string()), "where a syslog source listens: udp://0.0.0.0:514, tcp://0.0.0.0:6514")),
        ("timezone", describe(string(), "the zone an RFC 3164 time (which has none) is in: an IANA name, UTC unless said")),
        ("predicate", describe(string(), "the unified log's events read: an NSPredicate (subsystem == \"com.example.app\"); every one when none")),
        ("level", describe(one_of(&["default", "info", "debug"]), "the unified log's level read (info unless said)")),
        ("channels", describe(array(string()), "the Windows Event Log's channels read: System, Application, Microsoft-Windows-Sysmon/Operational")),
        ("query", describe(string(), "which of their events: an XPath, *[...] (*[System[(Level=1 or Level=2)]]); every one when none")),
        ("service", string()),
        ("source", string()),
        ("tags", array(string())),
        ("rules", array(rule)),
        ("on", describe(string(), "`server`, a runner's name, or `*`: every machine, each its own")),
    ];
    put("LogSourceIn", object(&source_fields, &["type"]));
    let source_state = object(
        &[
            ("since_ms", nullable(integer())),
            ("files", integer()),
            ("over_limit", integer()),
            ("bytes", integer()),
            ("records", integer()),
            ("in_flight", integer()),
            ("excluded", integer()),
            ("truncated", integer()),
            ("lag_bytes", integer()),
            ("connections", integer()),
            ("dropped", integer()),
            ("last_ms", nullable(integer())),
            ("error", nullable(string())),
            ("errors", integer()),
        ],
        &["files", "records", "errors"],
    );
    let mut listed: Vec<(&str, Value)> = source_fields.to_vec();
    listed.push(("name", string()));
    listed.push(("machines", array(object(&[("machine", string()), ("state", source_state)], &["machine", "state"]))));
    put("LogSource", object(&listed, &["name", "type", "format", "start", "encoding", "tags", "rules", "on", "machines"]));
    put(
        "LogSourceTest",
        object(
            &[
                ("ok", boolean()),
                ("files", array(object(&[("path", string()), ("bytes", integer())], &["path", "bytes"]))),
                ("matched", integer()),
                (
                    "records",
                    array(object(
                        &[("ts_ns", integer()), ("status", string()), ("service", string()), ("host", string()), ("source", string()), ("message", string()), ("attributes", map_of(string()))],
                        &["ts_ns", "status", "service", "host", "source", "message", "attributes"],
                    )),
                ),
                ("excluded", integer()),
                ("error", nullable(string())),
                ("took_ms", integer()),
            ],
            &["ok", "files", "matched", "records", "excluded", "took_ms"],
        ),
    );
    // Log pipelines (§8.2): Datadog's own shape, a processor its `type` and that type's fields.
    let processor = describe(
        object(
            &[
                (
                    "type",
                    one_of(&[
                        "grok-parser",
                        "date-remapper",
                        "status-remapper",
                        "service-remapper",
                        "message-remapper",
                        "trace-id-remapper",
                        "attribute-remapper",
                        "url-parser",
                        "user-agent-parser",
                        "category-processor",
                        "arithmetic-processor",
                        "string-builder-processor",
                        "lookup-processor",
                        "pipeline",
                    ]),
                ),
                ("name", string()),
                ("is_enabled", boolean()),
            ],
            &["type"],
        ),
        "Datadog's processor: its type's fields beside these (a grok parser's source and grok rules, a remapper's sources and target, …)",
    );
    let pipeline_fields = [
        ("name", string()),
        ("is_enabled", boolean()),
        ("filter", object(&[("query", describe(string(), "a log search; empty: every record"))], &[])),
        ("processors", array(processor)),
    ];
    put("PipelineIn", object(&pipeline_fields, &["name"]));
    let mut kept: Vec<(&str, Value)> = pipeline_fields.to_vec();
    kept.push(("id", string()));
    kept.push(("builtin", describe(boolean(), "kiln's own: read-only, but for whether it is on")));
    kept.push(("created", integer()));
    kept.push(("modified", integer()));
    put("Pipeline", object(&kept, &["id", "builtin", "name", "is_enabled", "filter", "processors"]));
    put(
        "PipelineTest",
        object(
            &[
                ("pipeline", describe(schema("PipelineIn"), "the pipeline tried; none: every one applied now, in order")),
                (
                    "records",
                    array(object(&[("message", string()), ("service", string()), ("host", string()), ("source", string()), ("status", string()), ("attributes", map_of(string()))], &["message"])),
                ),
            ],
            &["records"],
        ),
    );
    // The sensitive data scanner (§8.9): Datadog's scanning groups, their rules within.
    let rule_fields = [
        ("id", describe(string(), "given by the server; kept as the group is changed")),
        ("name", string()),
        ("description", string()),
        ("standard_pattern_id", describe(string(), "a standard pattern's id (GET scanner/standard_patterns); else pattern")),
        ("pattern", describe(string(), "a regular expression (Rust's regex: no look-around); a group named v names what of a match is replaced")),
        ("namespaces", describe(array(string()), "where it looks: message and attributes by path (beneath them too); none, everywhere")),
        ("excluded_namespaces", describe(array(string()), "where it does not")),
        ("tags", describe(array(string()), "key:value tags a match puts on its record; a standard pattern's unless said")),
        ("is_enabled", boolean()),
        (
            "text_replacement",
            object(
                &[
                    ("type", one_of(&["none", "hash", "replacement_string", "partial_replacement_from_beginning", "partial_replacement_from_end"])),
                    ("replacement_string", describe(string(), "replacement_string's: [sensitive_data] unless said")),
                    ("number_of_chars", describe(integer(), "a partial replacement's: characters hidden")),
                ],
                &["type"],
            ),
        ),
        (
            "included_keyword_configuration",
            describe(
                object(
                    &[
                        ("keywords", array(string())),
                        ("character_count", describe(integer(), "how far before a match a keyword is looked for (30; 50 at the most)")),
                        ("use_recommended_keywords", boolean()),
                    ],
                    &[],
                ),
                "keywords one of which must be near a match for it to count",
            ),
        ),
        ("priority", describe(integer(), "1 (critical) to 5 (info): of overlapping matches, the higher's is replaced")),
    ];
    put("ScanningRule", object(&rule_fields, &["name"]));
    let group_fields = [
        ("name", string()),
        ("description", string()),
        ("filter", object(&[("query", describe(string(), "a log search: the records it scans; empty: every record"))], &[])),
        ("is_enabled", boolean()),
        ("rules", array(schema("ScanningRule"))),
    ];
    put("ScanningGroupIn", object(&group_fields, &["name"]));
    let mut kept_group: Vec<(&str, Value)> = group_fields.to_vec();
    kept_group.push(("id", string()));
    kept_group.push(("created", integer()));
    kept_group.push(("modified", integer()));
    put("ScanningGroup", object(&kept_group, &["id", "name", "filter", "is_enabled", "rules"]));
    put(
        "StandardPattern",
        object(
            &[
                ("id", string()),
                ("name", string()),
                ("category", string()),
                ("description", string()),
                ("pattern", string()),
                ("tags", array(string())),
                ("keywords", array(string())),
                ("keywords_required", describe(boolean(), "too loose alone: a match counts only with one of its keywords near")),
                ("priority", integer()),
            ],
            &["id", "name", "category", "pattern", "tags", "priority"],
        ),
    );
    put(
        "ScannerTest",
        object(
            &[
                ("group", describe(schema("ScanningGroupIn"), "the group tried; none: every one that runs now, in order")),
                (
                    "records",
                    array(object(&[("message", string()), ("service", string()), ("host", string()), ("source", string()), ("status", string()), ("attributes", map_of(string()))], &["message"])),
                ),
            ],
            &["records"],
        ),
    );
    // DORA metrics (§10.7): Datadog's deployments and failures, kiln's view of them, the four keys.
    let when = || describe(integer(), "seconds, milliseconds or nanoseconds since the epoch (told apart by size)");
    let git = || object(&[("commit_sha", string()), ("repository_url", describe(string(), "its clone URL, or kiln's name for it (owner/name)"))], &["commit_sha", "repository_url"]);
    let dora_id = || describe(string(), "16 to 128 letters, digits, -, _, . and :; made when not given (a repeat replaces what it said)");
    let custom_tags = || describe(array(string()), "key:value tags, 100 at the most");
    put(
        "DoraDeploymentIn",
        describe(
            object(
                &[
                    ("service", string()),
                    ("env", string()),
                    ("started_at", when()),
                    ("finished_at", when()),
                    ("git", git()),
                    ("id", dora_id()),
                    ("team", string()),
                    ("version", string()),
                    ("custom_tags", custom_tags()),
                ],
                &["service", "started_at", "finished_at"],
            ),
            "Datadog's deployment event; as its API sends it (in {\"data\": {\"attributes\": ...}}) or bare",
        ),
    );
    put(
        "DoraFailureIn",
        describe(
            object(
                &[
                    ("services", array(string())),
                    ("team", describe(string(), "services or team: one is required")),
                    ("env", string()),
                    ("name", string()),
                    ("severity", string()),
                    ("started_at", when()),
                    ("finished_at", describe(integer(), "as started_at; none while it goes on")),
                    ("git", git()),
                    ("id", dora_id()),
                    ("version", string()),
                    ("custom_tags", custom_tags()),
                ],
                &["started_at"],
            ),
            "Datadog's failure (incident) event; enveloped or bare",
        ),
    );
    put(
        "DoraPatch",
        object(
            &[
                ("change_failure", describe(boolean(), "marked failed (true unless said), or no longer")),
                (
                    "remediation",
                    describe(
                        object(
                            &[
                                ("id", string()),
                                ("version", describe(string(), "the first deployment of it after the failed one, its service and environment's")),
                                ("type", one_of(&["rollback", "rollforward"])),
                            ],
                            &["type"],
                        ),
                        "the deployment that remedied it: its id or its version",
                    ),
                ),
                ("service", describe(string(), "with version (and env): the deployment marked, when no id is in the path")),
                ("env", string()),
                ("version", string()),
            ],
            &[],
        ),
    );
    put(
        "DoraList",
        object(
            &[
                ("from", describe(any(), "RFC 3339, or seconds, milliseconds or nanoseconds (30 days before to)")),
                ("to", describe(any(), "as from (now)")),
                ("query", describe(string(), "a log search of their fields: service:, @env:, @team:, @version:, @git.commit_sha:, @change_failure:true, tags")),
                ("limit", describe(integer(), "100 unless said; 1,000 at the most")),
                ("sort", one_of(&["-finished_at", "finished_at", "-started_at", "started_at"])),
            ],
            &[],
        ),
    );
    put(
        "DoraDeployment",
        object(
            &[
                ("id", string()),
                ("type", one_of(&["dora_deployment"])),
                (
                    "attributes",
                    object(
                        &[
                            ("service", string()),
                            ("env", string()),
                            ("team", string()),
                            ("version", string()),
                            ("started_at", describe(integer(), "ms since the epoch")),
                            ("finished_at", describe(integer(), "ms since the epoch")),
                            ("git", nullable(git())),
                            ("custom_tags", array(string())),
                            ("source", one_of(&["api", "kiln.cd"])),
                            ("lead", nullable(object(&[("commits", integer()), ("mean_s", number()), ("min_s", number()), ("max_s", number())], &["commits", "mean_s", "min_s", "max_s"]))),
                            (
                                "change_failure",
                                nullable(object(
                                    &[
                                        ("found", one_of(&["rollback", "revert", "hotfix", "manual"])),
                                        (
                                            "remediation",
                                            nullable(object(&[("id", string()), ("type", one_of(&["rollback", "rollforward"])), ("finished_at", integer())], &["id", "type", "finished_at"])),
                                        ),
                                    ],
                                    &["found"],
                                )),
                            ),
                            ("recovery_s", describe(nullable(number()), "from its finish to its remediation's")),
                            ("remedies", describe(array(string()), "the failed deployments it remedied")),
                        ],
                        &["service", "started_at", "finished_at", "source"],
                    ),
                ),
            ],
            &["id", "type", "attributes"],
        ),
    );
    put(
        "DoraFailure",
        object(
            &[
                ("id", string()),
                ("type", one_of(&["dora_failure"])),
                (
                    "attributes",
                    object(
                        &[
                            ("services", array(string())),
                            ("env", string()),
                            ("team", string()),
                            ("name", string()),
                            ("severity", string()),
                            ("started_at", describe(integer(), "ms")),
                            ("finished_at", describe(nullable(integer()), "ms; none while it goes on")),
                            ("git", nullable(git())),
                            ("version", string()),
                            ("custom_tags", array(string())),
                        ],
                        &["services", "started_at"],
                    ),
                ),
            ],
            &["id", "type", "attributes"],
        ),
    );
    let level = || nullable(one_of(&["elite", "high", "medium", "low"]));
    put(
        "DoraSummary",
        object(
            &[
                ("from", integer()),
                ("to", integer()),
                ("deployments", integer()),
                ("per_day", number()),
                ("frequency_level", describe(level(), "the 2024 report's cluster whose range it falls in")),
                ("lead_time_s", describe(nullable(number()), "the median of the deployments' (each its commits' mean)")),
                ("lead_time_level", level()),
                ("lead_deployments", integer()),
                ("change_failures", integer()),
                ("failure_rate", nullable(number())),
                ("failure_rates", describe(array(object(&[("level", string()), ("rate", number())], &["level", "rate"])), "each cluster's rate in the report (not ordered: no level is given)")),
                ("recovery_s", describe(nullable(number()), "the median of the failed deployments' recoveries")),
                ("recovery_level", describe(level(), "none for a day to a week: no cluster's")),
                ("recovered", integer()),
                ("failures", integer()),
                ("open_failures", integer()),
                ("restore_s", nullable(number())),
                (
                    "weeks",
                    array(object(
                        &[("start", integer()), ("deployments", integer()), ("change_failures", integer()), ("lead_time_s", nullable(number())), ("recovery_s", nullable(number()))],
                        &["start", "deployments", "change_failures"],
                    )),
                ),
                ("services", array(string())),
                ("envs", array(string())),
                ("teams", array(string())),
            ],
            &["from", "to", "deployments", "per_day", "change_failures", "weeks"],
        ),
    );
    // The Service Catalog (§10.8): Datadog's service definitions (v2.2), each service's facts.
    put(
        "ServiceDefinition",
        describe(
            object(
                &[
                    ("schema-version", one_of(&["v2.2", "v2.1"])),
                    ("dd-service", string()),
                    ("team", string()),
                    ("application", string()),
                    ("description", string()),
                    ("tier", string()),
                    ("lifecycle", string()),
                    ("type", one_of(&["web", "db", "cache", "function", "browser", "mobile", "custom"])),
                    ("languages", array(string())),
                    (
                        "contacts",
                        array(object(
                            &[("name", string()), ("type", describe(string(), "email, slack, microsoft-teams…")), ("contact", describe(string(), "an email address, else a URI"))],
                            &["type", "contact"],
                        )),
                    ),
                    (
                        "links",
                        array(object(
                            &[("name", string()), ("type", describe(string(), "runbook, doc, repo, dashboard, other (unless said)")), ("url", string()), ("provider", string())],
                            &["name", "type", "url"],
                        )),
                    ),
                    ("tags", array(string())),
                    (
                        "integrations",
                        object(
                            &[
                                ("pagerduty", object(&[("service-url", describe(string(), "a service's URL in PagerDuty's service directory"))], &["service-url"])),
                                ("opsgenie", object(&[("service-url", string()), ("region", one_of(&["US", "EU"]))], &["service-url"])),
                            ],
                            &[],
                        ),
                    ),
                    ("extensions", describe(any(), "anything, kept as it is")),
                    ("ci-pipeline-fingerprints", array(string())),
                ],
                &["schema-version", "dd-service"],
            ),
            "Datadog's service definition v2.2: what it does not name is refused",
        ),
    );
    let score = || {
        object(
            &[("passed", integer()), ("total", integer()), ("scorecards", array(object(&[("name", string()), ("passed", integer()), ("total", integer())], &["name", "passed", "total"])))],
            &["passed", "total", "scorecards"],
        )
    };
    let service_fields = || {
        vec![
            ("service", string()),
            ("source", describe(one_of(&["api", "repository", "seen"]), "where its definition came from: none, seen by its traces or deployments")),
            ("repo", nullable(string())),
            ("team", nullable(string())),
            ("tier", nullable(string())),
            ("lifecycle", nullable(string())),
            ("type", nullable(string())),
            ("description", nullable(string())),
            ("requests", describe(number(), "the last hour's, from the trace metrics")),
            ("errors", number()),
            ("error_rate", nullable(number())),
            ("p95_s", nullable(number())),
            ("open_issues", integer()),
            ("monitors", describe(integer(), "those that read it: service:<name> in their query or tags")),
            ("alerting", integer()),
            ("slos", integer()),
            ("deployments_30d", integer()),
            ("failed_30d", integer()),
            ("last_deployment", nullable(integer())),
            ("score", score()),
        ]
    };
    put("CatalogService", object(&service_fields(), &["service", "source", "score"]));
    let mut detail = service_fields();
    detail.extend([
        (
            "definition",
            nullable(object(&[("id", string()), ("type", string()), ("attributes", object(&[("schema", schema("ServiceDefinition")), ("meta", any())], &["schema"]))], &["id", "attributes"])),
        ),
        ("rules", array(object(&[("scorecard", string()), ("id", string()), ("name", string()), ("passed", boolean()), ("why", string())], &["scorecard", "id", "name", "passed", "why"]))),
        ("monitor_list", array(object(&[("id", string()), ("name", string()), ("state", string())], &["id", "name", "state"]))),
        ("slo_list", array(object(&[("id", string()), ("name", string())], &["id", "name"]))),
        ("deployments", array(object(&[("id", string()), ("env", string()), ("version", string()), ("finished_at", integer()), ("failed", boolean())], &["id", "finished_at", "failed"]))),
        ("traced", boolean()),
        ("logs_traced", describe(integer(), "the last day's logs that carry a trace id")),
    ]);
    put("CatalogServiceDetail", object(&detail, &["service", "source", "score", "rules"]));
    // The infrastructure list (§10.9): Datadog's host, kiln's own beside.
    let figure = || nullable(number());
    let host_fields = || {
        vec![
            ("name", string()),
            ("host_name", string()),
            ("id", describe(integer(), "a stable number of its name")),
            ("aliases", array(string())),
            ("apps", describe(array(string()), "its metrics' namespaces (the Agent's `agent`)")),
            ("sources", describe(array(string()), "what reports it: kiln, agent, otel, a cloud")),
            ("up", describe(boolean(), "a sample within two hours")),
            ("is_muted", boolean()),
            ("mute_timeout", describe(nullable(integer()), "when its mute ends, seconds since the epoch")),
            ("last_reported_time", describe(nullable(integer()), "its newest sample, seconds since the epoch")),
            ("metrics", describe(object(&[("cpu", figure()), ("iowait", figure()), ("load", figure())], &[]), "CPU in use (%), I/O wait (%), the load over 15 minutes")),
            ("tags_by_source", map_of(array(string()))),
            ("meta", describe(map_of(any()), "what the Agent says (include_hosts_metadata)")),
            (
                "kiln",
                object(
                    &[
                        ("status", one_of(&["reporting", "late", "gone"])),
                        ("seen", nullable(integer())),
                        ("series", integer()),
                        ("figures", object(&[("cpu", figure()), ("iowait", figure()), ("load", figure()), ("load_norm", figure()), ("memory", figure()), ("disk", figure())], &[])),
                        ("platform", map_of(any())),
                        ("runner", nullable(map_of(any()))),
                        ("mute", nullable(object(&[("downtime", string()), ("end", nullable(integer())), ("message", string())], &["downtime"]))),
                    ],
                    &["status", "series", "figures", "platform"],
                ),
            ),
        ]
    };
    put("Host", object(&host_fields(), &["name", "up", "is_muted", "metrics", "kiln"]));
    let mut host_detail = host_fields();
    host_detail.push(("containers", describe(array(map_of(any())), "its containers: id, name, image, pod, last reported, CPU (nanocores), memory (bytes)")));
    put("HostDetail", object(&host_detail, &["name", "up", "is_muted", "metrics", "kiln"]));
    // Incidents (§9.5): Datadog's JSON:API.
    let rel = |t: &str| object(&[("data", nullable(object(&[("type", one_of(&[t])), ("id", string())], &["type", "id"])))], &["data"]);
    let field =
        || describe(object(&[("type", one_of(&["dropdown", "textbox", "multiselect", "textarray", "metrictag", "autocomplete"])), ("value", any())], &["type", "value"]), "a text, or a list of texts");
    let handle = || object(&[("display_name", string()), ("handle", describe(string(), "@slack-ops, @pagerduty-oncall, …"))], &["handle"]);
    let seconds = || describe(nullable(integer()), "seconds");
    let time = || describe(nullable(string()), "RFC 3339");
    put(
        "Incident",
        object(
            &[
                ("id", string()),
                ("type", one_of(&["incidents"])),
                (
                    "attributes",
                    object(
                        &[
                            ("public_id", integer()),
                            ("title", string()),
                            ("customer_impacted", boolean()),
                            ("customer_impact_scope", nullable(string())),
                            ("customer_impact_start", time()),
                            ("customer_impact_end", time()),
                            ("customer_impact_duration", seconds()),
                            ("time_to_detect", seconds()),
                            ("time_to_internal_response", seconds()),
                            ("time_to_repair", seconds()),
                            ("time_to_resolve", seconds()),
                            ("created", string()),
                            ("modified", string()),
                            ("detected", string()),
                            ("declared", string()),
                            ("resolved", time()),
                            ("severity", one_of(&super::incidents::SEVERITIES)),
                            ("state", one_of(&super::incidents::STATES)),
                            ("fields", map_of(field())),
                            ("notification_handles", array(handle())),
                            ("is_test", boolean()),
                            ("kiln", describe(map_of(any()), "kiln's own: the monitor that declared it, its todos open, its postmortem")),
                        ],
                        &["public_id", "title", "severity", "state", "created"],
                    ),
                ),
                ("relationships", object(&[("commander_user", rel("users")), ("created_by_user", rel("users")), ("last_modified_by_user", rel("users")), ("attachments", any())], &[])),
            ],
            &["id", "type", "attributes"],
        ),
    );
    put(
        "IncidentBody",
        object(
            &[(
                "data",
                object(
                    &[
                        ("type", one_of(&["incidents"])),
                        (
                            "attributes",
                            object(
                                &[
                                    ("title", string()),
                                    ("customer_impacted", boolean()),
                                    ("customer_impact_scope", describe(nullable(string()), "how customers were impacted: said when they were")),
                                    ("customer_impact_start", time()),
                                    ("customer_impact_end", time()),
                                    ("detected", time()),
                                    ("severity", one_of(&super::incidents::SEVERITIES)),
                                    ("state", one_of(&super::incidents::STATES)),
                                    ("fields", describe(map_of(field()), "its fields (severity and state among them, or others: a null value removes one)")),
                                    ("notification_handles", array(handle())),
                                    (
                                        "initial_cells",
                                        array(object(
                                            &[("cell_type", one_of(&["markdown"])), ("content", object(&[("content", string())], &["content"])), ("important", boolean())],
                                            &["cell_type", "content"],
                                        )),
                                    ),
                                    ("is_test", boolean()),
                                ],
                                &[],
                            ),
                        ),
                        ("relationships", object(&[("commander_user", rel("users"))], &[])),
                    ],
                    &["attributes"],
                ),
            )],
            &["data"],
        ),
    );
    put(
        "IncidentTodo",
        object(
            &[
                ("id", string()),
                ("type", one_of(&["incident_todos"])),
                (
                    "attributes",
                    object(
                        &[
                            ("content", string()),
                            ("assignees", array(string())),
                            ("due_date", nullable(string())),
                            ("completed", time()),
                            ("created", string()),
                            ("modified", string()),
                            ("incident_id", string()),
                        ],
                        &["content"],
                    ),
                ),
            ],
            &["id", "type", "attributes"],
        ),
    );
    put(
        "IncidentAttachment",
        object(
            &[
                ("id", string()),
                ("type", one_of(&["incident_attachments"])),
                (
                    "attributes",
                    object(
                        &[("attachment_type", one_of(&["postmortem", "link"])), ("attachment", object(&[("documentUrl", string()), ("title", string())], &["documentUrl"])), ("modified", string())],
                        &["attachment_type", "attachment"],
                    ),
                ),
            ],
            &["id", "type", "attributes"],
        ),
    );
    put(
        "TimelineCell",
        object(
            &[
                ("id", string()),
                ("created", string()),
                ("author", string()),
                ("cell_type", one_of(&["markdown", "changed", "monitor"])),
                ("content", describe(string(), "a note's Markdown")),
                ("important", boolean()),
                ("what", describe(string(), "a change's: declared, severity, state, commander, title, postmortem")),
                ("from", string()),
                ("to", string()),
                ("monitor_id", string()),
                ("monitor", string()),
                ("state", string()),
                ("value", nullable(number())),
                ("message", string()),
            ],
            &["id", "created", "cell_type"],
        ),
    );
    // Notebooks (§11.1): Datadog's v1.
    let notebook_time = describe(
        object(&[("live_span", one_of(&super::notebooks::LIVE_SPANS.map(|(n, _)| n))), ("start", string()), ("end", string()), ("live", boolean())], &[]),
        "the last of a span, or from start to end (RFC 3339)",
    );
    let notebook_metadata = object(&[("is_template", boolean()), ("take_snapshots", boolean()), ("type", nullable(one_of(&super::notebooks::TYPES)))], &[]);
    let cell_attributes = describe(
        object(
            &[
                (
                    "definition",
                    describe(
                        object(&[("type", one_of(&super::notebooks::CELL_TYPES)), ("text", string()), ("requests", array(any())), ("query", string())], &["type"]),
                        "Datadog's widget definition, or Markdown's",
                    ),
                ),
                ("graph_size", one_of(&super::notebooks::GRAPH_SIZES)),
                ("split_by", any()),
                ("time", nullable(notebook_time.clone())),
            ],
            &["definition"],
        ),
        "a cell: Markdown, or a timeseries, top list, heatmap, distribution or log stream",
    );
    put(
        "Notebook",
        object(
            &[
                ("id", integer()),
                ("type", one_of(&["notebooks"])),
                (
                    "attributes",
                    object(
                        &[
                            ("author", object(&[("handle", string()), ("name", string())], &[])),
                            (
                                "cells",
                                array(object(
                                    &[
                                        ("id", string()),
                                        ("type", one_of(&["notebook_cells"])),
                                        ("attributes", cell_attributes.clone()),
                                        ("kiln", describe(any(), "how kiln draws a graph cell: its widget, and what it said of it")),
                                    ],
                                    &["id", "type", "attributes"],
                                )),
                            ),
                            ("created", string()),
                            ("modified", string()),
                            ("name", string()),
                            ("status", one_of(&["published"])),
                            ("time", notebook_time.clone()),
                            ("metadata", notebook_metadata.clone()),
                            ("template_variables", nullable(array(any()))),
                            ("kiln", describe(object(&[("incident", nullable(string()))], &[]), "the incident it is the postmortem of")),
                        ],
                        &["name"],
                    ),
                ),
            ],
            &["id", "type", "attributes"],
        ),
    );
    put(
        "NotebookBody",
        object(
            &[(
                "data",
                object(
                    &[
                        ("type", one_of(&["notebooks"])),
                        (
                            "attributes",
                            object(
                                &[
                                    ("name", describe(string(), "80 characters at the most")),
                                    (
                                        "cells",
                                        array(object(
                                            &[("id", describe(string(), "a cell kept, a change's")), ("type", one_of(&["notebook_cells"])), ("attributes", cell_attributes)],
                                            &["type", "attributes"],
                                        )),
                                    ),
                                    ("time", notebook_time),
                                    ("status", one_of(&["published"])),
                                    ("metadata", notebook_metadata),
                                    ("template_variables", nullable(array(any()))),
                                ],
                                &["name", "cells", "time"],
                            ),
                        ),
                    ],
                    &["type", "attributes"],
                ),
            )],
            &["data"],
        ),
    );
    // Downtimes (§9.3): Datadog's v2 downtime.
    let time_in = describe(any(), "ms since the epoch, or RFC 3339");
    let downtime_fields = [
        ("message", string()),
        ("monitor_identifier", describe(object(&[("monitor_id", string()), ("monitor_tags", array(string()))], &[]), "the monitors held: one, or those that carry every tag (`*` every monitor)")),
        ("scope", describe(string(), "the groups held: env:prod host:(web-1 OR web-2) -region:eu; * every one")),
        (
            "schedule",
            describe(
                object(
                    &[
                        ("start", time_in.clone()),
                        ("end", time_in),
                        ("recurrences", array(object(&[("rrule", describe(string(), "RFC 5545: FREQ=WEEKLY;BYDAY=SA,SU")), ("start", describe(string(), "a wall-clock time in the timezone: 2026-10-24T02:00")), ("duration", describe(string(), "4h, 1h30m, 2d"))], &["rrule", "start", "duration"]))),
                        ("timezone", describe(string(), "an IANA name; UTC unless said")),
                    ],
                    &[],
                ),
                "once (from start, now when none, to end, until canceled when none), or recurring",
            ),
        ),
        ("mute_first_recovery_notification", boolean()),
        ("notify_end_states", array(one_of(&["alert", "no data", "warn"]))),
        ("notify_end_types", array(one_of(&["canceled", "expired"]))),
    ];
    put("DowntimeIn", object(&downtime_fields, &["monitor_identifier", "schedule"]));
    let mut kept: Vec<(&str, Value)> = downtime_fields.to_vec();
    kept.push(("id", string()));
    kept.push(("canceled", describe(integer(), "ms it was canceled at")));
    kept.push(("created", integer()));
    kept.push(("modified", integer()));
    kept.push(("status", one_of(&["active", "scheduled", "ended", "canceled"])));
    kept.push(("window", nullable(object(&[("start", integer()), ("end", nullable(integer()))], &["start"]))));
    put("Downtime", object(&kept, &["id", "monitor_identifier", "schedule", "status"]));
    // Metrics from logs (§8.3): Datadog's definition, kiln's shape or theirs.
    let log_metric_fields = [
        ("name", describe(string(), "the metric's name: a letter, then letters, digits, _ and . (kiln.*, system.* and datadog.* are kiln's)")),
        (
            "compute",
            object(
                &[
                    ("aggregation_type", one_of(&["count", "distribution"])),
                    ("path", describe(string(), "a distribution's attribute (@duration)")),
                    ("include_percentiles", describe(boolean(), "a distribution's buckets kept, so any percentile is read")),
                ],
                &["aggregation_type"],
            ),
        ),
        ("filter", object(&[("query", describe(string(), "a log search; empty: every record"))], &[])),
        ("group_by", array(object(&[("path", describe(string(), "an attribute (@http.status_code), or service, host, source, status")), ("tag_name", describe(string(), "the tag it becomes (its path without the @)"))], &["path"]))),
    ];
    put("LogMetricIn", describe(object(&log_metric_fields, &["name", "compute"]), "kiln's shape; Datadog's (`{\"data\": {\"id\", \"type\": \"logs_metrics\", \"attributes\"}}`) is taken too"));
    let mut kept: Vec<(&str, Value)> = log_metric_fields.to_vec();
    kept.push(("created", integer()));
    kept.push(("modified", describe(integer(), "windows from this (ms) on are counted as it is now")));
    kept.push((
        "status",
        nullable(object(
            &[("window", describe(integer(), "its latest window with records of it (ms)")), ("records", integer()), ("groups", integer()), ("overflowed", describe(integer(), "records counted in its overflow group (groups past 2000)"))],
            &["window", "records", "groups", "overflowed"],
        )),
    ));
    put("LogMetric", object(&kept, &["name", "compute", "filter", "group_by", "created", "modified"]));
    put(
        "LogMetricCounting",
        object(
            &[
                ("settle_ms", describe(integer(), "how long after a window ends it is first counted, learned from how late records come")),
                ("windows_counted", integer()),
                ("windows_recounted", integer()),
                ("windows_expired", describe(integer(), "windows records came for past the twenty minutes: those not counted")),
                ("windows_waiting", integer()),
                ("late_batches", integer()),
                ("rows_scanned", integer()),
                ("count_ms", integer()),
                ("error", nullable(string())),
            ],
            &["settle_ms", "windows_counted", "windows_recounted", "windows_expired", "windows_waiting"],
        ),
    );
    put(
        "LogMetricTest",
        object(&[("metric", schema("LogMetricIn")), ("from", describe(integer(), "ms; 15 minutes before to")), ("to", describe(integer(), "ms; now"))], &["metric"]),
    );
    let common = [
        ("interval_s", describe(integer(), "seconds between pulls (300)")),
        ("lookback_s", describe(integer(), "seconds a pull reads back (three intervals)")),
        ("tags", array(string())),
        ("max_series", integer()),
        ("paused", boolean()),
    ];
    let with = |own: &[(&'static str, Value)]| -> Vec<(&'static str, Value)> { own.iter().cloned().chain(common.iter().cloned()).collect() };
    let aws = with(&[
        ("kind", one_of(&["aws"])),
        ("regions", array(string())),
        ("namespaces", array(string())),
        ("stats", array(string())),
        ("period_s", integer()),
        ("access_key_id", string()),
        ("secret_access_key", describe(string(), "sealed at rest, shown \"…\"")),
        ("session_token", string()),
        ("role_arn", string()),
        ("external_id", string()),
        ("endpoint", string()),
        ("sts_endpoint", string()),
        ("log_groups", describe(array(string()), "names, or prefixes ending in *")),
        ("log_filter", string()),
        ("logs_endpoint", string()),
    ]);
    let gcp = with(&[
        ("kind", one_of(&["gcp"])),
        ("projects", array(string())),
        ("services", array(string())),
        ("period_s", integer()),
        ("service_account", describe(string(), "a service account key's JSON: sealed at rest, shown \"…\"")),
        ("endpoint", string()),
        ("logs", describe(string(), "a filter in Cloud Logging's query language; empty, every entry; left out, no logs")),
        ("logs_endpoint", string()),
    ]);
    let azure = with(&[
        ("kind", one_of(&["azure"])),
        ("tenant_id", string()),
        ("client_id", string()),
        ("client_secret", describe(string(), "sealed at rest, shown \"…\"")),
        ("subscriptions", array(string())),
        ("resource_types", array(string())),
        ("aggregations", array(string())),
        ("period_s", integer()),
        ("authority", string()),
        ("endpoint", string()),
        ("workspaces", describe(array(string()), "Log Analytics workspaces' ids")),
        ("log_query", describe(string(), "a table, or KQL; every table unless said")),
        ("logs_endpoint", string()),
        ("metrics", describe(boolean(), "Azure Monitor's metrics read (true unless said); off, only logs")),
        (
            "event_hubs",
            describe(
                object(
                    &[
                        ("namespace", describe(string(), "its name, or its host")),
                        ("hubs", array(string())),
                        ("connection_string", describe(string(), "with Listen rights; sealed at rest, shown \"…\"; none, the integration's credentials (Entra ID)")),
                        ("endpoint", describe(string(), "host:port of a Kafka endpoint instead of the namespace's")),
                        ("tls", describe(boolean(), "true unless said; off only with an endpoint of its own")),
                    ],
                    &["namespace", "hubs"],
                ),
                "event hubs read as their events come, over Event Hubs' Kafka endpoint",
            ),
        ),
    ]);
    put("CloudIn", json!({ "oneOf": [object(&aws, &["kind"]), object(&gcp, &["kind"]), object(&azure, &["kind"])] }));
    put(
        "CloudStatus",
        object(
            &[
                ("state", one_of(&["starting", "ok", "failing", "paused"])),
                ("error", string()),
                ("warnings", array(string())),
                ("last_pull", nullable(integer())),
                ("last_ok", nullable(integer())),
                ("next_pull", nullable(integer())),
                ("series", integer()),
                ("over", integer()),
                ("points", integer()),
                ("deferred", integer()),
                ("calls", integer()),
                ("took_ms", integer()),
                ("pulls", integer()),
                ("failures", integer()),
                ("records", integer()),
                ("records_deferred", integer()),
                ("records_total", integer()),
                (
                    "streams",
                    array(object(
                        &[
                            ("hub", string()),
                            ("state", one_of(&["starting", "reading", "failing"])),
                            ("error", string()),
                            ("warnings", array(string())),
                            ("partitions", integer()),
                            ("lag", describe(integer(), "events not yet read, every partition's")),
                            ("events", integer()),
                            ("records", integer()),
                            ("skipped", describe(integer(), "metric records passed over")),
                            ("last_ms", nullable(integer())),
                        ],
                        &["hub", "state", "partitions", "lag", "events", "records", "skipped"],
                    )),
                ),
            ],
            &["state", "series", "points", "pulls", "failures", "records", "records_total"],
        ),
    );
    put("Cloud", json!({ "allOf": [schema("CloudIn"), object(&[("name", string()), ("status", schema("CloudStatus"))], &["name", "status"])] }));
    let integration = describe(
        object(&[("cloud", describe(string(), "the name of the kiln cloud integration whose credentials write here (`/clouds`), of the destination's cloud"))], &["cloud"]),
        "Datadog's fields besides it (account_id, role_name, client_email, project_id, tenant_id, client_id) are kept as given",
    );
    let prefix = describe(string(), "where in the bucket or container its objects go: <path>/dt=<YYYYMMDD>/hour=<HH>/archive_<HHmmss.SSSS>.<uuid>.json.gz");
    put(
        "ArchiveDestination",
        json!({ "oneOf": [
            object(
                &[
                    ("type", one_of(&["s3"])),
                    ("bucket", string()),
                    ("path", prefix.clone()),
                    ("integration", integration.clone()),
                    ("region", describe(string(), "the bucket's; none: the cloud integration's first, else AWS_REGION, else us-east-1")),
                    ("storage_class", describe(string(), "S3's storage class (STANDARD_IA, GLACIER_IR, …); none: the bucket's")),
                    ("encryption", object(&[("type", one_of(&["NO_OVERRIDE", "SSE_S3", "SSE_KMS"])), ("key", describe(string(), "SSE_KMS's key, its ARN"))], &["type"])),
                    ("endpoint", describe(string(), "an S3-compatible store's address (MinIO, R2, …): objects at <endpoint>/<bucket>/<key>; none: AWS's")),
                ],
                &["type", "bucket", "integration"],
            ),
            object(
                &[("type", one_of(&["gcs"])), ("bucket", string()), ("path", prefix.clone()), ("integration", integration.clone()), ("endpoint", describe(string(), "Cloud Storage's address; none: Google's"))],
                &["type", "bucket", "integration"],
            ),
            object(
                &[
                    ("type", one_of(&["azure"])),
                    ("storage_account", string()),
                    ("container", string()),
                    ("path", prefix),
                    ("integration", integration),
                    ("endpoint", describe(string(), "the account's blob address; none: https://<account>.blob.core.windows.net")),
                ],
                &["type", "storage_account", "container", "integration"],
            ),
        ] }),
    );
    let archive_fields = [
        ("name", string()),
        (
            "query",
            describe(
                string(),
                "a log search: the records it takes, each record to the first archive in order whose search it matches (kiln's own events only if it names source:kiln.event); `*` unless said",
            ),
        ),
        ("destination", schema("ArchiveDestination")),
        ("include_tags", describe(boolean(), "events carry the record's unified service tags (service, host, source, env, version)")),
        ("rehydration_tags", describe(array(string()), "Datadog's, kept as given")),
        ("rehydration_max_scan_size_in_gb", describe(integer(), "Datadog's, kept as given")),
    ];
    put("ArchiveIn", describe(object(&archive_fields, &["name", "destination"]), "an archive, or Datadog's {\"data\": {\"type\": \"archives\", \"attributes\": …}}"));
    let archive_state = one_of(&["UNKNOWN", "WORKING", "FAILING"]);
    put(
        "ArchiveStatus",
        object(
            &[
                ("state", archive_state.clone()),
                ("error", nullable(string())),
                ("events", integer()),
                ("objects", integer()),
                ("bytes", describe(integer(), "written, gzip'd")),
                ("last_written", nullable(integer())),
                ("pending", describe(integer(), "log files held not yet archived to it")),
            ],
            &["state", "events", "objects", "bytes", "pending"],
        ),
    );
    put(
        "Archive",
        json!({ "allOf": [schema("ArchiveIn"), object(&[("id", string()), ("created", ms()), ("modified", ms()), ("state", archive_state), ("status", schema("ArchiveStatus"))], &["id", "state", "status"])] }),
    );
    put(
        "RehydrationIn",
        object(
            &[
                ("name", string()),
                ("archive_id", string()),
                ("query", describe(string(), "a log search: the records kept; every record unless said")),
                ("from", ms()),
                ("to", describe(integer(), "milliseconds since the epoch: the records' times kept are [from, to)")),
                ("retention_days", describe(integer(), "how long its view is kept once read (15 unless said; 180 at the most)")),
                ("max_scan_size_in_gb", describe(number(), "bytes of objects read at the most, in GB; the archive's rehydration_max_scan_size_in_gb bounds it too")),
            ],
            &["name", "archive_id", "from", "to"],
        ),
    );
    put(
        "Rehydration",
        object(
            &[
                ("id", describe(string(), "its view's: what a logs query names as its view")),
                ("name", string()),
                ("archive_id", string()),
                ("archive", describe(string(), "the archive's name when it was asked")),
                ("query", string()),
                ("from", ms()),
                ("to", ms()),
                ("retention_days", integer()),
                ("max_scan_bytes", nullable(integer())),
                ("created", ms()),
                ("expires", describe(integer(), "when its view goes (ms)")),
                ("state", one_of(&["queued", "reading", "done", "failed"])),
                ("error", nullable(string())),
                ("objects", describe(integer(), "objects of its range")),
                ("objects_read", integer()),
                ("bytes_read", integer()),
                ("events_read", integer()),
                ("records", describe(integer(), "records kept in its view")),
                ("malformed", describe(integer(), "lines that were no event")),
                ("limited", describe(boolean(), "its scan's bound stopped it before its range's last object")),
                ("started", nullable(integer())),
                ("finished", nullable(integer())),
            ],
            &["id", "name", "archive_id", "query", "from", "to", "state", "records"],
        ),
    );
    put(
        "CloudTest",
        object(
            &[
                ("ok", boolean()),
                ("error", string()),
                ("series", integer()),
                ("over", integer()),
                ("points", integer()),
                ("records", integer()),
                ("records_sample", array(string())),
                ("calls", integer()),
                ("took_ms", integer()),
                ("metrics", integer()),
                ("sample", array(string())),
                ("warnings", array(string())),
            ],
            &["ok"],
        ),
    );
    put(
        "Widget",
        object(
            &[
                ("id", string()),
                ("title", string()),
                ("kind", one_of(super::dashboards::KINDS)),
                ("queries", describe(array(string()), "metric queries, and log, span and event queries as operands (`logs(\"status:error\").by(\"service\")`)")),
                ("search", string()),
                ("text", describe(string(), "a note's Markdown")),
                ("layout", object(&[("x", integer()), ("y", integer()), ("w", integer()), ("h", integer())], &["x", "y", "w", "h"])),
                ("options", any()),
                ("widgets", describe(array(schema("Widget")), "a group's widgets, on its own grid (no group among them)")),
            ],
            &["kind"],
        ),
    );
    let dashboard_fields = [
        ("title", string()),
        ("description", string()),
        ("template_variables", array(object(&[("name", string()), ("tag", string()), ("default", string())], &["name", "tag"]))),
        ("widgets", array(schema("Widget"))),
    ];
    put(
        "DatadogDashboard",
        describe(
            object(&[("title", string()), ("description", string()), ("layout_type", one_of(&["ordered", "free"])), ("template_variables", array(any())), ("widgets", array(any()))], &["title", "widgets"]),
            "Datadog's dashboard JSON, as its API (`/api/v1/dashboard/{id}`) gives one",
        ),
    );
    let said = object(&[("title", string()), ("type", describe(string(), "Datadog's widget type")), ("why", string())], &["title", "type", "why"]);
    put(
        "DashboardImport",
        object(
            &[
                ("widgets", describe(integer(), "Datadog's widgets, its groups' among them")),
                ("drawn", describe(integer(), "those kiln draws as Datadog does")),
                ("otherwise", describe(array(said.clone()), "those kiln draws otherwise (a change as a top list of the changes, ...)")),
                ("not_drawn", describe(array(said), "those kept as notes that say what they were, or left out (Datadog's own images)")),
            ],
            &["widgets", "drawn", "otherwise", "not_drawn"],
        ),
    );
    let slo_fields = [
        ("name", string()),
        ("description", string()),
        ("type", describe(one_of(&["ratio", "slices"]), "good events over total, or time slices meeting a condition")),
        ("good", describe(string(), "a ratio's good events: a metric query of counts")),
        ("total", describe(string(), "a ratio's total events: a metric query of counts")),
        ("query", describe(string(), "a time slice SLO's metric query")),
        ("condition", describe(string(), "a time slice's condition: a comparison and a number (< 0.3)")),
        ("slice_s", describe(integer(), "a time slice's width: 60 or 300")),
        ("target", describe(number(), "percent: 99.9")),
        ("warning", describe(number(), "percent, above the target: WARN below it")),
        ("timeframe", one_of(&["7d", "30d", "90d"])),
        ("tags", array(string())),
    ];
    put("SloIn", object(&slo_fields, &["name", "type", "target", "timeframe"]));
    let mut slo_kept: Vec<(&str, Value)> = slo_fields.to_vec();
    slo_kept.extend([("id", string()), ("created", ms()), ("modified", ms())]);
    put("SloKept", object(&slo_kept, &["id", "name", "type", "target", "timeframe"]));
    put(
        "SloStanding",
        object(
            &[
                ("state", one_of(&["OK", "WARN", "BREACHED", "NO DATA", "INVALID"])),
                ("sli", describe(nullable(number()), "percent, over the timeframe")),
                ("good", number()),
                ("total", number()),
                ("budget_left", describe(nullable(number()), "percent of the error budget (below 0 once spent)")),
                ("burn_rates", describe(map_of(nullable(number())), "over the last 1h, 6h, 1d and 3d: the error rate over the budget's")),
                ("days", array(object(&[("day", ms()), ("sli", nullable(number())), ("budget_left", nullable(number()))], &["day"]))),
                ("error", string()),
            ],
            &["state"],
        ),
    );
    put(
        "Slo",
        json!({ "allOf": [schema("SloKept"), object(&[("standing", schema("SloStanding")), ("alerts", array(object(&[("id", string()), ("name", string()), ("query", string()), ("state", string())], &["id", "name", "query", "state"])))], &["standing"])] }),
    );
    put("DashboardIn", object(&dashboard_fields, &["title"]));
    let mut kept: Vec<(&str, Value)> = dashboard_fields.to_vec();
    kept.extend([("id", string()), ("builtin", boolean()), ("created", ms()), ("modified", ms())]);
    put("Dashboard", object(&kept, &["id", "title", "description", "template_variables", "widgets", "builtin"]));
    put(
        "DashboardSummary",
        object(&[("id", string()), ("title", string()), ("description", string()), ("builtin", boolean()), ("widgets", describe(integer(), "how many")), ("modified", ms())], &["id", "title", "builtin", "widgets"]),
    );
    let tags_in = describe(json!({ "anyOf": [array(string()), map_of(string())] }), "`key:value` strings, or an object");
    put(
        "SeriesIntake",
        describe(
            object(
                &[(
                    "series",
                    array(object(
                        &[("metric", string()), ("type", one_of(&["gauge", "count", "rate", "counter"])), ("tags", tags_in.clone()), ("host", string()), ("points", describe(array(array(number())), "[seconds since the epoch, value]"))],
                        &["metric", "points"],
                    )),
                )],
                &["series"],
            ),
            "Datadog's v1 series",
        ),
    );
    put(
        "DistributionIntake",
        describe(
            object(
                &[("series", array(object(&[("metric", string()), ("tags", tags_in), ("host", string()), ("points", describe(array(array(any())), "[seconds since the epoch, [values]]"))], &["metric", "points"])))],
                &["series"],
            ),
            "Datadog's v1 distribution_points",
        ),
    );
    put(
        "LogIntake",
        describe(
            array(object(
                &[
                    ("message", string()),
                    ("timestamp", describe(integer(), "ms since the epoch; now when none")),
                    ("status", string()),
                    ("service", string()),
                    ("hostname", string()),
                    ("source", describe(string(), "or `ddsource`")),
                    ("attributes", describe(map_of(any()), "nested objects' keys joined by dots, as Datadog's intake reads them (`{\"http\": {\"status_code\": 500}}` is `@http.status_code`)")),
                ],
                &["message"],
            )),
            "Datadog's log intake",
        ),
    );
    s
}

fn any() -> Value {
    json!({})
}

/// A JSON body of `schema_name`.
fn body(schema_name: &str) -> Value {
    json!({ "required": true, "content": { "application/json": { "schema": schema(schema_name) } } })
}

fn answer(what: &str, schema_name: Option<&str>) -> Value {
    match schema_name {
        Some(n) => json!({ "description": what, "content": { "application/json": { "schema": schema(n) } } }),
        None => json!({ "description": what }),
    }
}

/// An answer of an inline schema.
fn answer_of(what: &str, s: Value) -> Value {
    json!({ "description": what, "content": { "application/json": { "schema": s } } })
}

/// The errors every route can answer.
fn errors(responses: &mut Map<String, Value>) {
    for (code, what) in [("400", "the request is not one this route takes"), ("401", "not signed in"), ("403", "the capability is missing"), ("404", "no such thing"), ("503", "observe is starting, or behind on its writes (try again)")] {
        responses.entry(code.to_owned()).or_insert_with(|| answer(what, Some("Error")));
    }
}

/// An operation: its summary, capability, parameters, body and answers.
struct Op<'a> {
    method: &'a str,
    path: &'a str,
    summary: &'a str,
    capability: &'a str,
    params: Vec<Value>,
    body: Option<Value>,
    answers: Vec<(&'a str, Value)>,
}

fn param(name: &str, at: &str, required: bool, s: Value, what: &str) -> Value {
    json!({ "name": name, "in": at, "required": required, "schema": s, "description": what })
}

fn path_param(name: &str, what: &str) -> Value {
    param(name, "path", true, string(), what)
}

/// Every operation, its path under `/api/observe/v1` unless it begins at the root (`/v1/…`).
fn ops() -> Vec<Op<'static>> {
    let ok = |s: &'static str| ("200", answer("taken, or what was asked", Some(s)));
    let op = |method, path, summary, capability, body: Option<Value>, answers: Vec<(&'static str, Value)>| Op { method, path, summary, capability, params: Vec::new(), body, answers };
    let id = || path_param("id", "its id");
    let name = || path_param("name", "its name");
    let no_content = || ("204", answer("done", None));
    let accepted = |s: Value| ("202", answer_of("taken", s));
    let mut v = vec![
        op("post", "/query", "Asks a metric query: its series on a grid (or a heatmap, with `heatmap`)", "observe.read", Some(body("QueryRequest")), vec![("200", answer_of("the series, or the heatmap", json!({ "oneOf": [schema("QueryAnswer"), schema("HeatmapAnswer")] })))]),
        op("post", "/checks/status", "A service check's status by group (Datadog's check status widget)", "observe.read", Some(body("CheckStatusRequest")), vec![ok("CheckStatus")]),
        Op {
            params: vec![
                param("query", "query", true, string(), "a metric query"),
                param("from", "query", true, integer(), "ms since the epoch"),
                param("to", "query", true, integer(), "ms since the epoch"),
                param("step", "query", false, integer(), "ms"),
            ],
            ..op("get", "/query", "Asks a metric query, its parts in the query string", "observe.read", None, vec![ok("QueryAnswer")])
        },
        Op {
            params: vec![param("match", "query", false, string(), "a glob of names (`system.*`)"), param("limit", "query", false, integer(), "names at the most (10,000)")],
            ..op("get", "/metrics", "Lists metric names", "observe.read", None, vec![("200", answer_of("the names", object(&[("metrics", array(string()))], &["metrics"])))])
        },
        Op {
            params: vec![param("match", "query", false, string(), "a glob of names"), param("limit", "query", false, integer(), "metrics at the most (100 unless said; 1,000)")],
            ..op("get", "/metrics/summary", "Each metric's series, kinds, tags' cardinalities and samples, the metrics of most series first", "observe.read", None, vec![("200", answer_of("the metrics", object(&[("matched", integer()), ("metrics", array(schema("MetricSummary")))], &["matched", "metrics"])))])
        },
        Op { params: vec![path_param("name", "the metric's name")], ..op("get", "/metrics/summary/{name}", "A metric's summary, each tag's values of most series with it", "observe.read", None, vec![ok("MetricSummary")]) },
        Op {
            params: vec![param("key", "query", true, string(), "the tag's key"), param("match", "query", false, string(), "a glob of values"), param("limit", "query", false, integer(), "values at the most")],
            ..op("get", "/tags", "Lists a tag's values", "observe.read", None, vec![("200", answer_of("the values", object(&[("key", string()), ("values", array(string()))], &["key", "values"])))])
        },
        op("post", "/logs/search", "Searches log records, newest first", "observe.read", Some(body("SearchRequest")), vec![ok("SearchAnswer")]),
        op("post", "/logs/aggregate", "Log analytics: a measure by facets, in buckets of time with `step`", "observe.read", Some(body("AggregateRequest")), vec![ok("AggregateAnswer")]),
        op("post", "/logs/facets", "Each facet's most common values over a search", "observe.read", Some(body("FacetsRequest")), vec![ok("FacetsAnswer")]),
        op("post", "/logs/patterns", "A search's records grouped into patterns (Drain)", "observe.read", Some(body("SearchRequest")), vec![ok("PatternsAnswer")]),
        op("post", "/spans/search", "Searches spans, newest first (Datadog's trace explorer's syntax: resource_name:, @duration:>250ms)", "observe.read", Some(body("SearchRequest")), vec![ok("SpansAnswer")]),
        op("post", "/spans/aggregate", "Trace analytics: a measure by facets (resource_name among them), in buckets of time with `step`", "observe.read", Some(body("AggregateRequest")), vec![ok("AggregateAnswer")]),
        op("post", "/spans/facets", "Each facet's most common values over a span search", "observe.read", Some(body("FacetsRequest")), vec![ok("FacetsAnswer")]),
        Op {
            params: vec![path_param("id", "the trace's id, 32 hex digits"), param("from", "query", false, integer(), "ms since the epoch (with to: where to look)"), param("to", "query", false, integer(), "ms since the epoch")],
            ..op("get", "/traces/{id}", "A trace: its spans as a waterfall draws them, its services and errors", "observe.read", None, vec![ok("Trace"), ("404", answer("no spans of it", Some("Error")))])
        },
        Op {
            params: vec![param("from", "query", false, integer(), "ms since the epoch (an hour before to)"), param("to", "query", false, integer(), "ms since the epoch (now)"), param("env", "query", false, string(), "one environment")],
            ..op("get", "/apm/services", "The services: each one's requests, errors and latency over the range, from the trace metrics", "observe.read", None, vec![ok("ServicesAnswer")])
        },
        Op {
            params: vec![path_param("name", "the service"), param("from", "query", false, integer(), "ms since the epoch"), param("to", "query", false, integer(), "ms since the epoch"), param("env", "query", false, string(), "one environment")],
            ..op("get", "/apm/services/{name}", "A service: its requests by resource and kind, and the calls it makes by what it calls", "observe.read", None, vec![ok("ServiceAnswer"), ("404", answer("no requests nor calls of it", Some("Error")))])
        },
        Op {
            params: vec![param("from", "query", false, integer(), "ms since the epoch (15 minutes before to)"), param("to", "query", false, integer(), "ms since the epoch"), param("env", "query", false, string(), "one environment")],
            ..op("get", "/apm/map", "The service map over the range: who calls whom, from the spans; its figures from the trace metrics (every request and call, whatever traces were kept), each edge its share of the groups that count its calls", "observe.read", None, vec![ok("ServiceMap")])
        },
        Op {
            params: vec![
                param("from", "query", false, integer(), "ms since the epoch (a day before to)"),
                param("to", "query", false, integer(), "ms since the epoch (now)"),
                param("service", "query", false, string(), "one service"),
                param("env", "query", false, string(), "one environment"),
                param("status", "query", false, string(), "statuses, comma-separated: for_review, reviewed, resolved, ignored"),
                param("query", "query", false, string(), "words of its type, message, service, file or function"),
                param("all", "query", false, boolean(), "those with no error in the range too, the latest first"),
                param("limit", "query", false, integer(), "issues at the most (100; 1,000)"),
            ],
            ..op(
                "get",
                "/errors/issues",
                "Error Tracking: the issues with errors in the range, the most first (an entry span's or a log's errors, grouped as Datadog groups them)",
                "observe.read",
                None,
                vec![ok("IssueList")],
            )
        },
        Op {
            params: vec![
                path_param("id", "the issue's id"),
                param("from", "query", false, integer(), "ms since the epoch (a day before to)"),
                param("to", "query", false, integer(), "ms since the epoch"),
            ],
            ..op(
                "get",
                "/errors/issues/{id}",
                "An issue: its errors over the range by bucket, version and env, its latest error whole, its activity",
                "observe.read",
                None,
                vec![ok("IssueDetail"), ("404", answer("no such issue", Some("Error")))],
            )
        },
        Op {
            params: vec![path_param("id", "the issue's id")],
            ..op(
                "put",
                "/errors/issues/{id}",
                "Sets an issue's status (`{\"status\": \"reviewed\"}`: for_review, reviewed, resolved or ignored)",
                "observe.write",
                Some(body("IssueStatus")),
                vec![ok("Issue"), ("400", answer("not a status", Some("Error"))), ("404", answer("no such issue", Some("Error")))],
            )
        },
        Op {
            params: vec![param("query", "query", false, string(), "a log search"), param("rate", "query", false, integer(), "records a second at the most, a uniform sample past it")],
            ..op(
                "get",
                "/logs/tail",
                "Follows the records applied, live, over a WebSocket: messages `{type, records, read, matched, missed, ms}`",
                "observe.read",
                None,
                vec![("101", answer("switching to the WebSocket", None))],
            )
        },
        op("post", "/events", "Posts an event (Datadog's v1 events API)", "observe.write", Some(body("EventIn")), vec![accepted(object(&[("status", string())], &["status"]))]),
        op("post", "/events/search", "Searches events, newest first", "observe.read", Some(body("SearchRequest")), vec![ok("SearchAnswer")]),
        op("get", "/status", "This node's replica: its role, log, store and every machine's", "observe.read", None, vec![ok("Status")]),
        op("post", "/compact", "Cuts and compacts the log now", "observe.manage", None, vec![accepted(object(&[("compacting", boolean())], &["compacting"]))]),
        op("post", "/verify", "Takes every replica's full digest at one index", "observe.manage", None, vec![accepted(object(&[("verifying", boolean())], &["verifying"]))]),
        op("get", "/monitors", "Lists monitors, each its state and its groups'", "observe.read", None, vec![("200", answer_of("the monitors", object(&[("monitors", array(schema("Monitor")))], &["monitors"])))]),
        op("post", "/monitors", "Makes a monitor", "observe.write", Some(body("MonitorIn")), vec![("201", answer("made", Some("Monitor")))]),
        op("post", "/monitors/validate", "Checks a monitor's query, and says what it watches", "observe.read", Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("query", string()), ("type", string())], &["query"]) } } })), vec![ok("Validated")]),
        Op { params: vec![id()], ..op("get", "/monitors/{id}", "A monitor", "observe.read", None, vec![ok("Monitor")]) },
        Op { params: vec![id()], ..op("put", "/monitors/{id}", "Changes a monitor", "observe.write", Some(body("MonitorIn")), vec![ok("Monitor")]) },
        Op { params: vec![id()], ..op("delete", "/monitors/{id}", "Deletes a monitor (refused while a composite reads it: 409)", "observe.write", None, vec![no_content(), ("409", answer("composites read it", Some("Error")))]) },
        Op {
            params: vec![id()],
            ..op("post", "/monitors/{id}/mute", "Mutes a monitor, until `until` (unix ms) or unmuted", "observe.write", Some(json!({ "required": false, "content": { "application/json": { "schema": object(&[("until", integer())], &[]) } } })), vec![ok("Monitor")])
        },
        Op { params: vec![id()], ..op("post", "/monitors/{id}/unmute", "Unmutes a monitor", "observe.write", None, vec![ok("Monitor")]) },
        op("get", "/integrations", "Lists notification integrations, their secrets not shown", "observe.read", None, vec![("200", answer_of("the integrations", object(&[("integrations", array(schema("Integration")))], &["integrations"])))]),
        Op { params: vec![name()], ..op("put", "/integrations/{name}", "Makes or changes an integration", "observe.manage", Some(body("IntegrationIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/integrations/{name}", "Removes an integration", "observe.manage", None, vec![no_content()]) },
        Op { params: vec![name()], ..op("post", "/integrations/{name}/test", "Sends an integration a test notice", "observe.manage", None, vec![accepted(object(&[("sent", boolean())], &["sent"]))]) },
        op("get", "/scrapes", "Lists Prometheus scrape targets, each machine's state of them", "observe.read", None, vec![("200", answer_of("the targets", object(&[("scrapes", array(schema("Scrape")))], &["scrapes"])))]),
        op("get", "/statsd", "Lists DogStatsD listeners, each machine's state of them", "observe.read", None, vec![("200", answer_of("the listeners", object(&[("statsd", array(schema("Statsd")))], &["statsd"])))]),
        Op { params: vec![name()], ..op("put", "/statsd/{name}", "Makes or changes a DogStatsD listener", "observe.manage", Some(body("StatsdIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/statsd/{name}", "Removes a DogStatsD listener: its machines close it", "observe.manage", None, vec![no_content()]) },
        op("get", "/synthetics", "Lists synthetic tests, each location's state of them", "observe.read", None, vec![("200", answer_of("the tests", object(&[("synthetics", array(schema("Synthetic")))], &["synthetics"])))]),
        Op { params: vec![name()], ..op("put", "/synthetics/{name}", "Makes or changes a synthetic test (and the monitor it owns, when it alerts)", "observe.manage", Some(body("SyntheticIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/synthetics/{name}", "Removes a synthetic test and the monitor it owns", "observe.manage", None, vec![no_content()]) },
        Op { params: vec![name()], ..op("post", "/synthetics/{name}/test", "Runs a test once from the server (the one given, or the kept one), nothing written", "observe.manage", Some(json!({ "required": false, "content": { "application/json": { "schema": schema("SyntheticIn") } } })), vec![ok("SyntheticOutcome")]) },
        Op { params: vec![name()], ..op("post", "/synthetics/{name}/run", "Runs a test now at every location", "observe.manage", None, vec![("202", answer_of("asked", object(&[("run", boolean())], &["run"])))]) },
        Op { params: vec![name()], ..op("put", "/scrapes/{name}", "Makes or changes a scrape target", "observe.manage", Some(body("ScrapeIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/scrapes/{name}", "Removes a scrape target", "observe.manage", None, vec![no_content()]) },
        Op { params: vec![name()], ..op("post", "/scrapes/{name}/test", "Reads a target once from the server (the one given, or the kept one), nothing written", "observe.manage", Some(json!({ "required": false, "content": { "application/json": { "schema": schema("ScrapeIn") } } })), vec![ok("ScrapeTest")]) },
        op("get", "/log_sources", "Lists the logs machines collect (files, containers' logs, the journal), each machine's state of them", "observe.read", None, vec![("200", answer_of("the sources", object(&[("log_sources", array(schema("LogSource")))], &["log_sources"])))]),
        Op { params: vec![name()], ..op("put", "/log_sources/{name}", "Makes or changes a log source: what the machines it names read from now", "observe.manage", Some(body("LogSourceIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/log_sources/{name}", "Removes a log source", "observe.manage", None, vec![no_content()]) },
        Op { params: vec![name()], ..op("post", "/log_sources/{name}/test", "What a source (the one given, or the kept one) reads on the server: the files it matches and the records of the newest one's last lines, nothing written", "observe.manage", Some(json!({ "required": false, "content": { "application/json": { "schema": schema("LogSourceIn") } } })), vec![ok("LogSourceTest")]) },
        op("get", "/clouds", "Lists cloud integrations, each its status, their secrets \"…\"", "observe.read", None, vec![("200", answer_of("the clouds", object(&[("clouds", array(schema("Cloud")))], &["clouds"])))]),
        Op { params: vec![name()], ..op("put", "/clouds/{name}", "Makes or changes a cloud integration (a secret left out, or \"…\", is the kept one)", "observe.manage", Some(body("CloudIn")), vec![no_content()]) },
        Op { params: vec![name()], ..op("delete", "/clouds/{name}", "Removes a cloud integration", "observe.manage", None, vec![no_content()]) },
        Op { params: vec![name()], ..op("post", "/clouds/{name}/pull", "Pulls a cloud integration now", "observe.manage", None, vec![accepted(object(&[("pulling", boolean())], &["pulling"]))]) },
        Op { params: vec![name()], ..op("post", "/clouds/{name}/test", "Pulls a configuration once, nothing written: what it would read", "observe.manage", Some(body("CloudIn")), vec![ok("CloudTest")]) },
        op("get", "/dashboards", "Lists dashboards, without their widgets", "observe.read", None, vec![("200", answer_of("the dashboards", object(&[("dashboards", array(schema("DashboardSummary")))], &["dashboards"])))]),
        op("post", "/dashboards", "Makes a dashboard", "observe.write", Some(body("DashboardIn")), vec![("201", answer("made", Some("Dashboard")))]),
        op(
            "post",
            "/dashboards/import",
            "Makes a dashboard of Datadog's dashboard JSON: its widgets kiln's, and what became of each said",
            "observe.write",
            Some(body("DatadogDashboard")),
            vec![("201", answer_of("made", object(&[("dashboard", schema("Dashboard")), ("report", schema("DashboardImport"))], &["dashboard", "report"])))],
        ),
        Op { params: vec![id()], ..op("get", "/dashboards/{id}/datadog", "A dashboard as Datadog's dashboard JSON", "observe.read", None, vec![ok("DatadogDashboard")]) },
        Op { params: vec![id()], ..op("get", "/dashboards/{id}", "A dashboard, its widgets", "observe.read", None, vec![ok("Dashboard")]) },
        Op { params: vec![id()], ..op("put", "/dashboards/{id}", "Changes a dashboard", "observe.write", Some(body("DashboardIn")), vec![ok("Dashboard")]) },
        Op { params: vec![id()], ..op("delete", "/dashboards/{id}", "Deletes a dashboard", "observe.write", None, vec![no_content()]) },
        op("post", "/series", "Writes points (Datadog's v1 series)", "observe.write", Some(body("SeriesIntake")), vec![accepted(object(&[("points", integer())], &["points"]))]),
        op("post", "/distribution_points", "Writes a distribution's values (Datadog's v1 distribution points)", "observe.write", Some(body("DistributionIntake")), vec![accepted(object(&[("values", integer())], &["values"]))]),
        op("post", "/logs", "Writes log records (Datadog's log intake)", "observe.write", Some(body("LogIntake")), vec![accepted(object(&[("records", integer())], &["records"]))]),
        op("get", "/openapi.json", "This document", "observe.read", None, vec![("200", answer_of("OpenAPI 3.1", any()))]),
        op("get", "/downtimes", "Lists downtimes, newest first, each its status and its window now or next", "observe.read", None, vec![("200", answer_of("the downtimes", object(&[("downtimes", array(schema("Downtime")))], &["downtimes"])))]),
        op("post", "/downtimes", "Makes a downtime: the monitors and groups it holds, once or recurring", "observe.write", Some(body("DowntimeIn")), vec![("201", answer("made", Some("Downtime")))]),
        Op { params: vec![id()], ..op("get", "/downtimes/{id}", "A downtime", "observe.read", None, vec![ok("Downtime")]) },
        Op { params: vec![id()], ..op("put", "/downtimes/{id}", "Changes a downtime", "observe.write", Some(body("DowntimeIn")), vec![ok("Downtime")]) },
        Op { params: vec![id()], ..op("delete", "/downtimes/{id}", "Cancels a downtime (kept, its status canceled)", "observe.write", None, vec![no_content()]) },
        op(
            "get",
            "/log_metrics",
            "Lists the metrics from logs, each what its latest window with records held, and how counting goes",
            "observe.read",
            None,
            vec![("200", answer_of("the metrics from logs", object(&[("log_metrics", array(schema("LogMetric"))), ("counting", schema("LogMetricCounting"))], &["log_metrics", "counting"])))],
        ),
        op(
            "post",
            "/log_metrics",
            "Makes a metric from logs, counted from the next ten seconds on (names written by something else: 409)",
            "observe.write",
            Some(body("LogMetricIn")),
            vec![("201", answer("made", Some("LogMetric"))), ("409", answer("its series have a writer already", Some("Error")))],
        ),
        Op { params: vec![name()], ..op("get", "/log_metrics/{name}", "A metric from logs", "observe.read", None, vec![ok("LogMetric")]) },
        Op { params: vec![name()], ..op("put", "/log_metrics/{name}", "Changes a metric from logs: its filter, groups and percentiles (what it computes stays: 400)", "observe.write", Some(body("LogMetricIn")), vec![ok("LogMetric")]) },
        Op { params: vec![name()], ..op("delete", "/log_metrics/{name}", "Removes a metric from logs (its series stay, to retention)", "observe.write", None, vec![no_content()]) },
        op(
            "post",
            "/log_metric_test",
            "What a metric from logs would count of the records held, window by window, its series named preview (preview.count, … a distribution's): nothing written",
            "observe.read",
            Some(body("LogMetricTest")),
            vec![("200", answer_of("its series", object(&[("series", array(object(&[("metric", string()), ("tags", map_of(string())), ("points", array(any()))], &["metric", "tags", "points"]))), ("records", integer()), ("rows_scanned", integer())], &["series", "records"])))],
        ),
        op(
            "get",
            "/span_metrics",
            "Lists the metrics from spans, and kiln's trace metrics (trace.hits, trace.errors, trace.duration, trace.outbound.*) as they are counted",
            "observe.read",
            None,
            vec![(
                "200",
                answer_of(
                    "the metrics from spans",
                    object(
                        &[("span_metrics", array(schema("LogMetric"))), ("builtin", array(object(&[("name", string()), ("series", array(string())), ("group_by", array(string()))], &["name", "series", "group_by"]))), ("counting", schema("LogMetricCounting"))],
                        &["span_metrics", "builtin", "counting"],
                    ),
                ),
            )],
        ),
        op(
            "post",
            "/span_metrics",
            "Makes a metric from spans (its filter a span search; a distribution of @duration in seconds), counted from the next ten seconds on",
            "observe.write",
            Some(body("LogMetricIn")),
            vec![("201", answer("made", Some("LogMetric"))), ("409", answer("its series have a writer already", Some("Error")))],
        ),
        Op { params: vec![name()], ..op("get", "/span_metrics/{name}", "A metric from spans", "observe.read", None, vec![ok("LogMetric")]) },
        Op { params: vec![name()], ..op("put", "/span_metrics/{name}", "Changes a metric from spans: its filter, groups and percentiles", "observe.write", Some(body("LogMetricIn")), vec![ok("LogMetric")]) },
        Op { params: vec![name()], ..op("delete", "/span_metrics/{name}", "Removes a metric from spans (its series stay, to retention)", "observe.write", None, vec![no_content()]) },
        op(
            "post",
            "/span_metric_test",
            "What a metric from spans would count of the spans held, window by window, its series named preview: nothing written",
            "observe.read",
            Some(body("LogMetricTest")),
            vec![("200", answer_of("its series", object(&[("series", array(object(&[("metric", string()), ("tags", map_of(string())), ("points", array(any()))], &["metric", "tags", "points"]))), ("records", integer()), ("rows_scanned", integer())], &["series", "records"])))],
        ),
        op(
            "get",
            "/archives",
            "Lists the log archives in the order records are matched against them, each its status",
            "observe.read",
            None,
            vec![("200", answer_of("the archives", object(&[("archives", array(schema("Archive")))], &["archives"])))],
        ),
        op(
            "post",
            "/archives",
            "Makes a log archive, last in order: it takes the records of the log files cut from now on (checked: its search parses, its cloud integration is of its destination's cloud)",
            "observe.manage",
            Some(body("ArchiveIn")),
            vec![("201", answer("made", Some("Archive"))), ("409", answer("an archive of that name already", Some("Error")))],
        ),
        Op { params: vec![id()], ..op("get", "/archives/{id}", "A log archive and its status", "observe.read", None, vec![ok("Archive")]) },
        Op {
            params: vec![id()],
            ..op(
                "put",
                "/archives/{id}",
                "Changes a log archive: its search and tags apply to the log files cut after, where it writes to its next object",
                "observe.manage",
                Some(body("ArchiveIn")),
                vec![ok("Archive")],
            )
        },
        Op { params: vec![id()], ..op("delete", "/archives/{id}", "Removes a log archive (what it wrote stays)", "observe.manage", None, vec![no_content()]) },
        op(
            "get",
            "/rehydrations",
            "Lists the rehydrations from archives (Datadog's historical views), each how its reading goes",
            "observe.read",
            None,
            vec![("200", answer_of("the rehydrations", object(&[("rehydrations", array(schema("Rehydration")))], &["rehydrations"])))],
        ),
        op(
            "post",
            "/rehydrations",
            "Rehydrates an archive's records of a range that a search takes into a view of their own, searched by naming it (`view`); queued, read one at a time",
            "observe.manage",
            Some(body("RehydrationIn")),
            vec![("201", answer("queued", Some("Rehydration"))), ("409", answer("a rehydration of that name already", Some("Error")))],
        ),
        Op { params: vec![id()], ..op("get", "/rehydrations/{id}", "A rehydration and how its reading goes", "observe.read", None, vec![ok("Rehydration")]) },
        Op { params: vec![id()], ..op("delete", "/rehydrations/{id}", "Removes a rehydration and its view (its reading stopped)", "observe.manage", None, vec![no_content()]) },
        op(
            "get",
            "/archive_order",
            "The archives' ids, in the order records are matched against them",
            "observe.read",
            None,
            vec![("200", answer_of("their order", object(&[("archive_ids", array(string()))], &["archive_ids"])))],
        ),
        op(
            "put",
            "/archive_order",
            "Puts the archives in an order (every one of them, each once), from the log files cut after; Datadog's {\"data\": {\"type\": \"archive_order\", \"attributes\": …}} too",
            "observe.manage",
            Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("archive_ids", array(string()))], &["archive_ids"]) } } })),
            vec![("200", answer_of("their order", object(&[("archive_ids", array(string()))], &["archive_ids"])))],
        ),
        op("get", "/pipelines", "Lists the log pipelines in the order they apply: kiln's own, then yours", "observe.read", None, vec![("200", answer_of("the pipelines", object(&[("pipelines", array(schema("Pipeline")))], &["pipelines"])))]),
        op("post", "/pipelines", "Makes a log pipeline, last (checked: it compiles)", "observe.manage", Some(body("PipelineIn")), vec![("201", answer("made", Some("Pipeline")))]),
        Op { params: vec![id()], ..op("get", "/pipelines/{id}", "A log pipeline", "observe.read", None, vec![ok("Pipeline")]) },
        Op { params: vec![id()], ..op("put", "/pipelines/{id}", "Changes a log pipeline (kiln's own: whether it is on)", "observe.manage", Some(body("PipelineIn")), vec![ok("Pipeline")]) },
        Op { params: vec![id()], ..op("delete", "/pipelines/{id}", "Removes a log pipeline of yours (kiln's own are turned off instead: 400)", "observe.manage", None, vec![no_content()]) },
        op(
            "put",
            "/pipeline_order",
            "Puts your pipelines in an order (every one of them, each once)",
            "observe.manage",
            Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("pipeline_ids", array(string()))], &["pipeline_ids"]) } } })),
            vec![no_content()],
        ),
        op(
            "post",
            "/pipeline_test",
            "What a pipeline (or every one, in order) makes of records given: nothing kept",
            "observe.read",
            Some(body("PipelineTest")),
            vec![("200", answer_of("the records processed", object(&[("records", array(object(&[("ts_ns", integer()), ("status", string()), ("service", string()), ("host", string()), ("source", string()), ("message", string()), ("attributes", map_of(string())), ("trace_id", nullable(string()))], &["message", "attributes"])))], &["records"])))],
        ),
        op(
            "get",
            "/scanner/groups",
            "Lists the sensitive data scanner's groups in the order they run",
            "observe.read",
            None,
            vec![("200", answer_of("the groups", object(&[("groups", array(schema("ScanningGroup")))], &["groups"])))],
        ),
        op(
            "post",
            "/scanner/groups",
            "Makes a scanning group, last (checked: its filter and rules compile)",
            "observe.manage",
            Some(body("ScanningGroupIn")),
            vec![("201", answer("made", Some("ScanningGroup")))],
        ),
        Op { params: vec![id()], ..op("get", "/scanner/groups/{id}", "A scanning group", "observe.read", None, vec![ok("ScanningGroup")]) },
        Op {
            params: vec![id()],
            ..op("put", "/scanner/groups/{id}", "Changes a scanning group whole (its rules' ids kept)", "observe.manage", Some(body("ScanningGroupIn")), vec![ok("ScanningGroup")])
        },
        Op { params: vec![id()], ..op("delete", "/scanner/groups/{id}", "Removes a scanning group", "observe.manage", None, vec![no_content()]) },
        op(
            "put",
            "/scanner/group_order",
            "Puts the scanning groups in an order (every one, each once)",
            "observe.manage",
            Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("group_ids", array(string()))], &["group_ids"]) } } })),
            vec![no_content()],
        ),
        // Incidents (§9.5).
        Op {
            params: vec![
                param("page[size]", "query", false, integer(), "10 unless said, 100 at the most"),
                param("page[offset]", "query", false, integer(), "the first of the page"),
                param("include", "query", false, string(), "users, attachments"),
            ],
            ..op(
                "get",
                "/incidents",
                "Incidents, the newest first, as Datadog's API lists them",
                "observe.read",
                None,
                vec![("200", answer_of("a page of them", object(&[("data", array(schema("Incident"))), ("included", array(any())), ("meta", any())], &["data", "meta"])))],
            )
        },
        op(
            "post",
            "/incidents",
            "Declares an incident (Datadog's body); its handles told",
            "observe.write",
            Some(body("IncidentBody")),
            vec![("201", answer_of("declared", object(&[("data", schema("Incident"))], &["data"])))],
        ),
        Op {
            params: vec![
                param("query", "query", false, string(), "facets joined by AND, a facet's values by OR in parentheses: state:(active OR stable) AND severity:SEV-1"),
                param("sort", "query", false, one_of(&["created", "-created"]), "-created unless said"),
                param("page[size]", "query", false, integer(), "10 unless said, 100 at the most"),
                param("page[offset]", "query", false, integer(), "the first of the page"),
                param("include", "query", false, string(), "users, attachments"),
            ],
            ..op(
                "get",
                "/incidents/search",
                "Incidents a query matches, and their facets counted (Datadog's search)",
                "observe.read",
                None,
                vec![("200", answer_of("the incidents and their facets", object(&[("data", any()), ("meta", any())], &["data"])))],
            )
        },
        Op {
            params: vec![id(), param("include", "query", false, string(), "users, attachments")],
            ..op(
                "get",
                "/incidents/{id}",
                "An incident, by its id or its public id",
                "observe.read",
                None,
                vec![("200", answer_of("the incident", object(&[("data", schema("Incident"))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "patch",
                "/incidents/{id}",
                "Changes an incident: what its body says (each change of its severity, state, commander or title on its timeline; its handles told of a severity's or state's)",
                "observe.write",
                Some(body("IncidentBody")),
                vec![("200", answer_of("changed", object(&[("data", schema("Incident"))], &["data"])))],
            )
        },
        Op { params: vec![id()], ..op("delete", "/incidents/{id}", "Removes an incident", "observe.write", None, vec![no_content()]) },
        Op {
            params: vec![id()],
            ..op(
                "get",
                "/incidents/{id}/relationships/todos",
                "An incident's todos",
                "observe.read",
                None,
                vec![("200", answer_of("its todos", object(&[("data", array(schema("IncidentTodo")))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "post",
                "/incidents/{id}/relationships/todos",
                "Makes a todo of an incident",
                "observe.write",
                Some(
                    json!({ "required": true, "content": { "application/json": { "schema": object(&[("data", object(&[("attributes", object(&[("content", string()), ("assignees", array(string())), ("due_date", nullable(string()))], &["content"]))], &["attributes"]))], &["data"]) } } }),
                ),
                vec![("201", answer_of("made", object(&[("data", schema("IncidentTodo"))], &["data"])))],
            )
        },
        Op {
            params: vec![id(), path_param("todo", "its id")],
            ..op("get", "/incidents/{id}/relationships/todos/{todo}", "A todo", "observe.read", None, vec![("200", answer_of("the todo", object(&[("data", schema("IncidentTodo"))], &["data"])))])
        },
        Op {
            params: vec![id(), path_param("todo", "its id")],
            ..op(
                "patch",
                "/incidents/{id}/relationships/todos/{todo}",
                "Changes a todo: its content, assignees, due date; done (completed a time) or not (null)",
                "observe.write",
                Some(
                    json!({ "required": true, "content": { "application/json": { "schema": object(&[("data", object(&[("attributes", object(&[("content", string()), ("assignees", array(string())), ("due_date", nullable(string())), ("completed", describe(nullable(string()), "RFC 3339: done; null: not"))], &[]))], &["attributes"]))], &["data"]) } } }),
                ),
                vec![("200", answer_of("changed", object(&[("data", schema("IncidentTodo"))], &["data"])))],
            )
        },
        Op { params: vec![id(), path_param("todo", "its id")], ..op("delete", "/incidents/{id}/relationships/todos/{todo}", "Removes a todo", "observe.write", None, vec![no_content()]) },
        Op {
            params: vec![id()],
            ..op(
                "get",
                "/incidents/{id}/attachments",
                "An incident's attachments",
                "observe.read",
                None,
                vec![("200", answer_of("its attachments", object(&[("data", array(schema("IncidentAttachment")))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "post",
                "/incidents/{id}/attachments",
                "Attaches a postmortem (one an incident) or a link",
                "observe.write",
                Some(
                    json!({ "required": true, "content": { "application/json": { "schema": object(&[("data", object(&[("attributes", object(&[("attachment_type", one_of(&["postmortem", "link"])), ("attachment", object(&[("documentUrl", string()), ("title", string())], &["documentUrl"]))], &["attachment"]))], &["attributes"]))], &["data"]) } } }),
                ),
                vec![("201", answer_of("attached", object(&[("data", schema("IncidentAttachment"))], &["data"])))],
            )
        },
        Op {
            params: vec![id(), path_param("attachment", "its id")],
            ..op(
                "patch",
                "/incidents/{id}/attachments/{attachment}",
                "Changes an attachment's URL or title",
                "observe.write",
                Some(
                    json!({ "required": true, "content": { "application/json": { "schema": object(&[("data", object(&[("attributes", object(&[("attachment", object(&[("documentUrl", string()), ("title", string())], &[]))], &[]))], &["attributes"]))], &["data"]) } } }),
                ),
                vec![("200", answer_of("changed", object(&[("data", schema("IncidentAttachment"))], &["data"])))],
            )
        },
        Op { params: vec![id(), path_param("attachment", "its id")], ..op("delete", "/incidents/{id}/attachments/{attachment}", "Removes an attachment", "observe.write", None, vec![no_content()]) },
        Op {
            params: vec![id()],
            ..op(
                "get",
                "/incidents/{id}/timeline",
                "An incident's timeline: its notes, each change, its monitor's alerts and recoveries",
                "observe.read",
                None,
                vec![("200", answer_of("its cells, the oldest first", object(&[("data", array(schema("TimelineCell")))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "post",
                "/incidents/{id}/timeline",
                "Puts a note on an incident's timeline",
                "observe.write",
                Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("content", describe(string(), "Markdown")), ("important", boolean())], &["content"]) } } })),
                vec![("201", answer_of("put", object(&[("data", schema("TimelineCell"))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "post",
                "/incidents/{id}/postmortem",
                "Makes an incident's postmortem: a notebook of a template (kiln's own unless one is named) with the incident's variables filled, attached as its postmortem",
                "observe.write",
                Some(json!({ "required": false, "content": { "application/json": { "schema": object(&[("template_id", describe(integer(), "a notebook that is a template"))], &[]) } } })),
                vec![("201", answer_of("made", object(&[("data", schema("Notebook"))], &["data"])))],
            )
        },
        // Notebooks (§11.1).
        Op {
            params: vec![
                param("author_handle", "query", false, string(), "theirs alone"),
                param("exclude_author_handle", "query", false, string(), "not theirs"),
                param("query", "query", false, string(), "a part of the name or of the author's handle"),
                param("is_template", "query", false, boolean(), "templates alone (false unless said: none)"),
                param("type", "query", false, one_of(&super::notebooks::TYPES), "of that type"),
                param("sort_field", "query", false, one_of(&["modified", "name", "created"]), "modified unless said"),
                param("sort_dir", "query", false, one_of(&["asc", "desc"]), "desc unless said"),
                param("start", "query", false, integer(), "the first of the page"),
                param("count", "query", false, integer(), "100 unless said, 1,000 at the most"),
                param("include_cells", "query", false, boolean(), "each one's cells and time (true unless said)"),
            ],
            ..op(
                "get",
                "/notebooks",
                "Notebooks, as Datadog's API lists them",
                "observe.read",
                None,
                vec![("200", answer_of("a page of them", object(&[("data", array(schema("Notebook"))), ("meta", any())], &["data", "meta"])))],
            )
        },
        op(
            "post",
            "/notebooks",
            "Makes a notebook (Datadog's body)",
            "observe.write",
            Some(body("NotebookBody")),
            vec![("200", answer_of("made", object(&[("data", schema("Notebook"))], &["data"])))],
        ),
        Op { params: vec![id()], ..op("get", "/notebooks/{id}", "A notebook", "observe.read", None, vec![("200", answer_of("the notebook", object(&[("data", schema("Notebook"))], &["data"])))]) },
        Op {
            params: vec![id()],
            ..op(
                "put",
                "/notebooks/{id}",
                "Changes a notebook whole (its cells' ids kept where given)",
                "observe.write",
                Some(body("NotebookBody")),
                vec![("200", answer_of("changed", object(&[("data", schema("Notebook"))], &["data"])))],
            )
        },
        Op { params: vec![id()], ..op("delete", "/notebooks/{id}", "Removes a notebook", "observe.write", None, vec![no_content()]) },
        // The infrastructure list (§10.9).
        Op {
            params: vec![
                param("filter", "query", false, string(), "terms each a part of its name, an alias, an app or a source, or a tag (key:value, `*` for any part)"),
                param("sort_field", "query", false, one_of(&["name", "status", "cpu", "iowait", "load", "memory", "disk", "apps", "last_reported"]), "name unless said"),
                param("sort_dir", "query", false, one_of(&["asc", "desc"]), "asc unless said"),
                param("start", "query", false, integer(), "the first host of the page"),
                param("count", "query", false, integer(), "hosts a page: 100 unless said, 1,000 at the most"),
                param("from", "query", false, integer(), "seconds since the epoch: hosts that reported since (three hours ago unless said)"),
                param("include_hosts_metadata", "query", false, boolean(), "what the Agent says of each, as `meta`"),
            ],
            ..op(
                "get",
                "/hosts",
                "Every host that reports, as Datadog's host list gives it",
                "observe.read",
                None,
                vec![(
                    "200",
                    answer_of(
                        "the hosts",
                        object(&[("host_list", array(schema("Host"))), ("total_matching", integer()), ("total_returned", integer())], &["host_list", "total_matching", "total_returned"]),
                    ),
                )],
            )
        },
        Op {
            params: vec![param("from", "query", false, integer(), "seconds since the epoch: hosts that reported since")],
            ..op(
                "get",
                "/hosts/totals",
                "The hosts up (a sample within two hours) and active (one), as Datadog's totals",
                "observe.read",
                None,
                vec![("200", answer_of("the totals", object(&[("total_up", integer()), ("total_active", integer())], &["total_up", "total_active"])))],
            )
        },
        Op { params: vec![name()], ..op("get", "/hosts/{name}", "A host whole: its platform, what the Agent says, its runner, its containers", "observe.read", None, vec![ok("HostDetail")]) },
        Op {
            params: vec![name()],
            ..op(
                "post",
                "/hosts/{name}/mute",
                "Mutes a host: a downtime of every monitor's groups of it (Datadog's mute)",
                "observe.write",
                Some(
                    json!({ "required": false, "content": { "application/json": { "schema": object(&[("end", describe(integer(), "seconds since the epoch; until unmuted when none")), ("message", string()), ("override", describe(boolean(), "replaces a mute it has"))], &[]) } } }),
                ),
                vec![("200", answer_of("muted", object(&[("action", string()), ("hostname", string()), ("end", nullable(integer())), ("message", string())], &["action", "hostname"])))],
            )
        },
        Op {
            params: vec![name()],
            ..op(
                "post",
                "/hosts/{name}/unmute",
                "Unmutes a host: its mute's downtime canceled",
                "observe.write",
                None,
                vec![("200", answer_of("unmuted", object(&[("action", string()), ("hostname", string())], &["action", "hostname"])))],
            )
        },
        // The Service Catalog (§10.8).
        op(
            "get",
            "/catalog",
            "Every service, defined or seen, with its facts and scorecards",
            "observe.read",
            None,
            vec![("200", answer_of("the services", object(&[("services", array(schema("CatalogService")))], &["services"])))],
        ),
        Op {
            params: vec![name()],
            ..op("get", "/catalog/{name}", "A service: its definition, facts, every scorecard rule, what reads it, its latest deployments", "observe.read", None, vec![ok("CatalogServiceDetail")])
        },
        op(
            "post",
            "/catalog/definitions",
            "Makes or changes service definitions (Datadog's v2.2, JSON or YAML, several `---` apart); one a repository defines is refused",
            "observe.write",
            Some(json!({ "required": true, "content": { "application/json": { "schema": schema("ServiceDefinition") }, "application/x-yaml": { "schema": { "type": "string" } } } })),
            vec![("200", answer_of("kept, as Datadog's API answers", object(&[("data", array(any()))], &["data"]))), ("409", answer("a repository defines it", Some("Error")))],
        ),
        Op {
            params: vec![name()],
            ..op(
                "delete",
                "/catalog/definitions/{name}",
                "Removes a service's definition (not one a repository makes)",
                "observe.write",
                None,
                vec![no_content(), ("409", answer("a repository defines it", Some("Error")))],
            )
        },
        // DORA metrics (§10.7).
        Op {
            params: vec![
                param("from", "query", false, integer(), "ms since the epoch (30 days before to)"),
                param("to", "query", false, integer(), "ms since the epoch (now)"),
                param("service", "query", false, string(), "one service"),
                param("env", "query", false, string(), "one environment"),
                param("team", "query", false, string(), "one team"),
            ],
            ..op(
                "get",
                "/dora/summary",
                "The four keys over a range (deployment frequency, change lead time, change failure rate, failed deployment recovery time) and week by week, beside DORA's 2024 levels",
                "observe.read",
                None,
                vec![ok("DoraSummary")],
            )
        },
        op(
            "post",
            "/dora/deployment",
            "Takes a deployment (Datadog's body): its commits read, what it marks failed marked",
            "observe.write",
            Some(body("DoraDeploymentIn")),
            vec![("200", answer_of("taken", object(&[("data", object(&[("id", string()), ("type", string())], &["id", "type"]))], &["data"])))],
        ),
        op(
            "post",
            "/dora/failure",
            "Takes a failure (an incident; Datadog's body), or its end",
            "observe.write",
            Some(body("DoraFailureIn")),
            vec![("200", answer_of("taken", object(&[("data", object(&[("id", string()), ("type", string())], &["id", "type"]))], &["data"])))],
        ),
        op(
            "post",
            "/dora/deployments",
            "Lists deployments: a range, a search of their fields, an order",
            "observe.read",
            Some(body("DoraList")),
            vec![("200", answer_of("the deployments", object(&[("data", array(schema("DoraDeployment")))], &["data"])))],
        ),
        op(
            "post",
            "/dora/failures",
            "Lists failures: a range, a search of their fields, an order",
            "observe.read",
            Some(body("DoraList")),
            vec![("200", answer_of("the failures", object(&[("data", array(schema("DoraFailure")))], &["data"])))],
        ),
        Op {
            params: vec![id()],
            ..op(
                "get",
                "/dora/deployments/{id}",
                "A deployment, its lead time, whether it failed and what remedied it",
                "observe.read",
                None,
                vec![("200", answer_of("it", object(&[("data", schema("DoraDeployment"))], &["data"])))],
            )
        },
        Op {
            params: vec![id()],
            ..op(
                "patch",
                "/dora/deployments/{id}",
                "Marks a deployment failed (or not), and what remedied it",
                "observe.write",
                Some(body("DoraPatch")),
                vec![("200", answer_of("as it is now", object(&[("data", schema("DoraDeployment"))], &["data"])))],
            )
        },
        Op { params: vec![id()], ..op("delete", "/dora/deployments/{id}", "Removes a deployment (its marks with it)", "observe.write", None, vec![no_content()]) },
        Op { params: vec![id()], ..op("get", "/dora/failures/{id}", "A failure", "observe.read", None, vec![("200", answer_of("it", object(&[("data", schema("DoraFailure"))], &["data"])))]) },
        Op { params: vec![id()], ..op("delete", "/dora/failures/{id}", "Removes a failure", "observe.write", None, vec![no_content()]) },
        op(
            "get",
            "/scanner/standard_patterns",
            "The standard patterns a rule may name",
            "observe.read",
            None,
            vec![("200", answer_of("the patterns", object(&[("standard_patterns", array(schema("StandardPattern")))], &["standard_patterns"])))],
        ),
        op(
            "post",
            "/scanner/test",
            "What a scanning group (or every one that runs, in order) makes of records given, hashing with this kiln's key: nothing kept",
            "observe.manage",
            Some(body("ScannerTest")),
            vec![(
                "200",
                answer_of(
                    "the records scanned",
                    object(
                        &[(
                            "records",
                            array(object(
                                &[
                                    ("ts_ns", integer()),
                                    ("status", string()),
                                    ("service", string()),
                                    ("host", string()),
                                    ("source", string()),
                                    ("message", string()),
                                    ("attributes", map_of(string())),
                                    ("trace_id", nullable(string())),
                                ],
                                &["message", "attributes"],
                            )),
                        )],
                        &["records"],
                    ),
                ),
            )],
        ),
        op("get", "/slos", "Lists SLOs, each its standing", "observe.read", None, vec![("200", answer_of("the SLOs", object(&[("slos", array(schema("Slo")))], &["slos"])))]),
        op("post", "/slos", "Makes an SLO", "observe.write", Some(body("SloIn")), vec![("201", answer("made", Some("SloKept")))]),
        Op { params: vec![id()], ..op("get", "/slos/{id}", "An SLO, its standing day by day, and the monitors that alert on it", "observe.read", None, vec![ok("Slo")]) },
        Op { params: vec![id()], ..op("put", "/slos/{id}", "Changes an SLO", "observe.write", Some(body("SloIn")), vec![ok("SloKept")]) },
        Op { params: vec![id()], ..op("delete", "/slos/{id}", "Deletes an SLO (refused while a monitor reads it: 409)", "observe.write", None, vec![no_content(), ("409", answer("monitors read it", Some("Error")))]) },
    ];
    // What is sent in from outside, at the root, each with the ingest token its own way.
    let raw = |what: &str| json!({ "required": true, "content": { "application/x-protobuf": { "schema": { "type": "string", "format": "binary" } }, "application/json": { "schema": any() } }, "description": what });
    v.extend([
        Op { body: Some(raw("an ExportMetricsServiceRequest, protobuf or OTLP's JSON, gzip or not")), ..op("post", "/v1/metrics", "OTLP/HTTP metrics (OTLP/gRPC's: opentelemetry.proto.collector.metrics.v1.MetricsService/Export, over HTTP/2, the token as metadata)", "ingest", None, vec![("200", answer("taken (OTLP's partial success, if any)", None))]) },
        Op { body: Some(raw("an ExportLogsServiceRequest, protobuf or OTLP's JSON, gzip or not")), ..op("post", "/v1/logs", "OTLP/HTTP logs (OTLP/gRPC's: opentelemetry.proto.collector.logs.v1.LogsService/Export)", "ingest", None, vec![("200", answer("taken", None))]) },
        Op {
            body: Some(raw("an ExportTraceServiceRequest, protobuf or OTLP's JSON, gzip or not")),
            ..op("post", "/v1/traces", "OTLP/HTTP traces (OTLP/gRPC's: opentelemetry.proto.collector.trace.v1.TraceService/Export)", "ingest", None, vec![("200", answer("taken (spans without a valid trace or span id rejected, said as the partial success)", None))])
        },
        Op {
            params: vec![param("X-Amz-Firehose-Access-Key", "header", true, string(), "observe's ingest token"), param("X-Amz-Firehose-Request-Id", "header", true, string(), "Firehose's request id, echoed")],
            body: Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("requestId", string()), ("timestamp", integer()), ("records", array(object(&[("data", describe(string(), "base64"))], &["data"])))], &["records"]) } }, "description": "CloudWatch Metric Streams' JSON, CloudWatch Logs' subscriptions; gzip or not" })),
            ..op("post", "/v1/firehose", "Kinesis Data Firehose's HTTP endpoint delivery (docs/observe.md §10.2)", "ingest", None, vec![("200", answer_of("taken (or taken before)", object(&[("requestId", string()), ("timestamp", integer()), ("errorMessage", string())], &["requestId", "timestamp"])))])
        },
        Op {
            params: vec![
                param("Content-Encoding", "header", true, json!({ "const": "snappy" }), "Snappy's block format"),
                param("Authorization", "header", true, string(), "Bearer <the ingest token>, or Basic with it as the password (a runner's local receiver asks none)"),
            ],
            body: Some(json!({ "required": true, "content": { "application/x-protobuf": { "schema": { "type": "string", "format": "binary" } }, "application/x-protobuf;proto=io.prometheus.write.v2.Request": { "schema": { "type": "string", "format": "binary" } } }, "description": "remote write 1.0's prometheus.WriteRequest, or 2.0's io.prometheus.write.v2.Request" })),
            ..op("post", "/api/v1/write", "Prometheus's remote write, 1.0 and 2.0 (docs/observe.md §8.5)", "ingest", None, vec![("204", answer("taken: 2.0's X-Prometheus-Remote-Write-Samples-Written says how many", None)), ("415", answer("a message of neither protocol", None))])
        },
        // Datadog's Agent (docs/observe.md §8.6), at Datadog's own paths, its API key the ingest token.
        Op { params: dd_key(), body: Some(dd_body("a MetricPayload (agent-payload), zstd, deflate or gzip")), ..op("post", "/api/v2/series", "Datadog's Agent's metrics", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), body: Some(dd_body("Datadog's v1 series, JSON")), ..op("post", "/api/v1/series", "Datadog's Agent's metrics, v1", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), body: Some(dd_body("a SketchPayload (agent-payload)")), ..op("post", "/api/beta/sketches", "Datadog's Agent's distributions' sketches", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), body: Some(dd_body("service checks' runs, JSON")), ..op("post", "/api/v1/check_run", "Datadog's Agent's service checks", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), body: Some(dd_body("events by source, and host metadata (passed over), JSON")), ..op("post", "/intake/", "Datadog's Agent's events", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), body: Some(dd_body("log records, JSON")), ..op("post", "/api/v2/logs", "Datadog's Agent's logs", "ingest", None, vec![("202", answer("taken", None))]) },
        Op { params: dd_key(), ..op("get", "/api/v1/validate", "Whether a Datadog API key is one", "ingest", None, vec![("200", answer_of("valid", object(&[("valid", boolean())], &["valid"]))), ("403", answer("not one", None))]) },
        // Datadog's service definition API (docs/observe.md §10.8).
        Op {
            params: dd_key(),
            body: Some(json!({ "required": true, "content": { "application/json": { "schema": schema("ServiceDefinition") }, "application/x-yaml": { "schema": { "type": "string" } } } })),
            ..op("post", "/api/v2/services/definitions", "Datadog's: makes or changes service definitions", "ingest", None, vec![("200", answer_of("kept", object(&[("data", array(any()))], &["data"])))])
        },
        Op { params: { let mut p = dd_key(); p.push(path_param("name", "the service's")); p }, ..op("delete", "/api/v2/services/definitions/{name}", "Datadog's: removes a service's definition", "ingest", None, vec![("204", answer("removed", None))]) },
        // Datadog's DORA API (docs/observe.md §10.7): what a CD system sends.
        Op { params: dd_key(), body: Some(body("DoraDeploymentIn")), ..op("post", "/api/v2/dora/deployment", "Datadog's DORA deployment event", "ingest", None, vec![("200", answer_of("taken", object(&[("data", object(&[("id", string()), ("type", string())], &["id", "type"]))], &["data"])))]) },
        Op { params: dd_key(), body: Some(body("DoraFailureIn")), ..op("post", "/api/v2/dora/failure", "Datadog's DORA failure (incident) event", "ingest", None, vec![("200", answer_of("taken", object(&[("data", object(&[("id", string()), ("type", string())], &["id", "type"]))], &["data"])))]) },
        Op { params: dd_key(), body: Some(body("DoraFailureIn")), ..op("post", "/api/v2/dora/incident", "Datadog's DORA incident event (its earlier name for a failure)", "ingest", None, vec![("200", answer("taken", None))]) },
        Op {
            params: { let mut p = dd_key(); p.push(path_param("id", "the deployment's")); p },
            body: Some(body("DoraPatch")),
            ..op("patch", "/api/v2/dora/deployments/{id}", "Datadog's: marks a deployment failed, and what remedied it", "ingest", None, vec![("200", answer_of("as it is now", object(&[("data", schema("DoraDeployment"))], &["data"])))])
        },
        Op { params: dd_key(), body: Some(body("DoraPatch")), ..op("patch", "/api/v2/dora/deployments", "Datadog's: marks the deployment of a service, environment and version failed", "ingest", None, vec![("200", answer_of("as it is now", object(&[("data", schema("DoraDeployment"))], &["data"])))]) },
        Op { params: { let mut p = dd_key(); p.push(path_param("id", "the deployment's")); p }, ..op("delete", "/api/v2/dora/deployment/{id}", "Datadog's: removes a deployment", "ingest", None, vec![("204", answer("removed", None))]) },
        Op { params: { let mut p = dd_key(); p.push(path_param("id", "the failure's")); p }, ..op("delete", "/api/v2/dora/failure/{id}", "Datadog's: removes a failure", "ingest", None, vec![("204", answer("removed", None))]) },
        Op {
            params: vec![param("token", "query", true, string(), "observe's ingest token")],
            body: Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("message", object(&[("data", describe(string(), "base64: a Cloud Logging entry")), ("messageId", string()), ("attributes", map_of(string()))], &["data"])), ("subscription", string())], &["message"]) } } })),
            ..op("post", "/v1/pubsub", "A Pub/Sub push subscription's message: a Cloud Logging sink's entry (docs/observe.md §10.2)", "ingest", None, vec![("204", answer("taken (or taken before), acknowledged", None))])
        },
    ]);
    v
}

/// Datadog's API key, as its Agent sends it.
fn dd_key() -> Vec<Value> {
    vec![param("DD-API-KEY", "header", false, string(), "observe's ingest token (or ?api_key=)"), param("Content-Encoding", "header", false, json!({ "enum": ["zstd", "deflate", "gzip"] }), "as the Agent compresses")]
}

fn dd_body(what: &str) -> Value {
    json!({ "required": true, "content": { "application/x-protobuf": { "schema": { "type": "string", "format": "binary" } }, "application/json": { "schema": any() } }, "description": what })
}

/// The root of the paths under `/api/observe/v1`.
pub const BASE: &str = "/api/observe/v1";

/// The document.
pub fn document(version: &str) -> Value {
    let mut paths = Map::new();
    for o in ops() {
        // What is sent in is at the root, where senders send it (`/v1/metrics`, `/api/v1/write`).
        let full = if o.capability == "ingest" { o.path.to_owned() } else { format!("{BASE}{}", o.path) };
        let mut responses: Map<String, Value> = o.answers.into_iter().map(|(c, a)| (c.to_owned(), a)).collect();
        errors(&mut responses);
        let security = if o.capability == "ingest" { json!([{ "ingest": [] }]) } else { json!([{ "bearer": [] }, { "session": [] }]) };
        let mut operation = json!({ "summary": o.summary, "x-kiln-capability": o.capability, "security": security, "responses": responses });
        if let Some(obj) = operation.as_object_mut() {
            if !o.params.is_empty() {
                obj.insert("parameters".into(), Value::Array(o.params));
            }
            if let Some(b) = o.body {
                obj.insert("requestBody".into(), b);
            }
        }
        if let Some(entry) = paths.entry(full).or_insert_with(|| json!({})).as_object_mut() {
            entry.insert(o.method.to_owned(), operation);
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "kiln observe",
            "version": version,
            "description": "Observe's API (docs/observe.md §7): metric queries, log search and analytics, events, monitors, dashboards, integrations, clouds, scrapes, synthetic tests, log sources, and what is sent in. Times are ms since the epoch unless said (`_ns`: nanoseconds).",
        },
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer", "description": "the admin token or a token whose role has the capability (x-kiln-capability)" },
                "session": { "type": "apiKey", "in": "cookie", "name": "kiln_session" },
                "ingest": { "type": "http", "scheme": "bearer", "description": "observe's ingest token (<data>/observe/ingest.token), or the admin's: Firehose sends it as X-Amz-Firehose-Access-Key, a Pub/Sub push in `?token=`" },
            },
            "schemas": schemas(),
        },
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The router's routes, read from its source: each `(&Method::X, ["a", b, ...])`, a quoted part
    /// itself and a bare one a parameter.
    fn routed() -> Vec<(String, String)> {
        let src = include_str!("api.rs");
        let start = src.find("pub async fn route(").unwrap_or(0);
        let end = src.get(start..).and_then(|s| s.find("\n}\n")).map_or(src.len(), |e| start + e);
        let body = src.get(start..end).unwrap_or("");
        let mut out = Vec::new();
        for part in body.split("(&Method::").skip(1) {
            let Some((method, rest)) = part.split_once(", [") else { continue };
            let Some((segs, _)) = rest.split_once(']') else { continue };
            let path: String = segs
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| if let Some(q) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) { q.to_owned() } else { format!("{{{}}}", s.trim_end_matches(" @ ..").trim_start_matches("ref ")) })
                .collect::<Vec<_>>()
                .join("/");
            out.push((method.to_ascii_lowercase(), format!("{BASE}/{path}")));
        }
        out
    }

    #[test]
    fn every_route_is_described_with_the_capability_the_gate_asks_and_nothing_else_is() -> anyhow::Result<()> {
        let doc = document("test");
        let paths = doc.get("paths").and_then(Value::as_object).ok_or_else(|| anyhow::anyhow!("no paths"))?;
        let routes = routed();
        anyhow::ensure!(routes.len() >= 40, "the router's routes read from its source: {routes:?}");
        for (method, path) in &routes {
            anyhow::ensure!(paths.get(path).and_then(|p| p.get(method)).is_some(), "{method} {path} is routed and not described");
        }
        for (path, item) in paths {
            for (method, op) in item.as_object().into_iter().flatten() {
                let cap = op.get("x-kiln-capability").and_then(Value::as_str).unwrap_or("");
                if cap == "ingest" {
                    anyhow::ensure!(["/v1/", "/api/v1/", "/api/v2/", "/api/beta/", "/intake/"].iter().any(|p| path.starts_with(p)) && !path.starts_with(BASE), "{path}");
                    continue;
                }
                anyhow::ensure!(routes.iter().any(|(m, p)| m == method && p == path), "{method} {path} is described and not routed");
                let segs: Vec<&str> = path.trim_start_matches('/').split('/').map(|s| if s.starts_with('{') { "x" } else { s }).collect();
                let m = http::Method::from_bytes(method.to_ascii_uppercase().as_bytes())?;
                let gate = crate::authz::rule(&m, &segs).capability;
                anyhow::ensure!(gate == cap, "{method} {path}: described as needing {cap}, the gate asks {gate}");
            }
        }
        Ok(())
    }

    #[test]
    fn every_schema_named_is_one_the_document_holds() -> anyhow::Result<()> {
        let doc = document("test");
        let text = doc.to_string();
        let held = doc.pointer("/components/schemas").and_then(Value::as_object).ok_or_else(|| anyhow::anyhow!("no schemas"))?;
        for named in text.split("#/components/schemas/").skip(1).map(|s| s.split('"').next().unwrap_or("")) {
            anyhow::ensure!(held.contains_key(named), "{named} is named and not held");
        }
        Ok(())
    }
}
