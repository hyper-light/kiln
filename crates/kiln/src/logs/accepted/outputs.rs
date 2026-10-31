//! U05/W24-R07: read one immutable memo tree outside the shared writer.
//! The existing accepted-copy owner pins its root first, retains this bounded manifest across
//! CAS pin pages, and owns its durable cursor/completion and retirement.

use std::sync::Arc;

use kiln_ci::work::memo::{self, Digest};
use kiln_seams::blobs::Blobs;
use sha2::Digest as _;

/// The existing memo tree wire-size limit; a copy never narrows eligibility to one log frame.
const MAX_TREE_BYTES: u64 = 64 << 20;

fn valid(digest: &Digest) -> bool {
    digest.hash.len() == 64
        && digest.hash.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && i64::try_from(digest.size_bytes).is_ok()
}

/// Returns unique file digests in stable order, excluding the already-pinned root. A truncated,
/// unavailable, mismatched or invalid tree is an error; it never represents an empty manifest.
/// No SQL, object mutation, cursor advance or completion authority is created here.
pub(super) async fn manifest(blobs: Arc<dyn Blobs>, root: Digest) -> anyhow::Result<Vec<Digest>> {
    anyhow::ensure!(valid(&root) && root.size_bytes <= MAX_TREE_BYTES, "the original memo tree exceeds its digest or 64 MiB wire bound");
    let size = usize::try_from(root.size_bytes)?;
    let receive = crate::blobs::io::read_stream(
        blobs, crate::ci::services::cas::key(&root.hash), Some(root.hash.clone()),
        root.size_bytes, 0, root.size_bytes.saturating_sub(1),
    );
    let mut bytes = Vec::with_capacity(size);
    while let Ok(piece) = receive.recv().await {
        anyhow::ensure!(piece.len() <= size.saturating_sub(bytes.len()), "the original memo tree returned excess bytes");
        bytes.extend_from_slice(&piece);
    }
    anyhow::ensure!(bytes.len() == size, "the original memo tree is unavailable or truncated");
    // Parsing, path expansion and sorting are CPU work. Keep them out of the Hub's async poll;
    // the existing bounded owner retains this future while the established blocking pool runs it.
    crate::rt::blocking(move || -> anyhow::Result<Vec<Digest>> {
        anyhow::ensure!(hex::encode(sha2::Sha256::digest(&bytes)) == root.hash, "the original memo tree does not match its digest");
        let tree: memo::Tree = serde_json::from_slice(&bytes)?;
        // Release raw bytes before constructing the existing normalized entry collection.
        drop(bytes);
        let entries = memo::entries(&tree).map_err(anyhow::Error::msg)?;
        drop(tree);
        let mut files = Vec::new();
        for entry in entries {
            if let memo::Entry::File { digest, .. } = entry {
                anyhow::ensure!(valid(&digest), "the original memo tree contains an invalid file digest");
                if digest.hash == root.hash {
                    anyhow::ensure!(digest.size_bytes == root.size_bytes, "a memo file conflicts with the original tree size");
                } else {
                    files.push(digest);
                }
            }
        }
        files.sort_unstable();
        for adjacent in files.windows(2) {
            let [left, right] = adjacent else { continue };
            anyhow::ensure!(left.hash != right.hash || left.size_bytes == right.size_bytes, "the original memo tree gives conflicting file sizes");
        }
        files.dedup();
        Ok(files)
    }).await?
}
