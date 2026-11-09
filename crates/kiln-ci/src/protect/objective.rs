//! What protection is for (LB05 step 1, docs/protect.md "Objectives"): the requests someone waits
//! on, by who waits, each with a deadline, and how a request ended. A request is served when it was
//! answered by its deadline; one that failed served no one, however fast it failed: a fast 503 is
//! failed service, never a fast answer, so a server that sheds can't look healthy to what watches it.

use serde::{Deserialize, Serialize};

/// Who waits on a request, as kiln's server classes them (`kiln::overload::class_of`), least urgent
/// first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Useful {
    /// What can wait: agents' and CI's polling, catch-ups, sync, telemetry.
    Background,
    /// The API: what agents, tools and the CLI call.
    Normal,
    /// A person's own: health, sign-in, their pages and actions (a cancel among them). The server
    /// never sheds it and runs it first (`kiln::overload`): its reserved capacity is a class, not a
    /// share.
    Critical,
}

/// The longest a person waits on an answer before their flow of thought breaks: "1.0 second is
/// about the limit for the user's flow of thought to stay uninterrupted" (J. Nielsen, "Response
/// Times: The 3 Important Limits", from Usability Engineering, 1993; after R. B. Miller, "Response
/// time in man-computer conversational transactions", AFIPS FJCC 1968, and S. K. Card, G. G.
/// Robertson and J. D. Mackinlay, "The information visualizer", CHI 1991). The deadline of a request
/// a person waits on where nothing declares one, and the default of every deadline a person sets
/// (docs/protect.md).
pub const FLOW_US: f64 = 1_000_000.0;

/// How a request ended.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// Answered without failing, after `us`.
    Answered { us: f64 },
    /// Answered with a failure after `us`: a server error, or a refusal for load (`shed`: it said
    /// it was overloaded, with `Retry-After` or kiln's `x-kiln-shed`).
    Failed { us: f64, shed: bool },
    /// No answer at all: refused, reset, unreachable.
    Unreachable,
}

impl Outcome {
    /// How an HTTP request answered with `status` after `us` ended: 500 and up is the server failing
    /// (RFC 9110 §15.6), and 429 a refusal for load as much as a 503 is (RFC 6585 §4: "the user has
    /// sent too many requests in a given amount of time"); any other status is an answer, the
    /// request's own errors among them (a 404 is the service working). `shed`: the answer said it
    /// was a refusal for load.
    pub fn of_status(status: u16, us: f64, shed: bool) -> Outcome {
        if status >= 500 || status == 429 { Outcome::Failed { us, shed: shed || status == 429 } } else { Outcome::Answered { us } }
    }

    /// Whether it served whoever waited on it, by `deadline_us`: answered, and in time. A failure
    /// served no one however fast it came (LB05: a fast 503 is failed service), and neither did no
    /// answer.
    pub fn met(&self, deadline_us: f64) -> bool {
        matches!(self, Outcome::Answered { us } if us.is_finite() && *us <= deadline_us)
    }

    /// How long it took, where it answered at all.
    pub fn took_us(&self) -> Option<f64> {
        match self {
            Outcome::Answered { us } | Outcome::Failed { us, .. } => Some(*us),
            Outcome::Unreachable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LB05 step 1: injected fast rejection can't appear healthy. A 503 in a microsecond, shed or
    /// not, meets no deadline, however long; an answer meets one it came by, and not one it missed.
    #[test]
    fn lb05_a_fast_503_is_failed_service_never_a_fast_answer() {
        for shed in [false, true] {
            let fast = Outcome::of_status(503, 1.0, shed);
            assert!(matches!(fast, Outcome::Failed { .. }), "{fast:?}");
            assert!(!fast.met(FLOW_US) && !fast.met(f64::INFINITY), "a fast 503 met a deadline: {fast:?}");
        }
        assert!(Outcome::of_status(200, FLOW_US, false).met(FLOW_US), "an answer at its deadline meets it");
        assert!(!Outcome::of_status(200, FLOW_US + 1.0, false).met(FLOW_US), "an answer past its deadline doesn't");
        assert!(!Outcome::Unreachable.met(f64::INFINITY));
        assert!(!Outcome::Answered { us: f64::NAN }.met(FLOW_US), "an answer of no length met a deadline");
    }

    #[test]
    fn lb05_a_refusal_for_load_fails_and_the_requests_own_errors_answer() {
        assert_eq!(Outcome::of_status(429, 5.0, false), Outcome::Failed { us: 5.0, shed: true });
        assert_eq!(Outcome::of_status(500, 5.0, false), Outcome::Failed { us: 5.0, shed: false });
        assert_eq!(Outcome::of_status(599, 5.0, true), Outcome::Failed { us: 5.0, shed: true });
        assert_eq!(Outcome::of_status(404, 5.0, false), Outcome::Answered { us: 5.0 });
        assert_eq!(Outcome::of_status(499, 5.0, false), Outcome::Answered { us: 5.0 });
        assert_eq!(Outcome::Unreachable.took_us(), None);
        assert_eq!(Outcome::of_status(503, 7.0, true).took_us(), Some(7.0));
    }

    #[test]
    fn classes_order_by_who_waits() {
        assert!(Useful::Background < Useful::Normal && Useful::Normal < Useful::Critical);
    }
}
