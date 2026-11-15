//! A Shards image VM's credential leases (Shards' D242; joint plan step 33, its step 6; docs/design/credential-grants.md
//! C3): the seam through which its orchestrator's broker answers them (CLAUDE.md §9). A VM's credential grants
//! ([`crate::CredentialGrant`]) name credentials by reference; each value is leased just in time, as the VM's egress
//! first needs it, from its orchestrator, to that run alone, and every lease is given back as the run lets go of it,
//! its end among the ways.
//!
//! - [`Lessors`]: what a node's orchestrator gives it: a [`Lessor`] for each run of a VM with credential grants, made
//!   as the run boots.
//! - [`Lessor`]: one run's leases, as Shards' run host asks them (`shards_api::image::Lessor`, which Hex's image class
//!   answers through it).
//! - [`sim::SimLessors`], its simulator, and [`conformance`], the contract both it and every orchestrator's pass.
//!
//! A value is held as briefly as it can be: in a [`Value`], wiped as it drops and never printed.

use std::sync::Arc;
use std::time::Duration;

use crate::{BoxId, Refusal};

pub mod conformance;
pub mod sim;

/// A credential's value: wiped as it drops, and never printed (its `Debug` says its length alone).
pub struct Value(Vec<u8>);

impl Value {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Its bytes, handed on whole with nothing left here to wipe: to a holder that wipes them in turn (Shards'
    /// `Secret`).
    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Value({} bytes)", self.0.len())
    }
}

/// Two values alike byte for byte: what the node's wire compares a lease it carried by (docs/design/credential-grants.md
/// C7), never printed either way.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for Value {}

/// `bytes` wiped, their whole allocation, and left empty: what a holder of a value's bytes outside a [`Value`] does as it
/// lets go of them, the node's wire carrying one between its orchestrator and the run among them
/// (docs/design/credential-grants.md C7).
pub fn wipe(bytes: &mut Vec<u8>) {
    zeroize::Zeroize::zeroize(bytes);
}

/// `bytes` wiped in place: a buffer read into again, as the node's socket's is, whose message carried a value.
pub fn wipe_in_place(bytes: &mut [u8]) {
    zeroize::Zeroize::zeroize(bytes);
}

/// A copy a transport makes of bytes on their way (a socket's framed message, what is left of it to send, a
/// connection's queued frame): when they may carry a lease's value or a sign-in's tokens, wiped, their whole
/// allocation, as they drop, by whichever way they go, so no copy outlives its use unwiped (docs/design/credential-
/// grants.md C7, C8; the review's E-6). Never printed.
#[derive(Default)]
pub struct Wiped {
    bytes: Vec<u8>,
    secret: bool,
}

impl Wiped {
    /// `bytes`, wiped as they drop where `secret`: a copy of a message that carries a value or tokens
    /// (`hex_core::wire::carries_secret`), or of what a connection that carries such messages holds.
    pub fn new(bytes: Vec<u8>, secret: bool) -> Self {
        Self { bytes, secret }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Whether it is wiped as it drops.
    pub fn secret(&self) -> bool {
        self.secret
    }

    /// Its bytes from `at` on, in a buffer of their own, wiped as it drops as this one is: what a socket hasn't taken
    /// yet.
    pub fn rest(&self, at: usize) -> Self {
        Self::new(
            self.bytes.get(at..).unwrap_or_default().to_vec(),
            self.secret,
        )
    }

    /// Its first `n` bytes let go, as a socket took them: the rest moved to the front, and the bytes that leaves behind
    /// it wiped.
    pub fn take_front(&mut self, n: usize) {
        self.bytes.drain(..n.min(self.bytes.len()));
        if self.secret {
            zeroize::Zeroize::zeroize(self.bytes.spare_capacity_mut());
        }
    }

    /// Its first `n` bytes, what a read put there, wiped in place and kept: a buffer read into again once what it read
    /// is framed.
    pub fn wipe_read(&mut self, n: usize) {
        let n = n.min(self.bytes.len());
        if self.secret
            && let Some(read) = self.bytes.get_mut(..n)
        {
            wipe_in_place(read);
        }
    }

    /// Its bytes let go, wiped first: empty from here.
    pub fn clear(&mut self) {
        if self.secret {
            wipe(&mut self.bytes);
        }
        self.bytes.clear();
    }

    /// Its bytes handed on whole, with nothing left here to wipe: to a write the kernel holds them for until its
    /// completion gives them back, where they are a `Wiped` again.
    pub fn lend(mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl Drop for Wiped {
    fn drop(&mut self) {
        if self.secret {
            wipe(&mut self.bytes);
        }
    }
}

impl std::fmt::Debug for Wiped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wiped({} bytes{})",
            self.bytes.len(),
            if self.secret { ", secret" } else { "" }
        )
    }
}

/// A credential's value leased to a run: the lessor's name for the lease, by which the run gives it back
/// ([`Lessor::release`]), never its value; the value; and how long it lives from now, none where it lives until its
/// run ends.
#[derive(Debug, PartialEq, Eq)]
pub struct Lease {
    pub lease: String,
    pub value: Value,
    pub life: Option<Duration>,
}

/// Why a lease is refused, by Shards' kinds (D242's `LeaseRefusal`), which its run host answers the VM's egress with,
/// each in its orchestrator's words, never a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseRefused {
    /// Its broker leases the run none of it: a credential its store doesn't hold, or one its policy keeps from the run.
    #[error("its broker leases the run none of it: {0}")]
    Denied(String),
    /// Asked under the key the run had before a move replaced it (Shards' D238, D176 part 6): that key, and the run's
    /// key now where it has one, each 64 lowercase hex digits. Its broker alone says which key is current.
    #[error("{asked} is the key its run had before it moved: its key is {}", .current.as_deref().unwrap_or("none"))]
    KeyReplaced {
        asked: String,
        current: Option<String>,
    },
    /// Its broker couldn't be asked, or failed, in its words: a later ask may be leased.
    #[error("its broker couldn't lease it: {0}")]
    Unavailable(String),
    /// The run has ended: nothing is leased to it after.
    #[error("the run has ended: nothing is leased to it after")]
    Ended,
    /// Its lease ended before it was answered: a later ask is leased anew.
    #[error("its lease ended before it was answered")]
    Lapsed,
}

/// A sign-in's tokens as its run's injector took them from its CLI's token response (Shards' D242, step 8;
/// docs/design/credential-grants.md C8), each wiped as it drops and never printed: its access token, its refresh token
/// where the response had one, the access token's life in seconds where it said (RFC 6749 §5.1); and, no credential, what
/// its CLI's own file keeps beside them: its ID token made inert (its header and claims, no signature) and the scope its
/// access token was granted.
#[derive(Debug, PartialEq, Eq)]
pub struct SignInTokens {
    pub access: Value,
    pub refresh: Option<Value>,
    pub expires_in: Option<u64>,
    pub id_token: Option<Value>,
    pub scope: Option<String>,
}

/// Why a sign-in's ask is refused, by Shards' kinds (D242's `SignInRefusal`), each in its orchestrator's words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignInRefused {
    /// Its broker refused it: a sign-in its store doesn't hold, or a refresh its provider refused.
    #[error("its broker refused the sign-in: {0}")]
    Denied(String),
    /// Its broker couldn't be asked, or failed: a later ask may be answered.
    #[error("its broker couldn't answer the sign-in: {0}")]
    Unavailable(String),
    /// The run has ended.
    #[error("the run has ended")]
    Ended,
    /// The run's orchestrator answers no sign-in.
    #[error("the run's orchestrator answers no sign-in")]
    Unanswered,
}

/// One run's leases (Shards' D242): its orchestrator's, for that run alone, from its broker. Every lease it gives is
/// given back once, as the run lets go of it or ends.
pub trait Lessor: Send + Sync {
    /// Credential `reference`'s value, leased to the run now, or why not, typed. Called on a thread of its own, once
    /// for each ask, `reference` one the run's grants name. `key` is the SHA-256 of the run's own key where it binds an
    /// agent-control endpoint (Shards' D252), the identity its leases are issued to, which its broker alone says is
    /// current; none where it binds none, its lessor's run alone asking.
    fn lease(&self, reference: &str, key: Option<[u8; 32]>) -> Result<Lease, LeaseRefused>;
    /// The lease [`lease`](Self::lease) named `lease`, given back: its run let go of it (it lapsed before it was passed
    /// on, or came after the run released the rest) or ended, or the value it gave rotated (its sign-in refreshed).
    /// Called once for each lease given.
    fn release(&self, lease: &str);
    /// Sign-in `sign_in`'s tokens, as the run's injector took them from its CLI's token response (Shards' D242, step 8;
    /// C8): kept outside by the orchestrator's broker, which answers the placeholders its CLI holds in their places and
    /// the reference the run's leases name its access token by from then on. Called on a thread of its own, once for
    /// each capture, `sign_in` one of the run's sign-in grants, `key` as [`lease`](Self::lease) is given it. An
    /// orchestrator that serves no sign-in refuses it `Unanswered`, as this does.
    fn capture(
        &self,
        sign_in: &str,
        tokens: SignInTokens,
        key: Option<[u8; 32]>,
    ) -> Result<crate::Captured, SignInRefused> {
        let _ = (sign_in, tokens, key);
        Err(SignInRefused::Unanswered)
    }
    /// Sign-in `sign_in` refreshed by its broker, which alone calls its provider: the token response its CLI is to have,
    /// its tokens the placeholders it holds. Once it answers, each lease of the access token given the run before is
    /// given back ([`release`](Self::release)), its value rotated. Refused `Unanswered` by an orchestrator that serves no
    /// sign-in, as by this.
    fn refresh(&self, sign_in: &str, key: Option<[u8; 32]>) -> Result<Value, SignInRefused> {
        let _ = (sign_in, key);
        Err(SignInRefused::Unanswered)
    }
    /// The run's VM revoked sign-in `sign_in` (its CLI's logout, or a sign-in anew), answered in its network process and
    /// its stored tokens kept: told, so its broker may say so; never a revocation outside.
    fn revoked(&self, sign_in: &str, key: Option<[u8; 32]>) {
        let _ = (sign_in, key);
    }
}

/// What a node's orchestrator gives it for its Shards image VMs' credentials: a [`Lessor`] for each run of a VM with
/// credential grants, made as the run boots, for that run alone (box `id`'s, one run each).
pub trait Lessors: Send + Sync {
    fn lessor(&self, id: &BoxId) -> Result<Arc<dyn Lessor>, Refusal>;
}

#[cfg(test)]
mod tests;
