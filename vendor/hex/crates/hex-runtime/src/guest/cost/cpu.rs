//! Optional measured CPU-response rows at the head of the EXISTING cost evidence (AC-T7.4.1). The
//! evidence's SHA-256 is the RuntimeCost id, so every row is keyed to that same cost. A row is one host
//! class's measured stop-response distribution for the macOS managed CPU control, with the digest of
//! its raw samples; a host with no row has no response, never a default.

use std::time::Duration;

use hex_api::Refusal;

use super::{exhausted, hash, invalid};

const NAME: &[u8] = b"hex-macos-cpu-response";
const HEADER: &[u8] = b"hex-macos-cpu-response 1";

/// One host class's measured response. The managed CPU control takes its maximum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuResponse {
    /// The machine class it was measured on (`hw.model`).
    pub host: String,
    pub samples: u64,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub max: Duration,
    /// SHA-256 of the raw samples the distribution came from.
    pub raw: [u8; 32],
}

impl CpuResponse {
    /// The response the control uses: the measured maximum, never a lower percentile.
    pub fn response(&self) -> Duration {
        self.max
    }

    fn check(&self) -> Result<(), Refusal> {
        if self.host.is_empty()
            || !self.host.bytes().all(|byte| byte.is_ascii_graphic())
            || self.samples == 0
            || self.p50.is_zero()
            || self.p50 > self.p90
            || self.p90 > self.p99
            || self.p99 > self.max
        {
            return Err(invalid(
                "cpu-response row needs a host class, samples and ordered positive percentiles",
            ));
        }
        Ok(())
    }
}

/// The section for `rows`, sorted by host class, to put before the original evidence bytes.
pub fn section(rows: &[CpuResponse]) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    let mut put = |text: &[u8]| -> Result<(), Refusal> {
        out.try_reserve(text.len()).map_err(|_| exhausted())?;
        out.extend_from_slice(text);
        Ok(())
    };
    put(HEADER)?;
    put(b"\n")?;
    let mut previous: Option<&str> = None;
    for row in rows {
        row.check()?;
        if previous.is_some_and(|host| host >= row.host.as_str()) {
            return Err(invalid(
                "cpu-response rows must be sorted by unique host class",
            ));
        }
        previous = Some(row.host.as_str());
        let mut text = String::new();
        use std::fmt::Write as _;
        write!(
            text,
            "{} {} {} {} {} {} ",
            row.host,
            row.samples,
            row.p50.as_nanos(),
            row.p90.as_nanos(),
            row.p99.as_nanos(),
            row.max.as_nanos()
        )
        .map_err(|_| exhausted())?;
        for byte in row.raw {
            write!(text, "{byte:02x}").map_err(|_| exhausted())?;
        }
        put(text.as_bytes())?;
        put(b"\n")?;
    }
    put(b"\n")?;
    Ok(out)
}

/// The rows at the head of `bytes` and the rest (any native-image section and the original body),
/// or no rows and all of `bytes`.
pub fn split(bytes: &[u8]) -> Result<(Vec<CpuResponse>, &[u8]), Refusal> {
    if !bytes.starts_with(NAME) {
        return Ok((Vec::new(), bytes));
    }
    let (header, mut rest) = line(bytes)?;
    if header != HEADER {
        return Err(invalid(
            "cpu-response evidence header is malformed or unknown",
        ));
    }
    let mut rows: Vec<CpuResponse> = Vec::new();
    loop {
        let (text, after) = line(rest)?;
        rest = after;
        if text.is_empty() {
            return Ok((rows, rest));
        }
        let row = parse(text)?;
        if rows.last().is_some_and(|last| last.host >= row.host) {
            return Err(invalid(
                "cpu-response rows must be sorted by unique host class",
            ));
        }
        rows.try_reserve(1).map_err(|_| exhausted())?;
        rows.push(row);
    }
}

fn line(bytes: &[u8]) -> Result<(&[u8], &[u8]), Refusal> {
    let at = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| invalid("cpu-response section is truncated"))?;
    let text = bytes
        .get(..at)
        .ok_or_else(|| invalid("cpu-response section is truncated"))?;
    let rest = bytes
        .get(at.checked_add(1).ok_or_else(exhausted)?..)
        .ok_or_else(|| invalid("cpu-response section is truncated"))?;
    Ok((text, rest))
}

fn parse(text: &[u8]) -> Result<CpuResponse, Refusal> {
    let text = std::str::from_utf8(text).map_err(|_| invalid("cpu-response row is not ASCII"))?;
    let mut words = text.split(' ');
    let mut word = || {
        words
            .next()
            .ok_or_else(|| invalid("cpu-response row is missing a field"))
    };
    let number = |text: &str| -> Result<u64, Refusal> {
        if text.is_empty()
            || !text.bytes().all(|byte| byte.is_ascii_digit())
            || (text.len() > 1 && text.starts_with('0'))
        {
            return Err(invalid(
                "cpu-response number is not a canonical unsigned integer",
            ));
        }
        text.parse::<u64>()
            .map_err(|_| invalid("cpu-response number exceeds u64"))
    };
    let host = word()?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(host.len())
        .map_err(|_| exhausted())?;
    owned.push_str(host);
    let row = CpuResponse {
        host: owned,
        samples: number(word()?)?,
        p50: Duration::from_nanos(number(word()?)?),
        p90: Duration::from_nanos(number(word()?)?),
        p99: Duration::from_nanos(number(word()?)?),
        max: Duration::from_nanos(number(word()?)?),
        raw: hash(word()?)?,
    };
    if words.next().is_some() {
        return Err(invalid("cpu-response row has extra fields"));
    }
    row.check()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(host: &str) -> CpuResponse {
        CpuResponse {
            host: host.to_owned(),
            samples: 3,
            p50: Duration::from_nanos(5),
            p90: Duration::from_nanos(7),
            p99: Duration::from_nanos(11),
            max: Duration::from_nanos(13),
            raw: [0xab; 32],
        }
    }

    /// AC-T7.4.1: the producer's section, put before the original evidence, reads back the same rows
    /// and leaves the original bytes after it untouched; evidence without it has no rows.
    #[test]
    fn the_section_round_trips_ahead_of_the_original_evidence() {
        let rows = vec![row("Mac15,8"), row("Mac16,10")];
        let mut evidence = section(&rows).unwrap();
        evidence.extend_from_slice(b"hex-shards-native-images 1\noriginal body");
        let (read, rest) = split(&evidence).unwrap();
        assert_eq!(read, rows);
        assert_eq!(rest, b"hex-shards-native-images 1\noriginal body");
        assert_eq!(read[1].response(), Duration::from_nanos(13));
        let (none, all) = split(b"original body").unwrap();
        assert!(none.is_empty());
        assert_eq!(all, b"original body");
    }

    /// AC-T7.4.1: a row out of order, repeated, malformed or with disordered percentiles is refused,
    /// never read as a usable response.
    #[test]
    fn malformed_rows_are_refused() {
        assert!(section(&[row("b"), row("a")]).is_err());
        assert!(section(&[row("a"), row("a")]).is_err());
        let mut disordered = row("a");
        disordered.p99 = Duration::from_nanos(17);
        assert!(section(&[disordered]).is_err());
        let good = String::from_utf8(section(&[row("a")]).unwrap()).unwrap();
        for bad in [
            good.replace(" 3 ", " 03 "),
            good.replace(" 13 ", " 13 14 "),
            good.replace("hex-macos-cpu-response 1", "hex-macos-cpu-response 2"),
            good.trim_end_matches('\n').to_owned(),
            good.replace(&"ab".repeat(32), "zz"),
        ] {
            assert!(split(bad.as_bytes()).is_err(), "accepted: {bad:?}");
        }
    }
}
