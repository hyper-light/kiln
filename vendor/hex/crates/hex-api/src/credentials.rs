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
/// `reference` names filled in at its VM's egress, and no other request may (Shards' D242). Its way is out, as an
/// egress grant's: a value is filled into what the member sends, never into what comes back. It grants no egress:
/// each host is one the member's own egress grant reaches by name, by TCP, as TLS names a server by its DNS name alone
/// (RFC 6066 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialGrant {
    pub member: Member,
    pub reference: String,
    pub toward: Vec<String>,
}

/// Whether every credential grant could be filled in exactly as it says, against the box's network `grants` (which
/// [`crate::check_grants`] has taken), refused `Invalid` naming the field that fails (CLAUDE.md §3), never dropped
/// (C2):
/// - its member's name, as a network grant's member's;
/// - its reference, a name its orchestrator's store could hold ([`REFERENCE_MAX`]);
/// - one host at least, each named once, and each one an egress grant of its member's own reaches by name, by TCP;
/// - each member's credential granted once;
/// - each host one credential's alone: Shards' injector fills in the first credential a host's grants name (D242's
///   `decide`), so a second toward it would never be filled in;
/// - every member an egress grant lets reach a credential's host by name, by TCP, granted that credential toward it:
///   Shards' injector ends the TLS of every flow to a credential's host, whichever member's (D242, "Who"), so a
///   member left out here would have its flows ended there, unrecorded.
///
/// Bounded by its input (C5): two working lists, reserved fallibly and sorted, and no recursion.
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
        for (i, host) in grant.toward.iter().enumerate() {
            if grant.toward.get(..i).unwrap_or_default().contains(host) {
                return Err(invalid(format!(
                    "credentials[{at}].toward names {host:?} twice"
                )));
            }
            if !reached(grants, &grant.member, host) {
                return Err(invalid(format!(
                    "credentials[{at}].toward[{i}] {host:?} is no host an egress grant of {} reaches by name, by TCP: a \
                     credential grant grants no egress, and TLS names a server by its DNS name alone (RFC 6066 §3)",
                    grant.member
                )));
            }
            each.push((host.as_str(), grant.reference.as_str(), &grant.member, at));
        }
        granted.push((&grant.member, grant.reference.as_str(), at));
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
    each.sort_unstable();
    for pair in each.windows(2) {
        if let [(host, reference, _, a), (next, other, _, b)] = pair
            && host == next
            && reference != other
        {
            return Err(invalid(format!(
                "credentials[{a}] and credentials[{b}] fill {reference} and {other} in toward {host}: Shards' injector \
                 fills in the first credential a host's grants name (D242), so the other would never be"
            )));
        }
    }
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

/// Whether an egress grant of `member`'s reaches `host` by name, by TCP.
fn reached(grants: &[Grant], member: &Member, host: &str) -> bool {
    grants.iter().any(|grant| {
        matches!(grant, Grant::Egress { member: m, to: Destination::Name(name), ports }
            if m == member && name == host && ports.proto == Proto::Tcp)
    })
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
