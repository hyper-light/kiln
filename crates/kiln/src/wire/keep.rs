//! What a durable stream keeps so a restart picks it up where it was (docs/wire.md):
//! - its sender keeps its unanswered items, and the number its next one gets;
//! - its receiver keeps through which item it has answered, and the refusals its sender may not
//!   have heard.
//!
//! Everything is kept in `Records`: kiln's store today, Amphora once critical-path T1 lands. So it
//! moves with the rest of kiln's records and never needs a store of its own. A sender keeps an
//! item before sending it (`keep`), and lets it go once it's answered (`forget`). A receiver keeps
//! its state as it answers (`keep_in`), in the same transaction as the work the item did when it
//! can.

use kiln_seams::records::{Consistency, Records, Refusal, Scan, TX_MAX, Tx, Write};

use super::channel::{Has, Seq};

/// A sender's unanswered items: peer, stream, number → the item's bytes.
pub const OUT: &str = "wire.out";
/// The number a sender's next item gets: peer, stream → the number.
pub const NEXT: &str = "wire.next";
/// A receiver's state: peer, stream → `Has` (JSON).
pub const IN: &str = "wire.in";

fn part<'a>(name: &'a str, what: &'static str) -> Result<&'a [u8], Refusal> {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return Err(Refusal::Invalid { what, why: format!("{name:?}: not empty, and no NUL in it") });
    }
    Ok(name.as_bytes())
}

/// `peer` NUL `stream` NUL: the start of every key of one stream.
fn stream_key(peer: &str, stream: &str) -> Result<Vec<u8>, Refusal> {
    let mut k = part(peer, "a peer's name")?.to_vec();
    k.push(0);
    k.extend_from_slice(part(stream, "a stream's name")?);
    k.push(0);
    Ok(k)
}

fn item_key(peer: &str, stream: &str, seq: Seq) -> Result<Vec<u8>, Refusal> {
    let mut k = stream_key(peer, stream)?;
    k.extend_from_slice(&seq.to_be_bytes());
    Ok(k)
}

/// Keeps item `seq` of `stream` to `peer` before it's sent, with the number the next one gets.
pub async fn keep(r: &dyn Records, peer: &str, stream: &str, seq: Seq, body: &[u8]) -> Result<(), Refusal> {
    let tx = Tx {
        conds: Vec::new(),
        writes: vec![
            Write::Put { family: OUT.into(), key: item_key(peer, stream, seq)?, value: body.to_vec() },
            Write::Put { family: NEXT.into(), key: stream_key(peer, stream)?, value: seq.saturating_add(1).to_be_bytes().to_vec() },
        ],
    };
    r.commit(tx).await.map(|_| ())
}

/// Lets go of answered items.
pub async fn forget(r: &dyn Records, peer: &str, stream: &str, seqs: &[Seq]) -> Result<(), Refusal> {
    for chunk in seqs.chunks(TX_MAX) {
        let writes = chunk.iter().map(|s| item_key(peer, stream, *s).map(|key| Write::Delete { family: OUT.into(), key })).collect::<Result<Vec<_>, _>>()?;
        r.commit(Tx { conds: Vec::new(), writes }).await?;
    }
    Ok(())
}

/// Every row of `family` whose key starts with `prefix`.
async fn all(r: &dyn Records, family: &str, prefix: Vec<u8>) -> Result<Vec<(Vec<u8>, Vec<u8>)>, Refusal> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = r.scan(family, Scan { prefix: prefix.clone(), after: after.clone(), limit: 1000, at: Consistency::Linearizable }).await?;
        after = page.rows.last().map(|row| row.key.clone());
        out.extend(page.rows.into_iter().map(|row| (row.key, row.value)));
        if !page.more || after.is_none() {
            return Ok(out);
        }
    }
}

/// A durable stream's sending side, as kept: its name, the number its next item gets, and its
/// unanswered items in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub stream: String,
    pub next: Seq,
    pub pending: Vec<(Seq, Vec<u8>)>,
}

/// What a sender kept of its streams to `peer` (`Sender::restore` picks each up).
pub async fn load_out(r: &dyn Records, peer: &str) -> Result<Vec<Kept>, Refusal> {
    let mut prefix = part(peer, "a peer's name")?.to_vec();
    prefix.push(0);
    let mut kept: std::collections::BTreeMap<String, Kept> = std::collections::BTreeMap::new();
    for (key, value) in all(r, NEXT, prefix.clone()).await? {
        let Some(stream) = key.strip_prefix(prefix.as_slice()).and_then(|k| k.strip_suffix(&[0])).and_then(|s| std::str::from_utf8(s).ok()) else { continue };
        let next = <[u8; 8]>::try_from(value.as_slice()).map(u64::from_be_bytes).unwrap_or(1);
        kept.insert(stream.to_owned(), Kept { stream: stream.to_owned(), next, pending: Vec::new() });
    }
    for (key, value) in all(r, OUT, prefix.clone()).await? {
        let Some(rest) = key.strip_prefix(prefix.as_slice()) else { continue };
        let Some(nul) = rest.iter().position(|b| *b == 0) else { continue };
        let Some((name, seq)) = rest.split_at_checked(nul) else { continue };
        let Some(seq) = seq.strip_prefix(&[0]).and_then(|s| <[u8; 8]>::try_from(s).ok()).map(u64::from_be_bytes) else { continue };
        let Ok(stream) = std::str::from_utf8(name) else { continue };
        let k = kept.entry(stream.to_owned()).or_insert_with(|| Kept { stream: stream.to_owned(), next: seq.saturating_add(1), pending: Vec::new() });
        k.pending.push((seq, value));
    }
    Ok(kept.into_values().collect())
}

/// Keeps a receiver's state of `stream` from `peer`.
pub async fn keep_in(r: &dyn Records, peer: &str, has: &Has) -> Result<(), Refusal> {
    let value = serde_json::to_vec(has).map_err(|e| Refusal::Invalid { what: "a stream's state", why: e.to_string() })?;
    let key = stream_key(peer, &has.stream)?;
    r.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: IN.into(), key, value }] }).await.map(|_| ())
}

/// What a receiver kept of its streams from `peer` (`Receiver::restore` picks each up).
pub async fn load_in(r: &dyn Records, peer: &str) -> Result<Vec<Has>, Refusal> {
    let mut prefix = part(peer, "a peer's name")?.to_vec();
    prefix.push(0);
    let mut out = Vec::new();
    for (_, value) in all(r, IN, prefix).await? {
        if let Ok(has) = serde_json::from_slice::<Has>(&value) {
            out.push(has);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use kiln_seams::records::SimRecords;

    use super::super::channel::{Frame, Nack, Refusal as Why, Sender};
    use super::*;

    /// What a sender kept outlives it: a new one picks its streams up and sends them again.
    async fn a_restart_sends_what_was_kept(r: &dyn Records) -> anyhow::Result<()> {
        let mut s = Sender::new(1 << 20, Duration::from_secs(10));
        for n in 0..3u8 {
            let seq = s.push("cmd", vec![n]).map_err(|_| anyhow::anyhow!("full"))?;
            keep(r, "runner-a", "cmd", seq, &[n]).await?;
        }
        // Item 1 was answered; 2 and 3 weren't when it stopped.
        forget(r, "runner-a", "cmd", &[1]).await?;
        let kept = load_out(r, "runner-a").await?;
        assert_eq!(kept, vec![Kept { stream: "cmd".into(), next: 4, pending: vec![(2, vec![1]), (3, vec![2])] }]);
        let mut again = Sender::new(1 << 20, Duration::from_secs(10));
        for k in kept {
            again.restore(&k.stream, k.next, k.pending);
        }
        again.on_frame(Frame::Have(vec![Has { stream: "cmd".into(), upto: 1, nacks: Vec::new() }]));
        let seqs: Vec<Seq> = again.poll(Instant::now()).into_iter().filter_map(|f| if let Frame::Item { seq, .. } = f { Some(seq) } else { None }).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(again.push("cmd", vec![9]), Ok(4));
        // Another peer's streams are its own.
        assert!(load_out(r, "runner-b").await?.is_empty());
        // A receiver's state too, refusals and all.
        let has = Has { stream: "cmd".into(), upto: 7, nacks: vec![Nack { seq: 6, code: Why::Unknown, why: "no such approval".into() }] };
        keep_in(r, "server", &has).await?;
        assert_eq!(load_in(r, "server").await?, vec![has]);
        Ok(())
    }

    #[test]
    fn kept_in_the_simulator() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let r = SimRecords::new();
            a_restart_sends_what_was_kept(&r).await
        })
    }

    #[test]
    fn kept_in_kilns_store() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let r = crate::records::StoreRecords::open(crate::store::Store::in_memory()?)?;
            a_restart_sends_what_was_kept(&r).await
        })
    }

    #[test]
    fn a_name_with_a_nul_is_refused() {
        assert!(stream_key("a\0b", "s").is_err());
        assert!(stream_key("a", "").is_err());
    }
}
