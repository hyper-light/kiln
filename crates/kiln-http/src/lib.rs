//! kiln's HTTP, on hyper-rt: HTTP/1.1 and HTTP/2, server and client, over any `futures-io`
//! transport (a hyper-rt TCP stream, TLS over one, an upgraded connection), with the `http` crate's
//! types for messages and `http-body`'s for bodies.
//!
//! Built for a thread-per-core runtime: a connection is one task, and so are its requests (an HTTP/1.1
//! connection's handler is polled in the connection's task, an HTTP/2 connection's handlers all in
//! its one); reads go through one scratch buffer per thread, so an idle connection holds no buffer;
//! answers gather and leave in as few writes as they can.
//!
//! * [`server`]: serving connections ([`server::http1`], [`server::http2`]).
//! * [`client`]: client connections ([`client::http1`], [`client::http2`]).
//! * [`Incoming`]: a body arriving.
//! * [`upgrade`]: connections handed over (WebSockets, tunnels).
//! * [`TcpIo`]: a hyper-rt TCP stream as `futures-io`.
//! * [`h3`]: HTTP/3 for one purpose, the WebTransport door browsers open sessions through (goal 4).

pub mod body;
mod buf;
mod date;
mod error;
pub(crate) mod h1;
pub(crate) mod h2;
pub mod h3;
mod io;
mod maps;
pub(crate) mod rt;
pub mod service;
pub mod turn;
pub mod upgrade;

#[cfg(test)]
mod testing;

// Tests count what they allocate (the HTTP/3 door's memory bounds, goal 4).
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: hyper_measure::alloc::Counting = hyper_measure::alloc::Counting;

pub use body::Incoming;
pub use error::{Error, Kind, Parse};

/// When a request arrived: the read that brought its head's first bytes off the connection (an
/// HTTP/2 request's, its headers' read). Every request a server connection hands its service carries
/// it (an extension), so a handler can tell how long the request waited before it started (kiln's
/// `kiln_route_wait_seconds`, goal 1 of the API runtime program, plan 01a1a59c2a68 step 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received(pub std::time::Instant);
pub use io::TcpIo;
pub use service::{Service, service_fn};

/// Serving connections.
pub mod server {
    /// HTTP/1.1 server connections.
    pub mod http1 {
        pub use crate::h1::server::Builder;
    }

    /// HTTP/2 server connections.
    pub mod http2 {
        pub use crate::h2::server::Builder;
    }
}

/// Client connections.
pub mod client {
    /// HTTP/1.1 client connections.
    pub mod http1 {
        pub use crate::h1::client::{Builder, Connection, SendRequest, handshake};
    }

    /// HTTP/2 client connections.
    pub mod http2 {
        pub use crate::h2::client::{Connection, SendRequest, handshake};
    }
}
