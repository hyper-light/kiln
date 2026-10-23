//! What can go wrong, typed by what the caller does about it: a connection that closed (try another),
//! a request canceled before it was sent, a peer that broke the protocol, a deadline, a limit.

use std::fmt;

/// The kind of an [`Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The transport failed (reading or writing).
    Io,
    /// The peer sent something that isn't HTTP, or isn't allowed.
    Parse(Parse),
    /// The connection closed: before a message began, or in the middle of one.
    Closed,
    /// The request never went out (its connection was gone, or busy with another).
    Canceled,
    /// A deadline passed: the head of a request didn't arrive in time, or a ping went unanswered.
    TimedOut,
    /// A message or a part of one was larger than allowed.
    TooLarge,
    /// A body (ours to send) failed.
    Body,
    /// An upgrade could not be handed over (the response didn't switch protocols, or the connection
    /// ended first).
    Upgrade,
    /// HTTP/2: the peer, or we, ended a stream or the connection with this error code (RFC 9113 §7).
    H2(u32),
}

/// What was wrong with what the peer sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parse {
    /// A malformed start line or header.
    Head,
    /// More headers than allowed, or a head larger than allowed.
    HeadTooLarge,
    /// The method isn't a token.
    Method,
    /// The request target isn't a URI.
    Uri,
    /// Not HTTP/1.0 or HTTP/1.1.
    Version,
    /// `Content-Length` and `Transfer-Encoding` disagree, or one is malformed.
    Length,
    /// A malformed chunk of a chunked body.
    Chunk,
    /// HTTP/2 framing or header compression broken.
    Frame,
    /// Bytes arrived that no message expected.
    Unexpected,
}

/// An HTTP error: its kind and, when there was one, its cause.
pub struct Error {
    kind: Kind,
    cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub(crate) fn new(kind: Kind) -> Error {
        Error { kind, cause: None }
    }

    pub(crate) fn with(kind: Kind, cause: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
        Error { kind, cause: Some(cause.into()) }
    }

    pub(crate) fn io(e: std::io::Error) -> Error {
        Error::with(Kind::Io, e)
    }

    pub(crate) fn parse(p: Parse) -> Error {
        Error::new(Kind::Parse(p))
    }

    pub(crate) fn closed() -> Error {
        Error::new(Kind::Closed)
    }

    pub(crate) fn canceled() -> Error {
        Error::new(Kind::Canceled)
    }

    pub(crate) fn body(cause: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
        Error::with(Kind::Body, cause)
    }

    pub(crate) fn h2(code: u32) -> Error {
        Error::new(Kind::H2(code))
    }

    /// This error's kind.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The connection closed (before an answer, or partway through a message).
    pub fn is_closed(&self) -> bool {
        matches!(self.kind, Kind::Closed)
    }

    /// The request never went out.
    pub fn is_canceled(&self) -> bool {
        matches!(self.kind, Kind::Canceled)
    }

    /// A deadline passed.
    pub fn is_timeout(&self) -> bool {
        matches!(self.kind, Kind::TimedOut)
    }

    /// The peer sent something that isn't (allowed) HTTP.
    pub fn is_parse(&self) -> bool {
        matches!(self.kind, Kind::Parse(_))
    }

    /// The transport failed.
    pub fn is_io(&self) -> bool {
        matches!(self.kind, Kind::Io)
    }

    /// A copy of this error to hand to a second party (a body and its connection both learn of a
    /// failure): the same kind, the cause as text.
    pub(crate) fn share(&self) -> Error {
        Error { kind: self.kind, cause: self.cause.as_ref().map(|c| -> Box<dyn std::error::Error + Send + Sync> { c.to_string().into() }) }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut t = f.debug_tuple("kiln_http::Error");
        t.field(&self.kind);
        if let Some(cause) = &self.cause {
            t.field(cause);
        }
        t.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            Kind::Io => "connection error",
            Kind::Parse(Parse::Head) => "malformed message head",
            Kind::Parse(Parse::HeadTooLarge) => "message head too large",
            Kind::Parse(Parse::Method) => "invalid method",
            Kind::Parse(Parse::Uri) => "invalid request target",
            Kind::Parse(Parse::Version) => "unsupported HTTP version",
            Kind::Parse(Parse::Length) => "invalid message length",
            Kind::Parse(Parse::Chunk) => "malformed chunked body",
            Kind::Parse(Parse::Frame) => "malformed HTTP/2 frame",
            Kind::Parse(Parse::Unexpected) => "unexpected bytes on the connection",
            Kind::Closed => "connection closed",
            Kind::Canceled => "request canceled",
            Kind::TimedOut => "timed out",
            Kind::TooLarge => "message too large",
            Kind::Body => "body error",
            Kind::Upgrade => "upgrade failed",
            Kind::H2(code) => return write!(f, "HTTP/2 error {}", reason(code)),
        };
        match &self.cause {
            Some(cause) => write!(f, "{what}: {cause}"),
            None => f.write_str(what),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.as_deref().map(|c| c as &(dyn std::error::Error + 'static))
    }
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> std::io::Error {
        match e.kind {
            Kind::Closed => std::io::Error::new(std::io::ErrorKind::UnexpectedEof, e),
            Kind::TimedOut => std::io::Error::new(std::io::ErrorKind::TimedOut, e),
            _ => std::io::Error::other(e),
        }
    }
}

/// An HTTP/2 error code's name (RFC 9113 §7).
pub(crate) fn reason(code: u32) -> &'static str {
    match code {
        0x0 => "NO_ERROR",
        0x1 => "PROTOCOL_ERROR",
        0x2 => "INTERNAL_ERROR",
        0x3 => "FLOW_CONTROL_ERROR",
        0x4 => "SETTINGS_TIMEOUT",
        0x5 => "STREAM_CLOSED",
        0x6 => "FRAME_SIZE_ERROR",
        0x7 => "REFUSED_STREAM",
        0x8 => "CANCEL",
        0x9 => "COMPRESSION_ERROR",
        0xa => "CONNECT_ERROR",
        0xb => "ENHANCE_YOUR_CALM",
        0xc => "INADEQUATE_SECURITY",
        0xd => "HTTP_1_1_REQUIRED",
        _ => "UNKNOWN",
    }
}
