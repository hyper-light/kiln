//! Signatures on the work ledger's entries (docs/ci.md W24-R22), pure: the claim a runner signs of
//! a result, what the server countersigns, how both are kept in an entry's `signatures`, and what a
//! reader concludes from checking them. Keys and their arithmetic are the caller's (the kiln crate
//! signs and verifies with aws-lc-rs); here are the bytes signed and the rules.
//!
//! - **The runner signs** a result's [`Claim`] (its scope and strong key, outcome, exit code,
//!   outputs, log and reports, domain and lineage, and where it was made) with its identity key:
//!   DSSE's PAE of [`RESULT`] over the claim's [`digest`] ([`runner_message`]).
//! - **The server countersigns** it into the scope with its own key: PAE of [`COUNTERSIGN`] over
//!   the claim's digest and the runner's key and signature ([`server_message`]). So the
//!   countersignature vouches for which key signed, checked as it was written against the key the
//!   server knew for that runner, and a reader needs no directory of runners' keys to check both.
//! - **Readers check both** ([`judge`]): a signature that doesn't verify, or no countersignature,
//!   breaks the rules (an altered, forged or misplaced row: the claim names its scope and its key);
//!   a result its runner didn't sign (a runner without an identity key) stands on the
//!   countersignature alone for every unit but a protected one, which takes only results their
//!   runners signed and counts its two independent runners by their keys.

use std::collections::BTreeMap;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::key::Key;
use super::trust::{Domain, Passed};

/// DSSE's payload types: what a runner signs, and what the server countersigns.
pub const RESULT: &str = "application/vnd.kiln.work.result.v1";
pub const COUNTERSIGN: &str = "application/vnd.kiln.work.countersign.v1";
/// Bumped when what a claim covers changes: every signature made before then stops verifying.
pub const VERSION: &str = "kiln/work/claim/v1";

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// What's signed of a result: all a reader acts on, and where it was made. Who verified it since
/// and its signatures themselves aren't in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim<'a> {
    /// The scope it's recorded in (`owner/repo|domain|`).
    pub scope: &'a str,
    pub strong: &'a Key,
    pub outcome: &'a str,
    pub exit: Option<i32>,
    pub outputs: &'a BTreeMap<String, String>,
    pub logs: Option<&'a str>,
    pub reports: &'a [String],
    pub domain: &'a str,
    pub lineage: &'a [String],
    /// The entries it reused (`<scope><strong>`), which its provenance lists.
    pub reused: &'a [String],
    pub runner: &'a str,
    pub run: &'a str,
    pub leg: &'a str,
    pub step: Option<u32>,
    pub attempt: u32,
    pub commit: &'a str,
    pub tree: &'a str,
}

/// The canonical encoding: every string and list length-prefixed, every option tagged.
struct Enc(Sha256);

impl Enc {
    fn len(&mut self, n: usize) {
        self.0.update((n as u64).to_be_bytes());
    }

    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.update(b);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn list(&mut self, l: &[String]) {
        self.len(l.len());
        for s in l {
            self.str(s);
        }
    }

    fn opt(&mut self, o: Option<&[u8]>) {
        match o {
            None => self.0.update([0]),
            Some(b) => {
                self.0.update([1]);
                self.bytes(b);
            }
        }
    }
}

/// A claim's digest: SHA-256 over [`VERSION`] and each field in a fixed order, so two claims that
/// differ anywhere digest differently.
pub fn digest(c: &Claim) -> [u8; 32] {
    let mut e = Enc(Sha256::new());
    e.str(VERSION);
    e.str(c.scope);
    e.bytes(&c.strong.0);
    e.str(c.outcome);
    e.opt(c.exit.map(i32::to_be_bytes).as_ref().map(|b| b.as_slice()));
    e.len(c.outputs.len());
    for (name, value) in c.outputs {
        e.str(name);
        e.str(value);
    }
    e.opt(c.logs.map(str::as_bytes));
    e.list(c.reports);
    e.str(c.domain);
    e.list(c.lineage);
    e.list(c.reused);
    e.str(c.runner);
    e.str(c.run);
    e.str(c.leg);
    e.opt(c.step.map(u32::to_be_bytes).as_ref().map(|b| b.as_slice()));
    e.0.update(c.attempt.to_be_bytes());
    e.str(c.commit);
    e.str(c.tree);
    e.0.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A key's id: the SHA-256 of its SubjectPublicKeyInfo (DER), in hex, as reach's fingerprints and
/// the attestation key's id are.
pub fn keyid(spki: &[u8]) -> String {
    hex(&Sha256::digest(spki))
}

/// What a runner signs of a claim: PAE of [`RESULT`] over its digest, in hex.
pub fn runner_message(claim: &[u8; 32]) -> Vec<u8> {
    crate::attest::pae(RESULT, hex(claim).as_bytes())
}

/// What the server signs: PAE of [`COUNTERSIGN`] over the claim's digest and the runner's key id
/// and signature, a line each, `-` for both when its runner signed nothing.
pub fn server_message(claim: &[u8; 32], runner: Option<&RunnerSig>) -> Vec<u8> {
    let (key, sig) = runner.map_or_else(|| ("-".to_owned(), "-".to_owned()), |r| (r.keyid.clone(), B64.encode(&r.sig)));
    crate::attest::pae(COUNTERSIGN, format!("{}\n{key}\n{sig}", hex(claim)).as_bytes())
}

/// A runner's signature: its key (SubjectPublicKeyInfo, DER), whose [`keyid`] names it, and what it
/// signed with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSig {
    pub keyid: String,
    pub spki: Vec<u8>,
    pub sig: Vec<u8>,
}

/// The server's countersignature: its key's id and the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSig {
    pub keyid: String,
    pub sig: Vec<u8>,
}

/// An entry's signatures, as its `signatures` lists them: `runner:<keyid>:<key>:<signature>` and
/// `server:<keyid>:<signature>`, key and signatures in base64, each at most once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Signatures {
    pub runner: Option<RunnerSig>,
    pub server: Option<ServerSig>,
}

impl RunnerSig {
    /// A runner's signature with key `spki`, its id worked out.
    pub fn new(spki: Vec<u8>, sig: Vec<u8>) -> RunnerSig {
        RunnerSig { keyid: keyid(&spki), spki, sig }
    }
}

impl Signatures {
    /// An entry's `signatures`, read: anything else in them is an error, and so is a runner
    /// signature whose key id isn't its key's.
    pub fn parse(list: &[String]) -> Result<Signatures, String> {
        let mut out = Signatures::default();
        let decode = |part: &str, what: &str| -> Result<Vec<u8>, String> { B64.decode(part).map_err(|_| format!("a signature's {what} isn't base64")) };
        for s in list {
            let parts: Vec<&str> = s.split(':').collect();
            match parts.as_slice() {
                ["runner", id, spki, sig] => {
                    let r = RunnerSig { keyid: (*id).to_owned(), spki: decode(spki, "key")?, sig: decode(sig, "signature")? };
                    if r.keyid != keyid(&r.spki) {
                        return Err(format!("a runner signature names key {id}, which isn't the key it carries"));
                    }
                    if out.runner.replace(r).is_some() {
                        return Err("two runner signatures".into());
                    }
                }
                ["server", id, sig] => {
                    let r = ServerSig { keyid: (*id).to_owned(), sig: decode(sig, "signature")? };
                    if out.server.replace(r).is_some() {
                        return Err("two countersignatures".into());
                    }
                }
                _ => return Err(format!("`{}`: a signature is runner:<keyid>:<key>:<signature> or server:<keyid>:<signature>", s.chars().take(24).collect::<String>())),
            }
        }
        Ok(out)
    }

    /// As an entry lists them: the runner's, then the server's.
    pub fn to_list(&self) -> Vec<String> {
        let runner = self.runner.iter().map(|r| format!("runner:{}:{}:{}", r.keyid, B64.encode(&r.spki), B64.encode(&r.sig)));
        let server = self.server.iter().map(|s| format!("server:{}:{}", s.keyid, B64.encode(&s.sig)));
        runner.chain(server).collect()
    }
}

/// What checking one signature found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Valid,
    /// There's none.
    Missing,
    /// There's one, and it doesn't verify or isn't by the key it should be: why.
    Invalid(String),
}

/// What checking an entry's two signatures found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub runner: Check,
    pub server: Check,
}

/// Whether a unit of `reader` takes an entry among `scope`'s entries whose signatures checked so
/// (R22): both valid; or the countersignature valid and no runner signature (a runner without an
/// identity key), for any unit but a protected one. A signature that doesn't verify, and an entry
/// with no countersignature, which nothing written through the server lacks, break the rules.
pub fn judge(reader: &Domain, scope: &Domain, c: &Checked) -> Result<(), Passed> {
    match &c.server {
        Check::Valid => {}
        Check::Missing => return Err(Passed::Breach(format!("not countersigned into {scope}'s entries (W24-R22)"))),
        Check::Invalid(why) => return Err(Passed::Breach(format!("its countersignature doesn't verify: {why} (W24-R22)"))),
    }
    match &c.runner {
        Check::Valid => Ok(()),
        Check::Invalid(why) => Err(Passed::Breach(format!("its runner's signature doesn't verify: {why} (W24-R22)"))),
        Check::Missing if reader.needs_two_runners() => Err(Passed::Unfit("a protected unit takes only results their runners signed, and this one's runner has no identity key (W24-R22)".into())),
        Check::Missing => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim_parts() -> (Key, BTreeMap<String, String>, Vec<String>, Vec<String>) {
        (Key([7; 32]), BTreeMap::from([("bin".to_owned(), "sha256:aa".to_owned())]), vec!["sha256:r1".to_owned()], vec!["trusted".to_owned()])
    }

    fn base<'a>(strong: &'a Key, outputs: &'a BTreeMap<String, String>, reports: &'a [String], lineage: &'a [String]) -> Claim<'a> {
        Claim {
            scope: "acme/app|trusted|",
            strong,
            outcome: "success",
            exit: Some(0),
            outputs,
            logs: Some("sha256:log"),
            reports,
            domain: "trusted",
            lineage,
            reused: lineage,
            runner: "r1",
            run: "run1",
            leg: "build",
            step: Some(2),
            attempt: 1,
            commit: "c0ffee",
            tree: "7ree",
        }
    }

    /// W24-R22: a claim's digest covers every field, so a signature over it covers them all: an
    /// entry moved to another scope or key, or with any field changed, no longer verifies.
    #[test]
    fn w24_r22_claims_digest_every_field() {
        let (strong, outputs, reports, lineage) = claim_parts();
        let c = base(&strong, &outputs, &reports, &lineage);
        let d = digest(&c);
        assert_eq!(d, digest(&base(&strong, &outputs, &reports, &lineage)));
        let other_key = Key([8; 32]);
        let more = BTreeMap::from([("bin".to_owned(), "sha256:bb".to_owned())]);
        let none: Vec<String> = Vec::new();
        let edits: Vec<Claim> = vec![
            Claim { scope: "acme/app|untrusted|", ..c },
            Claim { strong: &other_key, ..c },
            Claim { outcome: "failure", ..c },
            Claim { exit: None, ..c },
            Claim { exit: Some(1), ..c },
            Claim { outputs: &more, ..c },
            Claim { logs: None, ..c },
            Claim { logs: Some("sha256:other"), ..c },
            Claim { reports: &none, ..c },
            Claim { domain: "untrusted", ..c },
            Claim { lineage: &none, ..c },
            Claim { reused: &none, ..c },
            Claim { runner: "r2", ..c },
            Claim { run: "run2", ..c },
            Claim { leg: "test", ..c },
            Claim { step: None, ..c },
            Claim { attempt: 2, ..c },
            Claim { commit: "decade", ..c },
            Claim { tree: "other", ..c },
        ];
        let mut seen = std::collections::BTreeSet::from([d]);
        for (i, e) in edits.iter().enumerate() {
            assert!(seen.insert(digest(e)), "edit {i} digests as another claim does");
        }
        // Fields split differently between neighbours digest differently.
        assert_ne!(digest(&Claim { run: "ab", leg: "c", ..c }), digest(&Claim { run: "a", leg: "bc", ..c }));
    }

    /// W24-R22: signatures as an entry keeps them, read back; what isn't one is refused.
    #[test]
    fn w24_r22_signatures_read_and_print() -> Result<(), String> {
        let runner = RunnerSig::new(b"spki-of-r1".to_vec(), b"sig-1".to_vec());
        let both = Signatures { runner: Some(runner.clone()), server: Some(ServerSig { keyid: "ab12".into(), sig: b"sig-2".to_vec() }) };
        let list = both.to_list();
        assert_eq!(list.len(), 2);
        assert!(list.first().is_some_and(|r| r.starts_with(&format!("runner:{}:", runner.keyid))));
        assert_eq!(Signatures::parse(&list)?, both);
        assert_eq!(Signatures::parse(&[])?, Signatures::default());
        let server_only = Signatures { runner: None, server: both.server.clone() };
        assert_eq!(Signatures::parse(&server_only.to_list())?, server_only);
        let bad = |s: &str| Signatures::parse(&[s.to_owned()]);
        assert!(bad("notary:x:y").is_err());
        assert!(bad("server:ab12:***").is_err_and(|e| e.contains("base64")));
        assert!(bad(&format!("runner:{}:{}:c2ln", "00".repeat(32), B64.encode(b"spki-of-r1"))).is_err_and(|e| e.contains("isn't the key")), "a key id that isn't its key's");
        assert!(bad("server:ab12:c2ln:extra").is_err());
        assert!(Signatures::parse(&[list.clone(), list].concat()).is_err_and(|e| e.contains("two")));
        Ok(())
    }

    /// W24-R22: the countersignature covers the runner's key and signature, or says there was none.
    #[test]
    fn w24_r22_what_each_signs() {
        let d = [3u8; 32];
        let m = runner_message(&d);
        assert!(m.starts_with(format!("DSSEv1 {} {RESULT} 64 ", RESULT.len()).as_bytes()));
        let r1 = RunnerSig::new(b"k1".to_vec(), b"s1".to_vec());
        let r2 = RunnerSig::new(b"k2".to_vec(), b"s1".to_vec());
        let unsigned = server_message(&d, None);
        assert!(unsigned.ends_with(format!("{}\n-\n-", hex(&d)).as_bytes()));
        assert_ne!(unsigned, server_message(&d, Some(&r1)));
        assert_ne!(server_message(&d, Some(&r1)), server_message(&d, Some(&r2)), "another runner's key");
        assert_ne!(server_message(&d, Some(&r1)), server_message(&[4; 32], Some(&r1)), "another claim");
    }

    /// W24-R22: readers check both signatures. A bad one or no countersignature is a breach; no
    /// runner signature leaves the countersignature standing alone, but not for a protected unit.
    #[test]
    fn w24_r22_readers_check_both() {
        let (p, t, u, ada) = (Domain::Protected, Domain::Trusted, Domain::Untrusted, Domain::Person("ada".into()));
        let checked = |runner: Check, server: Check| Checked { runner, server };
        let bad = || Check::Invalid("altered".into());
        for reader in [&p, &t, &u, &ada] {
            assert!(judge(reader, &t, &checked(Check::Valid, Check::Valid)).is_ok());
            for c in [checked(Check::Valid, Check::Missing), checked(Check::Missing, Check::Missing), checked(Check::Valid, bad()), checked(bad(), Check::Valid)] {
                assert!(judge(reader, &t, &c).is_err_and(|e| e.is_breach()), "{reader}: {c:?}");
            }
        }
        for reader in [&t, &u, &ada] {
            assert!(judge(reader, &t, &checked(Check::Missing, Check::Valid)).is_ok(), "{reader}: the countersignature alone");
        }
        assert!(judge(&p, &t, &checked(Check::Missing, Check::Valid)).is_err_and(|e| !e.is_breach() && e.to_string().contains("their runners signed")));
        assert!(judge(&t, &t, &checked(Check::Valid, Check::Missing)).is_err_and(|e| e.to_string().contains("not countersigned into trusted's")));
    }
}
