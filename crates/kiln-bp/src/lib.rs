//! kiln's browser protocol (docs/browser-protocol.md; goal 2 of the browser protocol program,
//! plan `01a1a832eb04756a9f5a6c3fef8cf63a`).
//!
//! One connection per browser carries its requests, answers and live subscriptions in small
//! postcard frames. This crate is the protocol without a carrier: the frames and their codec, and
//! the sans-IO connection (a server half and a client half) that a carrier drives with the bytes
//! that came (`receive`) and asks for bytes to send when it has room (`poll_transmit(room)`). It
//! has no runtime, clock, IO or randomness of its own: time and random bytes are passed in.
//!
//! | Part | What it is |
//! |---|---|
//! | [`wire`] | the encoding: varints, strings, bytes, options, lists; refusals |
//! | [`schema`] | every type as data: what the TypeScript codec is made from, and the schema hash |
//! | [`frames`] | the frame kinds, their bodies, the envelope, reading a message |
//! | [`join`] | fragments cut and joined |
//! | [`samples`] | one or more frames of every kind, for tests and the shared vectors |
//! | [`fuzz`] | seeded inputs made the same way in Rust and TypeScript |
//! | [`rng`] | SplitMix64, as kiln-sim draws it |
//! | [`ts`] | the TypeScript codec, generated |
//! | [`vectors`] | the shared test vectors |
//! | [`conn`] | what both halves share: priorities, classes, lanes, transmits |
//! | [`tickets`] | the open's single-use ticket |
//! | [`answers`] | requests taken once per id |
//! | [`liveness`] | liveness by bytes arriving |
//! | [`state`] | a topic's state, kept without its types |
//! | [`topic`] | a topic on the server: its state, revision and change log |
//! | [`server`] | the server half, its send scheduler among it |
//! | [`client`] | the client half (the browser's is `ui/src/api/bp/client.ts`) |
//! | [`compress`] | compression by trust class |

pub mod answers;
pub mod client;
pub mod compress;
pub mod conn;
pub mod frames;
pub mod fuzz;
pub mod join;
pub mod liveness;
mod macros;
pub mod rng;
pub mod samples;
pub mod schema;
pub mod server;
pub mod state;
pub mod tickets;
pub mod topic;
pub mod ts;
pub mod vectors;
pub mod wire;

pub use frames::{Frame, Kind, KINDS, OLDEST, VERSION};
pub use wire::{Bits, Bytes, Refusal, Why};
