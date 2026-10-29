//! LLM observability (`docs/observe.md` §8.12): an application's calls to language models read from
//! its spans in each convention that names them (`spans`), Datadog's own intake of them (`intake`),
//! what each cost (`prices`), and the metrics Datadog makes of them (`metrics`).

pub mod intake;
pub mod metrics;
pub mod prices;
pub mod spans;
