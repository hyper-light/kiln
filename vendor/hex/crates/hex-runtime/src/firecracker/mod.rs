//! AC-T7.4.1: Firecracker's Linux KVM backend, with exact preboot device attachment.
//! The existing native record service owns every claim; this backend owns only its VM effects.

mod config;
pub use config::{Config, PREBOOT_REVISION};

#[cfg(any(target_os = "linux", test))]
mod observations;

#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod single_use;
#[cfg(target_os = "linux")]
pub use single_use::MarksAt;
#[cfg(target_os = "linux")]
mod transport;
#[cfg(target_os = "linux")]
mod vfio;
#[cfg(target_os = "linux")]
pub use native::FirecrackerRuntime;

pub use crate::devices::{Books, Transaction};
use hex_api::Refusal;

fn invalid(why: impl AsRef<str>) -> Refusal {
    let why = why.as_ref();
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "Firecracker refusal bytes",
            bound: why.len(),
        };
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "Firecracker backend",
        why: text,
    }
}

#[cfg(any(target_os = "linux", test))]
fn exhausted(what: &'static str, bound: usize) -> Refusal {
    Refusal::Exhausted { what, bound }
}
