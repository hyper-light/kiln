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

pub mod body;
mod buf;
mod date;
mod error;
pub(crate) mod h1;
pub(crate) mod h2;
mod io;
pub(crate) mod rt;
pub mod service;
pub mod upgrade;

#[cfg(test)]
mod testing;

pub use body::Incoming;
pub use error::{Error, Kind, Parse};
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
