//! A Shards image VM's credentials (joint plan step 33, its step 6; Shards' D242; docs/design/credential-grants.md C3,
//! C4): the box's credential grants lowered to what Shards' run host takes, and the run's lessor, its orchestrator's,
//! through which the run host asks each value as the VM's egress first needs it and gives each lease back.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::lease::{Lease, LeaseRefused, Lessor, Lessors, SignInRefused, SignInTokens, Value};
use hex_api::{BoxId, Refusal, ShardsImage};
use shards_api::image::{CredentialGrant, LeaseRefusal, Leased, RunLessor, Secret, SignInRefusal};

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
        if !leases(image) || self.lessors.is_some() {
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
        if !leases(image) {
            return Ok(None);
        }
        let lessors = self.lessors.as_ref().ok_or_else(unconfigured)?;
        let run = lessors.lessor(id)?;
        Ok(Some(RunLessor(Arc::new(RunLeases(run)))))
    }
}

/// Whether a run of `image` asks its orchestrator anything: a credential's lease or a sign-in's capture (C3, C8).
fn leases(image: &ShardsImage) -> bool {
    !image.credentials.is_empty() || !image.sign_ins.is_empty()
}

/// `image`'s sign-in grants as Shards' run host takes them (C8): run-wide, each its name, its CLI, its hosts and an
/// earlier capture's placeholders, checked again here (C2, C8) so no boot asks for one the check refuses; Hex adds none.
pub(super) fn sign_ins(
    image: &ShardsImage,
) -> Result<Vec<shards_api::image::SignInGrant>, Refusal> {
    hex_api::check_sign_ins(&image.sign_ins, &image.credentials, &image.grants)?;
    let mut lowered = Vec::new();
    lowered
        .try_reserve_exact(image.sign_ins.len())
        .map_err(|_| Refusal::Exhausted {
            what: "sign-ins",
            bound: image.sign_ins.len(),
        })?;
    for grant in &image.sign_ins {
        lowered.push(shards_api::image::SignInGrant {
            sign_in: grant.sign_in.clone(),
            cli: match grant.cli {
                hex_api::SignInCli::ClaudeCode => shards_api::image::Cli::ClaudeCode,
                hex_api::SignInCli::Codex => shards_api::image::Cli::Codex,
            },
            toward: grant.toward.clone(),
            captured: grant
                .captured
                .as_ref()
                .map(|captured| shards_api::image::Captured {
                    access: captured.access.clone(),
                    access_placeholder: captured.access_placeholder.clone(),
                    refresh_placeholder: captured.refresh_placeholder.clone(),
                }),
        });
    }
    Ok(lowered)
}

fn unconfigured() -> Refusal {
    Refusal::Unconfigured {
        what: "lessors for its image VMs' credentials",
        why: "a Shards image VM granted a credential leases its value from its orchestrator's broker as its egress first \
              needs it (Shards' D242), and this node was given none to lease from"
            .to_owned(),
    }
}

/// `image`'s credential grants as Shards' run host takes them (C4): one for each credential and placeholder, toward
/// every host its grants with that placeholder name, each once, in order. Shards' grant is its run's, its network
/// process ending the TLS of a granted host's flows whichever member's they are and filling a value in only where its
/// grant's placeholder stands (D242), so each member's grant of a credential is one of the run's, by its placeholder.
/// Checked again here (C2), so no boot asks for one the check refuses; each the box's, Hex adding none.
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
    let mut each: Vec<(&str, &str, &str)> = Vec::new();
    each.try_reserve_exact(hosts)
        .map_err(|_| exhausted(hosts))?;
    for grant in &image.credentials {
        for host in &grant.toward {
            each.push((
                grant.reference.as_str(),
                grant.placeholder.as_str(),
                host.as_str(),
            ));
        }
    }
    each.sort_unstable();
    each.dedup();
    let mut lowered: Vec<CredentialGrant> = Vec::new();
    for (reference, placeholder, host) in each {
        match lowered.last_mut() {
            Some(last) if last.reference == reference && last.placeholder == placeholder => {
                last.toward.try_reserve(1).map_err(|_| exhausted(hosts))?;
                last.toward.push(host.to_owned());
            }
            _ => {
                lowered.try_reserve(1).map_err(|_| exhausted(hosts))?;
                lowered.push(CredentialGrant {
                    reference: reference.to_owned(),
                    toward: vec![host.to_owned()],
                    placeholder: placeholder.to_owned(),
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

    /// A sign-in's tokens handed on whole to the run's lessor, each moved into a [`Value`] that wipes it, nothing of
    /// them kept here; its placeholders answered back, or its refusal of Shards' kind and words (C8).
    fn capture(
        &self,
        sign_in: &str,
        tokens: shards_api::image::SignInTokens,
        key: Option<[u8; 32]>,
    ) -> Result<shards_api::image::Captured, SignInRefusal> {
        let shards_api::image::SignInTokens {
            mut access,
            refresh,
            expires_in,
            id_token,
            scope,
        } = tokens;
        let moved = |mut secret: Secret| Value::new(std::mem::take(&mut secret.0));
        let tokens = SignInTokens {
            access: Value::new(std::mem::take(&mut access.0)),
            refresh: refresh.map(moved),
            expires_in,
            id_token: id_token.map(moved),
            scope,
        };
        let captured = self
            .0
            .capture(sign_in, tokens, key)
            .map_err(sign_in_refusal)?;
        Ok(shards_api::image::Captured {
            access: captured.access.clone(),
            access_placeholder: captured.access_placeholder.clone(),
            refresh_placeholder: captured.refresh_placeholder.clone(),
        })
    }

    /// A sign-in refreshed by the run's lessor: its token response handed on whole into Shards' `Secret` (C8).
    fn refresh(&self, sign_in: &str, key: Option<[u8; 32]>) -> Result<Secret, SignInRefusal> {
        let response = self.0.refresh(sign_in, key).map_err(sign_in_refusal)?;
        Ok(Secret(response.into_bytes()))
    }

    fn revoked(&self, sign_in: &str, key: Option<[u8; 32]>) {
        self.0.revoked(sign_in, key);
    }
}

/// The orchestrator's refusal of a sign-in as Shards' run host takes it: kind for kind, its words kept.
fn sign_in_refusal(why: SignInRefused) -> SignInRefusal {
    match why {
        SignInRefused::Denied(why) => SignInRefusal::Denied(why),
        SignInRefused::Unavailable(why) => SignInRefusal::Unavailable(why),
        SignInRefused::Ended => SignInRefusal::Ended,
        SignInRefused::Unanswered => SignInRefusal::Unanswered,
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
