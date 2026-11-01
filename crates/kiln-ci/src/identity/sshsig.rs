//! A commit's signature in git's SSH form (`gpg.format = ssh`: OpenSSH's SSHSIG, its
//! PROTOCOL.sshsig), checked against the keys people have bound to their identities in kiln
//! (docs/ci.md W27-R03), as pure code. A key is bound to one person; its signature counts only in
//! git's namespace (`git`), over the commit as git signs it ([`super::gitsig::payload`]). Ed25519,
//! ECDSA P-256 and RSA (SHA-256 or SHA-512) keys are read; others are refused, saying so.

use aws_lc_rs::signature::{self, UnparsedPublicKey};
use base64::Engine as _;
use sha2::Digest as _;

/// Why an SSH signature isn't taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("its SSH signature isn't one kiln reads: {0}")]
    Malformed(String),
    #[error("its SSH signature is for `{0}`, not git's")]
    Namespace(String),
    #[error("its SSH key ({0}) is bound to no one in kiln")]
    Unbound(String),
    #[error("its SSH key's kind ({0}) isn't one kiln checks")]
    Kind(String),
    #[error("its SSH signature doesn't verify under its key ({0})")]
    Bad(String),
}

/// A public key as an `authorized_keys` line gives it: its kind, its blob and its comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub kind: String,
    pub blob: Vec<u8>,
    pub comment: String,
}

impl PublicKey {
    /// `<kind> <base64 blob> [comment]`, as `ssh-keygen` writes a `.pub` file; its kind must be
    /// the one its blob says, and one kiln checks.
    pub fn parse(line: &str) -> Result<PublicKey, String> {
        let mut words = line.split_whitespace();
        let (Some(kind), Some(b64)) = (words.next(), words.next()) else { return Err("an SSH public key is `<kind> <base64> [comment]`, as its .pub file has it".to_owned()) };
        let blob = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| format!("its key isn't base64: {e}"))?;
        let mut r = blob.as_slice();
        let said = string(&mut r).ok_or_else(|| "its key is cut short".to_owned())?;
        if said != kind.as_bytes() {
            return Err(format!("its key says it's {}, not {kind}", String::from_utf8_lossy(said)));
        }
        if !KINDS.contains(&kind) {
            return Err(format!("{kind} isn't a kind of key kiln checks ({})", KINDS.join(", ")));
        }
        Ok(PublicKey { kind: kind.to_owned(), blob, comment: words.collect::<Vec<_>>().join(" ") })
    }

    /// Its fingerprint as `ssh-keygen -l` shows it: `SHA256:` and its blob's SHA-256 in base64.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.blob)
    }
}

/// The kinds of key kiln checks.
pub const KINDS: [&str; 3] = ["ssh-ed25519", "ecdsa-sha2-nistp256", "ssh-rsa"];

/// A key blob's fingerprint, as `ssh-keygen -l` shows it.
pub fn fingerprint(blob: &[u8]) -> String {
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha2::Sha256::digest(blob)))
}

/// Whom `armored` (an SSHSIG, as git keeps it in a commit's `gpgsig`) says signed `payload`: the
/// person whose bound key (`keys`: each person and their key's blob) made it, in git's namespace.
pub fn verify<'a>(payload: &[u8], armored: &str, keys: &'a [(String, Vec<u8>)]) -> Result<&'a str, Refusal> {
    let bad = |why: &str| Refusal::Malformed(why.to_owned());
    let body: String = armored.lines().map(str::trim).skip_while(|l| *l != "-----BEGIN SSH SIGNATURE-----").skip(1).take_while(|l| *l != "-----END SSH SIGNATURE-----").collect();
    let blob = base64::engine::general_purpose::STANDARD.decode(body).map_err(|e| Refusal::Malformed(format!("not base64: {e}")))?;
    let mut r = blob.as_slice();
    if take(&mut r, 6) != Some(b"SSHSIG".as_slice()) {
        return Err(bad("not an SSHSIG"));
    }
    if u32_of(&mut r) != Some(1) {
        return Err(bad("a version kiln doesn't read"));
    }
    let (Some(key), Some(namespace), Some(reserved), Some(hash), Some(sig)) = (string(&mut r), string(&mut r), string(&mut r), string(&mut r), string(&mut r)) else { return Err(bad("cut short")) };
    if namespace != b"git" {
        return Err(Refusal::Namespace(String::from_utf8_lossy(namespace).into_owned()));
    }
    let person = keys.iter().find(|(_, b)| b.as_slice() == key).map(|(p, _)| p.as_str()).ok_or_else(|| Refusal::Unbound(fingerprint(key)))?;
    let digest = match hash {
        b"sha256" => sha2::Sha256::digest(payload).to_vec(),
        b"sha512" => sha2::Sha512::digest(payload).to_vec(),
        other => return Err(Refusal::Malformed(format!("hash {} isn't one kiln reads", String::from_utf8_lossy(other)))),
    };
    // What the key signed: the preamble, the namespace, the reserved field, the hash's name and
    // the payload's hash, each as SSH's strings.
    let mut signed = b"SSHSIG".to_vec();
    for part in [namespace, reserved, hash, digest.as_slice()] {
        put(&mut signed, part);
    }
    check(key, sig, &signed)?;
    Ok(person)
}

/// Whether `sig` (SSH's signature blob) is `key`'s (SSH's key blob) over `signed`.
fn check(key: &[u8], sig: &[u8], signed: &[u8]) -> Result<(), Refusal> {
    let bad = |why: &str| Refusal::Malformed(why.to_owned());
    let mut k = key;
    let mut s = sig;
    let (Some(kind), Some(format), Some(bytes)) = (string(&mut k), string(&mut s), string(&mut s)) else { return Err(bad("its key or signature is cut short")) };
    let verified = match (kind, format) {
        (b"ssh-ed25519", b"ssh-ed25519") => {
            let pk = string(&mut k).ok_or_else(|| bad("its Ed25519 key is cut short"))?;
            UnparsedPublicKey::new(&signature::ED25519, pk).verify(signed, bytes)
        }
        (b"ecdsa-sha2-nistp256", b"ecdsa-sha2-nistp256") => {
            let (Some(_curve), Some(point)) = (string(&mut k), string(&mut k)) else { return Err(bad("its ECDSA key is cut short")) };
            let mut rs = bytes;
            let (Some(r), Some(s)) = (string(&mut rs), string(&mut rs)) else { return Err(bad("its ECDSA signature is cut short")) };
            let mut fixed = fixed32(r).ok_or_else(|| bad("its ECDSA signature's r is too long"))?;
            fixed.extend(fixed32(s).ok_or_else(|| bad("its ECDSA signature's s is too long"))?);
            UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(signed, &fixed)
        }
        (b"ssh-rsa", f @ (b"rsa-sha2-256" | b"rsa-sha2-512")) => {
            let (Some(e), Some(n)) = (string(&mut k), string(&mut k)) else { return Err(bad("its RSA key is cut short")) };
            let params = if f == b"rsa-sha2-256" { &signature::RSA_PKCS1_2048_8192_SHA256 } else { &signature::RSA_PKCS1_2048_8192_SHA512 };
            signature::RsaPublicKeyComponents { n: unpadded(n), e: unpadded(e) }.verify(params, signed, bytes)
        }
        (k, f) => return Err(Refusal::Kind(format!("{} signed as {}", String::from_utf8_lossy(k), String::from_utf8_lossy(f)))),
    };
    verified.map_err(|_| Refusal::Bad(String::from_utf8_lossy(kind).into_owned()))
}

/// An SSH mpint as a 32-byte big-endian number: `None` when it doesn't fit.
fn fixed32(mpint: &[u8]) -> Option<Vec<u8>> {
    let n = unpadded(mpint);
    let pad = 32usize.checked_sub(n.len())?;
    let mut out = vec![0u8; pad];
    out.extend_from_slice(n);
    Some(out)
}

/// A big-endian number without its leading zeros.
fn unpadded(n: &[u8]) -> &[u8] {
    let zeros = n.iter().take_while(|b| **b == 0).count();
    n.get(zeros..).unwrap_or_default()
}

/// The next `n` bytes of `r`.
fn take<'a>(r: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    if r.len() < n {
        return None;
    }
    let (head, rest) = r.split_at(n);
    *r = rest;
    Some(head)
}

fn u32_of(r: &mut &[u8]) -> Option<u32> {
    let b: [u8; 4] = take(r, 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(b))
}

/// SSH's `string`: a length, then that many bytes.
fn string<'a>(r: &mut &'a [u8]) -> Option<&'a [u8]> {
    let n = usize::try_from(u32_of(r)?).ok()?;
    take(r, n)
}

fn put(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(s);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair as _};

    /// An SSHSIG of `payload` in `namespace` by a key (its blob) whose signature blob `sign` makes
    /// of what it's given, armored as `ssh-keygen -Y sign` writes it.
    fn sshsig(key: &[u8], namespace: &str, payload: &[u8], sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
        let digest = sha2::Sha512::digest(payload);
        let mut signed = b"SSHSIG".to_vec();
        for part in [namespace.as_bytes(), b"", b"sha512", digest.as_slice()] {
            put(&mut signed, part);
        }
        let mut blob = b"SSHSIG".to_vec();
        blob.extend_from_slice(&1u32.to_be_bytes());
        for part in [key, namespace.as_bytes(), b"", b"sha512", sign(&signed).as_slice()] {
            put(&mut blob, part);
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(blob);
        let lines: Vec<String> = b64.as_bytes().chunks(70).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
        format!("-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----\n", lines.join("\n"))
    }

    fn blob(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            put(&mut out, p);
        }
        out
    }

    fn ed25519() -> anyhow::Result<(Ed25519KeyPair, Vec<u8>)> {
        let pair = Ed25519KeyPair::generate()?;
        let key = blob(&[b"ssh-ed25519", pair.public_key().as_ref()]);
        Ok((pair, key))
    }

    #[test]
    fn w27_r03_an_ssh_signature_verifies_as_the_person_its_key_is_bound_to() -> anyhow::Result<()> {
        let (pair, key) = ed25519()?;
        let (_, other) = ed25519()?;
        let keys = vec![("kim".to_owned(), other.clone()), ("sam".to_owned(), key.clone())];
        let payload = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor sam <sam@example.com> 1793300000 +0000\n\nsam's\n";
        let armored = sshsig(&key, "git", payload, |m| blob(&[b"ssh-ed25519", pair.sign(m).as_ref()]));
        assert_eq!(verify(payload, &armored, &keys), Ok("sam"));
        // Another commit; another namespace; a key bound to no one.
        assert!(matches!(verify(b"tree 0\n", &armored, &keys), Err(Refusal::Bad(_))));
        let file = sshsig(&key, "file", payload, |m| blob(&[b"ssh-ed25519", pair.sign(m).as_ref()]));
        assert_eq!(verify(payload, &file, &keys), Err(Refusal::Namespace("file".into())));
        assert_eq!(verify(payload, &armored, keys.get(..1).unwrap_or_default()), Err(Refusal::Unbound(fingerprint(&key))));
        assert!(matches!(verify(payload, "-----BEGIN SSH SIGNATURE-----\nU1NIU0lH\n-----END SSH SIGNATURE-----\n", &keys), Err(Refusal::Malformed(_))));
        Ok(())
    }

    #[test]
    fn w27_r03_an_ecdsa_key_signs_too() -> anyhow::Result<()> {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, &rng)?;
        let pair = EcdsaKeyPair::from_pkcs8(&signature::ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())?;
        let key = blob(&[b"ecdsa-sha2-nistp256", b"nistp256", pair.public_key().as_ref()]);
        let payload = b"tree 1\n\nkim's\n";
        let armored = sshsig(&key, "git", payload, |m| {
            let sig = pair.sign(&rng, m).map(|s| s.as_ref().to_vec()).unwrap_or_default();
            let (r, s) = sig.split_at(sig.len() / 2);
            // SSH's mpints: a leading zero where the top bit is set.
            let mpint = |n: &[u8]| if n.first().is_some_and(|b| *b >= 0x80) { [&[0u8][..], n].concat() } else { n.to_vec() };
            blob(&[b"ecdsa-sha2-nistp256", &blob(&[&mpint(r), &mpint(s)])])
        });
        assert_eq!(verify(payload, &armored, &[("kim".to_owned(), key)]), Ok("kim"));
        Ok(())
    }

    #[test]
    fn w27_r03_a_public_key_reads_as_its_pub_file_has_it() -> anyhow::Result<()> {
        let (_, key) = ed25519()?;
        let line = format!("ssh-ed25519 {} sam@laptop", base64::engine::general_purpose::STANDARD.encode(&key));
        let k = PublicKey::parse(&line).map_err(anyhow::Error::msg)?;
        assert_eq!((k.kind.as_str(), k.blob.as_slice(), k.comment.as_str()), ("ssh-ed25519", key.as_slice(), "sam@laptop"));
        assert!(k.fingerprint().starts_with("SHA256:") && !k.fingerprint().ends_with('='));
        assert!(PublicKey::parse(&format!("ssh-rsa {}", base64::engine::general_purpose::STANDARD.encode(&key))).is_err_and(|e| e.contains("says it's ssh-ed25519")));
        assert!(PublicKey::parse("ssh-dss AAAAB3NzaC1kc3M=").is_err());
        assert!(PublicKey::parse("not a key").is_err());
        Ok(())
    }
}
