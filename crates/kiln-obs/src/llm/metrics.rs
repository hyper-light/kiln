//! Datadog's LLM Observability metrics (`ml_obs.*`, `docs/observe.md` §8.12), as metrics from spans
//! (`crate::logs::metrics`): counted on the server from the LLM spans it holds (those
//! `crate::llm::spans` gave a kind, of any intake), each window of ten seconds whole once its spans
//! have come, as the trace metrics are (`crate::apm::trace_metrics`):
//!
//! - `ml_obs.span` (each span), `ml_obs.span.duration` (seconds, any percentile) and
//!   `ml_obs.span.error`, by `ml_app`, `model_name`, `model_provider`, `span_kind`, `env`,
//!   `service`, `version` and `status`;
//! - an LLM call's tokens' distributions (`ml_obs.span.llm.input.tokens`, `.output`, `.total`,
//!   `.input.cache_read`, `.input.cache_write`, `.input.non_cached`, `.output.reasoning`), an
//!   embedding's (`ml_obs.span.embedding.input.tokens`), by `ml_app`, `model_name`,
//!   `model_provider`, `env`, `service`, `version` and the catalog's model (`matched_model_name`,
//!   `matched_model_provider`);
//! - their cost's, nanodollars (`ml_obs.span.llm.total.cost` of both, `.input.cost` and its parts,
//!   `.output.cost` and its reasoning's, `ml_obs.span.embedding.input.cost`), by `ml_app`,
//!   `model_name`, `model_provider`, `env`, `service`, `version`, `matched_model_name` and where the
//!   cost came from (`source`: `auto`, `user`).
//!
//! A metric from logs is grouped by eight facets at the most (`LogMetric::check`): Datadog's
//! tokens' and cost's tags past them are left out (`span_kind`, which the metric's name says;
//! `error`; a cost's `matched_model_provider`, its model's).
//! - `ml_obs.trace`, `.duration` and `.error` of an LLM trace's root (a span of no parent in it:
//!   Datadog's tracer's says its LLM parent, `llmobs.parent_id`; any other's its own), by `ml_app`,
//!   `span_kind`, `env`, `service`, `version` and `status`. An OpenTelemetry LLM span under a span
//!   of no convention (a request's) is no root: its trace is counted by the spans it holds.
//!
//! Datadog's legacy names (`prompt`, `completion`) and its counts of characters are not made.

use crate::logs::metrics::{Compute, LogMetric};
use crate::logs::search::parse_spans;
use crate::logs::store::Facet;

/// The names kiln writes them under, and keeps for them.
pub const PREFIX: &str = "ml_obs.";
/// An LLM span: one given a kind.
pub const SPAN: &str = "@meta.span.kind:*";
/// An LLM trace's root.
pub const ROOT: &str = "@meta.span.kind:* ((@llmobs.trace_id:* -@llmobs.parent_id:*) OR (-@llmobs.trace_id:* -@parent_id:*))";

const BY_SPAN: [(&str, &str); 8] = [
    ("@ml_app", "ml_app"),
    ("@meta.model_name", "model_name"),
    ("@meta.model_provider", "model_provider"),
    ("@meta.span.kind", "span_kind"),
    ("@env", "env"),
    ("service", "service"),
    ("@version", "version"),
    ("status", "status"),
];
/// A call's tokens' (eight, a metric's most).
const BY_TOKENS: [(&str, &str); 8] = [
    ("@ml_app", "ml_app"),
    ("@meta.model_name", "model_name"),
    ("@meta.model_provider", "model_provider"),
    ("@env", "env"),
    ("service", "service"),
    ("@version", "version"),
    ("@matched_model_name", "matched_model_name"),
    ("@matched_model_provider", "matched_model_provider"),
];
/// A call's cost's.
const BY_COST: [(&str, &str); 8] = [
    ("@ml_app", "ml_app"),
    ("@meta.model_name", "model_name"),
    ("@meta.model_provider", "model_provider"),
    ("@env", "env"),
    ("service", "service"),
    ("@version", "version"),
    ("@matched_model_name", "matched_model_name"),
    ("@cost_source", "source"),
];
const BY_TRACE: [(&str, &str); 6] = [("@ml_app", "ml_app"), ("@meta.span.kind", "span_kind"), ("@env", "env"), ("service", "service"), ("@version", "version"), ("status", "status")];

fn def(name: &str, query: &str, by: &[(&str, &str)], compute: Compute) -> Option<LogMetric> {
    let search = parse_spans(query).ok()?;
    let by = by.iter().map(|(f, tag)| (Facet::parse_span(f), (*tag).to_owned())).collect();
    Some(LogMetric { name: name.to_owned(), search, by, compute, since: 0 })
}

fn dist(path: &str) -> Compute {
    Compute::Distribution { path: path.to_owned(), percentiles: true }
}

/// Datadog's LLM Observability metrics, as metrics from spans.
pub fn metrics() -> Vec<LogMetric> {
    let errors = |q: &str| format!("({q}) status:error");
    let (tokens, costs) = (&BY_TOKENS[..], &BY_COST[..]);
    let (llm, embedding, priced) = ("@meta.span.kind:llm", "@meta.span.kind:embedding", "@meta.span.kind:llm OR @meta.span.kind:embedding");
    let mut out = vec![
        def("ml_obs.span", SPAN, &BY_SPAN, Compute::Count),
        def("ml_obs.span.duration", SPAN, &BY_SPAN, Compute::Duration { percentiles: true }),
        def("ml_obs.span.error", &errors(SPAN), &BY_SPAN, Compute::Count),
        def("ml_obs.trace", ROOT, &BY_TRACE, Compute::Count),
        def("ml_obs.trace.duration", ROOT, &BY_TRACE, Compute::Duration { percentiles: true }),
        def("ml_obs.trace.error", &errors(ROOT), &BY_TRACE, Compute::Count),
        def("ml_obs.span.embedding.input.tokens", embedding, tokens, dist("@metrics.input_tokens")),
        def("ml_obs.span.embedding.input.cost", embedding, costs, dist("@metrics.estimated_input_cost")),
        def("ml_obs.span.llm.total.cost", priced, costs, dist("@metrics.estimated_total_cost")),
    ];
    for (name, path) in [
        ("ml_obs.span.llm.input.tokens", "@metrics.input_tokens"),
        ("ml_obs.span.llm.output.tokens", "@metrics.output_tokens"),
        ("ml_obs.span.llm.total.tokens", "@metrics.total_tokens"),
        ("ml_obs.span.llm.input.cache_read.tokens", "@metrics.cache_read_input_tokens"),
        ("ml_obs.span.llm.input.cache_write.tokens", "@metrics.cache_write_input_tokens"),
        ("ml_obs.span.llm.input.non_cached.tokens", "@metrics.non_cached_input_tokens"),
        ("ml_obs.span.llm.output.reasoning.tokens", "@metrics.reasoning_output_tokens"),
    ] {
        out.push(def(name, llm, tokens, dist(path)));
    }
    for (name, path) in [
        ("ml_obs.span.llm.input.cost", "@metrics.estimated_input_cost"),
        ("ml_obs.span.llm.output.cost", "@metrics.estimated_output_cost"),
        ("ml_obs.span.llm.input.cache_read.cost", "@metrics.estimated_cache_read_input_cost"),
        ("ml_obs.span.llm.input.cache_write.cost", "@metrics.estimated_cache_write_input_cost"),
        ("ml_obs.span.llm.input.non_cached.cost", "@metrics.estimated_non_cached_input_cost"),
        ("ml_obs.span.llm.output.reasoning.cost", "@metrics.estimated_reasoning_output_cost"),
    ] {
        out.push(def(name, llm, costs, dist(path)));
    }
    out.into_iter().flatten().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::logs::record::{Record, put_row};

    fn span(attrs: &[(&str, &str)], error: bool) -> Record {
        Record {
            ts: 1_700_000_000_000_000_000,
            severity: if error { 17 } else { 9 },
            service: "bot".into(),
            host: String::new(),
            source: "otlp".into(),
            message: "chat".into(),
            attrs: attrs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect(),
            trace_id: Some([1; 16]),
            span_id: Some([2; 8]),
        }
    }

    /// Every definition compiles, under kiln's names for them; counted from a store of spans, each
    /// counts what it says: a call's tokens and cost by its model, an LLM trace's root (Datadog's
    /// tracer's by its LLM parent, any other's by its own), nothing of a span no convention made one.
    #[test]
    fn llm_metrics_count_what_datadog_s_do() {
        let defs = metrics();
        assert_eq!(defs.len(), 22, "{:?}", defs.iter().map(|d| &d.name).collect::<Vec<_>>());
        for d in &defs {
            d.check().unwrap();
            assert!(d.name.starts_with(PREFIX));
        }
        let spans = [
            span(
                &[
                    ("duration", "2000000000"),
                    ("meta.span.kind", "llm"),
                    ("meta.model_name", "gpt-6"),
                    ("ml_app", "bot"),
                    ("metrics.input_tokens", "1200"),
                    ("metrics.estimated_total_cost", "129000"),
                    ("matched_model_name", "gpt-6"),
                    ("cost_source", "auto"),
                    ("parent_id", "0000000000000003"),
                ],
                true,
            ),
            span(&[("duration", "5000000000"), ("meta.span.kind", "workflow"), ("ml_app", "bot")], false),
            span(&[("duration", "1"), ("meta.span.kind", "agent"), ("ml_app", "bot"), ("llmobs.trace_id", "ab"), ("parent_id", "0000000000000009")], false),
            span(&[("duration", "1"), ("meta.span.kind", "tool"), ("ml_app", "bot"), ("llmobs.trace_id", "ab"), ("llmobs.parent_id", "0000000000000004")], false),
            span(&[("duration", "1"), ("http.route", "/")], false),
        ];
        let mut store = crate::logs::store::LogStore::spans();
        let (mut raw, mut prev) = (Vec::new(), 0);
        for s in &spans {
            put_row(&mut raw, s, prev);
            prev = s.ts;
        }
        store.append(raw, spans.len() as u32).unwrap();
        let window = crate::logs::metrics::window_of(1_700_000_000_000_000_000);
        let counted = crate::logs::metrics::count(&store.snapshot(), &defs, window).unwrap();
        let value = |metric: &str, tags: &[(&str, &str)]| -> f64 {
            counted
                .series
                .iter()
                .filter(|(k, _)| {
                    let key = crate::batch::parse_key(&k.key).unwrap();
                    key.metric == metric && tags.iter().all(|t| key.tags.contains(t))
                })
                .map(|(_, p)| p.iter().map(|x| x.1).sum::<f64>())
                .sum()
        };
        assert_eq!(value("ml_obs.span", &[("ml_app", "bot")]), 4.0, "the LLM spans, not the request's");
        assert_eq!(value("ml_obs.span.error", &[("span_kind", "llm"), ("model_name", "gpt-6")]), 1.0);
        assert_eq!(value("ml_obs.span.llm.input.tokens.sum", &[("model_name", "gpt-6"), ("matched_model_name", "gpt-6")]), 1200.0);
        assert_eq!(value("ml_obs.span.llm.total.cost.sum", &[("source", "auto")]), 129_000.0);
        assert_eq!(value("ml_obs.trace", &[]), 2.0, "the workflow (no parent) and the agent (no LLM parent): not the call or the tool");
        assert_eq!(value("ml_obs.trace.duration.sum", &[("span_kind", "workflow")]), 5.0);
    }
}
