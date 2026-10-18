//! kiln's HTTP layer, entirely on hyper-raft's stack: hyper's runtime-agnostic HTTP/1.1 and HTTP/2
//! protocol code driven by hyper-rt sockets, with TLS from hyper-tls (hyper-raft's rustls fork).
//!
//! * [`server`] — accept loop glue (HTTP/1.1 and HTTP/2, TLS or not), request/response helpers.
//! * [`ws`] — WebSocket upgrades (events, live logs, terminals).
//! * [`client`] — outbound HTTP(S): GitHub, the Kubernetes API, agent⇄server HTTP.
//! * [`io`] — hyper-rt TCP (and TLS) streams as hyper IO.
//! * [`tls`] — hyper-tls client/server configuration and the TLS stream adapter.

pub mod client;
pub mod io;
pub mod proxy;
pub mod roots;
pub mod server;
pub mod tls;
pub mod ws;
