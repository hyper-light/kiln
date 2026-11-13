//! A peer's enrollment kept in a folder of its own (docs/design/hex-quic.md Q6, Q9): what `hex node enroll` begins and
//! `hex node --quic` serves with. Its key is made here and written here alone, readable by its owner alone; its
//! request is what goes to kiln's CA; the CA's answer is kept beside them. Each file is PEM (RFC 7468):
//!
//! - [`KEY`]: the private key, PKCS #8 (RFC 5958), made here and never sent;
//! - [`REQUEST`]: the signing request for kiln's CA, PKCS #10 (RFC 2986), naming the peer;
//! - [`CHAIN`]: the CA's certificates for the key, its own first;
//! - [`ROOTS`]: the CA's roots, every peer's anchors.

use std::io::Write;
use std::path::{Path, PathBuf};

use hyper_transport::tls::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use super::{Issued, Key, Request};
use crate::Refused;
use crate::tls::Credentials;

pub const KEY: &str = "key.pem";
pub const REQUEST: &str = "request.pem";
pub const CHAIN: &str = "chain.pem";
pub const ROOTS: &str = "roots.pem";

/// RFC 7468's labels: §10 (PKCS #8), §7 (PKCS #10), §5 (certificates).
const KEY_LABEL: &str = "PRIVATE KEY";
const REQUEST_LABEL: &str = "CERTIFICATE REQUEST";
const CERTIFICATE_LABEL: &str = "CERTIFICATE";

fn refused(path: &Path, why: impl std::fmt::Display) -> Refused {
    Refused::Configuration {
        what: "the peer's enrollment",
        why: format!("{}: {why}", path.display()),
    }
}

/// Begins `name`'s enrollment in `dir`: a key made here, kept in [`KEY`] for this user alone, and its request for
/// kiln's CA in [`REQUEST`], whose path it returns. A folder that holds either already is refused, its enrollment
/// under way or done: a second key would leave the first's certificate without one.
pub fn begin(dir: &Path, name: &str) -> Result<PathBuf, Refused> {
    let (key, request) = super::request(name, Vec::new())?;
    make_dir(dir)?;
    let (key_path, request_path) = (dir.join(KEY), dir.join(REQUEST));
    for path in [&key_path, &request_path] {
        if path.exists() {
            return Err(refused(
                path,
                "is there already: this folder's enrollment has begun",
            ));
        }
    }
    write_new(&key_path, key.0.serialize_pem().as_bytes(), true)?;
    let csr = pem::encode(&pem::Pem::new(REQUEST_LABEL, request.csr));
    if let Err(e) = write_new(&request_path, csr.as_bytes(), false) {
        // Without its request the key is no one's: let it go, so the folder can begin again.
        let _ = std::fs::remove_file(&key_path);
        return Err(e);
    }
    Ok(request_path)
}

/// The request [`begin`] kept in `dir`, for kiln's CA with `grant` (kiln's enrollment token): the name it asks and its
/// PKCS #10.
pub fn request(dir: &Path, grant: Vec<u8>) -> Result<Request, Refused> {
    let path = dir.join(REQUEST);
    let text = read(&path)?;
    let csr = blocks(&path, &text, REQUEST_LABEL)?
        .into_iter()
        .next()
        .ok_or_else(|| refused(&path, "holds no signing request"))?;
    let params = rcgen::CertificateSigningRequestParams::from_pem(&text)
        .map_err(|e| refused(&path, format!("can't be read as a signing request: {e}")))?;
    let name = params
        .params
        .subject_alt_names
        .iter()
        .find_map(|san| match san {
            rcgen::SanType::DnsName(name) => Some(name.as_str().to_owned()),
            _ => None,
        })
        .ok_or_else(|| refused(&path, "names no peer"))?;
    Ok(Request { name, csr, grant })
}

/// Keeps kiln's CA's answer in `dir` once it proves to be for the key and request there ([`super::credentials`]):
/// [`CHAIN`] and [`ROOTS`]. An answer for another key or another name is refused, typed, and nothing is kept.
pub fn finish(dir: &Path, issued: &Issued) -> Result<(), Refused> {
    let key = key(dir)?;
    let request = request(dir, Vec::new())?;
    super::credentials(key, &request, issued.clone())?;
    let pems = |certificates: &[CertificateDer<'static>]| {
        pem::encode_many(
            &certificates
                .iter()
                .map(|c| pem::Pem::new(CERTIFICATE_LABEL, c.to_vec()))
                .collect::<Vec<_>>(),
        )
    };
    write_new(&dir.join(CHAIN), pems(&issued.chain).as_bytes(), false)?;
    write_new(&dir.join(ROOTS), pems(&issued.roots).as_bytes(), false)
}

/// The credentials kept in `dir`: its key, and the CA's chain and roots, the chain's own certificate the key's, naming
/// a peer, and verifying under the roots for both sides of a link now. Anything else is refused, typed, naming the
/// file.
pub fn load(dir: &Path) -> Result<Credentials, Refused> {
    let key = key(dir)?;
    let (chain_path, roots_path) = (dir.join(CHAIN), dir.join(ROOTS));
    let chain = certificates(&chain_path)?;
    let roots = certificates(&roots_path)?;
    let leaf = chain
        .first()
        .ok_or_else(|| refused(&chain_path, "holds no certificate"))?;
    let cert = webpki::EndEntityCert::try_from(leaf)
        .map_err(|e| refused(&chain_path, format!("its certificate can't be read: {e}")))?;
    let spki = rcgen::PublicKeyData::subject_public_key_info(&key.0);
    if cert.subject_public_key_info().as_ref() != spki.as_slice() {
        return Err(refused(
            &chain_path,
            format!("its certificate carries another key than {KEY}'s"),
        ));
    }
    if crate::driver::certificate_name(leaf).is_none() {
        return Err(refused(&chain_path, "its certificate names no peer"));
    }
    let anchors = roots
        .iter()
        .map(|root| {
            webpki::anchor_from_trusted_cert(root)
                .map_err(|e| refused(&roots_path, format!("a root can't be read: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let now = hyper_quic::rustls::pki_types::UnixTime::now();
    for usage in [
        webpki::KeyUsage::server_auth(),
        webpki::KeyUsage::client_auth(),
    ] {
        cert.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            &anchors,
            chain.get(1..).unwrap_or_default(),
            now,
            usage,
            None,
            None,
        )
        .map_err(|e| {
            refused(
                &chain_path,
                format!(
                    "its certificate doesn't verify under {ROOTS} for both sides of a link: {e}"
                ),
            )
        })?;
    }
    Ok(Credentials {
        chain,
        key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.0.serialize_der())),
        roots,
    })
}

fn key(dir: &Path) -> Result<Key, Refused> {
    let path = dir.join(KEY);
    let text = read(&path)?;
    if blocks(&path, &text, KEY_LABEL)?.is_empty() {
        return Err(refused(&path, "holds no private key"));
    }
    rcgen::KeyPair::from_pem(&text)
        .map(Key)
        .map_err(|e| refused(&path, format!("can't be read as a private key: {e}")))
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, Refused> {
    let text = read(path)?;
    let found = blocks(path, &text, CERTIFICATE_LABEL)?;
    if found.is_empty() {
        return Err(refused(path, "holds no certificate"));
    }
    Ok(found.into_iter().map(CertificateDer::from).collect())
}

/// The contents of `text`'s blocks labelled `label`; a block of another label is refused.
fn blocks(path: &Path, text: &str, label: &str) -> Result<Vec<Vec<u8>>, Refused> {
    pem::parse_many(text)
        .map_err(|e| refused(path, format!("isn't PEM: {e}")))?
        .into_iter()
        .map(|block| {
            if block.tag() == label {
                Ok(block.into_contents())
            } else {
                Err(refused(
                    path,
                    format!("holds a {:?} block where {label:?} belongs", block.tag()),
                ))
            }
        })
        .collect()
}

fn read(path: &Path) -> Result<String, Refused> {
    std::fs::read_to_string(path).map_err(|e| refused(path, e))
}

/// The folder, made for its owner alone where it isn't there.
fn make_dir(dir: &Path) -> Result<(), Refused> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| refused(dir, e))
}

/// Writes `bytes` to a file `path` names that isn't there yet; one `private` readable and writable by its owner alone
/// on Unix (on Windows, as its folder's access list says).
fn write_new(path: &Path, bytes: &[u8], private: bool) -> Result<(), Refused> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    let mut file = options.open(path).map_err(|e| refused(path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| refused(path, e))
}

#[cfg(test)]
mod tests;
