//! Credentials granted by reference to a Shards image VM's members (Shards' D242; joint plan step 33, its step 6;
//! docs/design/credential-grants.md C1, C2): which member may have which credential filled in at its VM's egress, and
//! toward which hosts. A grant names a credential by reference, a secret in its orchestrator's store, never by its
//! value, which is leased just in time ([`crate::lease`]) and filled in outside the VM. [`check_credentials`] is the one
//! check, run wherever credential grants enter (C2), beside [`crate::check_grants`] for the network grants they lean on.

use serde::{Deserialize, Serialize};

use crate::{Destination, Grant, Member, Proto, Refusal};

/// The longest reference: a secret's name as its orchestrator's store names one, kiln-seams' `Secrets` (`NAME_MAX`),
/// which keeps Secrets Manager's bound (AWS Secrets Manager API Reference, `CreateSecret`, `Name`: 1 to 512 characters
/// of ASCII letters, digits and `/_+=.@-`).
pub const REFERENCE_MAX: usize = 512;

/// A credential granted by reference: `member`'s requests to each of `toward`'s hosts may have the credential
/// `reference` names filled in at its VM's egress where its `placeholder` stands, and no other request may (Shards'
/// D242). Its way is out, as an egress grant's: a value is filled into what the member sends, never into what comes
/// back. It grants no egress: each host is one the member's own egress grant reaches by name, by TCP, as TLS names a
/// server by its DNS name alone (RFC 6066 §3).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialGrant {
    pub member: Member,
    pub reference: String,
    pub toward: Vec<String>,
    /// What stands for the credential in its member's requests (Shards' D242): minted by the orchestrator where it
    /// gives the credential to the member (kiln's broker: `kiln-placeholder-` and 256 random bits in hex), a token the
    /// member sends where the value would go, which the run's injector fills the value in for. Holding it is the
    /// capability, so it never prints. After `toward`, so a record written before it reads as none (append-only).
    #[serde(default)]
    pub placeholder: String,
}

/// Shows a grant's member, reference and hosts, and of its placeholder its length alone, as Shards' grant does.
impl std::fmt::Debug for CredentialGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialGrant")
            .field("member", &self.member)
            .field("reference", &self.reference)
            .field("toward", &self.toward)
            .field(
                "placeholder",
                &format_args!("{} bytes", self.placeholder.len()),
            )
            .finish()
    }
}

/// Whether every credential grant could be filled in exactly as it says, against the box's network `grants` (which
/// [`crate::check_grants`] has taken), refused `Invalid` naming the field that fails (CLAUDE.md §3), never dropped
/// (C2):
/// - its member's name, as a network grant's member's;
/// - its reference, a name its orchestrator's store could hold ([`REFERENCE_MAX`]);
/// - one host at least, each named once, and each one an egress grant of its member's own reaches by name, by TCP;
/// - its placeholder a token a request carries a credential as (RFC 6750 §2.1's `b64token`), as its member sends it
///   where Shards' injector fills the value in (D242); no refusal names it, as holding it is the capability;
/// - each placeholder one credential's alone: grants of one credential may share theirs, but one standing for two
///   could stand for neither, as Shards' injector could not tell which a request carrying it asks (D242);
/// - each member's credential granted once;
/// - every member an egress grant lets reach a credential's host by name, by TCP, granted a credential toward it:
///   Shards' injector ends the TLS of every flow to a credential's host, whichever member's (D242, "Who"), so a
///   member left out here would have its flows ended there, unrecorded.
///
/// Two credentials toward one host are taken: Shards' injector fills each in only where its own placeholder stands
/// (D242's step 3), so neither stands in the other's way.
///
/// Bounded by its input (C5): its working lists reserved fallibly, sorted and merged ([`unfit`]), O(n log n) in the
/// hosts named and the grants, and no recursion.
pub fn check_credentials(credentials: &[CredentialGrant], grants: &[Grant]) -> Result<(), Refusal> {
    let hosts = credentials
        .iter()
        .try_fold(0usize, |n, grant| n.checked_add(grant.toward.len()))
        .ok_or(Refusal::Exhausted {
            what: "credentials",
            bound: usize::MAX,
        })?;
    let mut each: Vec<(&str, &str, &Member, usize)> = working(hosts)?;
    let mut granted: Vec<(&Member, &str, usize)> = working(credentials.len())?;
    let mut standing: Vec<(&str, &str, usize)> = working(credentials.len())?;
    let mut lists: Vec<(&Member, &[String])> = working(credentials.len())?;
    lists.extend(
        credentials
            .iter()
            .map(|grant| (&grant.member, grant.toward.as_slice())),
    );
    let unfit = unfit(&lists, hosts, grants)?;
    for (at, grant) in credentials.iter().enumerate() {
        grant
            .member
            .check_named(&format!("credentials[{at}].member"))
            .map_err(invalid)?;
        check_reference(at, &grant.reference)?;
        if grant.toward.is_empty() {
            return Err(invalid(format!(
                "credentials[{at}] grants {} toward no host, where it could never be filled in",
                grant.reference
            )));
        }
        if let Some((i, why)) = unfit.get(at).copied().flatten() {
            let host = grant.toward.get(i).map(String::as_str).unwrap_or_default();
            return Err(invalid(match why {
                Unfit::Twice => format!("credentials[{at}].toward names {host:?} twice"),
                Unfit::Unreached => format!(
                    "credentials[{at}].toward[{i}] {host:?} is no host an egress grant of {} reaches by name, by TCP: a \
                     credential grant grants no egress, and TLS names a server by its DNS name alone (RFC 6066 §3)",
                    grant.member
                ),
            }));
        }
        for host in &grant.toward {
            each.push((host.as_str(), grant.reference.as_str(), &grant.member, at));
        }
        if !token(&grant.placeholder) {
            return Err(invalid(format!(
                "credentials[{at}].placeholder isn't a token a request carries a credential as, RFC 6750 §2.1's \
                 b64token (1 or more of A-Z a-z 0-9 - . _ ~ + /, then any =), so its member could never send it where \
                 Shards' injector fills {} in (D242)",
                grant.reference
            )));
        }
        granted.push((&grant.member, grant.reference.as_str(), at));
        standing.push((grant.placeholder.as_str(), grant.reference.as_str(), at));
    }
    granted.sort_unstable();
    for pair in granted.windows(2) {
        if let [(member, reference, a), (other, again, b)] = pair
            && (member, reference) == (other, again)
        {
            return Err(invalid(format!(
                "credentials[{a}] and credentials[{b}] both grant {member} {reference}: each is one grant's"
            )));
        }
    }
    standing.sort_unstable();
    for pair in standing.windows(2) {
        if let [(placeholder, reference, a), (same, other, b)] = pair
            && placeholder == same
            && reference != other
        {
            let ((first, its), (then, theirs)) = if a < b {
                ((a, reference), (b, other))
            } else {
                ((b, other), (a, reference))
            };
            return Err(invalid(format!(
                "credentials[{then}].placeholder is credentials[{first}]'s too, so it stands for {theirs} and {its}, \
                 neither alone: Shards' injector could not tell which credential a request carrying it asks (D242), \
                 so each credential's placeholder is its own"
            )));
        }
    }
    each.sort_unstable();
    for (g, grant) in grants.iter().enumerate() {
        let Grant::Egress {
            member,
            to: Destination::Name(host),
            ports,
        } = grant
        else {
            continue;
        };
        if ports.proto != Proto::Tcp {
            continue;
        }
        let from = each.partition_point(|(h, ..)| *h < host.as_str());
        let mut toward = each
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take_while(|(h, ..)| *h == host.as_str())
            .peekable();
        let Some((_, reference, ..)) = toward.peek().copied() else {
            continue;
        };
        if !toward.any(|(_, _, m, _)| *m == member) {
            return Err(invalid(format!(
                "grants[{g}] lets {member} reach {host}, toward which credential {reference} is filled in, and no \
                 credential grant names {member}: Shards' injector ends the TLS of every flow to a credential's host \
                 (D242), so grant it {reference} toward {host} too, or let it not reach {host}"
            )));
        }
    }
    Ok(())
}

/// Whether `reference` is a name its orchestrator's store could hold: 1 to [`REFERENCE_MAX`] of ASCII letters, digits
/// and `/_+=.@-`.
fn check_reference(at: usize, reference: &str) -> Result<(), Refusal> {
    let named = !reference.is_empty()
        && reference.len() <= REFERENCE_MAX
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_+=.@-".contains(&b));
    if named {
        return Ok(());
    }
    Err(invalid(format!(
        "credentials[{at}].reference {reference:?} isn't 1 to {REFERENCE_MAX} of A-Z a-z 0-9 / _ + = . @ -, a name its \
         orchestrator's store could hold"
    )))
}

/// Whether `placeholder` is a token a request carries a credential as: RFC 6750 §2.1's `b64token`, `1*( ALPHA / DIGIT /
/// "-" / "." / "_" / "~" / "+" / "/" ) *"="`, which a bearer token is and an `x-api-key` field's value may be (RFC 9110
/// §5.5's field-vchar holds each), as Shards' `shards_dns::credential::placeholder` takes it.
fn token(placeholder: &str) -> bool {
    let body = placeholder.trim_end_matches('=');
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
}

/// Why a grant's host is refused at its place in its list ([`unfit`]): named there before, or reached by no egress
/// grant of its member's by name, by TCP. A host that is both is named twice, which is looked at first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Unfit {
    Twice,
    Unreached,
}

/// Each list's first unfit host by its place, in the lists' order: where list `at` of `lists` (a member, and the hosts a
/// grant of its names) first names a host it named before, or one no egress grant of its member's in `grants` reaches
/// by name, by TCP; none where every host fits. `hosts` is how many the lists name in all. Sorted and merged (C5): every
/// list's hosts with their member and place, and every TCP egress grant's member and host by name, each sorted, then
/// walked together once, each look a step past one of them or at the host before, so O(n log n) in the hosts and the
/// grants, never a look at every earlier host and every grant for each host.
fn unfit(
    lists: &[(&Member, &[String])],
    hosts: usize,
    grants: &[Grant],
) -> Result<Vec<Option<(usize, Unfit)>>, Refusal> {
    let mut reach: Vec<(&Member, &str)> = working(grants.len())?;
    for grant in grants {
        if let Grant::Egress {
            member,
            to: Destination::Name(host),
            ports,
        } = grant
            && ports.proto == Proto::Tcp
        {
            reach.push((member, host.as_str()));
        }
    }
    reach.sort_unstable();
    let mut asked: Vec<(&Member, &str, usize, usize)> = working(hosts)?;
    for (at, (member, toward)) in lists.iter().enumerate() {
        for (i, host) in toward.iter().enumerate() {
            asked.push((*member, host.as_str(), at, i));
        }
    }
    asked.sort_unstable();
    let mut first: Vec<Option<(usize, Unfit)>> = working(lists.len())?;
    first.resize(lists.len(), None);
    let mut reached = reach.iter().peekable();
    let mut before = None;
    for &(member, host, at, i) in &asked {
        let twice = before.is_some_and(|before| {
            looked();
            before == (member, host, at)
        });
        let found = loop {
            let Some(&&next) = reached.peek() else {
                break false;
            };
            looked();
            match next.cmp(&(member, host)) {
                std::cmp::Ordering::Less => {
                    reached.next();
                }
                std::cmp::Ordering::Equal => break true,
                std::cmp::Ordering::Greater => break false,
            }
        };
        let why = if twice {
            Some(Unfit::Twice)
        } else if found {
            None
        } else {
            Some(Unfit::Unreached)
        };
        if let Some(why) = why
            && let Some(kept) = first.get_mut(at)
            && kept.is_none_or(|kept| (i, why) < kept)
        {
            *kept = Some((i, why));
        }
        before = Some((member, host, at));
    }
    Ok(first)
}

#[cfg(test)]
thread_local! {
    /// The looks this thread's checks have made at an earlier host or an egress grant ([`unfit`]): the work C5 bounds by
    /// the hosts named and the grants, which the tests count (CLAUDE.md §6).
    static LOOKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One look at an earlier host or an egress grant, which the tests count (`LOOKS`) and nothing else does.
fn looked() {
    #[cfg(test)]
    LOOKS.with(|looks| looks.set(looks.get().saturating_add(1)));
}

/// The check's working list: room for `count` entries, reserved fallibly (C5).
fn working<T>(count: usize) -> Result<Vec<T>, Refusal> {
    let mut list = Vec::new();
    list.try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "credentials",
            bound: count,
        })?;
    Ok(list)
}

fn invalid(why: String) -> Refusal {
    Refusal::Invalid {
        what: "credentials",
        why,
    }
}

#[cfg(test)]
mod tests;

/// A CLI whose own sign-in a run may capture outside its microVM (Shards' D242, step 8; docs/design/credential-grants.md
/// C8): its token endpoint's profile is Shards' network process's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInCli {
    ClaudeCode,
    Codex,
}

/// What a sign-in's capture gave (Shards' D242, step 8): its access token's reference in its orchestrator's store, the
/// placeholder that stands for that token in its CLI's requests, and its refresh token's placeholder where its token
/// response gave one. Holding a placeholder is the capability, so none prints: its `Debug` says their lengths alone.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub access: String,
    pub access_placeholder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_placeholder: Option<String>,
}

impl std::fmt::Debug for Captured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Captured")
            .field("access", &self.access)
            .field(
                "access_placeholder",
                &format_args!("{} bytes", self.access_placeholder.len()),
            )
            .field(
                "refresh_placeholder",
                &self
                    .refresh_placeholder
                    .as_ref()
                    .map(|p| format!("{} bytes", p.len())),
            )
            .finish()
    }
}

/// A sign-in grant (Shards' D242, step 8; C8): `member`'s CLI `cli` may sign in as the sign-in `sign_in` names (a name in
/// its orchestrator's store, never a value). Its token response is taken outside the microVM and its CLI given
/// placeholders in its tokens' places; its refreshes and revocations are answered outside; its access token is filled
/// in toward `toward`'s hosts alone, by the rules a credential grant's hosts keep. `captured` is what an earlier capture
/// of it gave, where there was one, so a later run's CLI holds the same placeholders. After the credential grants on
/// every wire and record that carries them (append-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignInGrant {
    pub member: Member,
    pub sign_in: String,
    pub cli: SignInCli,
    pub toward: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured: Option<Captured>,
}

/// The one check of a Shards image VM's sign-in grants (C8), run wherever they enter, beside [`check_credentials`]: each
/// field refused by name. It refuses:
/// - a member's name no Agentfile takes, and a sign-in's name, or a captured access token's reference, no store could
///   hold (1 to [`REFERENCE_MAX`] of A-Z a-z 0-9 / _ + = . @ -);
/// - a sign-in named twice, and a second sign-in of one CLI (Shards' D242: one sign-in of a CLI a run);
/// - no host, a host named twice, or one its member's own egress grant doesn't reach by name, by TCP, as a credential
///   grant's hosts are held (C2);
/// - a captured placeholder that isn't a token a request carries (RFC 6750 §2.1's b64token), and a placeholder that
///   stands for two things: a sign-in's access and refresh placeholders, two sign-ins', or a sign-in's and a credential
///   grant's (Shards' `check_sign_ins`), so each placeholder is one token's alone.
///
/// Bounded by its input as [`check_credentials`] is: its working lists reserved fallibly, sorted and merged ([`unfit`]),
/// and no recursion.
pub fn check_sign_ins(
    sign_ins: &[SignInGrant],
    credentials: &[CredentialGrant],
    grants: &[Grant],
) -> Result<(), Refusal> {
    let count = sign_ins
        .len()
        .checked_mul(2)
        .and_then(|n| n.checked_add(credentials.len()))
        .ok_or(Refusal::Exhausted {
            what: "sign-ins",
            bound: usize::MAX,
        })?;
    let mut standing: Vec<(&str, String)> = working(count)?;
    let hosts = sign_ins
        .iter()
        .try_fold(0usize, |n, grant| n.checked_add(grant.toward.len()))
        .ok_or(Refusal::Exhausted {
            what: "sign-ins",
            bound: usize::MAX,
        })?;
    let mut lists: Vec<(&Member, &[String])> = working(sign_ins.len())?;
    lists.extend(
        sign_ins
            .iter()
            .map(|grant| (&grant.member, grant.toward.as_slice())),
    );
    let unfit = unfit(&lists, hosts, grants)?;
    for (at, grant) in sign_ins.iter().enumerate() {
        grant
            .member
            .check_named(&format!("sign_ins[{at}].member"))
            .map_err(signed_in)?;
        if !named(&grant.sign_in) {
            return Err(signed_in(format!(
                "sign_ins[{at}].sign_in {:?} isn't 1 to {REFERENCE_MAX} of A-Z a-z 0-9 / _ + = . @ -, a name its \
                 orchestrator's store could hold",
                grant.sign_in
            )));
        }
        let before = sign_ins.get(..at).unwrap_or_default();
        if before.iter().any(|b| b.sign_in == grant.sign_in) {
            return Err(signed_in(format!(
                "sign_ins[{at}] names sign-in {} twice: each is one grant's",
                grant.sign_in
            )));
        }
        if before.iter().any(|b| b.cli == grant.cli) {
            return Err(signed_in(format!(
                "sign_ins[{at}] signs {:?} in again: a run signs a CLI in as one sign-in (Shards' D242)",
                grant.cli
            )));
        }
        if grant.toward.is_empty() {
            return Err(signed_in(format!(
                "sign_ins[{at}] grants {} toward no host, where its access token could never be filled in",
                grant.sign_in
            )));
        }
        if let Some((i, why)) = unfit.get(at).copied().flatten() {
            let host = grant.toward.get(i).map(String::as_str).unwrap_or_default();
            return Err(signed_in(match why {
                Unfit::Twice => format!("sign_ins[{at}].toward names {host:?} twice"),
                Unfit::Unreached => format!(
                    "sign_ins[{at}].toward[{i}] {host:?} is no host an egress grant of {} reaches by name, by TCP: a \
                     sign-in grants no egress, and TLS names a server by its DNS name alone (RFC 6066 §3)",
                    grant.member
                ),
            }));
        }
        let Some(captured) = &grant.captured else {
            continue;
        };
        if !named(&captured.access) {
            return Err(signed_in(format!(
                "sign_ins[{at}].captured.access {:?} isn't a name its orchestrator's store could hold",
                captured.access
            )));
        }
        let placeholders = [
            Some((captured.access_placeholder.as_str(), "access_placeholder")),
            captured
                .refresh_placeholder
                .as_deref()
                .map(|p| (p, "refresh_placeholder")),
        ];
        for (placeholder, field) in placeholders.into_iter().flatten() {
            if !token(placeholder) {
                return Err(signed_in(format!(
                    "sign_ins[{at}].captured.{field} isn't a token a request carries, RFC 6750 §2.1's b64token, so its \
                     CLI could never send it where Shards' injector looks (D242)"
                )));
            }
            standing.push((placeholder, format!("sign_ins[{at}].captured.{field}")));
        }
    }
    for (at, grant) in credentials.iter().enumerate() {
        standing.push((
            grant.placeholder.as_str(),
            format!("credentials[{at}].placeholder"),
        ));
    }
    standing.sort_unstable();
    for pair in standing.windows(2) {
        if let [(placeholder, first), (same, then)] = pair
            && placeholder == same
            && !(first.starts_with("credentials") && then.starts_with("credentials"))
        {
            return Err(signed_in(format!(
                "{then} is {first} too: each placeholder stands for one token alone, or Shards' injector could not \
                 tell which a request carrying it asks (D242)"
            )));
        }
    }
    Ok(())
}

/// Whether `name` is a name its orchestrator's store could hold: 1 to [`REFERENCE_MAX`] of ASCII letters, digits and
/// `/_+=.@-`.
fn named(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= REFERENCE_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_+=.@-".contains(&b))
}

fn signed_in(why: String) -> Refusal {
    Refusal::Invalid {
        what: "sign_ins",
        why,
    }
}
