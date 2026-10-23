//! HTTP/1.1 (RFC 9112): the server connection, the client connection, and what they share (heads,
//! body framing).

pub mod client;
pub(crate) mod decode;
pub(crate) mod head;
pub mod server;

#[cfg(test)]
mod tests;
