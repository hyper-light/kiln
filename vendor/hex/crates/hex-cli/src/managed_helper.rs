//! `hex native-managed-helper --binary <path> --endpoint <path> --receipt <path> --cpu-cost <64 hex>
//! --rpc-nanos <u64> --cpu-response-nanos <u64> --rate-bytes <u64> --rate-processes <u64>
//! --lateness-nanos <u64>`: the macOS node's persistent managed-VM owner, the same Hex binary
//! (hex_runtime::macos::run_managed_helper). Only a node starts it, inside an admitted Creating scope,
//! with its startup socket on stdout; each option comes once, in any order, and nothing else is taken
//! (AC-T7.4.1). It opens no Store, provider or catalog.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

use hex_runtime::macos::{CpuControl, ManagedHelper};

/// The parsed invocation, or what was wrong with it.
struct Invocation {
    config: ManagedHelper,
    cpu: CpuControl,
    rates: hex_core::watch::Rates,
    lateness: Duration,
}

/// Runs the helper until its last scope ends; a bad invocation or a refusal is a non-zero status.
pub fn main(args: &[OsString]) -> u8 {
    let failed = |why: &dyn std::fmt::Display| {
        let _ = writeln!(std::io::stderr().lock(), "native-managed-helper: {why}");
        2
    };
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(why) => return failed(&why),
    };
    match hex_runtime::macos::run_managed_helper(
        invocation.config,
        invocation.cpu,
        invocation.rates,
        invocation.lateness,
    ) {
        Ok(()) => 0,
        Err(refusal) => {
            let _ = writeln!(std::io::stderr().lock(), "native-managed-helper: {refusal}");
            1
        }
    }
}

const OPTIONS: [&str; 9] = [
    "--binary",
    "--endpoint",
    "--receipt",
    "--cpu-cost",
    "--rpc-nanos",
    "--cpu-response-nanos",
    "--rate-bytes",
    "--rate-processes",
    "--lateness-nanos",
];

fn parse(args: &[OsString]) -> Result<Invocation, String> {
    let mut values: [Option<&OsStr>; OPTIONS.len()] = [None; OPTIONS.len()];
    let mut words = args.iter();
    while let Some(flag) = words.next() {
        let at = OPTIONS
            .iter()
            .position(|name| OsStr::new(name) == flag.as_os_str())
            .ok_or_else(|| format!("unknown argument {}", flag.display()))?;
        let value = words
            .next()
            .ok_or_else(|| format!("{} needs a value", flag.display()))?;
        let slot = values
            .get_mut(at)
            .ok_or_else(|| format!("{} has no slot", flag.display()))?;
        if slot.replace(value.as_os_str()).is_some() {
            return Err(format!("{} given twice", flag.display()));
        }
    }
    let value = |at: usize| -> Result<&OsStr, String> {
        values
            .get(at)
            .copied()
            .flatten()
            .ok_or_else(|| format!("{} is missing", OPTIONS.get(at).copied().unwrap_or("?")))
    };
    let number = |at: usize| -> Result<u64, String> {
        let text = value(at)?.to_str().ok_or_else(|| {
            format!(
                "{} is not a number",
                OPTIONS.get(at).copied().unwrap_or("?")
            )
        })?;
        text.parse::<u64>()
            .map_err(|_| format!("{} is not a u64", OPTIONS.get(at).copied().unwrap_or("?")))
    };
    let positive = |at: usize| -> Result<NonZeroU64, String> {
        NonZeroU64::new(number(at)?).ok_or_else(|| {
            format!(
                "{} must be positive",
                OPTIONS.get(at).copied().unwrap_or("?")
            )
        })
    };
    let config = ManagedHelper::new(
        PathBuf::from(value(0)?),
        PathBuf::from(value(1)?),
        PathBuf::from(value(2)?),
        Duration::from_nanos(positive(4)?.get()),
    )
    .map_err(|refusal| refusal.to_string())?;
    let cpu = CpuControl {
        cost_id: cost_id(value(3)?)?,
        response: Duration::from_nanos(positive(5)?.get()),
    };
    Ok(Invocation {
        config,
        cpu,
        rates: hex_core::watch::Rates {
            bytes: positive(6)?,
            processes: positive(7)?,
        },
        lateness: Duration::from_nanos(number(8)?),
    })
}

/// Exactly 32 bytes as 64 hex digits, either case.
fn cost_id(text: &OsStr) -> Result<[u8; 32], String> {
    let bad = || "--cpu-cost must be 64 hex digits".to_owned();
    let mut digits = text
        .to_str()
        .ok_or_else(bad)?
        .chars()
        .map(|digit| digit.to_digit(16).ok_or_else(bad));
    let mut id = [0u8; 32];
    for byte in &mut id {
        let high = digits.next().ok_or_else(bad)??;
        let low = digits.next().ok_or_else(bad)??;
        *byte = high
            .checked_mul(16)
            .and_then(|value| value.checked_add(low))
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(bad)?;
    }
    if digits.next().is_some() {
        return Err(bad());
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn whole() -> Vec<&'static str> {
        vec![
            "--binary",
            "/private/hex",
            "--endpoint",
            "/private/h/helper.sock",
            "--receipt",
            "/private/h/helper.receipt",
            "--cpu-cost",
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
            "--rpc-nanos",
            "7",
            "--cpu-response-nanos",
            "11",
            "--rate-bytes",
            "13",
            "--rate-processes",
            "17",
            "--lateness-nanos",
            "0",
        ]
    }

    /// AC-T7.4.1: the node's exact startup words parse to the same typed configuration.
    #[test]
    fn the_nodes_startup_words_parse_exactly() {
        let parsed = parse(&args(&whole())).unwrap();
        assert_eq!(parsed.config.binary(), std::path::Path::new("/private/hex"));
        assert_eq!(parsed.config.rpc_budget(), Duration::from_nanos(7));
        assert_eq!(parsed.cpu.cost_id[0], 1);
        assert_eq!(parsed.cpu.cost_id[31], 0x20);
        assert_eq!(parsed.cpu.response, Duration::from_nanos(11));
        assert_eq!(parsed.rates.bytes.get(), 13);
        assert_eq!(parsed.rates.processes.get(), 17);
        assert_eq!(parsed.lateness, Duration::ZERO);
    }

    /// AC-T7.4.1: a duplicate, missing, unknown, valueless or malformed word is refused, never
    /// defaulted.
    #[test]
    fn every_malformed_invocation_is_refused() {
        let mut duplicate = whole();
        duplicate.extend(["--rate-bytes", "13"]);
        let missing: Vec<&str> = whole().into_iter().skip(2).collect();
        let mut unknown = whole();
        unknown.extend(["--extra", "1"]);
        let mut valueless = whole();
        valueless.push("--lateness-nanos");
        let mut short_cost = whole();
        short_cost[7] = "0102";
        let mut bad_digit = whole();
        bad_digit[7] = "zz02030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20";
        let mut zero_rate = whole();
        zero_rate[13] = "0";
        let mut zero_budget = whole();
        zero_budget[9] = "0";
        let mut relative = whole();
        relative[3] = "helper.sock";
        for (what, words) in [
            ("duplicate", duplicate),
            ("missing", missing),
            ("unknown", unknown),
            ("valueless", valueless),
            ("short cost", short_cost),
            ("bad digit", bad_digit),
            ("zero rate", zero_rate),
            ("zero budget", zero_budget),
            ("relative path", relative),
        ] {
            assert!(parse(&args(&words)).is_err(), "{what} was accepted");
        }
    }
}
