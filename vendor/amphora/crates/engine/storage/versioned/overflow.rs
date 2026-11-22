//! Amphora: rows larger than a page's cell (kiln plan 01a16b0f46fa72e384b8dd0065dc94f9, step 2,
//! `AC-NS.2.4`): a large value's tail kept as versioned rows of the database's overflow tree, written
//! by the same transaction as the row.
//!
//! A row's value of fewer bytes than its key leaves room for in a cell ([`inline_len`]) is its
//! cell's value, as before. A longer value keeps exactly that many bytes in the cell and the rest
//! in chunks of [`chunk_len`] bytes, chunk `i` under [`chunk_key`]`(tree, key, i)` in the overflow
//! tree, numbered from 0 and none missing: a cell's value of exactly [`inline_len`] bytes is how a
//! reader knows to look for chunks, so no value carries a header byte. Because the chunks are rows
//! like any other, a snapshot reads the chunks of the version it sees, and the redo log,
//! recovery, rollback, savepoints and collection carry them with nothing of their own.
//!
//! The other way, a header byte on every stored value naming whether it continues, was not taken:
//! it costs a byte and a branch on every row to serve the few that overflow. The choice is
//! reversible (Needs Ada's word, `AC-NS.2.4`).

use super::page::MAX_CELL;
use crate::error::LimboError;
use crate::types::Value;
use crate::Result;

/// Bytes a leaf's cell takes beyond its key and value: the key's and the value's lengths, a `u16`
/// each (the page format, `page.rs`'s header comment).
const CELL_LENGTHS: usize = 4;

/// Bytes a chunk's key adds to its row's key: the row's tree before it (`u64`, big-endian) and the
/// chunk's number after it (`u32`, big-endian). Both fixed widths, so two rows' chunk keys are
/// equal only for the same tree, key and number.
const CHUNK_KEY_EXTRA: usize = 12;

/// The longest value a row may hold: SQLite's `SQLITE_MAX_LENGTH`, the most bytes of a string, a
/// BLOB or a row (sqlite.org/limits.html, item 1), the engine's `Value::MAX_BLOB_LENGTH`. Longer
/// values are refused (`TooBig`); it bounds every walk of a row's chunks.
pub fn max_value() -> usize {
    usize::try_from(Value::MAX_BLOB_LENGTH).unwrap_or(usize::MAX)
}

/// The value bytes a cell of `key` holds: the most that fits [`MAX_CELL`] beside the key (None: the
/// key leaves no room, refused as `TooBig`).
pub fn inline_len(key: &[u8]) -> Option<usize> {
    MAX_CELL.checked_sub(CELL_LENGTHS)?.checked_sub(key.len())
}

/// The value bytes a chunk of a row of `key` holds: the most that fits [`MAX_CELL`] beside its
/// chunk key (None: none fit, refused as `TooBig`).
pub fn chunk_len(key: &[u8]) -> Option<usize> {
    inline_len(key)?
        .checked_sub(CHUNK_KEY_EXTRA)
        .filter(|&n| n > 0)
}

/// The most chunks a row of `key` may have: enough for [`max_value`].
pub fn max_chunks(key: &[u8]) -> Result<u32> {
    let per = chunk_len(key).ok_or(LimboError::TooBig)?;
    u32::try_from(max_value().div_ceil(per)).map_err(|_| LimboError::IntegerOverflow)
}

/// Chunk `i`'s key, of the row under `key` in `tree`.
pub fn chunk_key(tree: u64, key: &[u8], i: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(key.len().saturating_add(CHUNK_KEY_EXTRA));
    out.extend_from_slice(&tree.to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(&i.to_be_bytes());
    out
}

/// Whether a cell's value under `key` continues in chunks.
pub fn continues(key: &[u8], cell_value: &[u8]) -> bool {
    inline_len(key) == Some(cell_value.len())
}

/// How `value` under `key` is stored: the cell's value, and the chunks after it (empty for a value
/// that fits the cell whole). Refused (`TooBig`) past [`max_value`] or when the key leaves no room.
pub fn split<'a>(key: &[u8], value: &'a [u8]) -> Result<(&'a [u8], Vec<&'a [u8]>)> {
    let inline = inline_len(key).ok_or(LimboError::TooBig)?;
    if value.len() < inline {
        return Ok((value, Vec::new()));
    }
    if value.len() > max_value() {
        return Err(LimboError::TooBig);
    }
    let per = chunk_len(key).ok_or(LimboError::TooBig)?;
    let (head, tail) = value.split_at_checked(inline).ok_or(LimboError::TooBig)?;
    Ok((head, tail.chunks(per).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::page::{fits, Tail};

    /// AC-NS.2.4: a value one byte short of the cell's room is stored whole; at the room and past it,
    /// the cell holds exactly the room and chunks of the most a cell takes hold the rest, each
    /// chunk's cell fitting a page.
    #[test]
    fn ns2_4_split_fills_cells_to_the_page_format() {
        let key = b"row key".to_vec();
        let room = inline_len(&key).unwrap();
        let per = chunk_len(&key).unwrap();
        let short = vec![1u8; room - 1];
        let (head, chunks) = split(&key, &short).unwrap();
        assert_eq!((head.len(), chunks.len()), (room - 1, 0));
        assert!(!continues(&key, head));
        let exact = vec![2u8; room];
        let (head, chunks) = split(&key, &exact).unwrap();
        assert_eq!((head.len(), chunks.len()), (room, 0));
        assert!(continues(&key, head));
        let long = vec![3u8; room + 2 * per + 1];
        let (head, chunks) = split(&key, &long).unwrap();
        assert_eq!(head.len(), room);
        assert_eq!(
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            vec![per, per, 1]
        );
        assert!(fits(&key, Tail::Value(head)).unwrap());
        let ck = chunk_key(7, &key, 0);
        assert!(fits(&ck, Tail::Value(chunks[0])).unwrap());
        assert!(!fits(&ck, Tail::Value(&vec![0u8; per + 1])).unwrap());
    }

    /// AC-NS.2.4: a key that leaves a chunk no room refuses a value past its cell's.
    #[test]
    fn ns2_4_split_refuses_past_the_bounds() {
        let wide = vec![0u8; MAX_CELL - CELL_LENGTHS - CHUNK_KEY_EXTRA];
        assert!(chunk_len(&wide).is_none());
        let room = inline_len(&wide).unwrap();
        assert!(matches!(
            split(&wide, &vec![0u8; room + 1]),
            Err(LimboError::TooBig)
        ));
    }

    /// AC-NS.2.4: chunk keys of different rows never collide: a key that ends like another's chunk
    /// number is still a different length.
    #[test]
    fn ns2_4_chunk_keys_are_distinct() {
        let a = chunk_key(1, b"ab", 0);
        let b = chunk_key(1, b"ab\0\0\0\0", 0);
        let c = chunk_key(2, b"ab", 0);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len() + 4, b.len());
    }
}
