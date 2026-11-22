//! A query's answer as SQLite's runner writes it (slt_sqlite.c, through bench/corpus/slt.py's
//! `format_value` and `answer`): each value rendered by its column's type, the values put in the
//! record's sort, and compared with the file's, by MD5 past the hash threshold.
//!
//! Types: I is sqlite3_column_int (32 bits) as %d; R is sqlite3_column_double through SQLite's own
//! printf("%.3f"); T is sqlite3_column_text up to its first NUL, "(empty)" when that's nothing, each
//! byte outside ' '..'~' an @; NULL is NULL in any column.

use crate::md5::Md5;
use crate::slt::{Expected, Sort};

/// A value as an engine gives it back: SQLite's five storage classes. An engine that answers in
/// text (a server's text protocol) gives Text, which I and R read by its leading number as
/// SQLite's runner reads text.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

/// What renders a REAL as SQLite does: printf("%.3f") for R, and the text sqlite3_column_text
/// gives (CAST(x AS TEXT)) for T.
pub trait Render {
    fn real3(&mut self, r: f64) -> Result<String, String>;
    fn real_text(&mut self, r: f64) -> Result<String, String>;
}

/// sqlite3VdbeIntValue's view (slt.py `as_int`): a REAL truncated and saturated (NaN is 0), text
/// by its leading integer, saturated.
pub fn as_int(v: &Value) -> i64 {
    match v {
        Value::Null => 0,
        Value::Integer(i) => *i,
        Value::Real(r) => real_to_int(*r),
        Value::Text(b) | Value::Blob(b) => leading_int(b),
    }
}

/// A REAL as an integer: NaN is 0, and at or past ±2^63 (exact in an f64; i64::MAX itself has no
/// f64) it saturates.
fn real_to_int(r: f64) -> i64 {
    if r.is_nan() {
        0
    } else if r >= 9_223_372_036_854_775_808.0 {
        i64::MAX
    } else if r <= -9_223_372_036_854_775_808.0 {
        i64::MIN
    } else {
        trunc_in_range(r)
    }
}

/// A REAL strictly inside i64's range, truncated toward zero, from its IEEE 754 bits: the sign, the
/// 11-bit biased exponent and the 52-bit fraction with its implicit leading one (IEEE 754-2019, 3.4).
fn trunc_in_range(r: f64) -> i64 {
    let bits = r.to_bits();
    let biased = (bits >> 52) & 0x7ff;
    // |r| < 1: the exponent below the bias (1023).
    let Some(exp) = biased.checked_sub(1023) else {
        return 0;
    };
    let mantissa = (bits & 0x000f_ffff_ffff_ffff) | 0x0010_0000_0000_0000;
    let magnitude = match exp.checked_sub(52) {
        Some(up) => u32::try_from(up)
            .ok()
            .and_then(|up| mantissa.checked_shl(up)),
        None => u32::try_from(52u64.saturating_sub(exp))
            .ok()
            .and_then(|down| mantissa.checked_shr(down)),
    };
    let magnitude = magnitude
        .and_then(|m| i64::try_from(m).ok())
        .unwrap_or(i64::MAX);
    if bits >> 63 == 1 {
        magnitude.saturating_neg()
    } else {
        magnitude
    }
}

/// The low 32 bits of an integer, as C's conversion to int keeps them.
fn low32(i: i64) -> i32 {
    let [a, b, c, d, ..] = i.to_le_bytes();
    i32::from_le_bytes([a, b, c, d])
}

/// Whitespace as Python's `\s` matches it in a byte read as Latin-1.
fn space(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | 0x1c..=0x1f | 0x85 | 0xa0
    )
}

fn leading_int(b: &[u8]) -> i64 {
    let s: &[u8] = b
        .iter()
        .position(|&c| !space(c))
        .and_then(|at| b.get(at..))
        .unwrap_or_default();
    let (negative, digits) = match s.first() {
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    let mut n: i128 = 0;
    let mut any = false;
    for d in digits.iter().take_while(|d| d.is_ascii_digit()) {
        any = true;
        // Saturates past i64's range, as slt.py's clamp of Python's unbounded int does.
        n = n
            .saturating_mul(10)
            .saturating_add(i128::from(d.wrapping_sub(b'0')))
            .min(i128::from(i64::MAX).saturating_add(1));
    }
    if !any {
        return 0;
    }
    let n = if negative { n.saturating_neg() } else { n };
    i64::try_from(n.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap_or_default()
}

/// sqlite3VdbeRealValue's view (slt.py `as_real`): text by its longest leading real.
pub fn as_real(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Integer(i) => *i as f64,
        Value::Real(r) => *r,
        Value::Text(b) | Value::Blob(b) => leading_real(b),
    }
}

/// slt.py's REAL_PREFIX: `\s*[+-]?(\d+\.?\d*([eE][+-]?\d+)?|\.\d+([eE][+-]?\d+)?)`.
fn leading_real(b: &[u8]) -> f64 {
    let start = b.iter().position(|&c| !space(c)).unwrap_or(b.len());
    let s = b.get(start..).unwrap_or_default();
    let digits = |from: usize| {
        s.get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count()
    };
    let mut at = usize::from(matches!(s.first(), Some(b'+' | b'-')));
    let whole = digits(at);
    if whole > 0 {
        at = at.wrapping_add(whole);
        if s.get(at) == Some(&b'.') {
            at = at.wrapping_add(1);
            at = at.wrapping_add(digits(at));
        }
    } else if s.get(at) == Some(&b'.') && digits(at.wrapping_add(1)) > 0 {
        at = at.wrapping_add(1);
        at = at.wrapping_add(digits(at));
    } else {
        return 0.0;
    }
    if matches!(s.get(at), Some(b'e' | b'E')) {
        let mut e = at.wrapping_add(1);
        if matches!(s.get(e), Some(b'+' | b'-')) {
            e = e.wrapping_add(1);
        }
        let n = digits(e);
        if n > 0 {
            at = e.wrapping_add(n);
        }
    }
    let text = s
        .get(..at)
        .and_then(|t| std::str::from_utf8(t).ok())
        .unwrap_or_default();
    text.parse::<f64>().unwrap_or(0.0)
}

/// One value as its column's type renders it (slt.py `format_value`).
pub fn format_value(v: &Value, t: u8, render: &mut dyn Render) -> Result<String, String> {
    if *v == Value::Null {
        return Ok("NULL".to_owned());
    }
    match t {
        // %d of sqlite3_column_int: the low 32 bits.
        b'I' => Ok(low32(as_int(v)).to_string()),
        b'R' => render.real3(as_real(v)),
        _ => {
            let raw: Vec<u8> = match v {
                Value::Text(b) | Value::Blob(b) => b.clone(),
                Value::Real(r) => render.real_text(*r)?.into_bytes(),
                Value::Integer(i) => i.to_string().into_bytes(),
                Value::Null => Vec::new(),
            };
            let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
            if raw.is_empty() {
                return Ok("(empty)".to_owned());
            }
            Ok(raw
                .iter()
                .map(|&b| {
                    if (0x20..=0x7e).contains(&b) {
                        char::from(b)
                    } else {
                        '@'
                    }
                })
                .collect())
        }
    }
}

/// An answer's values as the runner writes them: rendered, then in the record's sort (slt.py
/// `answer`). A row wider than its types renders its extra columns as T.
pub fn answer(
    rows: &[Vec<Value>],
    types: &str,
    sort: Sort,
    render: &mut dyn Render,
) -> Result<Vec<String>, String> {
    let types = types.as_bytes();
    let mut out: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        let width = row.len().max(types.len());
        let mut r = Vec::with_capacity(width);
        for j in 0..width {
            let v = row.get(j).unwrap_or(&Value::Null);
            r.push(format_value(
                v,
                types.get(j).copied().unwrap_or(b'T'),
                render,
            )?);
        }
        out.push(r);
    }
    if sort == Sort::Rows {
        out.sort();
    }
    let mut flat: Vec<String> = out.into_iter().flatten().collect();
    if sort == Sort::Values {
        flat.sort();
    }
    Ok(flat)
}

/// The MD5 of values each followed by a newline, as sqllogictest.c hashes an answer.
pub fn md5_of(values: &[String]) -> String {
    let mut h = Md5::default();
    for v in values {
        h.update(v.as_bytes());
        h.update(b"\n");
    }
    h.hex()
}

/// Whether `got` is the file's answer: by the values, or by their hash when the record carries one
/// or the answer has more values than the threshold (slt.py's comparison). On a difference, both
/// sides as a person reads them.
pub fn compare(
    expected: &Expected,
    got: &[String],
    threshold: usize,
) -> Result<(), (String, String)> {
    let hash_it =
        matches!(expected, Expected::Hashed { .. }) || (threshold > 0 && got.len() > threshold);
    if hash_it {
        let want = match expected {
            Expected::Hashed { count, md5 } => format!("{count} values hashing to {md5}"),
            Expected::Values(v) => format!("{} values hashing to {}", v.len(), md5_of(v)),
        };
        let have = format!("{} values hashing to {}", got.len(), md5_of(got));
        return if want == have {
            Ok(())
        } else {
            Err((want, have))
        };
    }
    match expected {
        Expected::Values(v) if v.as_slice() == got => Ok(()),
        Expected::Values(v) => Err((v.join(" "), got.join(" "))),
        Expected::Hashed { .. } => Err((String::new(), got.join(" "))),
    }
}

/// How many values the file's answer holds: what an engine's answer can hold before it already
/// disagrees, so a runaway answer stops at the size of the right one.
pub fn expected_values(expected: &Expected) -> usize {
    match expected {
        Expected::Values(v) => v.len(),
        Expected::Hashed { count, .. } => *count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders REALs with Rust's formatter: enough for the values these tests use, which have no
    /// rounding SQLite's printf and Rust's could disagree on.
    struct Plain;
    impl Render for Plain {
        fn real3(&mut self, r: f64) -> Result<String, String> {
            Ok(format!("{r:.3}"))
        }
        fn real_text(&mut self, r: f64) -> Result<String, String> {
            Ok(format!("{r:?}"))
        }
    }

    fn t(s: &str) -> Value {
        Value::Text(s.as_bytes().to_vec())
    }

    /// Do: render values of every class as I, R and T. Expect: slt_sqlite.c's text for each
    /// (bench/corpus/slt.py's format_value).
    #[test]
    fn values_render_as_the_c_runner_writes_them() {
        let p = &mut Plain;
        assert_eq!(format_value(&Value::Null, b'I', p).unwrap(), "NULL");
        assert_eq!(
            format_value(&Value::Integer(1 << 32 | 5), b'I', p).unwrap(),
            "5"
        );
        assert_eq!(
            format_value(&Value::Integer(i64::from(i32::MAX) + 1), b'I', p).unwrap(),
            "-2147483648"
        );
        assert_eq!(format_value(&Value::Real(-2.9), b'I', p).unwrap(), "-2");
        assert_eq!(format_value(&Value::Real(f64::NAN), b'I', p).unwrap(), "0");
        assert_eq!(format_value(&Value::Real(0.75), b'I', p).unwrap(), "0");
        assert_eq!(
            format_value(&Value::Real(4_294_967_301.9), b'I', p).unwrap(),
            "5"
        );
        assert_eq!(as_int(&Value::Real(-9.0e18)), -9_000_000_000_000_000_000);
        assert_eq!(as_int(&Value::Real(1e300)), i64::MAX);
        assert_eq!(format_value(&t("  42abc"), b'I', p).unwrap(), "42");
        assert_eq!(
            format_value(&t("99999999999999999999999"), b'I', p).unwrap(),
            "-1"
        );
        assert_eq!(format_value(&t("abc"), b'I', p).unwrap(), "0");
        assert_eq!(format_value(&t(" 1.5e1x"), b'R', p).unwrap(), "15.000");
        assert_eq!(format_value(&t(".5"), b'R', p).unwrap(), "0.500");
        assert_eq!(format_value(&t("1e"), b'R', p).unwrap(), "1.000");
        assert_eq!(format_value(&Value::Integer(3), b'R', p).unwrap(), "3.000");
        assert_eq!(format_value(&t(""), b'T', p).unwrap(), "(empty)");
        assert_eq!(
            format_value(&Value::Blob(vec![b'a', 0x01, b'b', 0, b'c']), b'T', p).unwrap(),
            "a@b"
        );
        assert_eq!(format_value(&t("é"), b'T', p).unwrap(), "@@");
        assert_eq!(format_value(&Value::Real(1.5), b'T', p).unwrap(), "1.5");
    }

    /// Do: an answer in each sort, a row wider than its types. Expect: rows sorted by their text,
    /// values sorted alone, the extra column as T.
    #[test]
    fn answers_take_their_sort() {
        let p = &mut Plain;
        let rows = vec![
            vec![Value::Integer(2), t("b")],
            vec![Value::Integer(10), t("a")],
        ];
        assert_eq!(
            answer(&rows, "IT", Sort::No, p).unwrap(),
            ["2", "b", "10", "a"]
        );
        assert_eq!(
            answer(&rows, "IT", Sort::Rows, p).unwrap(),
            ["10", "a", "2", "b"]
        );
        assert_eq!(
            answer(&rows, "IT", Sort::Values, p).unwrap(),
            ["10", "2", "a", "b"]
        );
        assert_eq!(
            answer(&rows, "I", Sort::No, p).unwrap(),
            ["2", "b", "10", "a"]
        );
    }

    /// Do: compare answers within and past the threshold, and against a hashed answer. Expect:
    /// the values compared, then their MD5 (sqllogictest.c's: each value and a newline).
    #[test]
    fn answers_compare_by_value_then_by_hash() {
        let v: Vec<String> = ["1", "2", "3"].map(String::from).to_vec();
        assert_eq!(md5_of(&[]), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            md5_of(&["a".to_owned()]),
            "60b725f10c9c85c70d97880dfe8191b3"
        );
        assert!(compare(&Expected::Values(v.clone()), &v, 0).is_ok());
        assert!(compare(&Expected::Values(v.clone()), &v, 2).is_ok());
        let hashed = Expected::Hashed {
            count: 3,
            md5: md5_of(&v),
        };
        assert!(compare(&hashed, &v, 0).is_ok());
        let (want, got) = compare(&hashed, &v[..2], 0).unwrap_err();
        assert!(want.starts_with("3 values") && got.starts_with("2 values"));
        assert!(compare(&Expected::Values(v.clone()), &v[..2], 0).is_err());
    }
}
