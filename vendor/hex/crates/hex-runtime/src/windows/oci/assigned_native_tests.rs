//! ROOT-invoked actual HCS completion witness. Compile-only for implementation agents.
use super::*;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum Outcome {
    Completed,
    InputRejected { field: &'static str },
    Refused { phase: &'static str, code: u32 },
    Uncertain { phase: &'static str },
}

struct Inputs {
    id: hex_api::BoxId,
    root: PathBuf,
    document: String,
    budget: Duration,
}

fn required(name: &'static str) -> Result<String, Outcome> {
    std::env::var(name).map_err(|_| Outcome::InputRejected { field: name })
}

impl Inputs {
    fn read() -> Result<Self, Outcome> {
        let id = hex_api::BoxId::new(&required("HEX_HCS_NATIVE_ID")?).map_err(|_| {
            Outcome::InputRejected {
                field: "HEX_HCS_NATIVE_ID",
            }
        })?;
        let root = PathBuf::from(required("HEX_HCS_NATIVE_ROOT")?);
        let config = PathBuf::from(required("HEX_HCS_NATIVE_CONFIG")?);
        if !root.is_absolute() || !config.is_absolute() {
            return Err(Outcome::InputRejected {
                field: "private absolute root/config",
            });
        }
        let root = root.canonicalize().map_err(|_| Outcome::InputRejected {
            field: "HEX_HCS_NATIVE_ROOT",
        })?;
        let config = config.canonicalize().map_err(|_| Outcome::InputRejected {
            field: "HEX_HCS_NATIVE_CONFIG",
        })?;
        if !root.is_dir() || !config.is_file() || !config.starts_with(&root) {
            return Err(Outcome::InputRejected {
                field: "private root/config ownership",
            });
        }
        let bound = hex_core::bundle::MAX_CONFIG_BYTES;
        let mut bytes = Vec::new();
        std::fs::File::open(&config)
            .and_then(|file| {
                file.take(u64::try_from(bound).unwrap().checked_add(1).unwrap())
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| Outcome::InputRejected {
                field: "HCS config read",
            })?;
        if bytes.len() > bound {
            return Err(Outcome::InputRejected {
                field: "HCS config byte bound",
            });
        }
        let document = String::from_utf8(bytes).map_err(|_| Outcome::InputRejected {
            field: "HCS config UTF-8",
        })?;
        let value: serde_json::Value =
            serde_json::from_str(&document).map_err(|_| Outcome::InputRejected {
                field: "HCS config JSON",
            })?;
        let storage = required("HEX_HCS_NATIVE_STORAGE")?;
        if storage.is_empty()
            || storage.contains('\0')
            || !Path::new(&storage).is_dir()
            || value
                .get("Container")
                .is_none_or(|container| !container.is_object())
            || value.get("VirtualMachine").is_some()
            || value.get("HostedSystem").is_some()
            || value
                .pointer("/Container/Storage/Path")
                .and_then(serde_json::Value::as_str)
                != Some(storage.as_str())
            || value.get("ShouldTerminateOnLastHandleClosed")
                != Some(&serde_json::Value::Bool(true))
        {
            return Err(Outcome::InputRejected {
                field: "owned container/storage contract",
            });
        }
        let milliseconds: u32 =
            required("HEX_HCS_NATIVE_BUDGET_MS")?
                .parse()
                .map_err(|_| Outcome::InputRejected {
                    field: "HEX_HCS_NATIVE_BUDGET_MS",
                })?;
        // Win32's INFINITE sentinel is not a finite caller budget; no fixture default is supplied.
        if milliseconds == 0 || milliseconds == u32::MAX {
            return Err(Outcome::InputRejected {
                field: "finite positive native budget",
            });
        }
        Ok(Self {
            id,
            root,
            document,
            budget: Duration::from_millis(u64::from(milliseconds)),
        })
    }
}

#[derive(Serialize)]
struct ContextWitness {
    callback_completed: bool,
    native_receipt_retained: bool,
    owners: usize,
    operation_retained: bool,
    inputs_retained: bool,
    system_retained: bool,
}

fn context_witness(owned: &PendingCreate) -> Result<ContextWitness, Outcome> {
    let context = owned.context();
    let data = context.data.lock().map_err(|_| Outcome::Uncertain {
        phase: "native context mutex",
    })?;
    Ok(ContextWitness {
        callback_completed: context.completed.load(std::sync::atomic::Ordering::Acquire),
        native_receipt_retained: context
            .callback_owned
            .load(std::sync::atomic::Ordering::Acquire),
        owners: context.owners.load(std::sync::atomic::Ordering::Acquire),
        operation_retained: data.operation.is_some(),
        inputs_retained: data.inputs.is_some(),
        system_retained: data.system.is_some(),
    })
}

#[derive(Serialize)]
struct Report {
    requirement: &'static str,
    system_id: String,
    document_bytes: usize,
    caller_budget_ms: u128,
    create_patience_ms: u128,
    expected: &'static str,
    outcome: Outcome,
    immediate_pending: Option<bool>,
    cancellation: Option<String>,
    terminal_code: Option<u32>,
    returned_system: Option<bool>,
    own_system_observed: Option<bool>,
    cleanup_code: Option<u32>,
    own_system_absent_after_cleanup: Option<bool>,
    context: Option<ContextWitness>,
    duplicate_query_replayed: Option<bool>,
    last_runtime_receipt_dropped: bool,
    self_handles_entry: Option<u32>,
    self_handles_before_create: Option<u32>,
    self_handles_after: Option<u32>,
}

impl Report {
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }
}

fn self_handles() -> Result<u32, Outcome> {
    let mut count = 0;
    // SAFETY: GetCurrentProcess returns this process's pseudo handle, which must not be closed.
    // count is writable DWORD storage. These SDK calls read only this test process's handle count.
    let okay = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    if okay == 0 {
        return Err(
            match io::Error::last_os_error()
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
            {
                Some(code) => Outcome::Refused {
                    phase: "self handle count",
                    code,
                },
                None => Outcome::Uncertain {
                    phase: "self handle count refused without OS code",
                },
            },
        );
    }
    Ok(count)
}

fn observed_system(id: &hex_api::BoxId, phase: &'static str) -> Result<Option<System>, Outcome> {
    open_system(id.as_str()).map_err(|failure| Outcome::Refused {
        phase,
        code: failure.code,
    })
}

fn exercise(inputs: &Inputs, report: &mut Report, journal: &Path) {
    let mut owned: Option<PendingCreate> = None;
    let mut system: Option<System> = None;
    let mut terminal = false;
    let mut created = false;
    let result = (|| -> Result<(), Outcome> {
        report.self_handles_entry = Some(self_handles()?);
        if observed_system(&inputs.id, "preflight own ID")?.is_some() {
            return Err(Outcome::InputRejected {
                field: "system ID already exists; never touched",
            });
        }
        report.self_handles_before_create = Some(self_handles()?);
        report.write(journal).map_err(|_| Outcome::Uncertain {
            phase: "pre-create journal",
        })?;
        // Zero is the requested boundary, not a promise to observe Pending or permission to enlarge waits.
        owned = Some(
            create_system_owned(inputs.id.as_str(), &inputs.document, Duration::ZERO).map_err(
                |failure| Outcome::Refused {
                    phase: "create before operation start",
                    code: failure.code,
                },
            )?,
        );
        let create = owned.as_mut().unwrap();
        let mut answer = create.query();
        report.immediate_pending = Some(answer.is_none());
        report.cancellation = Some(format!("{:?}", create.cancellation()));
        if answer.is_none() {
            report.write(journal).map_err(|_| Outcome::Uncertain {
                phase: "pending journal",
            })?;
            // One actual operation wait at ROOT's explicit budget, no polling/retry/sleep.
            // The production callback uses try_lock, including if it arrives during this wait.
            {
                let context = create.context();
                let mut data = context.data.lock().map_err(|_| Outcome::Uncertain {
                    phase: "native wait mutex",
                })?;
                data.advance(context.calls, inputs.budget);
            }
            answer = create.query();
        }
        let answer = answer.ok_or(Outcome::Uncertain {
            phase: "create still pending at caller cut",
        })?;
        terminal = true;
        system = answer.system;
        report.returned_system = Some(system.is_some());
        report.terminal_code = Some(match answer.result {
            Ok(()) => {
                created = true;
                0
            }
            Err(failure) => failure.code,
        });
        Ok(())
    })();
    report.outcome = match result {
        Ok(()) => Outcome::Completed,
        Err(outcome) => outcome,
    };

    // Only a terminal create permits physical cleanup/absence claims. A pending create retains its
    // native completion owner and journal even if this test's runtime receipt is dropped below.
    if terminal {
        match observed_system(&inputs.id, "observe own created system") {
            Ok(found) => {
                report.own_system_observed = Some(found.is_some());
                if system.is_none() {
                    system = found;
                } else {
                    drop(found);
                }
            }
            Err(outcome) => report.outcome = outcome,
        }
        if let Some(held) = system.take() {
            report.cleanup_code = Some(match held.terminate(inputs.budget) {
                Ok(()) => 0,
                Err(failure) => failure.code,
            });
            drop(held);
        }
        if let Some(create) = owned.as_mut() {
            // The same public-to-this-module query must not replay its consumed native result.
            report.duplicate_query_replayed = Some(create.query().is_some());
            match context_witness(create) {
                Ok(witness) => report.context = Some(witness),
                Err(outcome) => report.outcome = outcome,
            }
        }
    } else if let Some(create) = owned.as_ref() {
        report.context = context_witness(create).ok();
    }
    drop(owned);
    report.last_runtime_receipt_dropped = report.immediate_pending.is_some();
    if terminal {
        match observed_system(&inputs.id, "establish own physical absence") {
            Ok(found) => report.own_system_absent_after_cleanup = Some(found.is_none()),
            Err(outcome) => report.outcome = outcome,
        }
    }
    report.self_handles_after = self_handles().ok();
    if matches!(report.outcome, Outcome::Completed) {
        report.outcome = if !created {
            Outcome::Refused {
                phase: "terminal native create",
                code: report.terminal_code.unwrap(),
            }
        } else if report.cleanup_code.is_some_and(|code| code != 0) {
            Outcome::Uncertain {
                phase: "native cleanup reported a refusal",
            }
        } else if report.own_system_observed != Some(true)
            || report.own_system_absent_after_cleanup != Some(true)
        {
            Outcome::Uncertain {
                phase: "own physical create/cleanup witness",
            }
        } else if report.duplicate_query_replayed != Some(false) {
            Outcome::Uncertain {
                phase: "terminal native query replayed a consumed result",
            }
        } else if !report.context.as_ref().is_some_and(|seen| {
            seen.callback_completed
                && !seen.native_receipt_retained
                && seen.owners == 1
                && !seen.operation_retained
                && !seen.inputs_retained
                && !seen.system_retained
        }) {
            Outcome::Uncertain {
                phase: "terminal callback/close/last context receipt not observed",
            }
        } else if report.self_handles_after.is_none() {
            Outcome::Uncertain {
                phase: "final self handle count",
            }
        } else {
            Outcome::Completed
        };
    }
}

/// AC-T7.4.1/T-T7.1.2: ROOT's actual HCS callback/query/cleanup experiment, never run by an agent.
#[test]
#[ignore = "ROOT-only actual HCS: requires private ID/config/storage and explicit budget/outer guard"]
fn assigned_native_create_completion_and_owned_cleanup() {
    let inputs = Inputs::read().expect("required ROOT-owned HCS fixture input; field/cause only");
    let run = inputs.root.join(format!("{}.completion", inputs.id));
    std::fs::create_dir(&run).expect("fresh exclusive private HCS evidence directory");
    std::fs::write(run.join("submitted-hcs.json"), &inputs.document)
        .expect("retain private submitted config before effects");
    let mut report = Report {
        requirement: "AC-T7.4.1/T-T7.1.2",
        system_id: inputs.id.to_string(),
        document_bytes: inputs.document.len(),
        caller_budget_ms: inputs.budget.as_millis(),
        create_patience_ms: Duration::ZERO.as_millis(),
        expected: "actual terminal create/callback, own system observed, native close, exact own cleanup; caller storage retained",
        outcome: Outcome::Uncertain {
            phase: "not started",
        },
        immediate_pending: None,
        cancellation: None,
        terminal_code: None,
        returned_system: None,
        own_system_observed: None,
        cleanup_code: None,
        own_system_absent_after_cleanup: None,
        context: None,
        duplicate_query_replayed: None,
        last_runtime_receipt_dropped: false,
        self_handles_entry: None,
        self_handles_before_create: None,
        self_handles_after: None,
    };
    let journal = run.join("report.json");
    exercise(&inputs, &mut report, &journal);
    report
        .write(&journal)
        .expect("retain HCS outcome/cleanup uncertainty without private payload output");
    assert!(
        matches!(report.outcome, Outcome::Completed),
        "ROOT native fixture outcome: {:?}; private report retained",
        report.outcome
    );
}
