//! Windows' owned stream ends (HEX-RT-17): pipe handles close through File/CloseHandle; sockets close
//! through OwnedSocket/closesocket. A transferred End carries ownership; an OwnedStream is its caller's I/O.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::windows::io::{
    AsHandle, AsSocket, BorrowedHandle, BorrowedSocket, OwnedHandle, OwnedSocket,
};

#[derive(Debug)]
pub enum End {
    Handle(std::fs::File),
    Socket(OwnedSocket),
}

impl End {
    /// A native pipe/file handle, never a socket cast to a handle. Socket forwarding must use a socket-aware path.
    pub fn file_handle(&self) -> Option<BorrowedHandle<'_>> {
        match self {
            Self::Handle(file) => Some(file.as_handle()),
            Self::Socket(_) => None,
        }
    }

    pub fn socket(&self) -> Option<BorrowedSocket<'_>> {
        match self {
            Self::Handle(_) => None,
            Self::Socket(socket) => Some(socket.as_socket()),
        }
    }
}

impl From<OwnedHandle> for End {
    fn from(handle: OwnedHandle) -> Self {
        Self::Handle(handle.into())
    }
}

impl From<OwnedSocket> for End {
    fn from(socket: OwnedSocket) -> Self {
        Self::Socket(socket)
    }
}

impl From<std::io::PipeReader> for End {
    fn from(pipe: std::io::PipeReader) -> Self {
        OwnedHandle::from(pipe).into()
    }
}

impl From<std::io::PipeWriter> for End {
    fn from(pipe: std::io::PipeWriter) -> Self {
        OwnedHandle::from(pipe).into()
    }
}

/// The caller's I/O on its end, with no intermediary thread or buffer. Dropping a socket stream ends its
/// connection explicitly before closesocket; dropping a transfer's retained End alone never shuts down a peer's copy.
#[derive(Debug)]
pub struct OwnedStream(End);

impl From<End> for OwnedStream {
    fn from(end: End) -> Self {
        Self(end)
    }
}

impl OwnedStream {
    pub fn socket(&self) -> Option<BorrowedSocket<'_>> {
        self.0.socket()
    }

    /// Half-close a socket stream. A pipe ends as its owned writer drops after the copy completes.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self.socket() {
            Some(socket) => hex_afunix::shutdown_socket(socket, how),
            None => Ok(()),
        }
    }
}

impl Read for OwnedStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match &mut self.0 {
            End::Handle(file) => file.read(bytes),
            End::Socket(socket) => hex_afunix::receive_socket(socket.as_socket(), bytes),
        }
    }
}

impl Write for OwnedStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            End::Handle(file) => file.write(bytes),
            End::Socket(socket) => hex_afunix::send_socket(socket.as_socket(), bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0 {
            End::Handle(file) => file.flush(),
            End::Socket(_) => Ok(()),
        }
    }
}

impl Drop for OwnedStream {
    fn drop(&mut self) {
        let _ = self.shutdown(Shutdown::Both);
    }
}
