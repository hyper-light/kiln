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
    put("SearchRequest", object(&[("query", describe(string(), "a log search (Datadog's syntax; empty, every record)")), ("from", ms()), ("to", ms()), ("limit", describe(integer(), "records at the most (100; 1,000)"))], &["from", "to"]));
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
            &[("records", describe(array(schema("Record")), "newest first")), ("blocks", object(&[("total", integer()), ("pruned", integer())], &["total", "pruned"])), ("rows_scanned", integer()), ("took_ms", number())],
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
            ],
            &["rows_scanned", "took_ms"],
        ),
    );
    put("FacetsRequest", object(&[("query", string()), ("from", ms()), ("to", ms()), ("facets", array(string())), ("top", describe(integer(), "values a facet shows at the most (10)"))], &["from", "to", "facets"]));
    put(
        "FacetsAnswer",
        object(
            &[
                ("facets", array(object(&[("facet", string()), ("distinct", integer()), ("values", array(object(&[("value", string()), ("count", integer())], &["value", "count"])))], &["facet", "distinct", "values"]))),
                ("matched", integer()),
                ("rows_scanned", integer()),
                ("blocks_pruned", integer()),
                ("took_ms", number()),
            ],
            &["facets", "matched", "rows_scanned", "took_ms"],
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
    let thresholds = object(&[("critical", nullable(number())), ("warning", nullable(number())), ("critical_recovery", nullable(number())), ("warning_recovery", nullable(number()))], &[]);
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
        ("type", describe(string(), "`composite` for a composite (its query an expression of monitors' ids); else the query says")),
        ("query", describe(string(), "Datadog's monitor query: `avg(last_5m):avg:system.load.1{*} by {host} > 4`, `logs(\"status:error\").last(\"5m\") > 10`")),
        ("message", describe(string(), "Datadog's template variables and @-mentions of integrations")),
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
                ("kind", one_of(&["metric", "change", "logs", "forecast", "composite"])),
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
        ] }),
    );
    put(
        "Integration",
        describe(
            object(&[("name", string()), ("kind", one_of(&["slack", "discord", "teams", "pagerduty", "webhook"])), ("url", string()), ("routing_key", string()), ("signed", boolean())], &["name", "kind"]),
            "as shown: a webhook URL's host alone, a routing key \"…\", whether a webhook signs",
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
    // Downtimes (§9.3): Datadog's v2 downtime.
    let time_in = describe(any(), "ms since the epoch, or RFC 3339");
    let downtime_fields = [
        ("message", string()),
        ("monitor_identifier", describe(object(&[("monitor_id", string()), ("monitor_tags", array(string()))], &[]), "the monitors held: one, or those that carry every tag")),
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
    let widget = object(
        &[
            ("id", string()),
            ("title", string()),
            ("kind", one_of(&["timeseries", "query_value", "toplist", "table", "heatmap", "distribution", "logs", "events", "monitors", "note"])),
            ("queries", array(string())),
            ("search", string()),
            ("text", describe(string(), "a note's Markdown")),
            ("layout", object(&[("x", integer()), ("y", integer()), ("w", integer()), ("h", integer())], &["x", "y", "w", "h"])),
            ("options", any()),
        ],
        &["kind"],
    );
    let dashboard_fields = [
        ("title", string()),
        ("description", string()),
        ("template_variables", array(object(&[("name", string()), ("tag", string()), ("default", string())], &["name", "tag"]))),
        ("widgets", array(widget)),
    ];
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
            params: vec![param("key", "query", true, string(), "the tag's key"), param("match", "query", false, string(), "a glob of values"), param("limit", "query", false, integer(), "values at the most")],
            ..op("get", "/tags", "Lists a tag's values", "observe.read", None, vec![("200", answer_of("the values", object(&[("key", string()), ("values", array(string()))], &["key", "values"])))])
        },
        op("post", "/logs/search", "Searches log records, newest first", "observe.read", Some(body("SearchRequest")), vec![ok("SearchAnswer")]),
        op("post", "/logs/aggregate", "Log analytics: a measure by facets, in buckets of time with `step`", "observe.read", Some(body("AggregateRequest")), vec![ok("AggregateAnswer")]),
        op("post", "/logs/facets", "Each facet's most common values over a search", "observe.read", Some(body("FacetsRequest")), vec![ok("FacetsAnswer")]),
        op("post", "/logs/patterns", "A search's records grouped into patterns (Drain)", "observe.read", Some(body("SearchRequest")), vec![ok("PatternsAnswer")]),
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
            params: vec![param("X-Amz-Firehose-Access-Key", "header", true, string(), "observe's ingest token"), param("X-Amz-Firehose-Request-Id", "header", true, string(), "Firehose's request id, echoed")],
            body: Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("requestId", string()), ("timestamp", integer()), ("records", array(object(&[("data", describe(string(), "base64"))], &["data"])))], &["records"]) } }, "description": "CloudWatch Metric Streams' JSON, CloudWatch Logs' subscriptions; gzip or not" })),
            ..op("post", "/v1/firehose", "Kinesis Data Firehose's HTTP endpoint delivery (docs/observe.md §10.2)", "ingest", None, vec![("200", answer_of("taken (or taken before)", object(&[("requestId", string()), ("timestamp", integer()), ("errorMessage", string())], &["requestId", "timestamp"])))])
        },
        Op {
            params: vec![param("token", "query", true, string(), "observe's ingest token")],
            body: Some(json!({ "required": true, "content": { "application/json": { "schema": object(&[("message", object(&[("data", describe(string(), "base64: a Cloud Logging entry")), ("messageId", string()), ("attributes", map_of(string()))], &["data"])), ("subscription", string())], &["message"]) } } })),
            ..op("post", "/v1/pubsub", "A Pub/Sub push subscription's message: a Cloud Logging sink's entry (docs/observe.md §10.2)", "ingest", None, vec![("204", answer("taken (or taken before), acknowledged", None))])
        },
    ]);
    v
}

/// The root of the paths under `/api/observe/v1`.
pub const BASE: &str = "/api/observe/v1";

/// The document.
pub fn document(version: &str) -> Value {
    let mut paths = Map::new();
    for o in ops() {
        let full = if o.path.starts_with("/v1/") { o.path.to_owned() } else { format!("{BASE}{}", o.path) };
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
            "description": "Observe's API (docs/observe.md §7): metric queries, log search and analytics, events, monitors, dashboards, integrations, clouds, scrapes, log sources, and what is sent in. Times are ms since the epoch unless said (`_ns`: nanoseconds).",
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
                    anyhow::ensure!(path.starts_with("/v1/"), "{path}");
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
