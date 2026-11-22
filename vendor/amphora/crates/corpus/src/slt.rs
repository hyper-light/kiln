//! sqllogictest files as SQLite's runner reads them (sqlite.org/sqllogictest: sqllogictest.c), the
//! reading bench/corpus/slt.py makes, record for record. A file is records separated by blank lines:
//!
//! ```text
//! statement ok | statement error      then the SQL
//! query <types> <sort> [<label>]      then the SQL, ----, and the answer: one value a line, or
//!                                     "<n> values hashing to <md5>"
//! skipif <engine> | onlyif <engine>   before a record: whether that engine runs it
//! hash-threshold <n>                  answers of more values than n are compared by their MD5
//! halt                                stop reading the file (the runner stops; the reader keeps
//!                                     going, as the halt may be under skipif or onlyif)
//! ```
//!
//! A line this reader doesn't know is skipped, as the C runner skips it.

/// What a query's answer is compared with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expected {
    /// The values, one a line, in the record's sort.
    Values(Vec<String>),
    /// "<count> values hashing to <md5>".
    Hashed { count: usize, md5: String },
}

/// How a query's values are put in order before they're compared (sqllogictest's sort modes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    /// As the engine returned them.
    No,
    /// Rows by their values' text.
    Rows,
    /// Every value on its own.
    Values,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `statement ok` (true) or `statement error` (false).
    Statement { ok: bool },
    /// `query <types> <sort>`: the expected answer, absent when the record carries none.
    Query {
        types: String,
        sort: Sort,
        expected: Option<Expected>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The line its first word is on, from 1.
    pub line: usize,
    pub kind: Kind,
    pub sql: String,
    /// `skipif` and `onlyif` lines before it: (whether `onlyif`, the engine named).
    pub conditions: Vec<(bool, String)>,
}

impl Record {
    /// Whether `engine` runs it (slt.py's `runs_on`).
    pub fn runs_on(&self, engine: &str) -> bool {
        self.conditions
            .iter()
            .all(|(only, e)| if *only { e == engine } else { e != engine })
    }

    /// A query that only reads: its first word SELECT or VALUES. Every other record may change
    /// what later ones see, so a file cut down to some of its queries keeps it.
    pub fn reads_only(&self) -> bool {
        if !matches!(self.kind, Kind::Query { .. }) {
            return false;
        }
        let first = first_word(&self.sql);
        first.eq_ignore_ascii_case("select") || first.eq_ignore_ascii_case("values")
    }
}

/// The first word of `sql`, past whitespace and comments.
fn first_word(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        } else {
            break;
        }
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    rest.get(..end).unwrap_or_default()
}

/// One item of a file, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Record(Record),
    HashThreshold(usize),
    /// `halt`, with the conditions before it.
    Halt(Vec<(bool, String)>),
}

/// A file's items, or the line that can't be read (a hash threshold that isn't a number).
pub fn parse(text: &str) -> Result<Vec<Item>, String> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut items = Vec::new();
    let mut conditions = Vec::new();
    let mut i = 0usize;
    while let Some(line) = lines.get(i) {
        let number = i.wrapping_add(1);
        i = number;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let head = words.first().copied().unwrap_or_default();
        let word = |n: usize| words.get(n).copied();
        match head {
            "skipif" | "onlyif" => {
                conditions.push((head == "onlyif", word(1).unwrap_or_default().to_owned()))
            }
            "hash-threshold" => {
                let n = word(1)
                    .and_then(|w| w.parse::<usize>().ok())
                    .ok_or_else(|| format!("line {number}: hash-threshold takes a count"))?;
                items.push(Item::HashThreshold(n));
            }
            "halt" => items.push(Item::Halt(std::mem::take(&mut conditions))),
            "statement" | "query" => {
                let mut sql = Vec::new();
                while let Some(l) = lines.get(i) {
                    if l.trim().is_empty() || *l == "----" {
                        break;
                    }
                    sql.push(*l);
                    i = i.wrapping_add(1);
                }
                let kind = if head == "statement" {
                    Kind::Statement {
                        ok: word(1).unwrap_or("ok") != "error",
                    }
                } else {
                    let sort = match word(2).unwrap_or("nosort") {
                        "rowsort" => Sort::Rows,
                        "valuesort" => Sort::Values,
                        _ => Sort::No,
                    };
                    let mut expected = None;
                    if lines.get(i).is_some_and(|l| *l == "----") {
                        i = i.wrapping_add(1);
                        let mut values = Vec::new();
                        while let Some(l) = lines.get(i) {
                            if l.trim().is_empty() {
                                break;
                            }
                            values.push((*l).to_owned());
                            i = i.wrapping_add(1);
                        }
                        expected = Some(match values.as_slice() {
                            [one] => {
                                hashed(one).unwrap_or_else(|| Expected::Values(values.clone()))
                            }
                            _ => Expected::Values(values),
                        });
                    }
                    Kind::Query {
                        types: word(1).unwrap_or_default().to_owned(),
                        sort,
                        expected,
                    }
                };
                items.push(Item::Record(Record {
                    line: number,
                    kind,
                    sql: sql.join("\n"),
                    conditions: std::mem::take(&mut conditions),
                }));
            }
            _ => {}
        }
    }
    Ok(items)
}

/// "<n> values hashing to <32 hex digits>".
fn hashed(line: &str) -> Option<Expected> {
    let (count, rest) = line.split_once(" values hashing to ")?;
    let count = count.parse::<usize>().ok()?;
    let ok = rest.len() == 32
        && rest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Some(Expected::Hashed {
            count,
            md5: rest.to_owned(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: read a file with every item kind. Expect: each as sqllogictest.c reads it, and the
    /// record after `halt` still read: a halt may sit under `skipif`/`onlyif`, so whether it stops
    /// a file is the runner's to decide for its engine (run.rs, slt.py's run), not the reader's.
    #[test]
    fn reads_every_item() {
        let text = "hash-threshold 8\n\n# a comment\nstatement ok\nCREATE TABLE t(a)\n\nskipif mysql\nonlyif sqlite\nquery I rowsort label-1\nSELECT a\nFROM t\n----\n1\n2\n\nquery T nosort\nSELECT 'x'\n----\n3 values hashing to 0123456789abcdef0123456789abcdef\n\nstatement error\nSELECT nope\n\nhalt\n\nstatement ok\nSELECT 1\n";
        let items = parse(text).unwrap();
        assert_eq!(items.len(), 7);
        assert_eq!(items[0], Item::HashThreshold(8));
        let Item::Record(r) = &items[2] else {
            panic!("{:?}", items[2])
        };
        assert_eq!(r.line, 9);
        assert_eq!(r.sql, "SELECT a\nFROM t");
        assert_eq!(
            r.conditions,
            vec![(false, "mysql".to_owned()), (true, "sqlite".to_owned())]
        );
        assert!(r.runs_on("sqlite") && !r.runs_on("mysql") && !r.runs_on("postgresql"));
        assert_eq!(
            r.kind,
            Kind::Query {
                types: "I".into(),
                sort: Sort::Rows,
                expected: Some(Expected::Values(vec!["1".into(), "2".into()]))
            }
        );
        let Item::Record(h) = &items[3] else { panic!() };
        assert!(matches!(
            &h.kind,
            Kind::Query {
                expected: Some(Expected::Hashed { count: 3, .. }),
                ..
            }
        ));
        let Item::Record(e) = &items[4] else { panic!() };
        assert_eq!(e.kind, Kind::Statement { ok: false });
        assert_eq!(items[5], Item::Halt(vec![]));
        let Item::Record(after) = &items[6] else {
            panic!()
        };
        assert_eq!((after.line, after.sql.as_str()), (26, "SELECT 1"));
    }

    /// Do: ask which queries only read. Expect: SELECT and VALUES past comments; nothing else.
    #[test]
    fn reads_only_is_select_or_values() {
        let q = |sql: &str| Record {
            line: 1,
            kind: Kind::Query {
                types: "I".into(),
                sort: Sort::No,
                expected: None,
            },
            sql: sql.into(),
            conditions: vec![],
        };
        assert!(q("  -- c\n/* d */ select 1").reads_only());
        assert!(q("VALUES (1)").reads_only());
        assert!(!q("INSERT INTO t VALUES (1) RETURNING a").reads_only());
        assert!(!q("WITH x AS (SELECT 1) DELETE FROM t").reads_only());
        assert!(!q("PRAGMA user_version = 3").reads_only());
    }

    /// Do: a hash threshold that isn't a number. Expect: refused with its line.
    #[test]
    fn a_bad_threshold_is_refused() {
        assert_eq!(
            parse("\nhash-threshold x\n"),
            Err("line 2: hash-threshold takes a count".to_owned())
        );
    }
}
