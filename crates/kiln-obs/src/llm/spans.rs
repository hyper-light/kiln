//! LLM spans recognized (`docs/observe.md` §8.12): a span's attributes read in each convention that
//! names a call to a model, an agent's or a tool's step, into what Datadog's LLM Observability makes
//! of a span, and put among its attributes under Datadog's names, where it says none of the name:
//!
//! - **The conventions**, each a span may say, read in one pass over its attributes, the first that
//!   says a thing taken: Datadog's own (`meta.span.kind`, `metrics.*`: an LLM Observability event's,
//!   as kiln's intake writes it); OpenTelemetry's GenAI conventions of every version since 1.26
//!   (`gen_ai.system` and `prompt_tokens` before 1.37 and 1.27, the cache write's name of 1.40–1.41
//!   and of the unreleased main); Arize's OpenInference (`openinference.span.kind`, `llm.*`);
//!   Traceloop's OpenLLMetry, its current names and its legacy `llm.usage.*`; Langfuse's
//!   (`langfuse.observation.type`); Claude Code's own spans (`claude_code.*`). Any other span is
//!   passed over by its keys alone ([`is_llm`]), nothing read.
//! - **Its kind**, as Datadog maps each convention's: an operation of chat, text_completion,
//!   generate_content or completion an `llm`, embeddings an `embedding`, execute_tool a `tool`,
//!   invoke_agent and create_agent an `agent`, retrieval a `retrieval`, anything else a `workflow`;
//!   a span of no operation that names a model or its tokens (OpenTelemetry's before operations) an
//!   `llm`. Claude Code's request an `llm`, its tool a `tool`, its interaction an `agent`, the rest
//!   (a tool's execution, its wait on the user, a hook) a `task`.
//! - **Its tokens** as Datadog counts them: the input every input token, cache reads and writes in
//!   it (OpenTelemetry's, Datadog's and OpenInference's count them in; Claude Code's and Anthropic's
//!   API leave them out, and they are put back; an input under its own cache reads and writes says it
//!   left them out); the output with its reasoning in it; the total, said or the two together.
//! - **Its cost**: what it says it cost (USD: Datadog's `metrics.*_cost`, OpenInference's
//!   `llm.cost.*`, OpenLIT's `gen_ai.usage.cost`, Claude Code's `cost_usd`), else its tokens priced
//!   by the catalog ([`crate::llm::prices`]) at the time it ran, its provider named as the catalog
//!   names it (OpenTelemetry's `x_ai` its `x-ai`, `azure.ai.openai` its `azure`, `gcp.gen_ai` its
//!   `google`), else found by the address it called, else by its model's name; kept as Datadog keeps
//!   them, in nanodollars (`metrics.estimated_*_cost`), the parts it said its own and the others the
//!   catalog's, with the catalog's model (`matched_model_name`, `matched_model_provider`), where it
//!   came from (`cost_source`: `user`, `auto`), or why it has none (`cost_unavailable`).
//! - **Not twice**: a span Datadog's tracer marks as its copy of an LLM span's tags
//!   (`_dd.llmobs.artificial_gen_ai_tags`: the span's LLM event goes on apart, and is read) is passed
//!   over, unless it carries that event itself (its `meta_struct`'s, read into Datadog's own names:
//!   `meta.span.kind` its kind).
//! - **A provider not named** (Datadog's tracer's `unknown` for a client of another address,
//!   Datadog's `custom`) is priced as none named: by the model's name.

use crate::llm::prices::{self, CACHE_READ_TOKENS, CACHE_WRITE_1H_TOKENS, CACHE_WRITE_5M_TOKENS, CACHE_WRITE_TOKENS, Call, INPUT_TOKENS, OUTPUT_REASONING_TOKENS, OUTPUT_TOKENS, Unpriced, Usage};
use crate::otlp::metrics::push_f64;
use crate::otlp::traces::{Attr, Val, resolve};

/// Datadog's LLM Observability's span kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Llm,
    Embedding,
    Retrieval,
    Tool,
    Agent,
    Workflow,
    Task,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Embedding => "embedding",
            Self::Retrieval => "retrieval",
            Self::Tool => "tool",
            Self::Agent => "agent",
            Self::Workflow => "workflow",
            Self::Task => "task",
        }
    }

    /// A kind by its name (Datadog's own spans, its intake's).
    pub fn of(name: &str) -> Option<Self> {
        Some(match name {
            "llm" => Self::Llm,
            "embedding" => Self::Embedding,
            "retrieval" => Self::Retrieval,
            "tool" => Self::Tool,
            "agent" => Self::Agent,
            "workflow" => Self::Workflow,
            "task" => Self::Task,
            _ => return None,
        })
    }

    /// Whether its calls are priced (a model's: an LLM's or an embedding's).
    pub fn priced(self) -> bool {
        matches!(self, Self::Llm | Self::Embedding)
    }
}

/// A value read: an attribute's (its handle), or a name kiln reads it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said<V> {
    At(V),
    Own(&'static str),
}

/// Tokens of a call, as Datadog counts them: `input` every input token (cache reads and writes in
/// it), the rest parts of it or of the output.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tokens {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub total: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub reasoning: Option<f64>,
}

impl Tokens {
    /// Its input not from a cache: its input less what was read from and written to one.
    pub fn non_cached(&self) -> Option<f64> {
        let input = self.input?;
        Some((input - self.cache_read.unwrap_or(0.0) - self.cache_write.unwrap_or(0.0)).max(0.0))
    }

    /// Its total: said, else its input and output.
    pub fn total(&self) -> Option<f64> {
        self.total.or_else(|| match (self.input, self.output) {
            (None, None) => None,
            (i, o) => Some(i.unwrap_or(0.0) + o.unwrap_or(0.0)),
        })
    }

    /// As the catalog prices it.
    pub fn usage(&self) -> Usage {
        let mut u = Usage::default();
        for (unit, v) in [
            (INPUT_TOKENS, self.input),
            (OUTPUT_TOKENS, self.output),
            (CACHE_READ_TOKENS, self.cache_read),
            (CACHE_WRITE_TOKENS, self.cache_write),
            (CACHE_WRITE_5M_TOKENS, self.cache_write_5m),
            (CACHE_WRITE_1H_TOKENS, self.cache_write_1h),
            (OUTPUT_REASONING_TOKENS, self.reasoning),
        ] {
            if let Some(v) = v {
                u.set(unit, v);
            }
        }
        u
    }
}

/// A cost, USD: as Datadog parts it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub total: Option<f64>,
    pub non_cached_input: Option<f64>,
    pub cache_read_input: Option<f64>,
    pub cache_write_input: Option<f64>,
    pub reasoning_output: Option<f64>,
}

impl Cost {
    fn any(&self) -> bool {
        [self.input, self.output, self.total, self.non_cached_input, self.cache_read_input, self.cache_write_input, self.reasoning_output].iter().any(Option::is_some)
    }
}

/// An LLM span, read: its values' handles (`V`) and what was made of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Read<V> {
    pub kind: Kind,
    pub provider: Option<Said<V>>,
    pub model: Option<V>,
    pub session: Option<V>,
    pub app: Option<V>,
    /// The address it called (`server.address`): its provider where it names none.
    pub address: Option<V>,
    /// Its service tier (OpenAI's), a price's context.
    pub service_tier: Option<V>,
    pub tokens: Tokens,
    /// What it says it cost.
    pub cost: Cost,
}

/// Whether a span of name `name` and attribute keys `keys` is one a convention makes an LLM span.
pub fn is_llm<'k>(name: &str, keys: impl IntoIterator<Item = &'k str>) -> bool {
    name.starts_with("claude_code.")
        || keys
            .into_iter()
            .any(|k| k.starts_with("gen_ai.") || k.starts_with("llm.") || matches!(k, "meta.span.kind" | "openinference.span.kind" | "traceloop.span.kind" | "langfuse.observation.type"))
}

/// What a group of keys says, each group's keys in the order one is taken over another.
const KIND: usize = 0;
const PROVIDER: usize = 1;
const MODEL: usize = 2;
const SESSION: usize = 3;
const APP: usize = 4;
const ADDRESS: usize = 5;
const TIER: usize = 6;
const INPUT: usize = 7;
const OUTPUT: usize = 8;
const TOTAL: usize = 9;
const NON_CACHED: usize = 10;
const CACHE_READ: usize = 11;
const CACHE_WRITE: usize = 12;
const WRITE_5M: usize = 13;
const WRITE_1H: usize = 14;
const REASONING: usize = 15;
const COST_INPUT: usize = 16;
const COST_OUTPUT: usize = 17;
const COST_TOTAL: usize = 18;
const COST_NON_CACHED: usize = 19;
const COST_READ: usize = 20;
const COST_WRITE: usize = 21;
const COST_REASONING: usize = 22;
const ARTIFICIAL: usize = 23;
const GROUPS: usize = 24;
/// The most keys a group has.
const WIDE: usize = 8;

/// A key's group and its place in it; Claude Code's own (`claude`: a span of its) only on its spans.
fn place(k: &str, claude: bool) -> Option<(usize, usize)> {
    Some(match k {
        "meta.span.kind" => (KIND, 0),
        "gen_ai.operation.name" => (KIND, 1),
        "openinference.span.kind" => (KIND, 2),
        "llm.request.type" => (KIND, 3),
        "traceloop.span.kind" => (KIND, 4),
        "langfuse.observation.type" => (KIND, 5),
        "meta.model_provider" => (PROVIDER, 0),
        "gen_ai.provider.name" => (PROVIDER, 1),
        "gen_ai.system" => (PROVIDER, 2),
        "llm.provider" => (PROVIDER, 3),
        "llm.system" => (PROVIDER, 4),
        "meta.model_name" => (MODEL, 0),
        "gen_ai.response.model" => (MODEL, 1),
        "llm.model_name" => (MODEL, 2),
        "llm.response.model_name" => (MODEL, 3),
        "gen_ai.request.model" => (MODEL, 4),
        "llm.request.model_name" => (MODEL, 5),
        "embedding.model_name" => (MODEL, 6),
        "model" if claude => (MODEL, 7),
        "session_id" => (SESSION, 0),
        "gen_ai.conversation.id" => (SESSION, 1),
        "session.id" => (SESSION, 2),
        "traceloop.association.properties.session_id" => (SESSION, 3),
        "ml_app" => (APP, 0),
        "server.address" => (ADDRESS, 0),
        "openai.response.service_tier" => (TIER, 0),
        "gen_ai.openai.response.service_tier" => (TIER, 1),
        "openai.request.service_tier" => (TIER, 2),
        "gen_ai.openai.request.service_tier" => (TIER, 3),
        "metrics.input_tokens" => (INPUT, 0),
        "gen_ai.usage.input_tokens" => (INPUT, 1),
        "gen_ai.usage.prompt_tokens" => (INPUT, 2),
        "llm.token_count.prompt" => (INPUT, 3),
        "llm.usage.prompt_tokens" => (INPUT, 4),
        "metrics.prompt_tokens" => (INPUT, 5),
        "input_tokens" if claude => (INPUT, 6),
        "metrics.output_tokens" => (OUTPUT, 0),
        "gen_ai.usage.output_tokens" => (OUTPUT, 1),
        "gen_ai.usage.completion_tokens" => (OUTPUT, 2),
        "llm.token_count.completion" => (OUTPUT, 3),
        "llm.usage.completion_tokens" => (OUTPUT, 4),
        "metrics.completion_tokens" => (OUTPUT, 5),
        "output_tokens" if claude => (OUTPUT, 6),
        "metrics.total_tokens" => (TOTAL, 0),
        "gen_ai.usage.total_tokens" => (TOTAL, 1),
        "llm.token_count.total" => (TOTAL, 2),
        "llm.usage.total_tokens" => (TOTAL, 3),
        "metrics.non_cached_input_tokens" => (NON_CACHED, 0),
        "metrics.cache_read_input_tokens" => (CACHE_READ, 0),
        "gen_ai.usage.cache_read.input_tokens" => (CACHE_READ, 1),
        "gen_ai.usage.cache_read_input_tokens" => (CACHE_READ, 2),
        "llm.token_count.prompt_details.cache_read" => (CACHE_READ, 3),
        "cache_read_tokens" if claude => (CACHE_READ, 4),
        "metrics.cache_write_input_tokens" => (CACHE_WRITE, 0),
        "gen_ai.usage.cache_write.input_tokens" => (CACHE_WRITE, 1),
        "gen_ai.usage.cache_creation.input_tokens" => (CACHE_WRITE, 2),
        "gen_ai.usage.cache_creation_input_tokens" => (CACHE_WRITE, 3),
        "llm.token_count.prompt_details.cache_write" => (CACHE_WRITE, 4),
        "cache_creation_tokens" if claude => (CACHE_WRITE, 5),
        "metrics.ephemeral_5m_input_tokens" => (WRITE_5M, 0),
        "metrics.ephemeral_1h_input_tokens" => (WRITE_1H, 0),
        "metrics.reasoning_output_tokens" => (REASONING, 0),
        "gen_ai.usage.reasoning.output_tokens" => (REASONING, 1),
        "gen_ai.usage.reasoning_tokens" => (REASONING, 2),
        "llm.token_count.completion_details.reasoning" => (REASONING, 3),
        "llm.usage.reasoning_tokens" => (REASONING, 4),
        "metrics.input_cost" => (COST_INPUT, 0),
        "llm.cost.prompt" => (COST_INPUT, 1),
        "metrics.output_cost" => (COST_OUTPUT, 0),
        "llm.cost.completion" => (COST_OUTPUT, 1),
        "metrics.total_cost" => (COST_TOTAL, 0),
        "llm.cost.total" => (COST_TOTAL, 1),
        "gen_ai.usage.cost" => (COST_TOTAL, 2),
        "cost_usd" if claude => (COST_TOTAL, 3),
        "metrics.non_cached_input_cost" => (COST_NON_CACHED, 0),
        "llm.cost.prompt_details.input" => (COST_NON_CACHED, 1),
        "metrics.cache_read_input_cost" => (COST_READ, 0),
        "llm.cost.prompt_details.cache_read" => (COST_READ, 1),
        "metrics.cache_write_input_cost" => (COST_WRITE, 0),
        "llm.cost.prompt_details.cache_write" => (COST_WRITE, 1),
        "metrics.reasoning_output_cost" => (COST_REASONING, 0),
        "llm.cost.completion_details.reasoning" => (COST_REASONING, 1),
        "_dd.llmobs.artificial_gen_ai_tags" => (ARTIFICIAL, 0),
        _ => return None,
    })
}

/// Provider names as the latest conventions say them: an older name read as its newer (the
/// conventions' renames: `xai` 1.37, `az.ai.*` 1.35, `vertex_ai` and `gemini` 1.33, `mistral_ai`
/// main's).
fn newer_provider(p: &str) -> Option<&'static str> {
    const RENAMED: [(&str, &str); 6] =
        [("xai", "x_ai"), ("az.ai.inference", "azure.ai.inference"), ("az.ai.openai", "azure.ai.openai"), ("vertex_ai", "gcp.vertex_ai"), ("gemini", "gcp.gemini"), ("mistral_ai", "mistral")];
    RENAMED.iter().find(|(old, _)| old.eq_ignore_ascii_case(p)).map(|(_, new)| *new)
}

/// A provider as the catalog names it, where its own rules would not find it (OpenTelemetry's
/// names the catalog's matching misses).
pub fn catalog_provider(p: &str) -> &str {
    const NAMED: [(&str, &str); 7] =
        [("x_ai", "x-ai"), ("xai", "x-ai"), ("azure.ai.openai", "azure"), ("azure.ai.inference", "azure"), ("az.ai.openai", "azure"), ("az.ai.inference", "azure"), ("gcp.gen_ai", "google")];
    NAMED.iter().find(|(ours, _)| ours.eq_ignore_ascii_case(p)).map_or(p, |(_, its)| its)
}

/// The span of name `name` and attributes `attrs` (each key and its value's handle, `value` its
/// text), read; none where it is no LLM span, or Datadog's tracer's copy of one.
pub fn read<'t, V: Copy>(name: &str, attrs: impl IntoIterator<Item = (&'t str, V)>, value: impl Fn(V) -> &'t str) -> Option<Read<V>> {
    let claude = name.strip_prefix("claude_code.");
    let mut found = [[None::<V>; WIDE]; GROUPS];
    let mut llm = claude.is_some();
    for (k, v) in attrs {
        llm |= k.starts_with("gen_ai.") || k.starts_with("llm.");
        if let Some((g, i)) = place(k, claude.is_some()) {
            llm = true;
            if let Some(slot) = found.get_mut(g).and_then(|g| g.get_mut(i)) {
                slot.get_or_insert(v);
            }
        }
    }
    let first = |g: usize| found.get(g).and_then(|g| g.iter().enumerate().find_map(|(i, v)| v.map(|v| (i, v))));
    // Datadog's tracer's copy of an LLM span's tags, but where the span carries its event itself.
    let own_kind = found.get(KIND).and_then(|g| g.first()).is_some_and(Option::is_some);
    if !llm || (!own_kind && first(ARTIFICIAL).is_some_and(|(_, v)| value(v) == "true")) {
        return None;
    }
    let num = |g: usize| found.get(g).and_then(|g| g.iter().flatten().find_map(|v| value(*v).trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0)));
    let text = |g: usize| first(g).map(|(i, v)| (i, value(v).trim()));

    // Its kind.
    let kind = match text(KIND) {
        Some((0, k)) => Kind::of(k).unwrap_or(Kind::Workflow),
        Some((1, op)) => match op {
            "chat" | "text_completion" | "generate_content" | "completion" => Kind::Llm,
            "embeddings" | "embedding" => Kind::Embedding,
            "execute_tool" => Kind::Tool,
            "invoke_agent" | "create_agent" => Kind::Agent,
            "retrieval" => Kind::Retrieval,
            _ => Kind::Workflow,
        },
        Some((2, k)) => match k.to_ascii_uppercase().as_str() {
            "LLM" => Kind::Llm,
            "EMBEDDING" => Kind::Embedding,
            "TOOL" => Kind::Tool,
            "AGENT" => Kind::Agent,
            "RETRIEVER" => Kind::Retrieval,
            _ => Kind::Workflow,
        },
        Some((3, t)) => match t {
            "chat" | "completion" => Kind::Llm,
            "embedding" => Kind::Embedding,
            _ => Kind::Workflow,
        },
        Some((4, k)) => match k {
            "agent" => Kind::Agent,
            "tool" => Kind::Tool,
            "task" => Kind::Task,
            _ => Kind::Workflow,
        },
        Some((_, k)) => match k {
            "generation" => Kind::Llm,
            "embedding" => Kind::Embedding,
            "tool" => Kind::Tool,
            "agent" => Kind::Agent,
            "retriever" => Kind::Retrieval,
            _ => Kind::Workflow,
        },
        None => match claude {
            Some("llm_request") => Kind::Llm,
            Some("tool") => Kind::Tool,
            Some("interaction") => Kind::Agent,
            Some(_) => Kind::Task,
            // A call of no operation said (OpenTelemetry's before 1.27): a model's, where it names one.
            None if first(MODEL).is_some() || first(INPUT).is_some() || first(OUTPUT).is_some() => Kind::Llm,
            None => Kind::Workflow,
        },
    };

    // Its provider: the newer name where it said an older; Claude Code's Anthropic's.
    let provider = match first(PROVIDER) {
        Some((_, v)) => Some(newer_provider(value(v).trim()).map_or(Said::At(v), Said::Own)),
        None => claude.map(|_| Said::Own("anthropic")),
    };

    // Its tokens.
    let mut t = Tokens {
        input: num(INPUT),
        output: num(OUTPUT),
        total: num(TOTAL),
        cache_read: num(CACHE_READ),
        cache_write: num(CACHE_WRITE),
        cache_write_5m: num(WRITE_5M),
        cache_write_1h: num(WRITE_1H),
        reasoning: num(REASONING),
    };
    let cached = t.cache_read.unwrap_or(0.0) + t.cache_write.unwrap_or(0.0);
    match (t.input, num(NON_CACHED)) {
        // Claude Code's input leaves out what was read from and written to the cache: put back.
        (Some(i), _) if claude.is_some() => t.input = Some(i + cached),
        // An input under its own cache reads and writes left them out (Anthropic's, passed on).
        (Some(i), _) if cached > i => t.input = Some(i + cached),
        // Datadog's parts, its input not said: their sum.
        (None, Some(n)) => t.input = Some(n + cached),
        _ => {}
    }
    let cost = Cost {
        input: num(COST_INPUT),
        output: num(COST_OUTPUT),
        total: num(COST_TOTAL),
        non_cached_input: num(COST_NON_CACHED),
        cache_read_input: num(COST_READ),
        cache_write_input: num(COST_WRITE),
        reasoning_output: num(COST_REASONING),
    };
    let at = |g: usize| first(g).map(|(_, v)| v);
    Some(Read { kind, provider, model: at(MODEL), session: at(SESSION), app: at(APP), address: at(ADDRESS), service_tier: at(TIER), tokens: t, cost })
}

/// kiln's LLM attributes on a span at the most ([`put`]).
pub const OWN: usize = 24;

/// A decoder's span of name `name`, read from its own attributes (`own`) and those it has under them
/// (its resource's), whole, before they are bounded: none where its own and its name make it no LLM
/// span ([`is_llm`]: a span of no other convention costs a look at its keys alone).
pub(crate) fn read_attrs<'a>(name: &str, own: &[Attr<'a>], under: &[Attr<'a>], text: &str) -> Option<Read<Val<'a>>> {
    if !is_llm(name, own.iter().map(|a| a.key)) {
        return None;
    }
    read(name, own.iter().chain(under).map(|a| (a.key, a.val)), |v| resolve(v, text))
}

/// An LLM span's fields (`r`) put among its attributes (`attrs`, their text `text`; `service` its
/// service: Datadog's `ml_app`; `at_ns` its start), each where it says none of the name; its tokens
/// priced where it says no cost, a model's call. [`OWN`] at the most.
pub(crate) fn put<'a>(attrs: &mut Vec<Attr<'a>>, text: &mut String, r: &Read<Val<'a>>, service: Option<Val<'a>>, at_ns: i64) {
    let auto = r.kind.priced().then(|| {
        let said = |s: Option<Said<Val<'a>>>| match s {
            Some(Said::At(v)) => resolve(v, text),
            Some(Said::Own(p)) => p,
            None => "",
        };
        let provider = said(r.provider);
        let model = r.model.map_or("", |v| resolve(v, text));
        let named = !(provider.is_empty() || provider.eq_ignore_ascii_case("unknown") || provider.eq_ignore_ascii_case("custom"));
        // The address called, where it names no provider (the catalog's patterns are URLs').
        let address = if named { None } else { r.address.map(|v| format!("https://{}", resolve(v, text))) };
        let tier = r.service_tier.map(|v| resolve(v, text)).filter(|t| !t.is_empty() && *t != "auto");
        let context: &[(&str, &str)] = &[("service_tier", tier.unwrap_or_default())];
        let call = Call { provider: named.then(|| catalog_provider(provider)), api_url: address.as_deref(), model, at_ns, context: if tier.is_some() { context } else { &[] } };
        if model.is_empty() {
            return Err(Unpriced::Model);
        }
        if r.tokens.input.is_none() && r.tokens.output.is_none() {
            return Err(Unpriced::Usage);
        }
        prices::catalog().price(&call, &r.tokens.usage())
    });
    let has = |attrs: &[Attr<'a>], k: &str| attrs.iter().any(|a| a.key == k);
    let put_val = |attrs: &mut Vec<Attr<'a>>, key: &'static str, val: Val<'a>| {
        if !has(attrs, key) {
            attrs.push(Attr { key, val });
        }
    };
    put_val(attrs, "meta.span.kind", Val::Str(r.kind.name()));
    match r.provider {
        Some(Said::At(v)) => put_val(attrs, "meta.model_provider", v),
        Some(Said::Own(p)) => put_val(attrs, "meta.model_provider", Val::Str(p)),
        // Datadog's, of a model's call that names no provider.
        None if r.kind.priced() => put_val(attrs, "meta.model_provider", Val::Str("custom")),
        None => {}
    }
    if let Some(v) = r.model {
        put_val(attrs, "meta.model_name", v);
    }
    if let Some(v) = r.app.or(service) {
        put_val(attrs, "ml_app", v);
    }
    if let Some(v) = r.session {
        put_val(attrs, "session_id", v);
    }
    let put_num = |attrs: &mut Vec<Attr<'a>>, text: &mut String, key: &'static str, v: Option<f64>| {
        if let Some(v) = v
            && !has(attrs, key)
        {
            let at = text.len();
            push_f64(text, v);
            attrs.push(Attr { key, val: Val::Text(at, text.len()) });
        }
    };
    let t = r.tokens;
    put_num(attrs, text, "metrics.input_tokens", t.input);
    put_num(attrs, text, "metrics.output_tokens", t.output);
    put_num(attrs, text, "metrics.total_tokens", t.total());
    put_num(attrs, text, "metrics.cache_read_input_tokens", t.cache_read);
    put_num(attrs, text, "metrics.cache_write_input_tokens", t.cache_write);
    put_num(attrs, text, "metrics.non_cached_input_tokens", t.non_cached());
    put_num(attrs, text, "metrics.reasoning_output_tokens", t.reasoning);
    // Its cost, nanodollars: each part said, else the catalog's.
    let user = r.cost;
    let priced = auto.as_ref().and_then(|a| a.as_ref().ok());
    if user.any() || priced.is_some() {
        let p = |f: fn(&prices::Priced<'_>) -> f64| priced.map(f);
        let input = user.input.or(p(|a| a.input));
        let output = user.output.or(p(|a| a.output));
        let total = user.total.or_else(|| if user.input.is_some() || user.output.is_some() { Some(input.unwrap_or(0.0) + output.unwrap_or(0.0)) } else { p(|a| a.total) });
        let nano = |v: Option<f64>| v.map(|usd| (usd * 1e9).round());
        put_num(attrs, text, "metrics.estimated_input_cost", nano(input));
        put_num(attrs, text, "metrics.estimated_output_cost", nano(output));
        put_num(attrs, text, "metrics.estimated_total_cost", nano(total));
        put_num(attrs, text, "metrics.estimated_non_cached_input_cost", nano(user.non_cached_input.or(p(|a| a.non_cached_input))));
        put_num(attrs, text, "metrics.estimated_cache_read_input_cost", nano(user.cache_read_input.or(p(|a| a.cache_read_input))));
        put_num(attrs, text, "metrics.estimated_cache_write_input_cost", nano(user.cache_write_input.or(p(|a| a.cache_write_input))));
        put_num(attrs, text, "metrics.estimated_reasoning_output_cost", nano(user.reasoning_output.or(p(|a| a.reasoning_output))));
        put_val(attrs, "cost_source", Val::Str(if user.any() { "user" } else { "auto" }));
    }
    if let Some(a) = priced {
        put_val(attrs, "matched_model_name", Val::Str(a.model));
        put_val(attrs, "matched_model_provider", Val::Str(a.provider));
    } else if let Some(Err(why)) = auto.as_ref()
        && !user.any()
    {
        // Datadog's reasons a cost is unavailable.
        let why = match why {
            Unpriced::Provider => "unsupported model provider",
            Unpriced::Model => "unsupported model name",
            Unpriced::Usage => "missing token counts",
        };
        put_val(attrs, "cost_unavailable", Val::Str(why));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::float_cmp)]
mod tests {
    use super::*;

    /// A span's attributes read: what was read, and its provider, model and session as text.
    fn read_of(name: &str, attrs: &[(&str, &str)]) -> Option<(Read<usize>, [String; 3])> {
        let r = read(name, attrs.iter().enumerate().map(|(i, (k, _))| (*k, i)), |i| attrs[i].1)?;
        let said = |s: Option<Said<usize>>| match s {
            Some(Said::At(i)) => attrs[i].1.to_owned(),
            Some(Said::Own(p)) => p.to_owned(),
            None => String::new(),
        };
        let shown = [said(r.provider), said(r.model.map(Said::At)), said(r.session.map(Said::At))];
        Some((r, shown))
    }

    /// A span's attributes after [`put`]: each key and its text.
    fn put_of(name: &str, attrs: &[(&'static str, &'static str)], service: &'static str, at_ns: i64) -> Option<Vec<(String, String)>> {
        let mut text = String::new();
        let mut v: Vec<Attr<'static>> = attrs.iter().map(|&(key, val)| Attr { key, val: Val::Str(val) }).collect();
        let r = read_attrs(name, &v, &[], &text)?;
        put(&mut v, &mut text, &r, Some(Val::Str(service)), at_ns);
        Some(v.iter().map(|a| (a.key.to_owned(), resolve(a.val, &text).to_owned())).collect())
    }

    fn get<'v>(attrs: &'v [(String, String)], key: &str) -> Option<&'v str> {
        attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// OpenTelemetry's conventions, the latest's names and the older ones', each read alike: a chat
    /// its model's; its tokens, cache reads and writes in its input; the older provider's name read
    /// as its newer.
    #[test]
    fn opentelemetry_s_spans_of_every_version_are_read() {
        let (r, s) = read_of(
            "chat claude-opus-5-5",
            &[
                ("gen_ai.operation.name", "chat"),
                ("gen_ai.provider.name", "anthropic"),
                ("gen_ai.request.model", "claude-opus-5-5"),
                ("gen_ai.response.model", "claude-opus-5-5-20261001"),
                ("gen_ai.usage.input_tokens", "1200"),
                ("gen_ai.usage.output_tokens", "40"),
                ("gen_ai.usage.cache_read.input_tokens", "1000"),
                ("gen_ai.usage.cache_write.input_tokens", "100"),
                ("gen_ai.conversation.id", "conv-1"),
            ],
        )
        .unwrap();
        assert_eq!((r.kind, &s), (Kind::Llm, &["anthropic".to_owned(), "claude-opus-5-5-20261001".to_owned(), "conv-1".to_owned()]));
        assert_eq!((r.tokens.input, r.tokens.cache_read, r.tokens.cache_write, r.tokens.non_cached(), r.tokens.total()), (Some(1200.0), Some(1000.0), Some(100.0), Some(100.0), Some(1240.0)));
        // 1.27–1.36: `gen_ai.system`, `prompt_tokens`; the 1.40–1.41 cache write's name.
        let (r, s) = read_of(
            "openai.chat",
            &[
                ("gen_ai.system", "xai"),
                ("gen_ai.request.model", "grok-5"),
                ("gen_ai.usage.prompt_tokens", "10"),
                ("gen_ai.usage.completion_tokens", "5"),
                ("gen_ai.usage.cache_creation.input_tokens", "4"),
            ],
        )
        .unwrap();
        assert_eq!((r.kind, s[0].as_str(), r.tokens.input, r.tokens.output, r.tokens.cache_write), (Kind::Llm, "x_ai", Some(10.0), Some(5.0), Some(4.0)));
        // Kinds by operation, as Datadog maps them.
        for (op, kind) in [
            ("embeddings", Kind::Embedding),
            ("execute_tool", Kind::Tool),
            ("invoke_agent", Kind::Agent),
            ("create_agent", Kind::Agent),
            ("retrieval", Kind::Retrieval),
            ("invoke_workflow", Kind::Workflow),
        ] {
            assert_eq!(read_of("x", &[("gen_ai.operation.name", op)]).unwrap().0.kind, kind, "{op}");
        }
    }

    /// An input that leaves its cache out (Anthropic's API's, passed on): its cache past it puts
    /// it back. OpenInference's, OpenLLMetry's and Langfuse's names read as OpenTelemetry's.
    #[test]
    fn other_conventions_and_an_input_without_its_cache_are_read() {
        let (r, _) = read_of("x", &[("gen_ai.operation.name", "chat"), ("gen_ai.usage.input_tokens", "200"), ("gen_ai.usage.cache_read.input_tokens", "5000")]).unwrap();
        assert_eq!(r.tokens.input, Some(5200.0));
        let (r, s) = read_of(
            "ChatOpenAI",
            &[
                ("openinference.span.kind", "LLM"),
                ("llm.model_name", "gpt-6-sol"),
                ("llm.provider", "openai"),
                ("llm.token_count.prompt", "300"),
                ("llm.token_count.completion", "20"),
                ("llm.token_count.prompt_details.cache_read", "100"),
                ("llm.cost.total", "0.0012"),
                ("session.id", "s-9"),
            ],
        )
        .unwrap();
        assert_eq!((r.kind, s[1].as_str(), s[2].as_str(), r.tokens.input, r.tokens.cache_read, r.cost.total), (Kind::Llm, "gpt-6-sol", "s-9", Some(300.0), Some(100.0), Some(0.0012)));
        assert_eq!(read_of("retriever", &[("openinference.span.kind", "RETRIEVER")]).unwrap().0.kind, Kind::Retrieval);
        assert_eq!(read_of("task", &[("traceloop.span.kind", "task"), ("traceloop.workflow.name", "w")]).unwrap().0.kind, Kind::Task);
        let (r, _) = read_of("openai.chat", &[("llm.request.type", "chat"), ("gen_ai.request.model", "gpt-5"), ("llm.usage.total_tokens", "77")]).unwrap();
        assert_eq!((r.kind, r.tokens.total()), (Kind::Llm, Some(77.0)));
        assert_eq!(read_of("gen", &[("langfuse.observation.type", "generation")]).unwrap().0.kind, Kind::Llm);
        // Datadog's own (its intake's event): its kind by name; its input the sum of its parts.
        let (r, _) = read_of("anthropic.request", &[("meta.span.kind", "llm"), ("metrics.non_cached_input_tokens", "20"), ("metrics.cache_read_input_tokens", "80")]).unwrap();
        assert_eq!((r.kind, r.tokens.input), (Kind::Llm, Some(100.0)));
    }

    /// Claude Code's own spans: its request a call to Anthropic's model, its input with its cache put
    /// back; its tool a tool, its interaction an agent's, a tool's execution a task; its generic keys
    /// read on its spans alone.
    #[test]
    fn claude_code_s_spans_are_read() {
        let (r, s) = read_of(
            "claude_code.llm_request",
            &[("model", "claude-opus-5-5"), ("input_tokens", "12"), ("output_tokens", "300"), ("cache_read_tokens", "40000"), ("cache_creation_tokens", "1500"), ("session.id", "sess")],
        )
        .unwrap();
        assert_eq!((r.kind, s[0].as_str(), s[1].as_str(), r.tokens.input, r.tokens.output), (Kind::Llm, "anthropic", "claude-opus-5-5", Some(41_512.0), Some(300.0)));
        assert_eq!(read_of("claude_code.tool", &[("tool_name", "Bash")]).unwrap().0.kind, Kind::Tool);
        assert_eq!(read_of("claude_code.tool.execution", &[]).unwrap().0.kind, Kind::Task);
        assert_eq!(read_of("claude_code.interaction", &[]).unwrap().0.kind, Kind::Agent);
        let (r, _) = read_of("x", &[("gen_ai.operation.name", "chat"), ("model", "m"), ("input_tokens", "5")]).unwrap();
        assert_eq!((r.model, r.tokens.input), (None, None));
    }

    /// The attributes real instrumentations were recorded writing (2026-10-23, an OpenAI client of
    /// a fake model server whose answer said 1,200 input tokens, 1,000 of them cached, 40 output):
    /// opentelemetry-instrumentation-openai-v2 2.4b0, as it is (1.36's names) and opted in to the
    /// latest (1.37's), saying no cache reads, so every input token is priced as the input's;
    /// openinference-instrumentation-openai 0.1.66, saying them; each embedding's 8 tokens.
    #[test]
    fn real_instrumentations_spans_are_priced_as_they_say() {
        let at = 1_792_800_000_000_000_000;
        for (name, attrs, cost) in [
            (
                "chat gpt-4o-mini",
                &[
                    ("gen_ai.operation.name", "chat"),
                    ("gen_ai.request.model", "gpt-4o-mini"),
                    ("gen_ai.system", "openai"),
                    ("gen_ai.response.model", "gpt-4o-mini-2024-07-18"),
                    ("gen_ai.openai.response.service_tier", "default"),
                    ("gen_ai.usage.input_tokens", "1200"),
                    ("gen_ai.usage.output_tokens", "40"),
                ][..],
                "204000",
            ),
            (
                "chat gpt-4o-mini",
                &[
                    ("gen_ai.operation.name", "chat"),
                    ("gen_ai.request.model", "gpt-4o-mini"),
                    ("gen_ai.provider.name", "openai"),
                    ("gen_ai.response.model", "gpt-4o-mini-2024-07-18"),
                    ("gen_ai.usage.input_tokens", "1200"),
                    ("gen_ai.usage.output_tokens", "40"),
                    ("openai.response.service_tier", "default"),
                ][..],
                "204000",
            ),
            (
                "ChatCompletion",
                &[
                    ("llm.system", "openai"),
                    ("llm.model_name", "gpt-4o-mini-2024-07-18"),
                    ("llm.token_count.total", "1240"),
                    ("llm.token_count.prompt", "1200"),
                    ("llm.token_count.completion", "40"),
                    ("llm.token_count.prompt_details.cache_read", "1000"),
                    ("llm.token_count.completion_details.reasoning", "10"),
                    ("openinference.span.kind", "LLM"),
                ][..],
                "129000",
            ),
            (
                "embeddings text-embedding-3-small",
                &[("gen_ai.operation.name", "embeddings"), ("gen_ai.request.model", "text-embedding-3-small"), ("gen_ai.system", "openai"), ("gen_ai.usage.input_tokens", "8")][..],
                "160",
            ),
            ("CreateEmbeddings", &[("llm.system", "openai"), ("llm.token_count.prompt", "8"), ("embedding.model_name", "text-embedding-3-small"), ("openinference.span.kind", "EMBEDDING")][..], "160"),
        ] {
            let a = put_of(name, attrs, "app", at).unwrap();
            assert_eq!((get(&a, "metrics.estimated_total_cost"), get(&a, "matched_model_provider")), (Some(cost), Some("openai")), "{name}: {a:?}");
        }
    }

    /// Any other span is passed over; Datadog's tracer's copy of its LLM span on its APM span too.
    #[test]
    fn other_spans_are_passed_over() {
        assert!(read_of("GET /cart", &[("http.method", "GET"), ("http.route", "/cart")]).is_none());
        assert!(!is_llm("GET /cart", ["http.method", "http.route"]));
        assert!(read_of("llm", &[("gen_ai.operation.name", "llm"), ("_dd.llmobs.artificial_gen_ai_tags", "true")]).is_none());
        assert!(put_of("GET /cart", &[("http.route", "/cart")], "shop", 0).is_none());
    }

    /// A call put among its span's attributes under Datadog's names: its kind, model, provider,
    /// app, session, tokens; its cost the catalog's, in nanodollars, with the model it matched.
    #[test]
    fn a_call_is_given_datadog_s_fields_and_its_cost() {
        let at = 1_792_800_000_000_000_000; // 2026-10-23
        let a = put_of(
            "chat",
            &[
                ("gen_ai.operation.name", "chat"),
                ("gen_ai.provider.name", "anthropic"),
                ("gen_ai.request.model", "claude-opus-5-5"),
                ("gen_ai.usage.input_tokens", "1000000"),
                ("gen_ai.usage.output_tokens", "1000000"),
                ("gen_ai.conversation.id", "c1"),
            ],
            "support-bot",
            at,
        )
        .unwrap();
        for (k, v) in [
            ("meta.span.kind", "llm"),
            ("meta.model_provider", "anthropic"),
            ("meta.model_name", "claude-opus-5-5"),
            ("ml_app", "support-bot"),
            ("session_id", "c1"),
            ("metrics.input_tokens", "1000000"),
            ("metrics.total_tokens", "2000000"),
            ("metrics.non_cached_input_tokens", "1000000"),
            ("metrics.estimated_input_cost", "4000000000"),
            ("metrics.estimated_output_cost", "20000000000"),
            ("metrics.estimated_total_cost", "24000000000"),
            ("cost_source", "auto"),
            ("matched_model_name", "claude-opus-5-5"),
            ("matched_model_provider", "anthropic"),
        ] {
            assert_eq!(get(&a, k), Some(v), "{k}");
        }
        assert!(a.len() <= 6 + OWN);
        // A cost it says is its own; a model the catalog lacks said, not guessed.
        let a = put_of("chat", &[("gen_ai.operation.name", "chat"), ("gen_ai.request.model", "claude-opus-5-5"), ("gen_ai.usage.input_tokens", "10"), ("gen_ai.usage.cost", "0.5")], "s", at).unwrap();
        assert_eq!((get(&a, "metrics.estimated_total_cost"), get(&a, "cost_source"), get(&a, "meta.model_provider")), (Some("500000000"), Some("user"), Some("custom")));
        let a =
            put_of("chat", &[("gen_ai.operation.name", "chat"), ("gen_ai.provider.name", "openai"), ("gen_ai.request.model", "no-such-model"), ("gen_ai.usage.input_tokens", "10")], "s", at).unwrap();
        assert_eq!((get(&a, "cost_unavailable"), get(&a, "metrics.estimated_total_cost")), (Some("unsupported model name"), None));
        // OpenTelemetry's provider names the catalog's rules miss, named as it names them.
        let a = put_of("chat", &[("gen_ai.operation.name", "chat"), ("gen_ai.provider.name", "x_ai"), ("gen_ai.request.model", "grok-4"), ("gen_ai.usage.input_tokens", "10")], "s", at).unwrap();
        assert_eq!(get(&a, "matched_model_provider"), Some("x-ai"));
        // A step not a model's call: its kind, no cost.
        let a = put_of("tool", &[("gen_ai.operation.name", "execute_tool"), ("gen_ai.tool.name", "search")], "s", at).unwrap();
        assert_eq!((get(&a, "meta.span.kind"), get(&a, "cost_unavailable"), get(&a, "meta.model_provider")), (Some("tool"), None, None));
    }
}
