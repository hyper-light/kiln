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
pub(crate) fn der_of(pem: &str) -> Option<(String, Vec<u8>)> {
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
/// Keyless bundles verify as Sigstore's do ([`super::sigstore`]): against the trusted root
/// (`[ci.attest] trusted_root`), signed by an identity of kiln's OIDC issuer.
pub async fn verify(hub: &Hub, bundle: &Value) -> anyhow::Result<bool> {
    if hub.cfg.ci.attest.signing.as_deref() == Some("keyless") {
        let root = super::sigstore::trusted_root(&hub.cfg).await?;
        let identity = super::sigstore::Identity { issuer: super::oidc::issuer(hub), subject: None, claims: Vec::new() };
        return Ok(super::sigstore::verify(bundle, &root, &identity, None).map_err(|e| tracing::debug!(error = %format!("{e:#}"), "a keyless bundle didn't verify")).is_ok());
    }
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

/// kiln's client of an outside Fulcio and Rekor: the real `kiln_seams::Sigstore` (docs/seams.md,
/// "Sigstore"; W27-R12).
pub struct Remote {
    fulcio: String,
    rekor: String,
    http: crate::http::client::Client,
}

impl Remote {
    pub fn new(fulcio: &str, rekor: &str) -> anyhow::Result<Self> {
        Ok(Self { fulcio: fulcio.trim_end_matches('/').to_owned(), rekor: rekor.trim_end_matches('/').to_owned(), http: crate::http::client::Client::public()? })
    }

    /// The Fulcio and Rekor `[ci.attest]` names.
    fn configured(hub: &Hub) -> anyhow::Result<Self> {
        let c = &hub.cfg.ci.attest;
        anyhow::ensure!(
            c.rekor_api == crate::config::ci::RekorApi::V1,
            "[ci.attest] rekor_api = \"v2\": keyless signing logs DSSE entries, which Rekor v2 doesn't take (it anchors the trail's checkpoints); sign keyless against a v1 Rekor"
        );
        let fulcio = c.fulcio_url.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"keyless\" needs fulcio_url"))?;
        let rekor = c.rekor_url.as_deref().ok_or_else(|| anyhow::anyhow!("[ci.attest] signing = \"keyless\" needs rekor_url"))?;
        Self::new(fulcio, rekor)
    }

    /// Rekor's entry `uuid`.
    async fn by_uuid(&self, uuid: &str) -> Result<Logged, SigRefusal> {
        let r = self.http.get(&format!("{}/api/v1/log/entries/{uuid}", self.rekor)).send().await.map_err(|e| SigRefusal::Unavailable(format!("Rekor: {e:#}")))?;
        let st = r.status();
        let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
        match st {
            200 => Self::logged(&v),
            _ => Err(SigRefusal::Unavailable(format!("Rekor's entry {uuid} ({st}): {v}"))),
        }
    }

    /// One of Rekor's entries (`{uuid: entry}`), as the seam has it: its proof of inclusion (hex,
    /// as v1 writes it) and its promise from its `verification`.
    fn logged(v: &Value) -> Result<Logged, SigRefusal> {
        let (uuid, e) = v.as_object().and_then(|o| o.iter().next()).ok_or_else(|| SigRefusal::Unavailable(format!("Rekor's answer has no entry: {v}")))?;
        let p = e.pointer("/verification/inclusionProof");
        let num = |k: &str| p.and_then(|p| p.get(k)).and_then(Value::as_u64);
        let proof = p.zip(num("logIndex")).zip(num("treeSize")).map(|((p, index), size)| Proof {
            index,
            size,
            root: hex::decode(p.get("rootHash").and_then(Value::as_str).unwrap_or_default()).unwrap_or_default(),
            hashes: p.get("hashes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|h| hex::decode(h).ok()).collect(),
            checkpoint: p.get("checkpoint").and_then(Value::as_str).unwrap_or_default().to_owned(),
        });
        Ok(Logged {
            uuid: uuid.clone(),
            index: e.get("logIndex").and_then(Value::as_u64).unwrap_or_default(),
            integrated_time: e.get("integratedTime").and_then(Value::as_i64).unwrap_or_default(),
            body: e.get("body").and_then(Value::as_str).unwrap_or_default().to_owned(),
            answer: e.clone(),
            proof,
            promise: e.pointer("/verification/signedEntryTimestamp").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

use kiln_seams::sigstore::{CertifyAsk, Certified, Logged, Proof, Refusal as SigRefusal};

/// kiln's client of a Rekor v2 (rekor-tiles), beside [`Remote`]'s Fulcio: an entry logged at
/// `/api/v2/log/entries`, a hashedrekord (the one kind it takes), and read back from its tiles (C2SP
/// tlog-tiles under `/api/v2`), proven against its checkpoint. It promises nothing: its checkpoints
/// and proofs are what hold it (docs/seams.md, "Sigstore").
pub struct RemoteV2 {
    fulcio: Remote,
    rekor: String,
    http: crate::http::client::Client,
}

impl RemoteV2 {
    pub fn new(fulcio: &str, rekor: &str) -> anyhow::Result<Self> {
        Ok(Self { fulcio: Remote::new(fulcio, rekor)?, rekor: rekor.trim_end_matches('/').to_owned(), http: crate::http::client::Client::public()? })
    }

    /// A v1 hashedrekord entry as v2 asks for it: its digest, its signature, and the key that made
    /// it (a certificate or a public key, DER) with its kind.
    fn request(entry: &Value) -> Result<Value, SigRefusal> {
        let bad = |why: &str| SigRefusal::Invalid { what: "entry", why: why.to_owned() };
        if entry.get("kind").and_then(Value::as_str) != Some("hashedrekord") {
            return Err(bad("Rekor v2 takes hashedrekord entries alone"));
        }
        let spec = entry.get("spec").ok_or_else(|| bad("it has no spec"))?;
        let digest = hex::decode(spec.pointer("/data/hash/value").and_then(Value::as_str).unwrap_or_default()).map_err(|_| bad("its hash isn't hex"))?;
        let key = B64.decode(spec.pointer("/signature/publicKey/content").and_then(Value::as_str).unwrap_or_default()).ok().and_then(|k| String::from_utf8(k).ok()).ok_or_else(|| bad("its key isn't base64 of a PEM"))?;
        let verifier = match der_of(&key) {
            Some((label, der)) if label == "CERTIFICATE" => json!({ "x509Certificate": { "rawBytes": B64.encode(der) }, "keyDetails": "PKIX_ECDSA_P256_SHA_256" }),
            Some((label, der)) if label == "PUBLIC KEY" => json!({ "publicKey": { "rawBytes": B64.encode(der) }, "keyDetails": "PKIX_ECDSA_P256_SHA_256" }),
            _ => return Err(bad("its key isn't a PEM certificate or public key")),
        };
        let content = spec.pointer("/signature/content").cloned().unwrap_or(Value::Null);
        Ok(json!({ "hashedRekordRequestV002": { "digest": B64.encode(digest), "signature": { "content": content, "verifier": verifier } } }))
    }

    /// One of Rekor v2's entries as it answers for it (a TransparencyLogEntry: its numbers as
    /// strings, its hashes base64), as the seam has it.
    fn logged(v: &Value) -> Result<Logged, SigRefusal> {
        let num = |x: Option<&Value>| x.and_then(|x| x.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| x.as_u64()));
        let index = num(v.get("logIndex")).ok_or_else(|| SigRefusal::Unavailable(format!("Rekor v2's answer has no index: {v}")))?;
        let body = v.get("canonicalizedBody").and_then(Value::as_str).unwrap_or_default().to_owned();
        let raw = B64.decode(&body).map_err(|_| SigRefusal::Unavailable(format!("entry {index}'s body isn't base64")))?;
        let proof = v.get("inclusionProof").and_then(|p| {
            Some(Proof {
                index: num(p.get("logIndex"))?,
                size: num(p.get("treeSize"))?,
                root: B64.decode(p.get("rootHash").and_then(Value::as_str).unwrap_or_default()).unwrap_or_default(),
                hashes: p.get("hashes").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).filter_map(|h| B64.decode(h).ok()).collect(),
                checkpoint: p.pointer("/checkpoint/envelope").and_then(Value::as_str).unwrap_or_default().to_owned(),
            })
        });
        let integrated_time = num(v.get("integratedTime")).and_then(|t| i64::try_from(t).ok()).unwrap_or(0);
        let promise = v.pointer("/inclusionPromise/signedEntryTimestamp").and_then(Value::as_str).map(str::to_owned);
        Ok(Logged { uuid: hex::encode(kiln_ci::identity::trail::leaf(&raw)), index, integrated_time, body, answer: v.clone(), proof, promise })
    }

    /// `<rekor>/api/v2/<path>`'s bytes; none for what it hasn't.
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, SigRefusal> {
        let r = self.http.get(&format!("{}/api/v2/{path}", self.rekor)).send().await.map_err(|e| SigRefusal::Unavailable(format!("Rekor v2: {e:#}")))?;
        match r.status() {
            200 => Ok(Some(r.bytes(64 << 20).await.map_err(|e| SigRefusal::Unavailable(format!("Rekor v2's {path}: {e:#}")))?.to_vec())),
            404 => Ok(None),
            st => Err(SigRefusal::Unavailable(format!("Rekor v2's {path} ({st})"))),
        }
    }

    /// Entry `index` read from the log's tiles: its bundle's entry, and the proof of it and the root
    /// hashed up from the tiles they ask for, held to the log's checkpoint now.
    async fn read_back(&self, index: u64) -> Result<Logged, SigRefusal> {
        use kiln_ci::identity::trail::{self, Tile};
        let down = SigRefusal::Unavailable;
        let note = String::from_utf8(self.get("checkpoint").await?.ok_or_else(|| down("Rekor v2 serves no checkpoint".into()))?).map_err(|_| down("Rekor v2's checkpoint isn't text".into()))?;
        let cp = trail::open(&note).ok().and_then(|(text, _)| trail::Checkpoint::parse(text).ok()).ok_or_else(|| down(format!("Rekor v2's checkpoint isn't one: {note:?}")))?;
        if index >= cp.size {
            return Err(SigRefusal::NotFound { index });
        }
        let bundle = Tile { level: 0, index: index / trail::TILE, width: trail::width_at(cp.size, 0, index / trail::TILE).unwrap_or(0) };
        let entries = trail::unbundle(&self.get(&bundle.path(true)).await?.ok_or_else(|| down(format!("Rekor v2 serves no {}", bundle.path(true))))?).ok_or_else(|| down("Rekor v2's entry bundle is cut short".into()))?;
        let data = entries.get(usize::try_from(index % trail::TILE).unwrap_or(usize::MAX)).cloned().ok_or_else(|| down(format!("entry {index} isn't in its bundle")))?;
        let leaf = trail::leaf(&data);
        let mut have: std::collections::HashMap<(u8, u64, u64), Vec<trail::Hash>> = std::collections::HashMap::new();
        let (hashes, root) = loop {
            let mut missing: Option<Tile> = None;
            let found = {
                let mut tile = |t: Tile| {
                    let got = have.get(&(t.level, t.index, t.width)).cloned();
                    if got.is_none() {
                        missing.get_or_insert(t);
                    }
                    got
                };
                let proof = trail::inclusion_by(cp.size, index, &mut |lo, hi| trail::mth_from(cp.size, lo, hi, &mut tile));
                proof.and_then(|p| trail::mth_from(cp.size, 0, cp.size, &mut tile).map(|r| (p, r)))
            };
            if let Some(done) = found {
                break done;
            }
            let t = missing.ok_or_else(|| down(format!("entry {index}'s proof can't be made from Rekor v2's tiles")))?;
            let bytes = self.get(&t.path(false)).await?.ok_or_else(|| down(format!("Rekor v2 serves no {}", t.path(false))))?;
            have.insert((t.level, t.index, t.width), trail::hashes_of(&t, &bytes).ok_or_else(|| down(format!("Rekor v2's {} isn't {} hashes", t.path(false), t.width)))?);
        };
        if root != cp.root || !trail::verify_inclusion(&leaf, index, cp.size, &hashes, &cp.root) {
            return Err(down(format!("Rekor v2's tiles don't make the root its checkpoint names at {}", cp.size)));
        }
        let body = B64.encode(&data);
        let encoded: Vec<String> = hashes.iter().map(|h| B64.encode(h)).collect();
        let answer = json!({ "logIndex": index.to_string(), "canonicalizedBody": body, "inclusionProof": { "logIndex": index.to_string(), "rootHash": B64.encode(cp.root), "treeSize": cp.size.to_string(), "hashes": encoded, "checkpoint": { "envelope": note } } });
        let proof = Proof { index, size: cp.size, root: cp.root.to_vec(), hashes: hashes.iter().map(|h| h.to_vec()).collect(), checkpoint: note };
        Ok(Logged { uuid: hex::encode(leaf), index, integrated_time: 0, body, answer, proof: Some(proof), promise: None })
    }
}

impl kiln_seams::Sigstore for RemoteV2 {
    fn certify(&self, ask: CertifyAsk) -> futures::future::BoxFuture<'_, Result<Certified, SigRefusal>> {
        kiln_seams::Sigstore::certify(&self.fulcio, ask)
    }

    fn log(&self, entry: Value) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
        Box::pin(async move {
            let request = Self::request(&entry)?;
            let r = self.http.post(&format!("{}/api/v2/log/entries", self.rekor)).json(&request).send().await.map_err(|e| SigRefusal::Unavailable(format!("Rekor v2: {e:#}")))?;
            let st = r.status();
            // Rekor v2 names the entry it has already in a header (x-log-index).
            let had = r.header("x-log-index").and_then(|i| i.trim().parse::<u64>().ok());
            let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
            match st {
                200 | 201 => Self::logged(&v),
                409 => Err(SigRefusal::Conflict { index: had.ok_or_else(|| SigRefusal::Unavailable(format!("Rekor v2 has the entry already but doesn't say where: {v}")))? }),
                400 => Err(SigRefusal::Invalid { what: "entry", why: v.to_string() }),
                _ => Err(SigRefusal::Unavailable(format!("Rekor v2 logged nothing ({st}): {v}"))),
            }
        })
    }

    fn entry(&self, index: u64) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
        Box::pin(async move { self.read_back(index).await })
    }
}

impl kiln_seams::Sigstore for Remote {
    fn certify(&self, ask: CertifyAsk) -> futures::future::BoxFuture<'_, Result<Certified, SigRefusal>> {
        Box::pin(async move {
            let body = json!({ "credentials": { "oidcIdentityToken": ask.token }, "publicKeyRequest": { "publicKey": { "algorithm": "ECDSA", "content": ask.public_key_pem }, "proofOfPossession": B64.encode(&ask.proof) } });
            let r = self.http.post(&format!("{}/api/v2/signingCert", self.fulcio)).json(&body).send().await.map_err(|e| SigRefusal::Unavailable(format!("Fulcio: {e:#}")))?;
            let st = r.status();
            let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
            match st {
                200..=299 => {}
                // Fulcio names what it couldn't take: "The public key supplied in the request could
                // not be parsed", "The signature supplied in the request could not be verified".
                400 => {
                    let message = v.get("message").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
                    let what = if message.contains("public key") {
                        "public key"
                    } else if message.contains("signature") {
                        "proof"
                    } else {
                        "request"
                    };
                    return Err(SigRefusal::Invalid { what, why: v.to_string() });
                }
                401 | 403 => return Err(SigRefusal::Unauthorized(v.to_string())),
                _ => return Err(SigRefusal::Unavailable(format!("Fulcio issued no certificate ({st}): {v}"))),
            }
            let chain = v.pointer("/signedCertificateEmbeddedSct/chain/certificates").or_else(|| v.pointer("/signedCertificateDetachedSct/chain/certificates")).and_then(Value::as_array).cloned().unwrap_or_default();
            let chain: Vec<String> = chain.iter().filter_map(Value::as_str).map(str::to_owned).collect();
            if chain.is_empty() {
                return Err(SigRefusal::Unavailable("Fulcio's answer has no certificate".into()));
            }
            Ok(Certified { chain })
        })
    }

    fn log(&self, entry: Value) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
        Box::pin(async move {
            let r = self.http.post(&format!("{}/api/v1/log/entries", self.rekor)).json(&entry).send().await.map_err(|e| SigRefusal::Unavailable(format!("Rekor: {e:#}")))?;
            let st = r.status();
            let location = r.header("location").map(str::to_owned);
            let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
            match st {
                200 | 201 => Self::logged(&v),
                // Rekor names the entry it has already where its answer points (Location), and in
                // its message ("… with UUID <uuid>"): where it is, read from there.
                409 => {
                    let uuid = location
                        .as_deref()
                        .and_then(|l| l.trim_end_matches('/').rsplit('/').next())
                        .or_else(|| v.get("message").and_then(Value::as_str).and_then(|m| m.rsplit(' ').next()))
                        .filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_hexdigit()))
                        .ok_or_else(|| SigRefusal::Unavailable(format!("Rekor has the entry already but doesn't say where: {v}")))?;
                    Err(SigRefusal::Conflict { index: self.by_uuid(uuid).await?.index })
                }
                400 => Err(SigRefusal::Invalid { what: "entry", why: v.to_string() }),
                _ => Err(SigRefusal::Unavailable(format!("Rekor logged nothing ({st}): {v}"))),
            }
        })
    }

    fn entry(&self, index: u64) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
        Box::pin(async move {
            let r = self.http.get(&format!("{}/api/v1/log/entries?logIndex={index}", self.rekor)).send().await.map_err(|e| SigRefusal::Unavailable(format!("Rekor: {e:#}")))?;
            let st = r.status();
            let v: Value = r.json(1 << 20).await.unwrap_or(Value::Null);
            match st {
                200 => Self::logged(&v),
                404 => Err(SigRefusal::NotFound { index }),
                _ => Err(SigRefusal::Unavailable(format!("Rekor ({st}): {v}"))),
            }
        })
    }
}

/// Keyless: an ephemeral key, Fulcio's certificate for it (proven with the identity token's
/// subject, signed), the signature logged in Rekor as a DSSE entry, and both in the bundle.
async fn keyless(hub: &Hub, payload: &[u8], pae: &[u8], workload: &kiln_ci::oidc::Workload<'_>) -> anyhow::Result<Value> {
    let c = &hub.cfg.ci.attest;
    let remote = Remote::configured(hub)?;
    // kiln's identity token for the build, for Fulcio.
    let issuer = super::oidc::issuer(hub);
    let claims = kiln_ci::oidc::workload_claims(workload, &issuer, c.audience.as_deref().unwrap_or("sigstore"), &crate::util::random_token(), crate::util::now_unix(), 300, None);
    let subject = claims.get("sub").and_then(Value::as_str).unwrap_or_default().to_owned();
    let token = super::oidc::mint(hub, &claims).await?;
    keyless_through(&remote, &token, &subject, payload, pae).await
}

/// Keyless signing through any Fulcio and Rekor (the seam's): `token` is the identity token and
/// `subject` its subject, which the one-off key signs as its proof.
pub async fn keyless_through(sigstore: &dyn kiln_seams::Sigstore, token: &str, subject: &str, payload: &[u8], pae: &[u8]) -> anyhow::Result<Value> {
    // A key for this one signature.
    let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("no signing key could be made"))?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, doc.as_ref()).map_err(|_| anyhow::anyhow!("the signing key"))?;
    let (public, _) = spki(&pair)?;
    let proof = pair.sign(&SystemRandom::new(), subject.as_bytes()).map_err(|_| anyhow::anyhow!("signing the proof"))?;
    let certified = sigstore
        .certify(CertifyAsk { token: token.to_owned(), public_key_pem: pem_of("PUBLIC KEY", &public), proof: proof.as_ref().to_vec() })
        .await
        .map_err(|e| anyhow::anyhow!("Fulcio: {e}"))?;
    let leaf_pem = certified.chain.first().cloned().ok_or_else(|| anyhow::anyhow!("Fulcio's answer has no certificate"))?;
    let (_, leaf) = der_of(&leaf_pem).ok_or_else(|| anyhow::anyhow!("Fulcio's certificate isn't PEM"))?;
    // The signature, and its entry in Rekor.
    let sig = pair.sign(&SystemRandom::new(), pae).map_err(|_| anyhow::anyhow!("signing failed"))?;
    let envelope = a::envelope(a::PAYLOAD_TYPE, payload, "", sig.as_ref());
    let entry = json!({ "apiVersion": "0.0.1", "kind": "dsse", "spec": { "proposedContent": { "envelope": envelope.to_string(), "verifiers": [B64.encode(leaf_pem.as_bytes())] } } });
    let logged = sigstore.log(entry).await.map_err(|e| anyhow::anyhow!("Rekor: {e}"))?;
    let e = &logged.answer;
    let tlog = json!({
        "logIndex": logged.index.to_string(),
        "logId": { "keyId": e.get("logID").and_then(Value::as_str).and_then(|h| hex::decode(h).ok()).map(|b| B64.encode(b)).unwrap_or_default() },
        "kindVersion": { "kind": "dsse", "version": "0.0.1" },
        "integratedTime": logged.integrated_time.to_string(),
        "inclusionPromise": { "signedEntryTimestamp": e.pointer("/verification/signedEntryTimestamp").cloned().unwrap_or(Value::Null) },
        "inclusionProof": e.pointer("/verification/inclusionProof").map(|p| json!({
            "logIndex": p.get("logIndex").map(|n| n.to_string()).unwrap_or_default(),
            "rootHash": p.get("rootHash").and_then(Value::as_str).and_then(|h| hex::decode(h).ok()).map(|b| B64.encode(b)),
            "treeSize": p.get("treeSize").map(|n| n.to_string()).unwrap_or_default(),
            "hashes": p.get("hashes").and_then(Value::as_array).map(|l| l.iter().filter_map(Value::as_str).filter_map(|h| hex::decode(h).ok()).map(|b| B64.encode(b)).collect::<Vec<_>>()),
            "checkpoint": { "envelope": p.get("checkpoint").cloned().unwrap_or(Value::Null) },
        })),
        "canonicalizedBody": logged.body,
    });
    tracing::debug!(entry = %logged.uuid, "an attestation logged in Rekor");
    Ok(json!({ "mediaType": a::BUNDLE, "verificationMaterial": { "certificate": { "rawBytes": B64.encode(&leaf) }, "tlogEntries": [tlog] }, "dsseEnvelope": envelope }))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;

    use kiln_seams::sigstore::{Asker, CertifyAsk, Certified, Logged, Refusal as SigRefusal, SigstoreFixture, SimSigstore};

    use crate::http::server::{self as srv, Request, Response};

    /// W27-R12: a bundle keyless signing makes through the simulated Fulcio and Rekor verifies with
    /// kiln's Sigstore verifier, against the simulator's trust, as the identity the token names; not
    /// as another issuer's, nor against another Sigstore's trust, nor with its statement changed.
    #[test]
    fn w27_r12_keyless_signing_through_the_simulator_verifies() -> anyhow::Result<()> {
        use super::super::sigstore::{Identity, Subject, TrustedRoot, verify};
        let (issuer, subject, digest) = ("https://kiln.test/oidc", "repo:acme/app:ref:refs/heads/main", "ab".repeat(32));
        let statement = json!({ "_type": "https://in-toto.io/Statement/v1", "subject": [{ "name": "app", "digest": { "sha256": digest } }], "predicateType": "https://slsa.dev/provenance/v1", "predicate": {} });
        let payload = serde_json::to_vec(&statement)?;
        let pae = a::pae(a::PAYLOAD_TYPE, &payload);
        let sim = SimSigstore::new(1_790_000_000);
        let bundle = futures::executor::block_on(keyless_through(&sim, &kiln_seams::sigstore::token_of(issuer, subject), subject, &payload, &pae))?;
        let trust = |s: &SimSigstore| -> anyhow::Result<TrustedRoot> { TrustedRoot::from_json(s.trusted_root("https://fulcio.test", "https://rekor.test").map_err(|e| anyhow::anyhow!("{e}"))?.to_string().as_bytes()) };
        let root = trust(&sim)?;
        let who = Identity { issuer: issuer.into(), subject: Some(Subject::Exact(subject.into())), claims: Vec::new() };
        let v = verify(&bundle, &root, &who, Some(("sha256", digest.as_str())))?;
        assert_eq!((v.issuer.as_str(), v.subject.as_str(), v.log_index, v.log.as_str()), (issuer, subject, 0, "https://rekor.test"));
        assert!(verify(&bundle, &root, &Identity { issuer: "https://token.actions.githubusercontent.com".into(), ..who.clone() }, None).is_err(), "another issuer's");
        assert!(verify(&bundle, &trust(&SimSigstore::seeded(1_790_000_000, 1))?, &who, None).is_err(), "another Sigstore's trust");
        let mut changed = bundle.clone();
        if let Some(p) = changed.pointer_mut("/dsseEnvelope/payload") {
            *p = json!(B64.encode(b"{\"_type\":\"https://in-toto.io/Statement/v1\",\"subject\":[]}"));
        }
        assert!(verify(&changed, &root, &who, None).is_err(), "its statement changed");
        Ok(())
    }

    /// Fulcio and Rekor over their HTTP APIs, answering as they do, in front of a simulator.
    struct FakeSigstore(SimSigstore);

    fn entry_json(l: &Logged) -> Value {
        let mut m = serde_json::Map::new();
        m.insert(l.uuid.clone(), l.answer.clone());
        Value::Object(m)
    }

    impl srv::Handler for FakeSigstore {
        async fn handle(self: Arc<Self>, req: Request) -> Response {
            use kiln_seams::Sigstore as _;
            let (method, path, query) = (req.method().clone(), req.uri().path().to_owned(), req.uri().query().unwrap_or_default().to_owned());
            let raw = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            let say = |status: u16, v: Value| srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR), &v);
            let where_ = |r: Response, uuid: &str| srv::with_header(r, http::header::LOCATION, &format!("/api/v1/log/entries/{uuid}"));
            let found = |l: Result<Logged, SigRefusal>| match l {
                Ok(l) => say(200, entry_json(&l)),
                Err(e) => say(404, json!({ "code": 404, "message": e.to_string() })),
            };
            let sim = &self.0;
            match (method.as_str(), path.as_str()) {
                ("POST", "/fulcio/api/v2/signingCert") => {
                    let s = |p: &str| body.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
                    let ask = CertifyAsk { token: s("/credentials/oidcIdentityToken"), public_key_pem: s("/publicKeyRequest/publicKey/content"), proof: B64.decode(s("/publicKeyRequest/proofOfPossession")).unwrap_or_default() };
                    match sim.certify(ask).await {
                        Ok(Certified { chain }) => say(201, json!({ "signedCertificateEmbeddedSct": { "chain": { "certificates": chain } } })),
                        Err(SigRefusal::Unauthorized(_)) => say(401, json!({ "code": 16, "message": "There was an error processing the identity token" })),
                        Err(SigRefusal::Invalid { what: "public key", .. }) => say(400, json!({ "code": 3, "message": "The public key supplied in the request could not be parsed" })),
                        Err(SigRefusal::Invalid { what: "proof", .. }) => say(400, json!({ "code": 3, "message": "The signature supplied in the request could not be verified" })),
                        Err(e) => say(500, json!({ "code": 13, "message": e.to_string() })),
                    }
                }
                ("POST", "/rekor/api/v1/log/entries") => match sim.log(body).await {
                    Ok(l) => where_(say(201, entry_json(&l)), &l.uuid),
                    Err(SigRefusal::Conflict { index }) => {
                        let uuid = sim.entry(index).await.map(|l| l.uuid).unwrap_or_default();
                        where_(say(409, json!({ "code": 409, "message": format!("An equivalent entry already exists in the transparency log with UUID {uuid}") })), &uuid)
                    }
                    Err(SigRefusal::Invalid { why, .. }) => say(400, json!({ "code": 400, "message": why })),
                    Err(e) => say(500, json!({ "code": 500, "message": e.to_string() })),
                },
                ("GET", "/rekor/api/v1/log/entries") => match query.split('&').find_map(|kv| kv.strip_prefix("logIndex=")).and_then(|n| n.parse::<u64>().ok()) {
                    Some(index) => found(sim.entry(index).await),
                    None => say(400, json!({ "code": 400, "message": "logIndex is required" })),
                },
                ("GET", p) if p.starts_with("/rekor/api/v1/log/entries/") => match sim.index_of(p.trim_start_matches("/rekor/api/v1/log/entries/")) {
                    Some(index) => found(sim.entry(index).await),
                    None => say(404, json!({ "code": 404, "message": "entry not found" })),
                },
                ("POST", "/rekor2/api/v2/log/entries") => v2_log(sim, &body).await,
                ("GET", p) if p.starts_with("/rekor2/api/v2/") => v2_read(sim, p.trim_start_matches("/rekor2/api/v2/")),
                _ => say(404, json!({ "code": 404, "message": format!("no {method} {path}") })),
            }
        }
    }

    /// Rekor v2's answer for an entry the simulator logged: a TransparencyLogEntry.
    fn v2_entry(l: &Logged, log_id: &[u8]) -> Value {
        let proof = l.proof.as_ref().map(|p| {
            let hashes: Vec<String> = p.hashes.iter().map(|h| B64.encode(h)).collect();
            json!({ "logIndex": p.index.to_string(), "rootHash": B64.encode(&p.root), "treeSize": p.size.to_string(), "hashes": hashes, "checkpoint": { "envelope": p.checkpoint } })
        });
        json!({ "logIndex": l.index.to_string(), "logId": { "keyId": B64.encode(log_id) }, "kindVersion": { "kind": "hashedrekord", "version": "0.0.1" }, "integratedTime": "0", "inclusionProof": proof, "canonicalizedBody": l.body })
    }

    /// `POST /api/v2/log/entries` as Rekor v2 answers it, in front of the simulator: a hashedrekord
    /// request alone; one it has already, 409 naming where (x-log-index).
    async fn v2_log(sim: &SimSigstore, body: &Value) -> Response {
        use kiln_seams::Sigstore as _;
        let say = |status: u16, v: Value| srv::json(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR), &v);
        let Some(r) = body.get("hashedRekordRequestV002") else { return say(400, json!({ "code": 3, "message": "invalid type, must be hashedrekord" })) };
        let text = |p: &str| r.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned();
        let (der, label) = if r.pointer("/signature/verifier/x509Certificate").is_some() { (text("/signature/verifier/x509Certificate/rawBytes"), "CERTIFICATE") } else { (text("/signature/verifier/publicKey/rawBytes"), "PUBLIC KEY") };
        let pem = kiln_seams::sigstore::pem(label, &B64.decode(der).unwrap_or_default());
        let entry = json!({ "apiVersion": "0.0.1", "kind": "hashedrekord", "spec": {
            "data": { "hash": { "algorithm": "sha256", "value": hex::encode(B64.decode(text("/digest")).unwrap_or_default()) } },
            "signature": { "content": text("/signature/content"), "publicKey": { "content": B64.encode(pem) } },
        } });
        match sim.log(entry).await {
            Ok(l) => say(201, v2_entry(&l, &sim.log_id().unwrap_or_default())),
            Err(SigRefusal::Conflict { index }) => srv::with_header(say(409, json!({ "code": 6, "message": "an equivalent entry already exists" })), http::header::HeaderName::from_static("x-log-index"), &index.to_string()),
            Err(SigRefusal::Invalid { why, .. }) => say(400, json!({ "code": 3, "message": why })),
            Err(e) => say(500, json!({ "code": 13, "message": e.to_string() })),
        }
    }

    /// Rekor v2's tiled reads in front of the simulator: its checkpoint, its tiles and its entry
    /// bundles (C2SP tlog-tiles).
    fn v2_read(sim: &SimSigstore, path: &str) -> Response {
        use kiln_ci::identity::trail::{self, Tile};
        let not_found = || srv::json(http::StatusCode::NOT_FOUND, &json!({ "message": format!("no {path}") }));
        let Ok((leaves, bodies, note)) = sim.tiled() else { return srv::json(http::StatusCode::INTERNAL_SERVER_ERROR, &json!({ "message": "the simulator" })) };
        if path == "checkpoint" {
            return srv::response(http::StatusCode::OK, "text/plain; charset=utf-8", srv::full(bytes::Bytes::from(note)));
        }
        let Some((t, entries)) = Tile::parse(path) else { return not_found() };
        let (Some((start, end)), Some(per)) = (t.span().filter(|(_, end)| *end <= leaves.len() as u64), trail::TILE.checked_pow(u32::from(t.level))) else { return not_found() };
        let (start, end, per) = (start as usize, end as usize, per as usize);
        let bytes = if entries {
            trail::bundle(bodies.get(start..end).unwrap_or_default())
        } else {
            Some((0..t.width as usize).filter_map(|i| leaves.get(start + i * per..start + (i + 1) * per).map(trail::root)).flat_map(|h| h.to_vec()).collect())
        };
        match bytes {
            Some(b) => srv::response(http::StatusCode::OK, "application/octet-stream", srv::full(bytes::Bytes::from(b))),
            None => not_found(),
        }
    }

    /// Serves a fake of `sim` on a free port: its base URL (Fulcio's under /fulcio, Rekor's under /rekor).
    async fn serve(sim: SimSigstore) -> anyhow::Result<String> {
        let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
        let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
        let fake = Arc::new(FakeSigstore(sim));
        crate::rt::spawn(async move {
            while let Ok(s) = tcp.accept().await {
                let (h, r) = (Arc::clone(&fake), s.peer_addr().ok());
                crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
            }
        });
        Ok(format!("http://{addr}"))
    }

    /// A Sigstore that's down: where kiln's client couldn't be made, every case says why.
    struct Down(String);

    impl kiln_seams::Sigstore for Down {
        fn certify(&self, _: CertifyAsk) -> futures::future::BoxFuture<'_, Result<Certified, SigRefusal>> {
            Box::pin(async move { Err(SigRefusal::Unavailable(self.0.clone())) })
        }
        fn log(&self, _: Value) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
            Box::pin(async move { Err(SigRefusal::Unavailable(self.0.clone())) })
        }
        fn entry(&self, _: u64) -> futures::future::BoxFuture<'_, Result<Logged, SigRefusal>> {
            Box::pin(async move { Err(SigRefusal::Unavailable(self.0.clone())) })
        }
    }

    /// What the suite runs against: kiln's client, on a fake of Fulcio and Rekor over a fresh
    /// simulator; `v2`, Rekor v2's (hashedrekord entries alone, no promises).
    struct RemoteFixture {
        sigstore: Box<dyn kiln_seams::Sigstore>,
        asker: Result<Asker, String>,
        v2: bool,
    }

    impl SigstoreFixture for RemoteFixture {
        fn sigstore(&self) -> &dyn kiln_seams::Sigstore {
            self.sigstore.as_ref()
        }
        fn asker(&self) -> Result<&Asker, String> {
            self.asker.as_ref().map_err(Clone::clone)
        }
        fn kinds(&self) -> &'static [&'static str] {
            if self.v2 { &["hashedrekord"] } else { &["dsse", "hashedrekord"] }
        }
        fn promises(&self) -> bool {
            !self.v2
        }
    }

    /// The suite on kiln's client of Rekor's API `v2` (or v1) in front of a fresh simulator.
    async fn suite_on(v2: bool) -> anyhow::Result<()> {
        let cases = kiln_seams::sigstore::cases::<RemoteFixture>();
        let make = move || async move {
            let made = match serve(SimSigstore::new(1_790_000_000)).await {
                Ok(base) if v2 => RemoteV2::new(&format!("{base}/fulcio"), &format!("{base}/rekor2")).map(|r| Box::new(r) as Box<dyn kiln_seams::Sigstore>).map_err(|e| format!("{e:#}")),
                Ok(base) => Remote::new(&format!("{base}/fulcio"), &format!("{base}/rekor")).map(|r| Box::new(r) as Box<dyn kiln_seams::Sigstore>).map_err(|e| format!("{e:#}")),
                Err(e) => Err(format!("{e:#}")),
            };
            let sigstore: Box<dyn kiln_seams::Sigstore> = made.unwrap_or_else(|e| Box::new(Down(e)));
            RemoteFixture { sigstore, asker: Asker::new(kiln_seams::sigstore::ISSUER, kiln_seams::sigstore::SUBJECT), v2 }
        };
        let clock = crate::rt::clock();
        let name = if v2 { "Sigstore (kiln's Rekor v2 client)" } else { "Sigstore (kiln's client)" };
        let report = kiln_seams::conformance::run_within(name, &cases, make, || clock.sleep(Duration::from_secs(60))).await;
        anyhow::ensure!(report.failed.is_empty() && report.passed.len() == cases.len(), "failed {:?}", report.failed);
        Ok(())
    }

    /// W27-R12: kiln's client of a Fulcio and Rekor passes the Sigstore seam's suite, over Fulcio's
    /// and Rekor's HTTP APIs as they answer, in front of the simulator.
    #[test]
    fn w27_r12_kilns_sigstore_client_passes_the_sigstore_suite() -> anyhow::Result<()> {
        crate::rt::block_on(suite_on(false))
    }

    /// W27-R12: kiln's client of a Rekor v2 passes it too: hashedrekord entries logged, each read
    /// back from the log's tiles and proven against its checkpoint, no promise asked of it.
    #[test]
    fn w27_r12_kilns_rekor_v2_client_passes_the_sigstore_suite() -> anyhow::Result<()> {
        crate::rt::block_on(suite_on(true))
    }

    /// W27-R09: an entry of a Rekor v2, as logged and as read back from its tiles, is proven against
    /// Sigstore's trusted root by its proof and the log's signed checkpoint, with no promise to go
    /// by; a proof changed isn't.
    #[test]
    fn w27_r09_a_rekor_v2_entry_is_proven_by_its_checkpoint() -> anyhow::Result<()> {
        use kiln_seams::Sigstore as _;
        crate::rt::block_on(async move {
            let sim = SimSigstore::new(1_790_000_000);
            let trust = crate::ci::sigstore::TrustedRoot::from_json(sim.trusted_root("https://fulcio.test", "https://rekor.test").map_err(|e| anyhow::anyhow!("{e}"))?.to_string().as_bytes())?;
            let base = serve(sim).await?;
            let v2 = RemoteV2::new(&format!("{base}/fulcio"), &format!("{base}/rekor2"))?;
            let asker = Asker::new(kiln_seams::sigstore::ISSUER, kiln_seams::sigstore::SUBJECT).map_err(|e| anyhow::anyhow!(e))?;
            let leaf = v2.certify(asker.ask()).await.map_err(|e| anyhow::anyhow!("{e}"))?.chain.into_iter().next().ok_or_else(|| anyhow::anyhow!("no certificate"))?;
            let entry = |message: &[u8]| {
                json!({ "apiVersion": "0.0.1", "kind": "hashedrekord", "spec": {
                    "data": { "hash": { "algorithm": "sha256", "value": crate::util::sha256_hex(message) } },
                    "signature": { "content": B64.encode(asker.sign(message)), "publicKey": { "content": B64.encode(&leaf) } },
                } })
            };
            let checkpoint = b"kiln://hyper-light/trail\n4\n";
            v2.log(entry(b"a checkpoint before")).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let logged = v2.log(entry(checkpoint)).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            assert!(logged.promise.is_none(), "Rekor v2 promises nothing");
            let again = v2.entry(logged.index).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            for l in [logged, again] {
                let body = crate::ci::sigstore::proven(&l, &trust)?;
                assert_eq!(body.pointer("/spec/data/hash/value").and_then(Value::as_str), Some(crate::util::sha256_hex(checkpoint).as_str()));
                let mut changed = l.clone();
                if let Some(h) = changed.proof.as_mut().and_then(|p| p.hashes.first_mut()).and_then(|h| h.first_mut()) {
                    *h ^= 1;
                }
                assert!(crate::ci::sigstore::proven(&changed, &trust).is_err(), "a proof changed is refused");
            }
            Ok(())
        })
    }


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
