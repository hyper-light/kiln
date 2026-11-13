//! A Shards image VM's credentials (joint plan step 33, its step 6; Shards' D242; docs/design/credential-grants.md C3,
//! C4): the box's credential grants lowered to what Shards' run host takes, and the run's lessor, its orchestrator's,
//! through which the run host asks each value as the VM's egress first needs it and gives each lease back.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::lease::{Lease, LeaseRefused, Lessor, Lessors};
use hex_api::{BoxId, Refusal, ShardsImage};
use shards_api::image::{CredentialGrant, LeaseRefusal, Leased, RunLessor, Secret};

use super::{GuestRuntime, ImageClass};

/// "Until its run ends", as Shards' run channel carries a lease's life: the most its u64 of nanoseconds holds, which the
/// run host and its network process read as living until the run ends wherever it passes what their clocks name
/// (Shards' D242, its literals).
const RUN_LIFE: Duration = Duration::from_nanos(u64::MAX);

impl GuestRuntime {
    /// Its Shards image VMs' credentials leased through `lessors`, its orchestrator's (C3), by the image class it holds
    /// ([`ImageClass::with_lessors`]).
    pub fn with_lessors(mut self, lessors: Arc<dyn Lessors>) -> Self {
        self.images.set_lessors(lessors);
        self
    }
}

impl ImageClass {
    /// Its VMs' credentials leased through `lessors`, its orchestrator's (C3). A class given none refuses a box granting
    /// a credential as it is created, `Unconfigured`.
    pub fn with_lessors(mut self, lessors: Arc<dyn Lessors>) -> Self {
        self.set_lessors(lessors);
        self
    }

    pub(super) fn set_lessors(&mut self, lessors: Arc<dyn Lessors>) {
        self.lessors = Some(lessors);
    }

    /// Whether this node can lease `image`'s credentials: none asks nothing; any asks for its lessors.
    pub(super) fn can_lease(&self, image: &ShardsImage) -> Result<(), Refusal> {
        if image.credentials.is_empty() || self.lessors.is_some() {
            return Ok(());
        }
        Err(unconfigured())
    }

    /// The lessor of box `id`'s run of `image`: none where it grants no credential, so its run asks nothing; else one
    /// the node's lessors make for this run alone.
    pub(super) fn run_lessor(
        &self,
        id: &BoxId,
        image: &ShardsImage,
    ) -> Result<Option<RunLessor>, Refusal> {
        if image.credentials.is_empty() {
            return Ok(None);
        }
        let lessors = self.lessors.as_ref().ok_or_else(unconfigured)?;
        let run = lessors.lessor(id)?;
        Ok(Some(RunLessor(Arc::new(RunLeases(run)))))
    }
}

fn unconfigured() -> Refusal {
    Refusal::Unconfigured {
        what: "lessors for its image VMs' credentials",
        why: "a Shards image VM granted a credential leases its value from its orchestrator's broker as its egress first \
              needs it (Shards' D242), and this node was given none to lease from"
            .to_owned(),
    }
}

/// `image`'s credential grants as Shards' run host takes them (C4): one for each credential, toward every host its
/// grants name, each once, in order. Shards' grant is its run's, its network process ending the TLS of a granted host's
/// flows whichever member's they are (D242), so each member's grant of a credential is one of the run's. Checked again
/// here (C2), so no boot asks for one the check refuses; each the box's, Hex adding none.
pub(super) fn lowered(image: &ShardsImage) -> Result<Vec<CredentialGrant>, Refusal> {
    hex_api::check_credentials(&image.credentials, &image.grants)?;
    let exhausted = |bound: usize| Refusal::Exhausted {
        what: "credentials",
        bound,
    };
    let hosts = image
        .credentials
        .iter()
        .try_fold(0usize, |n, grant| n.checked_add(grant.toward.len()))
        .ok_or_else(|| exhausted(usize::MAX))?;
    let mut each: Vec<(&str, &str)> = Vec::new();
    each.try_reserve_exact(hosts)
        .map_err(|_| exhausted(hosts))?;
    for grant in &image.credentials {
        for host in &grant.toward {
            each.push((grant.reference.as_str(), host.as_str()));
        }
    }
    each.sort_unstable();
    each.dedup();
    let mut lowered: Vec<CredentialGrant> = Vec::new();
    for (reference, host) in each {
        match lowered.last_mut() {
            Some(last) if last.reference == reference => {
                last.toward.try_reserve(1).map_err(|_| exhausted(hosts))?;
                last.toward.push(host.to_owned());
            }
            _ => {
                lowered.try_reserve(1).map_err(|_| exhausted(hosts))?;
                lowered.push(CredentialGrant {
                    reference: reference.to_owned(),
                    toward: vec![host.to_owned()],
                });
            }
        }
    }
    Ok(lowered)
}

/// A run's lessor as Shards' run host asks it (`shards_api::image::Lessor`), answered by the run's own, its
/// orchestrator's (C3): each value handed on whole, nothing of it kept here, living what its lease says from now; each
/// refusal of Shards' kind and words; and each lease given back as Shards gives it back, the run's end among the ways
/// (Shards' session gives back every lease it was given, once, by whatever end).
struct RunLeases(Arc<dyn Lessor>);

impl shards_api::image::Lessor for RunLeases {
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Leased, LeaseRefusal> {
        let Lease { lease, value, life } = self.0.lease(reference, key).map_err(refusal)?;
        let Some(until) = Instant::now().checked_add(life.unwrap_or(RUN_LIFE)) else {
            self.0.release(&lease);
            return Err(LeaseRefusal::Unavailable(
                "its lease's life passes what this host's clock names".to_owned(),
            ));
        };
        Ok(Leased {
            lease,
            value: Secret(value.into_bytes()),
            until,
        })
    }

    fn release(&self, lease: &str) {
        self.0.release(lease);
    }
}

/// The orchestrator's refusal as Shards' run host takes it: kind for kind, its words kept.
fn refusal(why: LeaseRefused) -> LeaseRefusal {
    match why {
        LeaseRefused::Denied(why) => LeaseRefusal::Denied(why),
        LeaseRefused::KeyReplaced { asked, current } => {
            LeaseRefusal::KeyReplaced { asked, current }
        }
        LeaseRefused::Unavailable(why) => LeaseRefusal::Unavailable(why),
        LeaseRefused::Ended => LeaseRefusal::Ended,
        LeaseRefused::Lapsed => LeaseRefusal::Lapsed,
    }
}

#[cfg(test)]
mod tests;
