//! bp-g2 step 12: docs/browser-protocol.md's frame table is held to kiln-bp's kinds, as
//! `wire::contract` holds docs/wire.md to its tables: each kind in the code has its row, with its
//! index, way, fields in order, class, priority, coalescing, compression, urgency and lane as the
//! code has them, and each row is a kind in the code. A kind added without its row (or a row left
//! for a kind gone) fails, naming it.

use std::collections::BTreeMap;

use kiln_bp::conn::info;
use kiln_bp::frames::{KINDS, fields_of};

fn cells(line: &str) -> Vec<String> {
    line.trim().trim_matches('|').split(" | ").map(|c| c.trim().to_owned()).collect()
}

fn unquote(s: &str) -> String {
    s.trim_matches('`').to_owned()
}

#[test]
fn bp_g2_s12_the_docs_frame_table_is_the_codes() -> Result<(), String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/browser-protocol.md");
    let doc = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = doc.find("<!-- bp:kinds -->").ok_or("no <!-- bp:kinds --> table")?;
    let end = doc.find("<!-- /bp:kinds -->").ok_or("no <!-- /bp:kinds --> end")?;
    let table = doc.get(start..end).unwrap_or_default();
    let mut rows: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in table.lines().filter(|l| l.starts_with("| `")) {
        let c = cells(line);
        if let Some(name) = c.first() {
            rows.insert(unquote(name), c);
        }
    }
    let mut wrong = Vec::new();
    for k in KINDS {
        let i = info(*k);
        let want = vec![format!("`{}`", k.name()), k.index().to_string(), k.way().name().to_owned(), format!("`{}`", fields_of(*k)), i.class.into(), i.prio.into(), i.coalesced.into(), i.compressed.into(), i.urgent.into(), i.lane.into()];
        match rows.remove(k.name()) {
            None => wrong.push(format!("{}: in the code, not in docs/browser-protocol.md's frame table", k.name())),
            Some(have) if have != want => wrong.push(format!("{}: the doc says {have:?}, the code {want:?}", k.name())),
            Some(_) => {}
        }
    }
    for name in rows.keys() {
        wrong.push(format!("{name}: in docs/browser-protocol.md's frame table, not in the code"));
    }
    if wrong.is_empty() { Ok(()) } else { Err(wrong.join("\n")) }
}
