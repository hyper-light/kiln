//! `kiln blobs`: kiln's store of bytes from the command line (docs/blobs.md). `kiln blobs copy`
//! moves a server's bytes from its local store to the S3 store `[blobs.s3]` names: every object,
//! each checked by its SHA-256 on the other side, those the S3 store holds already skipped, so a
//! copy cut short is finished by running it again.

use anyhow::{Context as _, Result};
use kiln_seams::blobs::{Blobs, LIST_MAX, Put, Range, Refusal};

use super::{io, local, s3};
use crate::config::Config;

#[derive(clap::Args)]
pub struct BlobsArgs {
    #[command(subcommand)]
    cmd: BlobsCmd,
}

#[derive(clap::Subcommand)]
enum BlobsCmd {
    /// Copy every object of this server's local store into the S3 store `[blobs.s3]` names. With
    /// the server stopped: copy, set `[blobs] store = "s3"`, start it. Objects the S3 store holds
    /// already (the same SHA-256) are skipped; run it again to finish a copy cut short.
    Copy,
}

pub async fn run(args: BlobsArgs, cfg: &Config) -> Result<()> {
    match args.cmd {
        BlobsCmd::Copy => copy(cfg).await,
    }
}

/// What a copy did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Copied {
    pub objects: u64,
    pub bytes: u64,
    pub skipped: u64,
}

async fn copy(cfg: &Config) -> Result<()> {
    let store = crate::store::Store::open(&cfg.data("kiln.db"))?;
    let presign = local::Presign { base: String::new(), key: [0; 32] };
    let from = local::LocalBlobs::new(store, cfg.data("cas"), crate::rt::clock(), local::Limits { object: cfg.blobs.object_max, ..local::Limits::default() }, presign);
    // What an older kiln kept as files is the local store's first, so it's copied too.
    crate::sync::contents::take_in(&from, &cfg.data("sync").join("blobs")).await?;
    crate::updates::take_in_from(cfg, &from).await?;
    let to = s3::S3Blobs::new(&cfg.blobs.s3, crate::rt::clock(), s3::Limits { object: cfg.blobs.object_max, ..s3::Limits::default() })?;
    if cfg.blobs.s3.create_bucket {
        to.ensure_bucket().await.map_err(|e| anyhow::anyhow!("making the bucket: {e}"))?;
    }
    let done = copy_all(&from, &to, |line| println!("{line}")).await?;
    println!("copied {} objects ({} MiB); {} were there already", done.objects, done.bytes >> 20, done.skipped);
    Ok(())
}

/// Copies every object of `from` into `to`, an object at a time, each checked by its SHA-256;
/// what `to` holds already (the same SHA-256) is skipped. `say` hears of each copied.
pub async fn copy_all(from: &dyn Blobs, to: &dyn Blobs, say: impl Fn(String)) -> Result<Copied> {
    let mut out = Copied::default();
    let mut after: Option<String> = None;
    loop {
        let page = from.list("", after.as_deref(), LIST_MAX).await.map_err(|e| anyhow::anyhow!("listing the local store: {e}"))?;
        for o in &page.objects {
            let meta = from.head(&o.key).await.map_err(|e| anyhow::anyhow!("{}: {e}", o.key))?;
            match to.head(&o.key).await {
                Ok(there) if there.etag == meta.etag => {
                    out.skipped = out.skipped.saturating_add(1);
                    continue;
                }
                Ok(_) | Err(Refusal::NotFound(_)) => {}
                Err(e) => anyhow::bail!("{}: {e}", o.key),
            }
            let copied = copy_one(from, to, &o.key, meta.size, &meta.etag).await.with_context(|| format!("copying {}", o.key))?;
            out.objects = out.objects.saturating_add(1);
            out.bytes = out.bytes.saturating_add(copied);
            say(format!("{} ({} bytes)", o.key, copied));
        }
        after = page.objects.last().map(|o| o.key.clone());
        if !page.truncated || after.is_none() {
            return Ok(out);
        }
    }
}

async fn copy_one(from: &dyn Blobs, to: &dyn Blobs, key: &str, size: u64, etag: &str) -> Result<u64> {
    if size <= io::SINGLE_MAX {
        let (_, bytes) = from.get(key, None).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        to.put(key, bytes, Put { sha256: Some(etag.to_owned()), ..Put::default() }).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(size);
    }
    let mut w = io::Writer::new(to, key);
    let mut at = 0u64;
    while at < size {
        let end = at.saturating_add(io::PART.saturating_sub(1)).min(size.saturating_sub(1));
        let piece = match from.get(key, Some(Range { start: at, end: Some(end) })).await {
            Ok((m, b)) if m.etag == etag => b,
            Ok(_) => {
                w.abort().await;
                anyhow::bail!("it changed while it was copied");
            }
            Err(e) => {
                w.abort().await;
                anyhow::bail!("{e}");
            }
        };
        at = at.saturating_add(piece.len() as u64);
        if let Err(e) = w.write(&piece).await {
            w.abort().await;
            anyhow::bail!("{e}");
        }
    }
    let made = w.finish().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    if made.etag != etag {
        let _ = to.delete(key).await;
        anyhow::bail!("its copy's SHA-256 is {}, not {etag}", made.etag);
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use kiln_seams::blobs::{SimBlobs, SimLimits};

    /// CP-T10.1: every object goes across, large ones in parts, each as it was; run again, the
    /// copy skips what's there.
    #[test]
    fn every_object_goes_across_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let limits = SimLimits { object: 1 << 30, total: 4 << 30, ..SimLimits::default() };
            let (from, to): (Arc<dyn Blobs>, Arc<dyn Blobs>) = (Arc::new(SimBlobs::new(crate::rt::clock(), limits)), Arc::new(SimBlobs::new(crate::rt::clock(), limits)));
            let big = kiln_seams::blobs::noise(40 << 20, 1);
            from.put("cas/big", big.clone(), Put::default()).await?;
            for i in 0..1500 {
                from.put(&format!("sync/{i:04}"), format!("{i}").into_bytes(), Put::default()).await?;
            }
            let first = copy_all(from.as_ref(), to.as_ref(), |_| {}).await?;
            assert_eq!((first.objects, first.skipped), (1501, 0));
            assert!(to.get("cas/big", None).await?.1 == big);
            assert_eq!(to.get("sync/0042", None).await?.1, b"42");
            let again = copy_all(from.as_ref(), to.as_ref(), |_| {}).await?;
            assert_eq!((again.objects, again.skipped), (0, 1501));
            Ok(())
        })
    }
}
