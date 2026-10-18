//! OpenTelemetry (OTLP) into kiln's model (`docs/observe.md` §8): the protobuf wire format read in
//! place, the metrics data model mapped to series, and log records to kiln's.

pub mod logs;
pub mod metrics;
pub mod pb;
