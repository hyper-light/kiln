//! Readiness of an actual guest workload, not a host DeviceId provider. Host
//! identity remains the canonical grant and same-scope renderer ACK/report.

use hex_api::Refusal;

pub(super) mod config;
mod driver;
mod facts;
mod io;
mod receipt;

#[cfg(all(test, unix))]
#[path = "observer/failure_tests.rs"]
mod failure_tests;

use io::{Captured, End};
use receipt::Receipt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AttemptPhase {
    Launching,
    Draining,
    Waiting,
    Deleting,
    Complete,
}

impl AttemptPhase {
    fn word(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Draining => "draining",
            Self::Waiting => "waiting",
            Self::Deleting => "deleting",
            Self::Complete => "complete",
        }
    }
    fn parse(word: &str) -> Result<Self, Refusal> {
        match word {
            "launching" => Ok(Self::Launching),
            "draining" => Ok(Self::Draining),
            "waiting" => Ok(Self::Waiting),
            "deleting" => Ok(Self::Deleting),
            "complete" => Ok(Self::Complete),
            _ => Err(invalid("retained guest observer has an unknown phase")),
        }
    }
}

/// Diagnostic/protocol state within the SAME native VM receipt. It owns no host
/// resource, device holder or reaper. Any incomplete phase forbids Live/relaunch.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Attempt {
    pub configuration: [u8; 32],
    pub max_queues: u32,
    pub phase: AttemptPhase,
    stdout: Captured,
    stderr: Captured,
    facts: Option<facts::Facts>,
}

impl Attempt {
    pub fn additional_receipt_room(&self) -> Result<usize, Refusal> {
        let streams = self
            .stdout
            .encoded_room()?
            .checked_add(self.stderr.encoded_room()?)
            .and_then(|bytes| bytes.checked_sub(2)) // two initial '-' frame tokens.
            .ok_or_else(|| exhausted(self.stdout.limit()))?;
        let queue_digits = usize::try_from(
            self.max_queues
                .checked_ilog10()
                .ok_or_else(|| exhausted(streams))?
                .checked_add(1)
                .ok_or_else(|| exhausted(streams))?,
        )
        .map_err(|_| exhausted(streams))?;
        let readiness = 16usize
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(32usize.checked_mul(2)?))
            .and_then(|bytes| bytes.checked_add(queue_digits))
            .and_then(|bytes| bytes.checked_add(3))
            .and_then(|bytes| bytes.checked_add(self.stdout.limit().checked_mul(2)?))
            .and_then(|bytes| bytes.checked_sub("none".len()))
            .ok_or_else(|| exhausted(streams))?;
        let facts = self
            .facts
            .as_ref()
            .map(facts::Facts::additional_room)
            .transpose()?
            .unwrap_or(0);
        streams
            .checked_add(readiness)
            .and_then(|bytes| bytes.checked_add(facts))
            .ok_or_else(|| exhausted(streams))
    }

    pub fn has_facts(&self) -> bool {
        self.facts.is_some()
    }

    pub fn render_facts(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        self.facts.as_ref().ok_or(std::fmt::Error)?.render(out)
    }

    pub fn parse_facts(&mut self, lines: &mut std::str::Lines<'_>) -> Result<(), Refusal> {
        self.facts = Some(facts::Facts::parse(lines)?);
        Ok(())
    }

    pub fn ready_facts(&self) -> Result<bool, Refusal> {
        self.facts
            .as_ref()
            .map(facts::Facts::ready)
            .transpose()
            .map(|ready| ready.unwrap_or(true))
    }
    pub fn check_config(
        &self,
        config: &config::GpuObserver,
        expected: usize,
    ) -> Result<(), Refusal> {
        if self.configuration != config.identity()
            || self.max_queues != config.queues()
            || self.stdout.limit() != config.output_bytes(expected)?
            || self.stderr.limit() > config.stderr_bytes()
        {
            return Err(invalid(
                "retained observer attempt differs from initialized configuration",
            ));
        }
        Ok(())
    }
    pub fn finished_streams(&self) -> bool {
        self.stdout.end == End::Eof
            && self.stderr.end == End::Eof
            && self.stdout.within_bound()
            && self.stderr.within_bound()
    }

    pub fn matches_report(&self, report: &Report) -> Result<bool, Refusal> {
        Ok(self.stdout.bytes()? == report.text().as_bytes())
    }

    pub fn matches_readiness(&self, readiness: &Evidence) -> Result<bool, Refusal> {
        Ok(self.phase == AttemptPhase::Complete
            && self.configuration == readiness.configuration
            && self.max_queues == readiness.max_queues
            && self.finished_streams()
            && self.matches_report(&readiness.report)?)
    }
    pub fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        for byte in self.configuration {
            write!(out, "{byte:02x}")?;
        }
        write!(out, " {} {} ", self.max_queues, self.phase.word())?;
        self.stdout.render(out)?;
        write!(out, " ")?;
        self.stderr.render(out)
    }

    pub fn parse(text: &str) -> Result<Option<Self>, Refusal> {
        if text == "none" {
            return Ok(None);
        }
        let [
            configuration,
            queues,
            phase,
            out_state,
            out_limit,
            stdout,
            err_state,
            err_limit,
            stderr,
        ] = fields::<9>(text)?;
        let configuration = hex_array(configuration)?;
        let max_queues = word(queues)?;
        if configuration == [0; 32] || max_queues == 0 {
            return Err(invalid(
                "retained observer attempt has unknown configuration",
            ));
        }
        Ok(Some(Self {
            configuration,
            max_queues,
            phase: AttemptPhase::parse(phase)?,
            stdout: Captured::parse(out_state, out_limit, stdout)?,
            stderr: Captured::parse(err_state, err_limit, stderr)?,
            facts: None,
        }))
    }
}

// Vulkan-Headers 691fa560: native name/info arrays are 256 bytes including NUL,
// VK_MAX_MEMORY_HEAPS is16, MESA_VENUS is22, and COMPUTE/DEVICE_LOCAL are bit1/0.
const NATIVE_TEXT_BYTES: usize = 256;
const MAX_HEAPS: u32 = 16;
const VENUS: u32 = 22;
const COMPUTE: u32 = 2;
const DEVICE_LOCAL: u32 = 1;
// VK_UUID_SIZE16, each native byte represented by exactly two hexadecimal digits.
const UUID_HEX_BYTES: usize = 32;
const HEADER: &str = "hex-vulkan-guest-inventory 1 ";

const WHAT: &str = "Shards guest GPU readiness";

fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return exhausted(why.len());
    }
    text.push_str(why);
    Refusal::Invalid {
        what: WHAT,
        why: text,
    }
}

/// What the refusal of a Program that exited unsuccessfully says, before what it wrote to stderr.
const EXITED: &str = "actual guest observer exited unsuccessfully";
const SAID: &str = "; its stderr:\n";

/// The Program's stderr a node keeps and shows (AC-T7.4.1): as many bytes as the refusal carrying
/// them ([`exited`]) can hold in one wire answer, sized by the refused answer's own codec, as the
/// Firecracker class sizes its VMM's output, so no size is configured or picked.
pub(super) fn stderr_room() -> Result<usize, Refusal> {
    let mut text = String::new();
    let bound = EXITED
        .len()
        .checked_add(SAID.len())
        .ok_or_else(|| invalid("observer refusal text overflows"))?;
    text.try_reserve_exact(bound)
        .map_err(|_| exhausted(bound))?;
    text.push_str(EXITED);
    text.push_str(SAID);
    let base = hex_core::wire::snapshot_refusal(
        &Refusal::Invalid {
            what: WHAT,
            why: text,
        },
        hex_core::wire::MAX_MESSAGE,
    )
    .map_err(|_| invalid("observer refusal exceeds the wire bound"))?
    .len();
    hex_core::wire::MAX_MESSAGE
        .checked_sub(base)
        .ok_or_else(|| invalid("observer refusal exceeds the wire bound"))
}

/// The refusal of a Program that exited unsuccessfully, carrying what it wrote to stderr, so the
/// person sees the stage and number hex-vulkan-guest-ready refused with. Lossy text can outgrow its
/// bytes (U+FFFD for an invalid one): its end is kept, as the Program's own refusal is its last line.
fn exited(stderr: &[u8]) -> Refusal {
    let Ok(room) = stderr_room() else {
        return invalid(EXITED);
    };
    if stderr.is_empty() {
        return invalid(EXITED);
    }
    let text = String::from_utf8_lossy(stderr);
    let mut start = text.len().saturating_sub(room);
    while start < text.len() && !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    let tail = text.get(start..).unwrap_or_default();
    let mut why = String::new();
    let bound = EXITED
        .len()
        .saturating_add(SAID.len())
        .saturating_add(tail.len());
    if why.try_reserve_exact(bound).is_err() {
        return invalid(EXITED);
    }
    why.push_str(EXITED);
    why.push_str(SAID);
    why.push_str(tail);
    Refusal::Invalid { what: WHAT, why }
}

fn exhausted(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "Shards guest GPU readiness bytes",
        bound,
    }
}

/// Exact worst-case v1 wire size: fixed words/spaces/LF, decimal type widths,
/// diagnostic UUID bytes and the pinned native string/heap ABI bounds.
pub(super) fn output_bound(expected: usize) -> Result<usize, Refusal> {
    if expected == 0 || expected > hex_api::MAX_DEVICES {
        return Err(invalid(
            "readiness requires the complete positive selected set",
        ));
    }
    let u32_digits = u32::MAX
        .ilog10()
        .checked_add(1)
        .ok_or_else(|| exhausted(expected))?;
    let u64_digits = u64::MAX
        .ilog10()
        .checked_add(1)
        .ok_or_else(|| exhausted(expected))?;
    let u32_digits = usize::try_from(u32_digits).map_err(|_| exhausted(expected))?;
    let u64_digits = usize::try_from(u64_digits).map_err(|_| exhausted(expected))?;
    let header = HEADER
        .len()
        .checked_add(u32_digits)
        .and_then(|n| n.checked_add(1));
    let device = 12usize
        .checked_mul(u32_digits)
        .and_then(|n| n.checked_add("device".len()))
        .and_then(|n| n.checked_add(16))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_add(16usize.checked_mul(2)?))
        .and_then(|n| {
            n.checked_add(
                3usize
                    .checked_mul(2)?
                    .checked_mul(NATIVE_TEXT_BYTES.checked_sub(1)?)?,
            )
        });
    let heap = 3usize
        .checked_mul(u32_digits)
        .and_then(|n| n.checked_add(u64_digits))
        .and_then(|n| n.checked_add("heap".len()))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(1));
    header
        .and_then(|header| {
            let per_device = heap?
                .checked_mul(usize::try_from(MAX_HEAPS).ok()?)?
                .checked_add(device?)?;
            expected.checked_mul(per_device)?.checked_add(header)
        })
        .ok_or_else(|| exhausted(expected))
}

fn number(token: &str) -> Result<u64, Refusal> {
    if token.is_empty()
        || !token.bytes().all(|byte| byte.is_ascii_digit())
        || (token.len() > 1 && token.starts_with('0'))
    {
        return Err(invalid("readiness number is not canonical decimal"));
    }
    token
        .parse()
        .map_err(|_| invalid("readiness number exceeds its native type"))
}

fn word(token: &str) -> Result<u32, Refusal> {
    u32::try_from(number(token)?).map_err(|_| invalid("readiness u32 exceeds its native type"))
}

fn fields<const N: usize>(line: &str) -> Result<[&str; N], Refusal> {
    let mut tokens = line.split(' ');
    let mut fields = [""; N];
    for field in &mut fields {
        *field = tokens
            .next()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| invalid("readiness row lacks a canonical field"))?;
    }
    if tokens.next().is_some() {
        return Err(invalid("readiness row has additional fields"));
    }
    Ok(fields)
}

fn lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn native_text(token: &str, required: Option<&[u8]>) -> Result<(), Refusal> {
    if token == "-" {
        return if required.is_none_or(|bytes| bytes.is_empty()) {
            Ok(())
        } else {
            Err(invalid("readiness lost its required driver name"))
        };
    }
    let (pairs, tail) = token.as_bytes().as_chunks::<2>();
    if pairs.is_empty()
        || pairs.len() >= NATIVE_TEXT_BYTES
        || !tail.is_empty()
        || !token.bytes().all(lower_hex)
    {
        return Err(invalid(
            "readiness name exceeds its native terminated UTF-8 array",
        ));
    }
    let mut decoded = [0u8; NATIVE_TEXT_BYTES - 1];
    for (pair, output) in pairs.iter().zip(decoded.iter_mut()) {
        *output = u8::from_str_radix(
            std::str::from_utf8(pair).map_err(|_| invalid("readiness hex is not ASCII"))?,
            16,
        )
        .map_err(|_| invalid("readiness hex is invalid"))?;
    }
    let decoded = decoded
        .get(..pairs.len())
        .ok_or_else(|| invalid("readiness name changed length"))?;
    if decoded.contains(&0)
        || std::str::from_utf8(decoded).is_err()
        || required.is_some_and(|required| required != decoded)
    {
        return Err(invalid(
            "readiness has a foreign or malformed native driver string",
        ));
    }
    Ok(())
}

/// Owned checked evidence from one bounded guest workload. A duplicate/zero
/// diagnostic UUID is never used to join, choose or mint a host physical ID.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Report {
    text: String,
}

/// The original native scope and reviewed artifact/configuration identity bind
/// this diagnostic guest result to its one already-paid pool incarnation.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Evidence {
    pub scope: hex_core::record::NativeScope,
    pub configuration: [u8; 32],
    pub max_queues: u32,
    pub report: Report,
}

impl Evidence {
    pub fn check(
        &self,
        scope: hex_core::record::NativeScope,
        expected: usize,
    ) -> Result<(), Refusal> {
        if self.scope != scope || self.configuration == [0; 32] {
            return Err(invalid(
                "guest readiness lost its original scope or artifact identity",
            ));
        }
        self.report.check(expected, self.max_queues)
    }

    pub fn render(&self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        for byte in self.scope.bytes() {
            write!(out, "{byte:02x}")?;
        }
        write!(out, " ")?;
        for byte in self.configuration {
            write!(out, "{byte:02x}")?;
        }
        write!(out, " {} ", self.max_queues)?;
        for byte in self.report.text().as_bytes() {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }

    pub fn parse(
        text: &str,
        scope: hex_core::record::NativeScope,
        expected: usize,
    ) -> Result<Option<Self>, Refusal> {
        if text == "none" {
            return Ok(None);
        }
        let [observed_scope, configuration, queues, frame] = fields::<4>(text)?;
        let observed_scope = hex_core::record::NativeScope::from_bytes(hex_array(observed_scope)?);
        let configuration = hex_array(configuration)?;
        if observed_scope != scope || configuration == [0; 32] {
            return Err(invalid(
                "retained readiness has another scope or unknown artifacts",
            ));
        }
        let max_queues = word(queues)?;
        let bound = driver::output_bound(expected)?;
        let (pairs, tail) = frame.as_bytes().as_chunks::<2>();
        if pairs.is_empty()
            || pairs.len() > bound
            || !tail.is_empty()
            || !frame.bytes().all(lower_hex)
        {
            return Err(invalid(
                "retained readiness has invalid or excessive frame bytes",
            ));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(pairs.len())
            .map_err(|_| exhausted(bound))?;
        for pair in pairs {
            bytes.push(hex_byte(pair)?);
        }
        let evidence = Self {
            scope,
            configuration,
            max_queues,
            report: Report::parse(&bytes, expected, max_queues)?,
        };
        evidence.check(scope, expected)?;
        Ok(Some(evidence))
    }
}

/// The workload is an actual Process in this already-paid guest. Its ordinary
/// guest row is owned/removed by that guest's existing Runtime, never a HOST Box.
pub(super) fn observe(
    configured: &config::GpuObserver,
    booted: &crate::guest::Booted,
    receipt: &mut crate::guest::pool::Receipt,
    path: &std::path::Path,
    budget: crate::guest::managed::Budget,
) -> Result<(), Refusal> {
    let key = receipt.original.key;
    observe_receipt(
        configured,
        booted,
        &mut Receipt::Pool(receipt),
        path,
        budget,
        &|| crate::guest::pool::refused(key, hex_api::LifecycleCause::Uncertain),
    )
}

pub(super) fn observe_selected(
    configured: &config::GpuObserver,
    booted: &crate::guest::Booted,
    receipt: &mut crate::guest::selected::Record,
    path: &std::path::Path,
    budget: crate::guest::managed::Budget,
) -> Result<(), Refusal> {
    budget.remaining()?;
    let id = receipt.id.try_clone()?;
    observe_receipt(
        configured,
        booted,
        &mut Receipt::Selected(receipt),
        path,
        budget,
        &|| crate::guest::books::uncertain(&id).unwrap_or_else(std::convert::identity),
    )
}

fn observe_receipt(
    configured: &config::GpuObserver,
    booted: &crate::guest::Booted,
    receipt: &mut Receipt<'_>,
    path: &std::path::Path,
    budget: crate::guest::managed::Budget,
    uncertain: &dyn Fn() -> Refusal,
) -> Result<(), Refusal> {
    use hex_api::{Ask, BoxId, BoxSpec, ClassSpec, Exit, Stdio};
    use std::fmt::Write as _;
    let scope = receipt.scope().ok_or_else(uncertain)?;
    if receipt.attempt().is_some() || receipt.readiness().is_some() {
        return Err(uncertain());
    }
    let count = receipt.count();
    let output = configured.output_bytes(count)?;
    let mut name = String::new();
    let prefix = "hex-gpu-observer-";
    let length = scope
        .bytes()
        .len()
        .checked_mul(2)
        .and_then(|digits| digits.checked_add(prefix.len()))
        .ok_or_else(|| exhausted(count))?;
    name.try_reserve_exact(length)
        .map_err(|_| exhausted(length))?;
    name.push_str(prefix);
    for byte in scope.bytes() {
        write!(&mut name, "{byte:02x}").map_err(|_| exhausted(length))?;
    }
    let id = BoxId::try_new(&name)?;
    let requested = id.try_clone()?;
    let program = driver::program(configured, count, budget.remaining()?)?;
    let bounds = configured.bounds();
    let spec = BoxSpec {
        id: requested,
        class: ClassSpec::Process,
        owner: None,
        allocation: None,
        route: None,
        devices: Vec::new(),
        bounds: hex_api::Bounds {
            cpu_weight: bounds.cpu_weight,
            cpu_max_milli: bounds.cpu_max_milli,
            memory_max: bounds.memory_max,
            memory_high: bounds.memory_high,
            io_weight: bounds.io_weight,
            io_latency_us: bounds.io_latency_us,
            io_max: bounds.io_max,
            pids_max: bounds.pids_max,
        },
    };
    spec.check_run()?;
    receipt.set_attempt(Attempt {
        configuration: configured.identity(),
        max_queues: configured.queues(),
        phase: AttemptPhase::Launching,
        stdout: Captured::new(output)?,
        stderr: Captured::new(0)?,
        facts: Some(facts::Facts::new()),
    });
    // The Program's stderr kept: as much as both of its carriers hold, the refusal that shows it
    // (one wire answer, stderr_room) and this receipt beside all else it holds from launch, its
    // three fact snapshots among them: two hex digits a byte, less the most its limit's decimal adds.
    let digits = usize::try_from(usize::MAX.ilog10())
        .ok()
        .and_then(|digits| digits.checked_add(1))
        .ok_or_else(|| exhausted(receipt.bound()))?;
    let held = receipt
        .encode()?
        .len()
        .checked_add(
            receipt
                .attempt()
                .ok_or_else(uncertain)?
                .additional_receipt_room()?,
        )
        .and_then(|bytes| bytes.checked_add(digits))
        .and_then(|bytes| receipt.bound().checked_sub(bytes))
        .and_then(|bytes| bytes.checked_div(2))
        .ok_or_else(|| exhausted(receipt.bound()))?;
    receipt.attempt_mut().ok_or_else(uncertain)?.stderr =
        Captured::new(configured.stderr_bytes().min(held))?;
    let initial = receipt.encode()?;
    let extra = receipt
        .attempt()
        .ok_or_else(uncertain)?
        .additional_receipt_room()?;
    if initial
        .len()
        .checked_add(extra)
        .is_none_or(|bytes| bytes > receipt.bound())
    {
        return Err(exhausted(receipt.bound()));
    }
    drop(initial);
    // The failed-stage marker and exact ordinary guest ID derivation survive an
    // interrupted launch. Recovery never retries or upgrades that attempt.
    receipt.write(path)?;
    budget.remaining()?;
    let mut process = match booted.client.run_before(
        spec,
        program,
        Stdio {
            ask: Ask {
                stdin: false,
                stdout: true,
                stderr: true,
            },
            ..Stdio::null()
        },
        budget.deadline(),
    ) {
        Ok(process) => process,
        Err(why) => {
            // A refused/timed-out launch may have reached the guest. Its exact
            // known guest ID is the cleanup authority; no PID lookup/retry.
            return failed(why, booted, receipt, path, &id, budget, uncertain);
        }
    };
    let result = (|| {
        let given = process.streams()?;
        let mut streams = io::Streams::new(given)?;
        receipt.attempt_mut().ok_or_else(uncertain)?.phase = AttemptPhase::Draining;
        receipt.write(path)?;
        let attempt = receipt.attempt_mut().ok_or_else(uncertain)?;
        streams.drain(&mut attempt.stdout, &mut attempt.stderr, budget)?;
        attempt.phase = AttemptPhase::Waiting;
        receipt.write(path)?;
        let exit = process
            .wait_for(budget.remaining()?)?
            .ok_or_else(uncertain)?;
        facts_mut(receipt, uncertain)?.exited(exit)?;
        receipt.write(path)?;
        if exit != Exit::Code(0) {
            let attempt = receipt.attempt().ok_or_else(uncertain)?;
            return Err(exited(attempt.stderr.bytes()?));
        }
        budget.remaining()?;
        receipt.attempt_mut().ok_or_else(uncertain)?.phase = AttemptPhase::Deleting;
        receipt.write(path)?;
        cleanup(booted, receipt, path, &id, budget, uncertain)?;
        budget.remaining()?;
        let attempt = receipt.attempt().ok_or_else(uncertain)?;
        let report = Report::parse(attempt.stdout.bytes()?, count, configured.queues())?;
        budget.remaining()?;
        receipt.set_readiness(Evidence {
            scope,
            configuration: configured.identity(),
            max_queues: configured.queues(),
            report,
        });
        receipt.attempt_mut().ok_or_else(uncertain)?.phase = AttemptPhase::Complete;
        receipt.write(path)?;
        receipt.checked_readiness()?;
        Ok(())
    })();
    if let Err(why) = result {
        // No extra deadline/reaper: an expired delete retains the canonical
        // Pool grant/attempt. Closing asked sockets cannot prove row/scope gone.
        return failed(why, booted, receipt, path, &id, budget, uncertain);
    }
    Ok(())
}

fn facts_mut<'a>(
    receipt: &'a mut Receipt<'_>,
    uncertain: &dyn Fn() -> Refusal,
) -> Result<&'a mut facts::Facts, Refusal> {
    receipt
        .attempt_mut()
        .and_then(|attempt| attempt.facts.as_mut())
        .ok_or_else(uncertain)
}

fn cleanup(
    booted: &crate::guest::Booted,
    receipt: &mut Receipt<'_>,
    path: &std::path::Path,
    id: &hex_api::BoxId,
    budget: crate::guest::managed::Budget,
    uncertain: &dyn Fn() -> Refusal,
) -> Result<(), Refusal> {
    facts_mut(receipt, uncertain)?.cleaning();
    receipt.write(path)?;
    let outcome = booted.client.delete_before(id, budget.deadline());
    let captured = match &outcome {
        Ok(()) => {
            facts_mut(receipt, uncertain)?.deleted(false);
            Ok(())
        }
        Err(Refusal::NoSuchBox(gone)) if gone == id => {
            facts_mut(receipt, uncertain)?.deleted(true);
            Ok(())
        }
        Err(why) => facts_mut(receipt, uncertain)?.cleanup_failed(why),
    };
    let retained = receipt.write(path);
    captured?;
    retained?;
    match outcome {
        Ok(()) => Ok(()),
        Err(Refusal::NoSuchBox(gone)) if gone == *id => Ok(()),
        Err(why) => Err(why),
    }
}

fn failed(
    why: Refusal,
    booted: &crate::guest::Booted,
    receipt: &mut Receipt<'_>,
    path: &std::path::Path,
    id: &hex_api::BoxId,
    budget: crate::guest::managed::Budget,
    uncertain: &dyn Fn() -> Refusal,
) -> Result<(), Refusal> {
    let phase = receipt.attempt().ok_or_else(uncertain)?.phase;
    // Save the original failed stage/cause before cleanup can produce another
    // error. A codec/write failure still allows the same bounded cleanup attempt;
    // it never manufactures a fact or changes the native grant.
    let original = facts_mut(receipt, uncertain)?
        .failed(phase, &why)
        .and_then(|()| receipt.write(path));
    let cleanup = if facts_mut(receipt, uncertain)?.needs_cleanup() {
        cleanup(booted, receipt, path, id, budget, uncertain)
    } else {
        Ok(())
    };
    let retained = receipt.write(path);
    original?;
    retained?;
    if facts_mut(receipt, uncertain)?.cleaned() {
        Err(why)
    } else {
        if !facts_mut(receipt, uncertain)?.failed_cleanup_recorded() {
            cleanup?;
        }
        Err(uncertain())
    }
}

fn hex_byte(pair: &[u8; 2]) -> Result<u8, Refusal> {
    u8::from_str_radix(
        std::str::from_utf8(pair).map_err(|_| invalid("readiness identity is not ASCII"))?,
        16,
    )
    .map_err(|_| invalid("readiness identity byte is invalid"))
}

fn hex_array<const N: usize>(text: &str) -> Result<[u8; N], Refusal> {
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    if pairs.len() != N || !tail.is_empty() || !text.bytes().all(lower_hex) {
        return Err(invalid(
            "readiness identity is not exact lowercase hexadecimal",
        ));
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        *byte = hex_byte(pair)?;
    }
    Ok(bytes)
}

impl Report {
    pub fn parse(bytes: &[u8], expected: usize, max_queues: u32) -> Result<Self, Refusal> {
        let bound = driver::output_bound(expected)?;
        if bytes.len() > bound {
            return Err(exhausted(bound));
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| invalid("readiness frame is not ASCII"))?;
        Self::check_text(text, expected, max_queues)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| exhausted(bound))?;
        owned.push_str(text);
        Ok(Self { text: owned })
    }

    pub fn check(&self, expected: usize, max_queues: u32) -> Result<(), Refusal> {
        if self.text.len() > driver::output_bound(expected)? {
            return Err(exhausted(self.text.len()));
        }
        Self::check_text(&self.text, expected, max_queues)
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn check_text(text: &str, expected: usize, max_queues: u32) -> Result<(), Refusal> {
        if text.starts_with(driver::HEADER) {
            return driver::check(text, expected, max_queues);
        }
        let mut lines = frame(text, HEADER, expected, max_queues)?;
        for index in 0..expected {
            let count = device(lines.next(), index, max_queues)?;
            heaps(&mut lines, index, count)?;
        }
        if lines.next().is_some() {
            return Err(invalid(
                "readiness frame has extra devices or trailing bytes",
            ));
        }
        Ok(())
    }
}

fn frame<'a>(
    text: &'a str,
    header: &str,
    expected: usize,
    max_queues: u32,
) -> Result<std::str::SplitTerminator<'a, char>, Refusal> {
    if max_queues == 0 || !text.is_ascii() || text.contains('\r') || !text.ends_with('\n') {
        return Err(invalid("readiness frame or queue bound is not canonical"));
    }
    let mut lines = text.split_terminator('\n');
    let count = lines
        .next()
        .and_then(|line| line.strip_prefix(header))
        .ok_or_else(|| invalid("readiness frame has a foreign header"))?;
    let count =
        usize::try_from(word(count)?).map_err(|_| invalid("readiness count exceeds this host"))?;
    if count != expected || count == 0 || count > hex_api::MAX_DEVICES {
        return Err(invalid(
            "readiness visible count differs from the full selected set",
        ));
    }
    Ok(lines)
}

fn device(row: Option<&str>, index: usize, max_queues: u32) -> Result<u32, Refusal> {
    let row = row.ok_or_else(|| invalid("readiness device row is missing"))?;
    let [
        kind,
        ordinal,
        api,
        driver,
        driver_version,
        vendor,
        device,
        device_type,
        uuid,
        device_name,
        driver_name,
        driver_info,
        queues,
        compute,
        queue_count,
        queue_flags,
        heaps,
    ] = fields::<17>(row)?;
    let ordinal = usize::try_from(word(ordinal)?)
        .map_err(|_| invalid("readiness device index exceeds this host"))?;
    let api = word(api)?;
    let queues = word(queues)?;
    let compute = word(compute)?;
    if kind != "device"
        || ordinal != index
        || api >> 29 != 0
        || api < ((1u32 << 22) | (2u32 << 12))
        || word(driver)? != VENUS
        || !matches!(word(device_type)?, 1..=3)
        || queues == 0
        || queues > max_queues
        || compute >= queues
        || word(queue_count)? == 0
        || word(queue_flags)? & COMPUTE == 0
    {
        return Err(invalid(
            "readiness has a foreign driver, device or compute queue",
        ));
    }
    for scalar in [driver_version, vendor, device] {
        word(scalar)?;
    }
    if uuid.len() != UUID_HEX_BYTES || !uuid.bytes().all(lower_hex) {
        return Err(invalid(
            "readiness diagnostic UUID is not native lowercase hex",
        ));
    }
    native_text(device_name, None)?;
    native_text(driver_name, Some(b"venus"))?;
    native_text(driver_info, None)?;
    let heaps = word(heaps)?;
    if heaps == 0 || heaps > MAX_HEAPS {
        return Err(invalid("readiness heap count exceeds its native ABI"));
    }
    Ok(heaps)
}

fn heaps(
    lines: &mut std::str::SplitTerminator<'_, char>,
    index: usize,
    heaps: u32,
) -> Result<(), Refusal> {
    let mut local = 0u64;
    for heap in 0..heaps {
        let row = lines
            .next()
            .ok_or_else(|| invalid("readiness heap row is missing"))?;
        let [kind, device, ordinal, flags, size] = fields::<5>(row)?;
        if kind != "heap"
            || word(device)? != u32::try_from(index).map_err(|_| exhausted(index))?
            || word(ordinal)? != heap
        {
            return Err(invalid("readiness heap order or device changed"));
        }
        let size = number(size)?;
        if word(flags)? & DEVICE_LOCAL != 0 {
            local = local
                .checked_add(size)
                .ok_or_else(|| invalid("readiness device-local heap sum overflows"))?;
        }
    }
    if local == 0 {
        return Err(invalid("readiness has no actual nonzero device-local heap"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(count: usize) -> String {
        let mut text = format!("{HEADER}{count}\n");
        for index in 0..count {
            // Deliberately identical UUID/model: pinned Venus may derive UUID
            // from vendor+device alone. They cannot be a host identity oracle.
            text.push_str(&format!(
                "device {index} 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 2\nheap {index} 0 1 4096\nheap {index} 1 0 8192\n",
                "ab".repeat(16)
            ));
        }
        text
    }

    /// AC-T7.4.1: full guest readiness is distinct from physical identity, and
    /// neither UUID collision nor multiple actual memory heaps selects a host GPU.
    #[test]
    fn complete_venus_readiness_keeps_duplicate_diagnostic_uuids_and_all_heaps() {
        assert_eq!(output_bound(1).unwrap(), 40 + 2649);
        let text = report(2);
        let parsed = Report::parse(text.as_bytes(), 2, 2).unwrap();
        assert_eq!(parsed.text(), text);
        parsed.check(2, 2).unwrap();
        assert!(parsed.check(1, 2).is_err());
        assert!(parsed.check(2, 1).is_err());
    }

    /// AC-T7.4.1: count/driver/queue/property failures reject the complete
    /// workload result; no partial device filtering can manufacture readiness.
    #[test]
    fn hostile_guest_inventory_never_becomes_partial_ready() {
        let good = report(1);
        for bad in [
            good.replace("inventory 1 1", "inventory 1 0"),
            good.replace("inventory 1 1", "inventory 1 4294967295"),
            good.replace("4202496 22", "4202496 21"),
            good.replace("4202496 22", "4194304 22"),
            good.replace("1 1 2 2\n", "2 1 2 2\n"),
            good.replace("76656e7573", "76656e7574"),
            good.replace("76656e7573", "76656E7573"),
            good.replace("heap 0 0 1 4096", "heap 0 0 1 0"),
            good.replace("heap 0 1 0 8192", "heap 0 0 0 8192"),
            good.replace("device 0", "device +0"),
            good.replace("device 0", "device 00"),
            good.replace("device 0", "device  0"),
            good.replace('\n', "\r\n"),
            format!("{good}\n"),
            good.trim_end().to_owned(),
        ] {
            assert!(Report::parse(bad.as_bytes(), 1, 2).is_err(), "{bad}");
        }
        assert!(Report::parse(good.as_bytes(), 0, 2).is_err());
        assert!(Report::parse(good.as_bytes(), 1, 0).is_err());
    }

    /// AC-T7.4.1: a retained result cannot move across scope/artifact identity,
    /// expand its selected set, hide a malformed name or overflow heap totals.
    #[test]
    fn retained_readiness_is_scoped_and_never_repairs_hostile_properties() {
        let scope = hex_core::record::NativeScope::from_bytes([7; 16]);
        let evidence = Evidence {
            scope,
            configuration: [9; 32],
            max_queues: 2,
            report: Report::parse(report(1).as_bytes(), 1, 2).unwrap(),
        };
        let mut text = String::new();
        evidence.render(&mut text).unwrap();
        assert_eq!(Evidence::parse(&text, scope, 1).unwrap(), Some(evidence));
        assert!(
            Evidence::parse(&text, hex_core::record::NativeScope::from_bytes([8; 16]), 1).is_err()
        );
        assert!(Evidence::parse(&text, scope, 2).is_err());
        for bad in [
            report(1).replace("677075", "670070"),
            report(1).replace("677075", "ff"),
            report(1).replace("heap 0 1 0 8192", "heap 0 1 1 18446744073709551615"),
            report(1).replace("677075", &"61".repeat(NATIVE_TEXT_BYTES)),
        ] {
            assert!(Report::parse(bad.as_bytes(), 1, 2).is_err());
        }
    }

    /// AC-T7.4.1: partial data/EOF state survives the same VM receipt; neither
    /// output content alone nor an incomplete stream phase is readiness.
    #[test]
    fn retained_observer_attempt_preserves_partial_eof_and_overrun_witness() {
        let text = format!("{} 2 draining open 2 616263 eof 2 -", "09".repeat(32));
        let attempt = Attempt::parse(&text).unwrap().unwrap();
        assert!(!attempt.finished_streams());
        let mut encoded = String::new();
        attempt.render(&mut encoded).unwrap();
        assert_eq!(encoded, text);
        assert!(Attempt::parse(&text.replace("616263", "61626364")).is_err());
        assert!(Attempt::parse(&text.replace("draining", "success")).is_err());
        assert!(Attempt::parse(&text.replace(" 2 draining", " 0 draining")).is_err());
        assert!(Attempt::parse(&format!("{text} extra")).is_err());
    }

    /// AC-T7.4.1: give an unsuccessful Program as much stderr as the node keeps and expect its refusal
    /// to fill one wire answer exactly; give it more and expect the refusal to keep the end, where the
    /// Program's own refusal line is, still within one answer; give it none and expect the plain refusal.
    #[test]
    fn an_unsuccessful_programs_stderr_fills_its_refusal_to_the_wire_bound() {
        let room = stderr_room().unwrap();
        let full = exited(&vec![b'x'; room]);
        let encoded = hex_core::wire::snapshot_refusal(&full, hex_core::wire::MAX_MESSAGE).unwrap();
        assert_eq!(encoded.len(), hex_core::wire::MAX_MESSAGE);
        let mut over = vec![0xff; room];
        over.extend_from_slice(b"hex-vulkan-guest-ready: queue-families 6\n");
        let kept = exited(&over);
        assert!(hex_core::wire::snapshot_refusal(&kept, hex_core::wire::MAX_MESSAGE).is_ok());
        let Refusal::Invalid { what, why } = kept else {
            panic!("{kept:?}");
        };
        assert_eq!(what, WHAT);
        assert!(why.starts_with("actual guest observer exited unsuccessfully; its stderr:\n"));
        assert!(
            why.ends_with("hex-vulkan-guest-ready: queue-families 6\n"),
            "{why}"
        );
        assert_eq!(exited(b""), invalid(EXITED));
    }
}
