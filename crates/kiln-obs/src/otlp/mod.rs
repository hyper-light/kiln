//! OpenTelemetry (OTLP) into kiln's model (`docs/observe.md` §8): the protobuf wire format read in
//! place, and the metrics data model mapped to series.

pub mod metrics;
pub mod pb;
