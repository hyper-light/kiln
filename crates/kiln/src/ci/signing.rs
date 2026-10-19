//! How kiln signs attestations (docs/ci.md W18-R02): `[ci.attest] signing`.
//! - **`kiln`** (the default): kiln's own P-256 key (`<data_dir>/keys/attest-signing.p8`), public at
//!   `/_kiln/attestation-key.pem` ([`super::attest::Signer`]).
//! - **`key`**: a key file of the person's: cosign's (`cosign generate-key-pair`, scrypt and
//!   NaCl's secretbox, its password in `key_password_file`), or an unencrypted PKCS#8 or SEC1 PEM.
//!   The public key is served as kiln's own is.
//! - **`kms`**: a key in AWS KMS (`awskms:///<arn or alias>`), which signs digests and never leaves
//!   AWS; kiln reaches it with an `aws` integration's role, federated with kiln's own ID token.
//! - **`keyless`**: Sigstore: Fulcio certifies a key made for the one signature to kiln's OIDC
//!   identity for the run (its workflow, as GitHub's keyless signing has it), and Rekor logs the
//!   signature; the bundle carries both, so verifiers need no key of kiln's.
//!
//! Each makes the same thing: a Sigstore bundle (v0.3) of a DSSE envelope.

use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use kiln_ci::attest as a;
use serde_json::{Value, json};

use crate::hub::Hub;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn pem_of(label: &str, der: &[u8]) -> String {
    let b64 = B64.encode(der);
    let lines: Vec<&str> = b64.as_bytes().chunks(64).filter_map(|c| std::str::from_utf8(c).ok()).collect();
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

/// The DER inside a PEM block, and its label.
fn der_of(pem: &str) -> Option<(String, Vec<u8>)> {
    let begin = pem.find("-----BEGIN ")?;
    let rest = pem.get(begin + 11..)?;
    let label = rest.split("-----").next()?.to_owned();
    let end = rest.find("-----END ")?;
    let body: String = rest.get(label.len() + 5..end)?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains(':')).collect();
    Some((label, B64.decode(body).ok()?))
}

/// A P-256 key from a key file: cosign's encrypted one (with `password`), or PKCS#8 (or SEC1) PEM.
pub fn key_from_file(text: &str, password: &[u8]) -> anyhow::Result<EcdsaKeyPair> {
    let (label, der) = der_of(text).ok_or_else(|| anyhow::anyhow!("the key file isn't PEM"))?;
    let pkcs8 = if label.contains("ENCRYPTED") && (label.contains("SIGSTORE") || label.contains("COSIGN")) {
        decrypt_cosign(&der, password)?
    } else {
        der
    };
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8)
        .or_else(|_| EcdsaKeyPair::from_private_key_der(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8))
        .map_err(|_| anyhow::anyhow!("the key isn't an ECDSA P-256 key (cosign's, or PKCS#8 or SEC1)"))
}

/// cosign's encrypted key: `{ kdf: { name: scrypt, params: { N, r, p }, salt }, cipher: { name:
/// nacl/secretbox, nonce }, ciphertext }`, the plaintext the key's PKCS#8.
fn decrypt_cosign(json_bytes: &[u8], password: &[u8]) -> anyhow::Result<Vec<u8>> {
    use crypto_secretbox::aead::{Aead as _, KeyInit as _};
    let v: Value = serde_json::from_slice(json_bytes).map_err(|_| anyhow::anyhow!("cosign's key isn't the JSON it should be"))?;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(s("/kdf/name") == "scrypt" && s("/cipher/name") == "nacl/secretbox", "cosign's key: kdf {} and cipher {}, not scrypt and nacl/secretbox", s("/kdf/name"), s("/cipher/name"));
    let n = v.pointer("/kdf/params/N").and_then(Value::as_u64).unwrap_or(65536);
    let r = u32::try_from(v.pointer("/kdf/params/r").and_then(Value::as_u64).unwrap_or(8))?;
    let p = u32::try_from(v.pointer("/kdf/params/p").and_then(Value::as_u64).unwrap_or(1))?;
    anyhow::ensure!(n.is_power_of_two() && n > 1, "cosign's key: scrypt's N is {n}");
    let log_n = u8::try_from(n.trailing_zeros())?;
    let (salt, nonce, ct) = (B64.decode(s("/kdf/salt"))?, B64.decode(s("/cipher/nonce"))?, B64.decode(s("/ciphertext"))?);
    let params = scrypt::Params::new(log_n, r, p, 32).map_err(|e| anyhow::anyhow!("cosign's key: scrypt's parameters: {e}"))?;
    let mut key = [0u8; 32];
    scrypt::scrypt(password, &salt, &params, &mut key).map_err(|e| anyhow::anyhow!("scrypt: {e}"))?;
    anyhow::ensure!(nonce.len() == 24, "cosign's key: a nonce of {} bytes", nonce.len());
    let cipher = crypto_secretbox::XSalsa20Poly1305::new(&key.into());
    cipher.decrypt(crypto_secretbox::Nonce::from_slice(&nonce), ct.as_slice()).map_err(|_| anyhow::anyhow!("cosign's key doesn't open with that password"))
}

/// A key's public half (SPKI DER) and its id (its SHA-256).
fn spki(pair: &EcdsaKeyPair) -> anyhow::Result<(Vec<u8>, String)> {
    let der = pair.public_key().as_der().map_err(|_| anyhow::anyhow!("the key's public half"))?.as_ref().to_vec();
    let id = crate::util::sha256_hex(&der);
    Ok((der, id))
}

/// The configured signer: what the public key is, for verifiers (none for keyless).
pub async fn public_pem(hub: &Hub) -> anyhow::Result<Option<String>> {
    let c = &hub.cfg.ci.attest;
    Ok(match c.signing.as_deref().unwrap_or("kiln") {
        "kiln" => Some(super::attest::signer(hub)?.public_pem()),
        "key" => {
            let pair = configured_key(hub)?;
            Some(pem_of("PUBLIC KEY", &spki(&pair)?.0))
        }
        "kms" => Some(pem_of("PUBLIC KEY", &kms_public(hub).await?)),
        _ => None,
    })
}

fn configured_key(hub: &Hub) -> anyhow::Result<EcdsaKeyPair> {
    let c = &hub.cfg.ci.attest;
    let file = c.key_file.as_ref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"key\" needs key_file"))?;
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let password = match &c.key_password_file {
        Some(p) => std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?.trim_end_matches(['\n', '\r']).to_owned(),
        None => std::env::var("COSIGN_PASSWORD").unwrap_or_default(),
    };
    key_from_file(&text, password.as_bytes())
}

/// A Sigstore bundle of `statement`, signed as `[ci.attest] signing` says. `workload`: whose build
/// it is, for keyless signing's identity and KMS's federation.
pub async fn bundle(hub: &Hub, statement: &Value, workload: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<Value> {
    let payload = serde_json::to_vec(statement)?;
    let pae = a::pae(a::PAYLOAD_TYPE, &payload);
    match hub.cfg.ci.attest.signing.as_deref().unwrap_or("kiln") {
        "kiln" => super::attest::signer(hub)?.bundle(statement),
        "key" => {
            let pair = configured_key(hub)?;
            let (_, keyid) = spki(&pair)?;
            let sig = pair.sign(&SystemRandom::new(), &pae).map_err(|_| anyhow::anyhow!("signing failed"))?;
            Ok(a::bundle(a::envelope(a::PAYLOAD_TYPE, &payload, "", sig.as_ref()), &keyid))
        }
        "kms" => {
            let sig = kms_sign(hub, &pae, workload).await?;
            let keyid = crate::util::sha256_hex(&kms_public(hub).await?);
            Ok(a::bundle(a::envelope(a::PAYLOAD_TYPE, &payload, "", &sig), &keyid))
        }
        "keyless" => keyless(hub, &payload, &pae, workload).await,
        other => anyhow::bail!("[ci.attest] signing = \"{other}\": kiln, key, kms or keyless"),
    }
}

/// Whether `bundle`'s signature is the configured signer's (kiln's key, the key file's, KMS's).
/// Keyless bundles are verified against Fulcio's and Rekor's roots, which kiln doesn't keep: they
/// answer `Err`.
pub async fn verify(hub: &Hub, bundle: &Value) -> anyhow::Result<bool> {
    let Some((signed, sig)) = a::signed_of(bundle) else { return Ok(false) };
    let spki = match hub.cfg.ci.attest.signing.as_deref().unwrap_or("kiln") {
        "kiln" => {
            let pem = super::attest::signer(hub)?.public_pem();
            der_of(&pem).map(|(_, d)| d).unwrap_or_default()
        }
        "key" => spki(&configured_key(hub)?)?.0,
        "kms" => kms_public(hub).await?,
        other => anyhow::bail!("`{other}` signatures are verified against Sigstore's roots, not by kiln"),
    };
    // A P-256 key's uncompressed point ends its SPKI.
    let point = spki.get(spki.len().saturating_sub(65)..).unwrap_or_default();
    let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1, point);
    Ok(key.verify(&signed, &sig).is_ok())
}

// ------------------------------------------------------------------ KMS

/// `awskms:///arn:aws:kms:…` (or `awskms:///alias/…`): the key's id, and its region.
fn aws_key(uri: &str) -> anyhow::Result<(String, String)> {
    let id = uri.strip_prefix("awskms:///").or_else(|| uri.strip_prefix("awskms://")).ok_or_else(|| anyhow::anyhow!("`{uri}`: kiln signs with AWS KMS keys (awskms:///<arn>)"))?;
    let region = id.split(':').nth(3).map(str::to_owned).filter(|r| !r.is_empty()).or_else(|| std::env::var("AWS_REGION").ok()).ok_or_else(|| anyhow::anyhow!("`{uri}`: which region? (an ARN says)"))?;
    Ok((id.to_owned(), region))
}

async fn kms_client(hub: &Hub, workload: Option<&kiln_ci::oidc::Workload<'_>>) -> anyhow::Result<(crate::ci::integrations::aws::Aws, String)> {
    let c = &hub.cfg.ci.attest;
    let uri = c.kms.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"kms\" needs kms (awskms:///<arn>)"))?;
    let (key, region) = aws_key(uri)?;
    let name = c.integration.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"kms\" needs integration (an aws one kiln may use)"))?;
    let i = crate::ci::integrations::get(hub, name).await?.ok_or_else(|| anyhow::anyhow!("no integration `{name}`"))?;
    let w = workload.cloned().unwrap_or(kiln_ci::oidc::Workload { repository: "kiln", environment: None, git_ref: None, sha: None, actor: "kiln", source: "service", app: None, deployment: None, trigger: Some("attest") });
    let creds = crate::ci::integrations::aws::credentials(hub, &i, &w).await?;
    Ok((crate::ci::integrations::aws::Aws { creds, region }, key))
}

/// The KMS key's public half (SPKI DER), kept once read.
async fn kms_public(hub: &Hub) -> anyhow::Result<Vec<u8>> {
    static KEPT: std::sync::OnceLock<parking_lot::Mutex<Option<(String, Vec<u8>)>>> = std::sync::OnceLock::new();
    let kept = KEPT.get_or_init(Default::default);
    let uri = hub.cfg.ci.attest.kms.clone().unwrap_or_default();
    if let Some((u, der)) = kept.lock().as_ref()
        && *u == uri
    {
        return Ok(der.clone());
    }
    let (aws, key) = kms_client(hub, None).await?;
    let v = aws.json("kms", "TrentService.GetPublicKey", &json!({ "KeyId": key })).await?;
    let der = B64.decode(v.get("PublicKey").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("KMS gave no public key"))?)?;
    *kept.lock() = Some((uri, der.clone()));
    Ok(der)
}

/// KMS's ECDSA signature (DER) over `msg`'s SHA-256.
async fn kms_sign(hub: &Hub, msg: &[u8], workload: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<Vec<u8>> {
    let (aws, key) = kms_client(hub, Some(workload)).await?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, msg);
    let v = aws.json("kms", "TrentService.Sign", &json!({ "KeyId": key, "Message": B64.encode(digest.as_ref()), "MessageType": "DIGEST", "SigningAlgorithm": "ECDSA_SHA_256" })).await?;
    Ok(B64.decode(v.get("Signature").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("KMS gave no signature"))?)?)
}

// ------------------------------------------------------------------ keyless

/// Keyless: an ephemeral key, Fulcio's certificate for it (proven with the identity token's
/// subject, signed), the signature logged in Rekor as a DSSE entry, and both in the bundle.
async fn keyless(hub: &Hub, payload: &[u8], pae: &[u8], workload: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<Value> {
    let c = &hub.cfg.ci.attest;
    let (fulcio, rekor) = (c.fulcio_url.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"keyless\" needs fulcio_url"))?, c.rekor_url.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"keyless\" needs rekor_url"))?);
    let (fulcio, rekor) = (fulcio.trim_end_matches('/'), rekor.trim_end_matches('/'));
    // kiln's identity token for the build, for Fulcio.
    let issuer = super::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(workload, &issuer, c.audience.as_deref().unwrap_or("sigstore"), &crate::util::random_token(), crate::util::now_unix(), 300, None);
    let subject = claims.get("sub").and_then(Value::as_str).unwrap_or_default().to_owned();
    let token = super::oidc::mint(hub, &claims).await?;
    // A key for this one signature.
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("no signing key could be made"))?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, doc.as_ref()).map_err(|_| anyhow::anyhow!("the signing key"))?;
    let (public, _) = spki(&pair)?;
    let proof = pair.sign(&SystemRandom::new(), subject.as_bytes()).map_err(|_| anyhow::anyhow!("signing the proof"))?;
    let http = crate::http::client::Client::public()?;
    let ask = json!({ "credentials": { "oidcIdentityToken": token }, "publicKeyRequest": { "publicKey": { "algorithm": "ECDSA", "content": pem_of("PUBLIC KEY", &public) }, "proofOfPossession": B64.encode(proof.as_ref()) } });
    let r = http.post(&format!("{fulcio}/api/v2/signingCert")).json(&ask).send().await.map_err(|e| anyhow::anyhow!("Fulcio: {e:#}"))?;
    let st = r.status();
    let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
    anyhow::ensure!((200..300).contains(&st), "Fulcio issued no certificate ({st}): {v}");
    let chain = v.pointer("/signedCertificateEmbeddedSct/chain/certificates").or_else(|| v.pointer("/signedCertificateDetachedSct/chain/certificates")).and_then(Value::as_array).cloned().unwrap_or_default();
    let leaf_pem = chain.first().and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Fulcio's answer has no certificate"))?.to_owned();
    let (_, leaf) = der_of(&leaf_pem).ok_or_else(|| anyhow::anyhow!("Fulcio's certificate isn't PEM"))?;
    // The signature, and its entry in Rekor.
    let sig = pair.sign(&SystemRandom::new(), pae).map_err(|_| anyhow::anyhow!("signing failed"))?;
    let envelope = a::envelope(a::PAYLOAD_TYPE, payload, "", sig.as_ref());
    let entry = json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": { "proposedContent": { "envelope": envelope.to_string(), "verifiers": [B64.encode(leaf_pem.as_bytes())] } } });
    let r = http.post(&format!("{rekor}/api/v1/log/entries")).json(&entry).send().await.map_err(|e| anyhow::anyhow!("Rekor: {e:#}"))?;
    let st = r.status();
    let logged: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
    anyhow::ensure!(st == 201 || st == 200, "Rekor logged nothing ({st}): {logged}");
    let (uuid, e) = logged.as_object().and_then(|o| o.iter().next()).map(|(k, v)| (k.clone(), v.clone())).ok_or_else(|| anyhow::anyhow!("Rekor's answer has no entry"))?;
    let tlog = json!({
        "logIndex": e.get("logIndex").map(|n| n.to_string()).unwrap_or_default(),
        "logId": { "keyId": e.get("logID").and_then(Value::as_str).and_then(|h| hex::decode(h).ok()).map(|b| B64.encode(b)).unwrap_or_default() },
        "kindVersion": { "kind": "dsse", "version": "0.0.1" },
        "integratedTime": e.get("integratedTime").map(|n| n.to_string()).unwrap_or_default(),
        "inclusionPromise": { "signedEntryTimestamp": e.pointer("/verification/signedEntryTimestamp").cloned().unwrap_or(Value::Null) },
        "inclusionProof": e.pointer("/verification/inclusionProof").map(|p| json!({
            "logIndex": p.get("logIndex").map(|n| n.to_string()).unwrap_or_default(),
            "rootHash": p.get("rootHash").and_then(Value::as_str).and_then(|h| hex::decode(h).ok()).map(|b| B64.encode(b)),
            "treeSize": p.get("treeSize").map(|n| n.to_string()).unwrap_or_default(),
            "hashes": p.get("hashes").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).filter_map(|h| hex::decode(h).ok()).map(|b| B64.encode(b)).collect::<Vec<_>>()),
            "checkpoint": { "envelope": p.get("checkpoint").cloned().unwrap_or(Value::Null) },
        })),
        "canonicalizedBody": e.get("body").cloned().unwrap_or(Value::Null),
    });
    tracing::debug!(entry = %uuid, "an attestation logged in Rekor");
    Ok(json!({ "mediaType": a::BUNDLE, "verificationMaterial": { "certificate": { "rawBytes": B64.encode(&leaf) }, "tlogEntries": [tlog] }, "dsseEnvelope": envelope }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w18_r02_key_files() -> anyhow::Result<()> {
        // An unencrypted PKCS#8 key, as PEM.
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("keygen"))?;
        let pem = pem_of("PRIVATE KEY", doc.as_ref());
        let pair = key_from_file(&pem, b"")?;
        let (_, id) = spki(&pair)?;
        assert_eq!(id.len(), 64);
        // cosign's encrypted format, made as cosign makes it (scrypt, then NaCl's secretbox).
        use crypto_secretbox::aead::{Aead as _, KeyInit as _};
        let (salt, nonce) = ([7u8; 32], [9u8; 24]);
        let params = scrypt::Params::new(10, 8, 1, 32).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut key = [0u8; 32];
        scrypt::scrypt(b"hunter2", &salt, &params, &mut key).map_err(|e| anyhow::anyhow!("{e}"))?;
        let ct = crypto_secretbox::XSalsa20Poly1305::new(&key.into()).encrypt(crypto_secretbox::Nonce::from_slice(&nonce), doc.as_ref()).map_err(|_| anyhow::anyhow!("seal"))?;
        let body = json!({ "kdf": { "name": "scrypt", "params": { "N": 1024, "r": 8, "p": 1 }, "salt": B64.encode(salt) }, "cipher": { "name": "nacl/secretbox", "nonce": B64.encode(nonce) }, "ciphertext": B64.encode(&ct) });
        let enc = pem_of("ENCRYPTED SIGSTORE PRIVATE KEY", body.to_string().as_bytes());
        let opened = key_from_file(&enc, b"hunter2")?;
        assert_eq!(spki(&opened)?.1, id, "the same key");
        assert!(key_from_file(&enc, b"wrong").is_err());
        assert_eq!(aws_key("awskms:///arn:aws:kms:eu-west-1:123:key/abc")?, ("arn:aws:kms:eu-west-1:123:key/abc".to_owned(), "eu-west-1".to_owned()));
        Ok(())
    }
}
