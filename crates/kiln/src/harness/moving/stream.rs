//! Private, bounded files for the existing handover operation. A chunk is authorized against
//! actual volume free bytes and the explicit effective operator floor before it is written;
//! failure retains the sender's original. This is neither durable mail custody nor an outbox.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use sha2::Digest as _;

pub(crate) fn same_package(path: &Path, spool: &Spool) -> io::Result<bool> {
    struct Hash(sha2::Sha256);
    impl Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { self.0.update(bytes); Ok(bytes.len()) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut previous = std::fs::File::open(path)?;
    if previous.metadata()?.len() != spool.len() { return Ok(false); }
    let mut original = Hash(sha2::Sha256::new());
    let mut repeated = Hash(sha2::Sha256::new());
    std::io::copy(&mut previous, &mut original)?;
    std::io::copy(&mut spool.reader()?, &mut repeated)?;
    Ok(original.0.finalize() == repeated.0.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoragePolicy { Unknown, Off, Reserved { bytes: u64 } }

impl StoragePolicy {
    pub(crate) fn offered(floor: Option<u64>) -> Self { match floor { Some(bytes) => Self::Reserved { bytes }, None => Self::Off } }
    pub(crate) fn floor(self) -> io::Result<u64> { match self { Self::Unknown => Err(io::Error::other("transfer storage policy has not been negotiated")), Self::Off => Ok(0), Self::Reserved { bytes } => Ok(bytes) } }
}

/// One package header and then one original journal record at a time. The same deserializer
/// retains its lookahead; recreating a parser per record could discard a consumed separator.
pub(crate) struct Incoming<R: Read> {
    pub(crate) package: super::Package,
    rows: serde_json::StreamDeserializer<'static, serde_json::de::IoRead<R>, crate::harness::inbox::Pending>,
}

impl<R: Read> Incoming<R> {
    pub(crate) fn read(reader: R) -> Result<Self, String> {
        let mut decoder = serde_json::Deserializer::from_reader(reader);
        let package: super::Package = serde::Deserialize::deserialize(&mut decoder).map_err(|_| "the package header is unreadable".to_owned())?;
        if !super::takes(package.version) || (package.mail.is_some() && package.version < 4) {
            return Err("the package does not support its mail transfer representation".into());
        }
        if package.mail.as_ref().is_some_and(|manifest| manifest.session != package.id || manifest.source_epoch != package.state.epoch || manifest.source_epoch.checked_add(1) != Some(manifest.destination_epoch) || package.attempt.as_deref() != Some(manifest.attempt.as_str())) {
            return Err("the package header does not match its mail transfer identity".into());
        }
        Ok(Self { package, rows: decoder.into_iter() })
    }

    pub(crate) fn next(&mut self) -> Result<Option<crate::harness::inbox::Pending>, String> {
        match self.rows.next() {
            Some(Ok(row)) if self.package.mail.is_some() => Ok(Some(row)),
            Some(Ok(_)) => Err("a legacy package cannot contain a mail journal".into()),
            Some(Err(_)) => Err("a package journal row is unreadable".into()),
            None => Ok(None),
        }
    }
}

pub struct Spool {
    file: std::fs::File,
    path: PathBuf,
    bound: u64,
    bytes: u64,
    permitted: u64,
    floor: u64,
    retained: bool,
}

impl std::fmt::Debug for Spool {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { out.debug_struct("Spool").field("bytes", &self.bytes).field("bound", &self.bound).finish() }
}

impl Spool {
    pub(crate) fn create(dir: &Path, bound: u64, floor: u64) -> io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        { use std::os::unix::fs::DirBuilderExt as _; builder.mode(0o700); }
        builder.create(dir)?;
        let path = dir.join(format!("mail-transfer-{}", crate::util::new_id()));
        let file = crate::os::with_mode(std::fs::OpenOptions::new().create_new(true).read(true).write(true), 0o600).open(&path)?;
        let spool = Self { file, path, bound, bytes: 0, permitted: 0, floor, retained: false };
        spool.room(0)?;
        Ok(spool)
    }

    fn room(&self, additional: u64) -> io::Result<()> {
        let required = self.floor.checked_add(additional).ok_or_else(|| io::Error::other("transfer storage authority overflow"))?;
        let free = crate::os::disk_available(&self.path).ok_or_else(|| io::Error::other("transfer volume free bytes are unknown"))?;
        if free < required { return Err(io::Error::other("transfer volume has no room above its operator reserve")); }
        Ok(())
    }

    /// The caller counted actual encoded bytes or has an actual received chunk, not an advertised
    /// collection allocation. The permit bounds the writer even if serialization emits more.
    pub(crate) fn begin(&mut self, bytes: u64) -> io::Result<()> {
        if self.bytes != self.permitted { return Err(io::Error::other("the previous transfer piece is incomplete")); }
        let end = self.bytes.checked_add(bytes).filter(|end| *end <= self.bound).ok_or_else(|| io::Error::other("transfer exceeds the protocol package bound"))?;
        self.room(bytes)?;
        self.permitted = end;
        Ok(())
    }

    pub(crate) fn complete(&mut self, sync: bool) -> io::Result<()> {
        if self.bytes != self.permitted { return Err(io::Error::other("transfer piece did not contain its exact counted bytes")); }
        self.file.flush()?;
        if sync {
            self.file.sync_all()?;
            self.path.parent().map_or(Ok(()), crate::blobs::chunks::sync_dir)?;
        }
        self.room(0)
    }

    pub(crate) fn append(&mut self, offset: u64, data: &[u8], done: bool) -> io::Result<()> {
        if offset != self.bytes { return Err(io::Error::other("transfer piece is out of order")); }
        self.begin(u64::try_from(data.len()).map_err(io::Error::other)?)?;
        self.write_all(data)?;
        self.complete(done)
    }

    pub(crate) fn len(&self) -> u64 { self.bytes }
    pub(crate) fn path(&self) -> &Path { &self.path }

    pub(crate) fn reader(&self) -> io::Result<std::fs::File> { std::fs::File::open(&self.path) }

    /// After complete sync, a persisted Inbox transfer descriptor owns cleanup. Its path is
    /// internal and never serialized as a wire file reference. Before that explicit handoff,
    /// dropping any partial/error operation removes only this newly created spool.
    pub(crate) fn retain(mut self) -> io::Result<PathBuf> {
        self.complete(true)?;
        self.retained = true;
        Ok(self.path.clone())
    }

}

impl Write for Spool {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.bytes.checked_add(u64::try_from(bytes.len()).map_err(io::Error::other)?).filter(|next| *next <= self.permitted).ok_or_else(|| io::Error::other("transfer writer exceeded its authorized piece"))?;
        self.file.write_all(bytes)?;
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { self.file.flush() }
}

impl Read for Spool {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> { self.file.read(bytes) }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if !self.retained { let _ = std::fs::remove_file(&self.path); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w20_r09_transfer_spool_bounds_order_cleanup_and_exact_bytes() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        {
            let mut spool = Spool::create(dir.path(), 3, 0)?;
            assert!(spool.append(1, b"a", false).is_err());
            spool.append(0, b"ab", false)?;
            assert!(spool.append(2, b"cd", false).is_err());
            spool.append(2, b"c", true)?;
            let mut bytes = Vec::new();
            spool.reader()?.read_to_end(&mut bytes)?;
            assert_eq!(bytes, b"abc");
        }
        assert!(std::fs::read_dir(dir.path())?.next().is_none());
        let mut spool = Spool::create(dir.path(), 3, 0)?;
        spool.append(0, b"abc", true)?;
        let path = spool.retain()?;
        assert_eq!(std::fs::read(&path)?, b"abc");
        std::fs::remove_file(path)?;
        Ok(())
    }
}
