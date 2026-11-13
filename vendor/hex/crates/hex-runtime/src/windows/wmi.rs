//! A WQL query of the local `ROOT\CIMV2` namespace, made in process through WMI's COM interface
//! (native plan step 6, HEX-INST-9; docs/research/runtime/installer-per-os.md §2.6): the node reads the
//! installer's features here rather than starting PowerShell for them. COM's calls are foreign
//! functions the `windows` crate binds as `unsafe` (CLAUDE.md §12), so they sit in this file alone.
//!
//! The read is bounded twice over: each `IEnumWbemClassObject::Next` waits at most the time left to the
//! deadline (WMI's own timeout, `WBEM_S_TIMEDOUT` past it), and the caller waits for the reading thread
//! no longer than the deadline, so a connect that never answers is refused too. One read is in flight at
//! a time: a caller asking while one is under way waits for its answer rather than starting another.
#![allow(unsafe_code)]

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use hex_api::Refusal;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    CoSetProxyBlanket, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL,
    RPC_C_IMP_LEVEL_IMPERSONATE,
};
use windows::Win32::System::Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE};
use windows::Win32::System::Variant::{VARIANT, VT_BSTR, VT_I4};
use windows::Win32::System::Wmi::{
    IEnumWbemClassObject, IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_FORWARD_ONLY,
    WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_S_FALSE, WBEM_S_TIMEDOUT, WbemLocator,
};
use windows::core::{BSTR, HSTRING, PCWSTR};

/// What a read answers: each instance's text and number.
type Rows = Result<Vec<(String, u32)>, Refusal>;

/// Every input affecting an answer must match before two callers share it (HEX-INST-9).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Request {
    wql: &'static str,
    text: &'static str,
    number: &'static str,
    most: usize,
    patience: Duration,
}

/// The result belongs to this read, so the next read cannot overwrite a waiting caller's answer.
struct Read {
    request: Request,
    answer: Mutex<Option<Rows>>,
    ended: Condvar,
}

impl Read {
    fn wait(&self, deadline: Instant) -> Rows {
        let answer = self.answer.lock().map_err(poisoned)?;
        let (answer, _) = self
            .ended
            .wait_timeout_while(
                answer,
                deadline.saturating_duration_since(Instant::now()),
                |a| a.is_none(),
            )
            .map_err(poisoned)?;
        answer
            .as_ref()
            .cloned()
            .unwrap_or_else(|| Err(late(self.request.patience)))
    }
}

/// At most one reading thread, even if a COM connect never answers. Completed results are retained
/// only as the last read and by its callers; a new caller starts a fresh read, without a timed cache.
struct Reads {
    current: Mutex<Option<Arc<Read>>>,
}

impl Reads {
    const fn new() -> Self {
        Self {
            current: Mutex::new(None),
        }
    }

    fn start(
        &self,
        request: Request,
        run: impl FnOnce() -> Rows + Send + 'static,
    ) -> Result<Arc<Read>, Refusal> {
        let mut current = self.current.lock().map_err(poisoned)?;
        if let Some(read) = current.as_ref()
            && read.answer.lock().map_err(poisoned)?.is_none()
        {
            return if read.request == request {
                Ok(Arc::clone(read))
            } else {
                Err(Refusal::os(OP, "a read of another query is under way"))
            };
        }
        let read = Arc::new(Read {
            request,
            answer: Mutex::new(None),
            ended: Condvar::new(),
        });
        let reading = Arc::clone(&read);
        std::thread::Builder::new()
            .name("hex-wmi".to_owned())
            .spawn(move || {
                let rows = run();
                // If this lock was poisoned, every waiter already receives that typed refusal;
                // there is no caller to which the result can be delivered through it.
                if let Ok(mut answer) = reading.answer.lock() {
                    *answer = Some(rows);
                }
                reading.ended.notify_all();
            })
            .map_err(|e| Refusal::os("starting WMI's reading thread", e))?;
        *current = Some(Arc::clone(&read));
        Ok(read)
    }
}

static READS: Reads = Reads::new();
const OP: &str = "reading WMI";

/// Each instance `wql` selects, as its `text` and `number` properties' values, at most `most` of them,
/// within `patience`. An identical query already under way is shared; an overlong answer or a read
/// past the deadline is refused by name. A timed-out reader remains the sole reader until it ends.
pub fn query(
    wql: &'static str,
    text: &'static str,
    number: &'static str,
    most: usize,
    patience: Duration,
) -> Rows {
    let deadline = Instant::now()
        .checked_add(patience)
        .ok_or_else(|| Refusal::os(OP, "the deadline is past the clock's range"))?;
    READS
        .start(
            Request {
                wql,
                text,
                number,
                most,
                patience,
            },
            move || read(wql, text, number, most, deadline, patience),
        )?
        .wait(deadline)
}

/// The refusal for a reads' lock a caller panicked holding.
fn poisoned<T>(_: T) -> Refusal {
    Refusal::os(OP, "a caller panicked holding the reads' lock")
}

/// The refusal for a read past `patience`, the bound stated as it is.
fn late(patience: Duration) -> Refusal {
    Refusal::Os {
        op: OP,
        why: format!(
            "the WMI query didn't finish within {} ms",
            patience.as_millis()
        ),
    }
}

/// [`query`]'s read, on the calling thread, which it initializes for COM and leaves as it found it.
fn read(
    wql: &str,
    text: &str,
    number: &str,
    most: usize,
    deadline: Instant,
    patience: Duration,
) -> Rows {
    if Instant::now() >= deadline {
        return Err(late(patience));
    }
    // SAFETY: called on a thread that hasn't initialized COM (query's own) or that pairs it with the
    // CoUninitialize below; no COM object of this read outlives that call (`rows` drops every one).
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        return Err(Refusal::os("starting COM", windows::core::Error::from(hr)));
    }
    let rows = rows(wql, text, number, most, deadline, patience);
    // SAFETY: CoInitializeEx succeeded on this thread above, and `rows` has released every interface.
    unsafe { CoUninitialize() };
    rows
}

/// The instances, read by `deadline` (`patience` from the read's start).
fn rows(
    wql: &str,
    text: &str,
    number: &str,
    most: usize,
    deadline: Instant,
    patience: Duration,
) -> Rows {
    let com = |op: &'static str| move |e: windows::core::Error| Refusal::os(op, e);
    // SAFETY: COM is initialized on this thread (read); WbemLocator is WMI's in-process locator class.
    let locator: IWbemLocator =
        unsafe { CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER) }
            .map_err(com("making WMI's locator"))?;
    let none = BSTR::new();
    // SAFETY: every argument is a valid BSTR (empty: this machine, this user) and no context object.
    let services: IWbemServices = unsafe {
        locator.ConnectServer(
            &BSTR::from(r"ROOT\CIMV2"),
            &none,
            &none,
            &none,
            0,
            &none,
            None,
        )
    }
    .map_err(com(r"connecting to WMI's ROOT\CIMV2"))?;
    // SAFETY: `services` is a live proxy; the blanket is the one Microsoft's local-WMI client sets
    // (Windows authentication, call-level, impersonate) with no principal name or identity of its own.
    unsafe {
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            PCWSTR::null(),
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
    }
    .map_err(com("setting WMI's proxy security"))?;
    // SAFETY: valid BSTRs and no context; semisynchronous and forward-only, so Next bounds each wait.
    let found: IEnumWbemClassObject = unsafe {
        services.ExecQuery(
            &BSTR::from("WQL"),
            &BSTR::from(wql),
            WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
            None,
        )
    }
    .map_err(com("querying WMI"))?;
    let mut out = Vec::new();
    // One Next per instance and one more for the end: past that the answer is longer than asked for.
    for _ in 0..=most {
        let left = deadline.saturating_duration_since(Instant::now());
        let wait = i32::try_from(left.as_millis()).unwrap_or(i32::MAX);
        if wait == 0 {
            return Err(late(patience));
        }
        let mut slot: [Option<IWbemClassObject>; 1] = [None];
        let mut got = 0u32;
        // SAFETY: `slot` holds one interface pointer Next may fill, and `got` is where it counts it.
        let hr = unsafe { found.Next(wait, &mut slot, &mut got) };
        if hr.is_err() {
            return Err(Refusal::os(
                "reading WMI's answer",
                windows::core::Error::from(hr),
            ));
        }
        if let [Some(row)] = slot {
            if out.len() >= most {
                break;
            }
            out.push((
                property(&row, text, string_value)?,
                property(&row, number, uint32_value)?,
            ));
        }
        if hr.0 == WBEM_S_FALSE.0 {
            return Ok(out);
        }
        if hr.0 == WBEM_S_TIMEDOUT.0 {
            return Err(late(patience));
        }
    }
    Err(Refusal::Exhausted {
        what: "the WMI query's instances",
        bound: most,
    })
}

/// `row`'s property `name`, converted by `as_value`.
fn property<T>(
    row: &IWbemClassObject,
    name: &str,
    as_value: impl Fn(&VARIANT) -> Result<T, String>,
) -> Result<T, Refusal> {
    let mut value = VARIANT::default();
    let wide = HSTRING::from(name);
    // SAFETY: `wide` is a NUL-terminated name that outlives the call; `value` is an empty VARIANT Get
    // fills (and VARIANT's Drop clears); no type or flavor is asked for.
    unsafe { row.Get(&wide, 0, &mut value, None, None) }
        .map_err(|e| Refusal::os("reading a WMI property", e))?;
    as_value(&value).map_err(|e| Refusal::Invalid {
        what: "a WMI property",
        why: format!("{name}: {e}"),
    })
}

/// A CIM `string`, which WMI returns as VT_BSTR. Read from the VARIANT itself: the `windows` crate's
/// VARIANT conversions call propsys.dll, which nanoserver doesn't carry, and hex.exe is an `oci` box's
/// launcher there, so importing it stops the launcher loading (STATUS_DLL_NOT_FOUND) before it replies.
fn string_value(value: &VARIANT) -> Result<String, String> {
    if value.vt() != VT_BSTR {
        return Err(format!(
            "VARIANT type {} is not a string (VT_BSTR)",
            value.vt().0
        ));
    }
    // SAFETY: the tag is VT_BSTR, so `bstrVal` is the union's live field: the BSTR `Get` stored, which
    // `value` owns until its Drop clears it. This only borrows it to copy its text.
    Ok(unsafe { &value.Anonymous.Anonymous.Anonymous.bstrVal }.to_string())
}

/// A CIM `uint32`, which WMI returns as VT_I4 carrying its 32 bits, Automation having no larger integer
/// (Microsoft, "Numbers (WMI)"). Read from the VARIANT itself, for [`string_value`]'s reason.
fn uint32_value(value: &VARIANT) -> Result<u32, String> {
    if value.vt() != VT_I4 {
        return Err(format!(
            "VARIANT type {} is not a uint32 (VT_I4)",
            value.vt().0
        ));
    }
    // SAFETY: the tag is VT_I4, so `lVal` is the union's live field, a plain i32.
    let bits = unsafe { value.Anonymous.Anonymous.Anonymous.lVal };
    Ok(u32::from_ne_bytes(bits.to_ne_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            wql: hex_core::setup::FEATURE_QUERY,
            text: "Name",
            number: "InstallState",
            most: hex_core::setup::WindowsFeature::ALL.len(),
            patience: hex_core::setup::FEATURE_QUERY_PATIENCE,
        }
    }

    fn deadline() -> Instant {
        Instant::now().checked_add(request().patience).unwrap()
    }

    /// HEX-INST-9: do: decode the VARIANT types WMI returns for a CIM string and uint32, and each
    /// other's type; expect the text, the uint32's 32 bits (VT_I4 -1 is u32::MAX), and a refusal
    /// naming the type it needed.
    #[test]
    fn properties_decode_from_the_variant_type_wmi_returns() {
        assert_eq!(
            string_value(&VARIANT::from("Containers")),
            Ok("Containers".to_owned())
        );
        assert_eq!(uint32_value(&VARIANT::from(2i32)), Ok(2));
        assert_eq!(uint32_value(&VARIANT::from(-1i32)), Ok(u32::MAX));
        assert!(
            string_value(&VARIANT::from(2i32))
                .unwrap_err()
                .contains("VT_BSTR")
        );
        assert!(
            uint32_value(&VARIANT::from("2"))
                .unwrap_err()
                .contains("VT_I4")
        );
        assert!(uint32_value(&VARIANT::default()).is_err());
    }

    /// HEX-INST-9: while the reader is held, a second identical caller joins that exact read; after
    /// completion, a fresh query cannot replace the answer still owned by either earlier caller.
    #[test]
    fn joined_callers_keep_their_answer_across_the_next_read() {
        let reads = Reads::new();
        let (release, held) = std::sync::mpsc::sync_channel(0);
        let first = reads
            .start(request(), move || {
                held.recv().unwrap();
                Ok(vec![("first".to_owned(), 1)])
            })
            .unwrap();
        let joined = reads
            .start(request(), || panic!("joined query started a second reader"))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &joined));
        release.send(()).unwrap();
        assert_eq!(first.wait(deadline()), Ok(vec![("first".to_owned(), 1)]));
        let next = reads
            .start(request(), || Ok(vec![("next".to_owned(), 2)]))
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &next));
        assert_eq!(next.wait(deadline()), Ok(vec![("next".to_owned(), 2)]));
        assert_eq!(joined.wait(deadline()), Ok(vec![("first".to_owned(), 1)]));
    }

    /// HEX-INST-9: all inputs describe the answer. Equal WQL with a different property, bound or
    /// patience cannot join an existing query; refusing it never starts another reader.
    #[test]
    fn a_different_request_cannot_join_the_in_flight_read() {
        let reads = Reads::new();
        let (release, held) = std::sync::mpsc::sync_channel(0);
        let first = reads
            .start(request(), move || {
                held.recv().unwrap();
                Ok(Vec::new())
            })
            .unwrap();
        let r = request();
        let changed = [
            Request {
                wql: "SELECT Name FROM Win32_Process",
                ..r
            },
            Request {
                text: "Caption",
                ..r
            },
            Request {
                number: "Status",
                ..r
            },
            Request {
                most: r.most + 1,
                ..r
            },
            Request {
                patience: Duration::ZERO,
                ..r
            },
        ];
        for different in changed {
            let refused = reads.start(different, || {
                panic!("different query started a second reader")
            });
            assert!(
                matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("another query"))
            );
        }
        release.send(()).unwrap();
        assert_eq!(first.wait(deadline()), Ok(Vec::new()));
    }

    /// HEX-INST-9: a caller's expired wait does not free the single reader slot or launch another
    /// potentially stuck COM thread. Joined callers still receive that reader's eventual refusal.
    #[test]
    fn an_expired_wait_keeps_the_in_flight_read_shared() {
        let reads = Reads::new();
        let (release, held) = std::sync::mpsc::sync_channel(0);
        let first = reads
            .start(request(), move || {
                held.recv().unwrap();
                Err(Refusal::os(OP, "the original read failed"))
            })
            .unwrap();
        let refused = first.wait(Instant::now());
        assert!(
            matches!(refused, Err(Refusal::Os { why, .. }) if why.contains("didn't finish within"))
        );
        let joined = reads
            .start(request(), || {
                panic!("expired waiter started another reader")
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first, &joined));
        release.send(()).unwrap();
        assert_eq!(
            joined.wait(deadline()),
            Err(Refusal::os(OP, "the original read failed"))
        );
    }

    /// HEX-INST-9 (native plan step 6): a read whose deadline has passed is refused by name, not waited
    /// on, and the refusal states its bound as it is (in ms: whole seconds read 4,842 ms as 4 s). The read
    /// runs on this thread, so the case leaves no reading thread behind.
    #[test]
    fn a_read_past_its_deadline_is_refused_with_its_bound_in_ms() {
        let patience = hex_core::setup::FEATURE_QUERY_PATIENCE;
        let refused = read(
            hex_core::setup::FEATURE_QUERY,
            "Name",
            "InstallState",
            hex_core::setup::WindowsFeature::ALL.len(),
            Instant::now(),
            patience,
        )
        .unwrap_err();
        let bound = format!("didn't finish within {} ms", patience.as_millis());
        assert!(
            matches!(&refused, Refusal::Os { why, .. } if why.contains(&bound)),
            "{refused:?} doesn't name {bound}"
        );
    }

    /// HEX-INST-9: callers that ask while a read is under way share it rather than being refused (two oci
    /// creates on a fresh node both ask): ask from four threads at once, expect four answers, each the same.
    #[test]
    fn concurrent_callers_share_one_read() {
        let ask = || {
            query(
                hex_core::setup::FEATURE_QUERY,
                "Name",
                "InstallState",
                hex_core::setup::WindowsFeature::ALL.len(),
                hex_core::setup::FEATURE_QUERY_PATIENCE,
            )
        };
        let barrier = std::sync::Barrier::new(4);
        let answers: Vec<_> = std::thread::scope(|s| {
            let asked: Vec<_> = (0..4)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        ask()
                    })
                })
                .collect();
            asked.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let first = answers[0].clone().unwrap();
        for answer in &answers {
            assert_eq!(answer.as_ref(), Ok(&first), "{answers:?}");
        }
    }

    /// HEX-INST-9: the read returns each instance the query selects, here both installer features, each
    /// with an `InstallState` WMI defines (1 enabled, 2 disabled, 3 absent, 4 unknown).
    #[test]
    fn the_feature_query_reads_each_feature_and_its_state() {
        let rows = query(
            hex_core::setup::FEATURE_QUERY,
            "Name",
            "InstallState",
            hex_core::setup::WindowsFeature::ALL.len(),
            hex_core::setup::FEATURE_QUERY_PATIENCE,
        )
        .unwrap();
        let mut names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Containers", "HypervisorPlatform"], "{rows:?}");
        assert!(rows.iter().all(|(_, s)| (1..=4).contains(s)), "{rows:?}");
    }
}
