//! A list of JSON objects written as columns (docs/ci.md W21-R02; docs/wire.md, "Lists as columns"): each key
//! once, then its values in the rows' order, where a list of objects writes every key in every row. An agent
//! session's row is 1.2 KB of JSON of which 0.5 KB is its keys, and most of its values are said by its neighbours
//! too (its harness, its machine, its counts at zero): 137 of them are 162.6 KB as objects and, without what a
//! list doesn't read, 18.0 KB as columns (`kiln-lab bar`, 2026-11-14).
//!
//! `{"rows": n, "cols": [[key, column], …]}`, a column one of:
//!
//! - `{"v": […]}`: its values, one a row;
//! - `{"of": […], "v": […]}`: its distinct values once (`of`), and for each row which of them (`v`);
//! - `{"d": value, "x": […], "v": […]}`: the value most rows have (`d`), and the rows that have another (`x`,
//!   with theirs in `v`);
//! - `{"pre": "…", "v": […]}`: texts that begin alike, the beginning once and each one's rest;
//! - `{"cols": […]}`: objects, as columns of their own.
//!
//! A column says `"at": […]` when only some rows have its key: those rows, in order, and its values (and `x`)
//! then count among them. Which form a column takes is whichever writes it shortest, found by writing each: no
//! size is chosen here. What's read back ([`unpack`], as `ui/src/api/columns.ts` reads it) is the rows as they
//! were, each key with its value and no other; only the order of an object's keys isn't kept, which JSON doesn't
//! promise.

use serde_json::{Map, Value, json};

/// `rows` as columns.
pub fn pack(rows: &[&Map<String, Value>]) -> Value {
    json!({ "rows": rows.len(), "cols": columns(rows) })
}

/// The columns of `rows`: a key's place is where a row first said it.
fn columns(rows: &[&Map<String, Value>]) -> Vec<Value> {
    let mut keys: Vec<&str> = Vec::new();
    let mut known = std::collections::HashSet::new();
    for row in rows {
        for key in row.keys() {
            if known.insert(key.as_str()) {
                keys.push(key);
            }
        }
    }
    keys.into_iter().map(|key| json!([key, column(rows, key)])).collect()
}

/// `key`'s column over `rows`: which rows have it (all of them said by saying nothing), and their values in
/// the form that writes shortest.
fn column(rows: &[&Map<String, Value>], key: &str) -> Value {
    let present: Vec<(usize, &Value)> = rows.iter().enumerate().filter_map(|(i, row)| row.get(key).map(|v| (i, v))).collect();
    let values: Vec<&Value> = present.iter().map(|(_, v)| *v).collect();
    let mut column = shortest(&values);
    if let (Some(fields), true) = (column.as_object_mut(), present.len() < rows.len()) {
        fields.insert("at".to_owned(), json!(present.iter().map(|(i, _)| *i).collect::<Vec<usize>>()));
    }
    column
}

/// One column of `values`, a value a row, in the form that writes them shortest: a list that isn't of objects
/// (ids, say) written as a column of its own. Read back by [`unpack_column`].
pub fn pack_column(values: &[&Value]) -> Value {
    shortest(values)
}

/// `values` in each form that can hold them, the shortest written kept (the first of two as short).
fn shortest(values: &[&Value]) -> Value {
    let forms = [Some(json!({ "v": values })), said_once(values), mostly(values), begun_alike(values), nested(values)];
    let mut best: Option<(usize, Value)> = None;
    for form in forms.into_iter().flatten() {
        let size = serde_json::to_vec(&form).map_or(usize::MAX, |written| written.len());
        if best.as_ref().is_none_or(|(least, _)| size < *least) {
            best = Some((size, form));
        }
    }
    best.map_or_else(|| json!({ "v": values }), |(_, form)| form)
}

/// A value as it's written: what two equal values share, and unequal ones never (`0` and `0.0` are written apart).
fn written(value: &Value) -> String {
    value.to_string()
}

/// `{"of", "v"}`: each distinct value once, in the order first met.
fn said_once(values: &[&Value]) -> Option<Value> {
    let mut of: Vec<&Value> = Vec::new();
    let mut place = std::collections::HashMap::new();
    let which: Vec<usize> = values
        .iter()
        .map(|value| {
            *place.entry(written(value)).or_insert_with(|| {
                of.push(value);
                of.len().saturating_sub(1)
            })
        })
        .collect();
    (of.len() < values.len()).then(|| json!({ "of": of, "v": which }))
}

/// `{"d", "x", "v"}`: the value most of them are (the first met of those met as often), and the others where they are.
fn mostly(values: &[&Value]) -> Option<Value> {
    let mut counts: Vec<(String, usize, &Value)> = Vec::new();
    let mut place = std::collections::HashMap::new();
    for value in values {
        let at = *place.entry(written(value)).or_insert_with(|| {
            counts.push((written(value), 0, value));
            counts.len().saturating_sub(1)
        });
        if let Some(count) = counts.get_mut(at) {
            count.1 = count.1.saturating_add(1);
        }
    }
    let (most, _, default) = counts.iter().fold(None::<&(String, usize, &Value)>, |best, c| if best.is_none_or(|b| c.1 > b.1) { Some(c) } else { best })?;
    let others: Vec<(usize, &Value)> = values.iter().enumerate().filter(|(_, value)| written(value) != *most).map(|(i, value)| (i, *value)).collect();
    Some(json!({ "d": default, "x": others.iter().map(|(i, _)| *i).collect::<Vec<usize>>(), "v": others.iter().map(|(_, value)| *value).collect::<Vec<&Value>>() }))
}

/// `{"pre", "v"}`: texts, and the beginning they all share (whole characters of it), when they share one.
fn begun_alike(values: &[&Value]) -> Option<Value> {
    let texts: Vec<&str> = values.iter().map(|value| value.as_str()).collect::<Option<_>>()?;
    let first = *texts.first()?;
    let mut shared = first.len();
    for text in texts.iter().skip(1) {
        shared = first.bytes().zip(text.bytes()).take(shared).take_while(|(a, b)| a == b).count();
    }
    while !first.is_char_boundary(shared) {
        shared = shared.saturating_sub(1);
    }
    let pre = first.get(..shared).filter(|pre| !pre.is_empty())?;
    let rests: Vec<&str> = texts.iter().map(|text| text.get(shared..)).collect::<Option<_>>()?;
    Some(json!({ "pre": pre, "v": rests }))
}

/// `{"cols"}`: objects, as columns of their own.
fn nested(values: &[&Value]) -> Option<Value> {
    let objects: Vec<&Map<String, Value>> = values.iter().map(|value| value.as_object()).collect::<Option<_>>()?;
    (!objects.is_empty()).then(|| json!({ "cols": columns(&objects) }))
}

/// The rows `packed` holds, as they were packed: none when it isn't a list as columns. As the UI reads one
/// (`ui/src/api/columns.ts`), here for the tests that hold [`pack`] to it and for a caller that reads one back.
pub fn unpack(packed: &Value) -> Option<Vec<Map<String, Value>>> {
    let n = usize::try_from(packed.get("rows")?.as_u64()?).ok()?;
    let mut rows = vec![Map::new(); n];
    fill(&mut rows, packed.get("cols")?.as_array()?)?;
    Some(rows)
}

/// The `n` values the column `column` holds, in order ([`pack_column`]'s): none when it isn't a column, or lists
/// another number of them.
pub fn unpack_column(column: &Value, n: usize) -> Option<Vec<Value>> {
    values(column, n).filter(|all| all.len() == n)
}

/// Gives `rows` what `cols` say of them.
fn fill(rows: &mut [Map<String, Value>], cols: &[Value]) -> Option<()> {
    for col in cols {
        let (key, column) = (col.get(0)?.as_str()?, col.get(1)?);
        let at: Vec<usize> = match column.get("at") {
            Some(at) => at.as_array()?.iter().map(|i| i.as_u64().and_then(|i| usize::try_from(i).ok())).collect::<Option<_>>()?,
            None => (0..rows.len()).collect(),
        };
        let values = values(column, at.len())?;
        if values.len() != at.len() {
            return None;
        }
        for (i, value) in at.into_iter().zip(values) {
            rows.get_mut(i)?.insert(key.to_owned(), value);
        }
    }
    Some(())
}

/// A column's `n` values, in its rows' order.
fn values(column: &Value, n: usize) -> Option<Vec<Value>> {
    if let Some(cols) = column.get("cols") {
        let mut inner = vec![Map::new(); n];
        fill(&mut inner, cols.as_array()?)?;
        return Some(inner.into_iter().map(Value::Object).collect());
    }
    let v = column.get("v")?.as_array()?;
    if let Some(of) = column.get("of") {
        let of = of.as_array()?;
        return v.iter().map(|which| of.get(usize::try_from(which.as_u64()?).ok()?).cloned()).collect();
    }
    if let Some(default) = column.get("d") {
        let mut all = vec![default.clone(); n];
        for (x, value) in column.get("x")?.as_array()?.iter().zip(v) {
            *all.get_mut(usize::try_from(x.as_u64()?).ok()?)? = value.clone();
        }
        return Some(all);
    }
    if let Some(pre) = column.get("pre") {
        let pre = pre.as_str()?;
        return v.iter().map(|rest| Some(Value::String(format!("{pre}{}", rest.as_str()?)))).collect();
    }
    Some(v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(list: &Value) -> anyhow::Result<Vec<Map<String, Value>>> {
        list.as_array().ok_or_else(|| anyhow::anyhow!("not a list"))?.iter().map(|row| row.as_object().cloned().ok_or_else(|| anyhow::anyhow!("not an object: {row}"))).collect()
    }

    fn packed(rows: &[Map<String, Value>]) -> Value {
        pack(&rows.iter().collect::<Vec<_>>())
    }

    /// The rows the UI's own test reads (`ui/tests/columns.test.ts`), as a list and as columns: sessions idle
    /// alike, one at work with what only a session at work says, one that ended and says little of the rest.
    fn sessions() -> Value {
        let idle = |id: &str, kind: &str, target: &str, name: &str, cwd: &str, created_at: u64, at: u64, tools: u64| {
            json!({"id": id, "kind": kind, "target": target, "name": name, "cwd": cwd, "created_at": created_at, "status": "idle", "title": "", "queued": 0, "cost_usd": 0.0, "approvals": [],
                "admission": {"epoch": 0, "generation": 2, "learning": "bootstrap"}, "last_turn": {"at": at, "how": "done", "tools": tools}})
        };
        json!([
            idle("01a1dd4b70467679af99118ba53a4a59", "claude", "agent:30f7df97", "Claude Code", "/home/ada/kiln/w/f1", 1_794_680_243_779, 1_794_680_244_718, 19),
            idle("01a1dd4b73c9728cba77f3dd42afc3d1", "claude", "agent:30f7df97", "Claude Code", "/home/ada/kiln/w/f2", 1_794_680_243_790, 1_794_680_244_901, 3),
            idle("01a1dd4b8a1e7c33a5d09f6e2b7c4d10", "claude", "agent:30f7df97", "Claude Code", "/home/ada/kiln/w/f3", 1_794_680_243_801, 1_794_680_245_002, 7),
            {"id": "01a1dd4bf09172899e19421c5a82acd2", "kind": "codex", "target": "agent:b1dbb123", "name": "Codex", "cwd": "/home/ada/kiln/w/f4", "created_at": 1_794_680_244_000_u64, "status": "running", "title": "Fix the gate", "queued": 2, "cost_usd": 0.25,
                "approvals": [{"id": "a1", "at": 1_794_680_245_000_u64}], "admission": {"epoch": 1, "generation": 3, "learning": "learned"}, "turn_started": 1_794_680_245_100_u64, "doing": "cargo test",
                "subagents": [{"id": "s1", "description": "the tests", "started": 1_794_680_245_200_u64}]},
            idle("01a1dd4c05d27e41b8f3a6c9d1e0f2a3", "codex", "agent:b1dbb123", "Codex", "/home/ada/kiln/w/f5", 1_794_680_244_500, 1_794_680_246_000, 11),
            {"id": "01a1dd4c1bc77178a68be541fc9ab6f6", "kind": "claude", "target": "local", "name": "Claude Code", "cwd": "/home/ada/other", "created_at": 1_794_680_250_000_u64, "status": "exited", "title": "", "queued": 0, "cost_usd": 0, "approvals": [],
                "exit": {"at": 1_794_680_251_000_u64, "error": null}, "note": null}
        ])
    }

    /// W21-R02: a list read back from its columns is the list: every row its keys with their values and no
    /// other (one it never said isn't given it; a `null` it said is; `0` isn't `0.0`), whatever form each column took.
    #[test]
    fn w21_r02_a_list_read_back_from_its_columns_is_the_list() -> anyhow::Result<()> {
        let rows = rows_of(&sessions())?;
        let columns = packed(&rows);
        assert_eq!(unpack(&columns), Some(rows.clone()));
        // Each form is among them: said once, mostly one value, begun alike, objects as columns, only some rows.
        let form = |col: &Value| {
            let mut parts: Vec<&str> = col.get(1)?.as_object()?.keys().map(String::as_str).collect();
            parts.sort_unstable();
            Some(format!("{}:{}", col.get(0)?.as_str()?, parts.join("+")))
        };
        let forms: Vec<String> = columns.pointer("/cols").and_then(Value::as_array).into_iter().flatten().filter_map(form).collect();
        for form in ["created_at:v", "kind:of+v", "status:d+v+x", "cwd:pre+v", "id:pre+v", "admission:at+d+v+x", "last_turn:at+cols", "turn_started:at+v"] {
            anyhow::ensure!(forms.iter().any(|f| f == form), "no column {form} among {forms:?}");
        }
        anyhow::ensure!(rows.get(5).and_then(|r| r.get("cost_usd")) != rows.first().and_then(|r| r.get("cost_usd")), "the fixture's 0 and 0.0 are two values");
        // Lists that are nothing much: none, one row, rows with nothing in them, a key only one row has.
        for list in [json!([]), json!([{"a": 1}]), json!([{}, {}]), json!([{}, {"a": {"b": {}}}, {"a": {}}]), json!([{"a": "é1"}, {"a": "é2"}, {"a": "è"}])] {
            let rows = rows_of(&list)?;
            assert_eq!(unpack(&packed(&rows)), Some(rows), "{list}");
        }
        // A column of its own: ids that begin alike, a value most of them are, nothing at all, values unlike each other.
        for list in [json!(["01a1dd4b70467679", "01a1dd4b73c9728c", "01a1dd4c05d27e41"]), json!([7, 7, 7, 7, 7, 7, 7, 7, 2, 7]), json!([]), json!([null, {"a": 1}, "x"])] {
            let values: Vec<&Value> = list.as_array().into_iter().flatten().collect();
            assert_eq!(unpack_column(&pack_column(&values), values.len()).map(Value::Array), Some(list.clone()), "{list}");
        }
        assert_eq!(unpack_column(&json!({"v": ["a", "b"]}), 3), None, "two values aren't three");
        Ok(())
    }

    /// W21-R02: a column takes the form that writes it shortest, so a list as columns is never longer than the
    /// same columns written plainly, and the fixture's is shorter than the list as objects.
    #[test]
    fn w21_r02_each_column_takes_the_form_that_writes_it_shortest() -> anyhow::Result<()> {
        let rows = rows_of(&sessions())?;
        let size = |value: &Value| serde_json::to_vec(value).map_or(0, |written| written.len());
        let columns = packed(&rows);
        anyhow::ensure!(size(&columns) < size(&sessions()), "as columns {} B, as objects {} B", size(&columns), size(&sessions()));
        for col in columns.pointer("/cols").and_then(Value::as_array).into_iter().flatten() {
            let (key, column) = (col.get(0).and_then(Value::as_str).unwrap_or_default(), col.get(1).cloned().unwrap_or_default());
            let values: Vec<&Value> = rows.iter().filter_map(|row| row.get(key)).collect();
            let mut plain = json!({ "v": values });
            if let (Some(at), Some(fields)) = (column.get("at"), plain.as_object_mut()) {
                fields.insert("at".to_owned(), at.clone());
            }
            anyhow::ensure!(size(&column) <= size(&plain), "{key}: {} B as it's written, {} B plainly", size(&column), size(&plain));
        }
        Ok(())
    }

    /// W21-R02: the fixture the UI's reader is tested on is what this writer writes of these rows: written by
    /// this test when `KILN_WRITE_FIXTURES` is set, compared with it otherwise, so the two stay each other's.
    #[test]
    fn w21_r02_the_uis_fixture_is_what_this_writes() -> anyhow::Result<()> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/fixtures/columns.json");
        let fixture = json!({ "rows": sessions(), "columns": packed(&rows_of(&sessions())?) });
        if std::env::var_os("KILN_WRITE_FIXTURES").is_some() {
            std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&fixture)?))?;
        }
        let kept: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}: {e} (write it with KILN_WRITE_FIXTURES=1)", path.display()))?)?;
        assert_eq!(kept, fixture, "ui/tests/fixtures/columns.json isn't what `pack` writes of these rows: write it again with KILN_WRITE_FIXTURES=1");
        Ok(())
    }
}
