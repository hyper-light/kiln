//! Decode array bounds before allocating owned API collections. Wire layout remains unchanged.
use amphora_api::{self as api, Value};
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeOwned, SeqAccess, Visitor},
};
use std::{fmt, marker::PhantomData};

pub(super) fn vector<'de, D, T, const N: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const N: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Bounded<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {N} elements")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if seq.size_hint().is_some_and(|n| n > N) {
                return Err(de::Error::custom("transport sequence exceeds bound"));
            }
            let mut values = Vec::new();
            for _ in 0..N {
                match seq.next_element()? {
                    Some(v) => values.push(v),
                    None => return Ok(values),
                }
            }
            if seq.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("transport sequence exceeds bound"));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, N>(PhantomData))
}

pub(super) fn params<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
    vector::<D, Value, { api::TX_MAX }>(d)
}

pub(super) fn watch_requests<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<(String, String, api::Rev)>, D::Error> {
    vector::<D, _, { super::wire::WATCH_BATCH_MAX }>(d)
}

pub(super) fn watch_answers<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<Result<Option<api::Rev>, api::Refusal>>, D::Error> {
    vector::<D, _, { super::wire::WATCH_BATCH_MAX }>(d)
}
#[derive(Deserialize)]
struct Statement {
    sql: String,
    #[serde(deserialize_with = "params")]
    params: Vec<Value>,
}
pub(super) fn statements<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<api::Statement>, D::Error> {
    Ok(vector::<D, Statement, { api::TX_MAX }>(d)?
        .into_iter()
        .map(|s| api::Statement {
            sql: s.sql,
            params: s.params,
        })
        .collect())
}
#[derive(Deserialize)]
struct Tx {
    #[serde(deserialize_with = "tx_vector")]
    conds: Vec<api::Cond>,
    #[serde(deserialize_with = "tx_vector")]
    writes: Vec<api::Write>,
}
fn tx_vector<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    vector::<D, T, { api::TX_MAX }>(d)
}
pub(super) fn transaction<'de, D: Deserializer<'de>>(d: D) -> Result<api::Tx, D::Error> {
    let tx = Tx::deserialize(d)?;
    Ok(api::Tx {
        conds: tx.conds,
        writes: tx.writes,
    })
}

// Charge each row and value's owned representation as well as its payload. A compact stream
// of Nulls cannot expand into an unbounded result allocation before API validation runs.
const VALUE_COUNT: usize = api::RESULT_MAX / std::mem::size_of::<Value>();
#[derive(Deserialize)]
struct Row(#[serde(deserialize_with = "row_values")] Vec<Value>);
fn row_values<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
    vector::<D, Value, VALUE_COUNT>(d)
}
pub(super) fn rows<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<Value>>, D::Error> {
    struct Rows;
    impl<'de> Visitor<'de> for Rows {
        type Value = Vec<Vec<Value>>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded SQL rows")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut rows = Vec::new();
            let mut bytes = 0usize;
            for _ in 0..=api::RESULT_MAX / 32 {
                let Some(Row(values)) = seq.next_element::<Row>()? else {
                    return Ok(rows);
                };
                bytes = bytes
                    .checked_add(std::mem::size_of::<Vec<Value>>())
                    .ok_or_else(|| de::Error::custom("result size overflow"))?;
                for value in &values {
                    let payload = match value {
                        Value::Text(s) => s.len(),
                        Value::Blob(b) => b.len(),
                        _ => 0,
                    };
                    bytes = bytes
                        .checked_add(std::mem::size_of::<Value>())
                        .and_then(|n| n.checked_add(payload))
                        .ok_or_else(|| de::Error::custom("result size overflow"))?;
                }
                if bytes > api::RESULT_MAX {
                    return Err(de::Error::custom("SQL result allocation exceeds bound"));
                }
                rows.push(values);
            }
            Err(de::Error::custom("SQL row count exceeds bound"))
        }
    }
    d.deserialize_seq(Rows)
}

#[derive(Deserialize)]
struct SqlResult {
    #[serde(deserialize_with = "rows")]
    rows: Vec<Vec<Value>>,
    affected: u64,
    last_insert_rowid: i64,
}
pub(super) fn sql_results<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Vec<api::SqlResult>, D::Error> {
    struct Results;
    impl<'de> Visitor<'de> for Results {
        type Value = Vec<api::SqlResult>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("bounded SQL results")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut results = Vec::new();
            let mut bytes = 0usize;
            for _ in 0..=api::TX_MAX {
                let Some(result) = seq.next_element::<SqlResult>()? else {
                    return Ok(results);
                };
                bytes = bytes
                    .checked_add(std::mem::size_of::<api::SqlResult>())
                    .ok_or_else(|| de::Error::custom("SQL result size overflow"))?;
                for row in &result.rows {
                    bytes = bytes
                        .checked_add(std::mem::size_of::<Vec<Value>>())
                        .ok_or_else(|| de::Error::custom("SQL result size overflow"))?;
                    for value in row {
                        let payload = match value {
                            Value::Text(s) => s.len(),
                            Value::Blob(b) => b.len(),
                            _ => 0,
                        };
                        bytes = bytes
                            .checked_add(std::mem::size_of::<Value>())
                            .and_then(|n| n.checked_add(payload))
                            .ok_or_else(|| de::Error::custom("SQL result size overflow"))?;
                    }
                }
                if bytes > api::RESULT_MAX {
                    return Err(de::Error::custom("SQL result allocation exceeds bound"));
                }
                results.push(api::SqlResult {
                    rows: result.rows,
                    affected: result.affected,
                    last_insert_rowid: result.last_insert_rowid,
                });
            }
            Err(de::Error::custom("SQL result count exceeds bound"))
        }
    }
    d.deserialize_seq(Results)
}
#[derive(Deserialize)]
struct Page {
    #[serde(deserialize_with = "scan_vector")]
    rows: Vec<api::Row>,
    more: bool,
}
fn scan_vector<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    vector::<D, T, { api::SCAN_MAX }>(d)
}
pub(super) fn page<'de, D: Deserializer<'de>>(d: D) -> Result<api::Page, D::Error> {
    let p = Page::deserialize(d)?;
    Ok(api::Page {
        rows: p.rows,
        more: p.more,
    })
}
#[derive(Deserialize)]
struct Changes {
    #[serde(deserialize_with = "scan_vector")]
    changes: Vec<api::Change>,
    head: api::Rev,
}
pub(super) fn changes<'de, D: Deserializer<'de>>(d: D) -> Result<api::Changes, D::Error> {
    let c = Changes::deserialize(d)?;
    Ok(api::Changes {
        changes: c.changes,
        head: c.head,
    })
}

#[derive(Deserialize)]
struct ImportState {
    head: api::Rev,
    horizon: api::Rev,
    feed: api::Rev,
    #[serde(deserialize_with = "tx_vector")]
    newest: Vec<(String, api::Rev)>,
}
#[derive(Deserialize)]
struct ImportPage {
    source: String,
    expected_cursor: Vec<u8>,
    next_cursor: Vec<u8>,
    #[serde(deserialize_with = "tx_vector")]
    versions: Vec<api::import::ImportVersion>,
    state: ImportState,
    done: bool,
}
pub(super) fn import_page<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<api::import::ImportPage, D::Error> {
    let p = ImportPage::deserialize(d)?;
    Ok(api::import::ImportPage {
        source: p.source,
        expected_cursor: p.expected_cursor,
        next_cursor: p.next_cursor,
        versions: p.versions,
        state: api::import::ImportState {
            head: p.state.head,
            horizon: p.state.horizon,
            feed: p.state.feed,
            newest: p.state.newest,
        },
        done: p.done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::wire::{Answer, Command, Record, Request};
    #[test]
    fn owned_api_values_keep_their_wire_shape_with_bounded_decoding() {
        // CP-T1.5: bounded adapters preserve the existing owned value representations.
        let request = Request::Records {
            namespace: "ledger".into(),
            op: Record::Commit(api::Tx {
                conds: vec![api::Cond::Absent {
                    family: "work".into(),
                    key: vec![1],
                }],
                writes: vec![api::Write::Put {
                    family: "work".into(),
                    key: vec![1],
                    value: vec![2],
                }],
            }),
        };
        let bytes = postcard::to_allocvec(&request).unwrap();
        let decoded: Request = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(postcard::to_allocvec(&decoded).unwrap(), bytes);
        let answer = Answer::Rows {
            rows: vec![vec![
                Value::Null,
                Value::Integer(7),
                Value::Text("ok".into()),
            ]],
            changes: 1,
            rowid: 7,
        };
        let bytes = postcard::to_allocvec(&answer).unwrap();
        let decoded: Answer = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(postcard::to_allocvec(&decoded).unwrap(), bytes);
        let command = Command::Query(
            "SELECT ?1".into(),
            vec![Value::Integer(1); api::TX_MAX + 1],
            false,
        );
        let bytes = postcard::to_allocvec(&command).unwrap();
        assert!(postcard::from_bytes::<Command>(&bytes).is_err());
    }
}
