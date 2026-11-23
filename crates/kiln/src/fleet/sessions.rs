//! Session nodes use their enrolled fleet's root and runner identity. Remember only direct
//! endpoints and authenticated peer keys, never a live placement decision or a person's token.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const SESSION_POLICY: &str = "session-policy.json";

pub fn session_policy(state: &Path) -> anyhow::Result<crate::convo::raft::Policy> {
    if std::env::var_os("KILN_CONVO_FAILOVER").is_some() {
        return Ok(crate::convo::raft::Policy::from_env());
    }
    match std::fs::read(state.join(SESSION_POLICY)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(crate::convo::raft::Policy::default()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn enroll_session_node(state: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(state)?;
    let token = state.join("session-admin.token");
    match std::fs::metadata(&token) {
        Ok(meta) => anyhow::ensure!(meta.is_file() && meta.len() > 0, "invalid session owner token file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::io::Write as _;
            let mut tmp = tempfile::NamedTempFile::new_in(state)?;
            tmp.write_all(crate::util::random_token().as_bytes())?;
            tmp.as_file().sync_all()?;
            if let Err(e) = tmp.persist_noclobber(&token)
                && e.error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e.into());
            }
        }
        Err(e) => return Err(e.into()),
    }
    let path = state.join(SESSION_POLICY);
    if !path.try_exists()? {
        let policy = crate::convo::raft::Policy::default();
        crate::util::write_secret(&path, &serde_json::to_vec(&policy)?, 0o600)?;
    }
    crate::blobs::chunks::sync_dir(state)?;
    Ok(())
}

pub(crate) fn local_admin_file(state: &Path) -> Option<PathBuf> {
    let path = state.join("session-admin.token");
    path.is_file().then_some(path)
}

pub(crate) fn start_uri(machine: &str) -> String {
    format!("urn:kiln:session-node:v1:{machine}")
}

pub(crate) fn bootstrap_authority(pki: &crate::pki::Pki, state: &Path, machine: &str, key: &str) -> anyhow::Result<()> {
    let cert_path = state.join("session-node.pem");
    let key_path = state.join("session-node.key");
    if let Ok(cert) = std::fs::read_to_string(&cert_path)
        && crate::pki::cert_matches_key(&cert, key)
        && let Ok(der) = crate::join::pem_der(&cert)
        && crate::pki::verify_issued(&pki.ca_der, &der)
        && has_start_uri(&der, machine)
        && std::fs::read_to_string(&key_path).is_ok_and(|stored| stored == key)
    {
        return Ok(());
    }
    let csr = crate::pki::make_csr_with_key(machine, key)?;
    let cert = pki.sign_session_node(&csr, machine, crate::pki::LEAF_DAYS)?;
    crate::util::write_secret(&key_path, key.as_bytes(), 0o600)?;
    crate::util::write_secret(&cert_path, cert.as_bytes(), 0o600)?;
    Ok(())
}

fn has_start_uri(der: &[u8], machine: &str) -> bool {
    let Ok((_, cert)) = x509_parser::parse_x509_certificate(der) else {
        return false;
    };
    let expected = start_uri(machine);
    cert.subject_alternative_name().ok().flatten().is_some_and(|names| {
        names
            .value
            .general_names
            .iter()
            .any(|n| matches!(n,x509_parser::extensions::GeneralName::URI(uri) if *uri == expected))
    })
}

pub(crate) const GRANT_PARENT: &str = "urn:kiln:session-node-parent:v1:";

fn prior_serials(der: &[u8]) -> anyhow::Result<Vec<String>> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).map_err(|e| anyhow::anyhow!("invalid session grant: {e}"))?;
    let mut serials = Vec::new();
    if let Some(san) = cert.subject_alternative_name()? {
        for name in &san.value.general_names {
            if let x509_parser::extensions::GeneralName::URI(uri) = name
                && let Some(serial) = uri.strip_prefix(GRANT_PARENT)
            {
                anyhow::ensure!(!serial.is_empty() && serial.bytes().all(|b| b.is_ascii_hexdigit()), "invalid grant ancestry");
                serials.push(serial.to_ascii_lowercase());
            }
        }
    }
    Ok(serials)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Renewal {
    csr: String,
    proof: String,
}

fn renewal_body(machine: &str, csr: &str) -> String {
    format!("renew session-node grant v1\n{machine}\n{}", crate::util::sha256_hex(csr.as_bytes()))
}

pub(crate) fn renew_after(state: &Path) -> Option<i64> {
    let cert = std::fs::read_to_string(state.join("session-node.pem")).ok()?;
    let stamp = crate::pki::fingerprints(&cert).ok()?;
    Some(renew_at(&stamp))
}

fn renew_at(stamp: &crate::pki::Fingerprints) -> i64 {
    // For unusually short grants, use at most half their lifetime to avoid reissuing a
    // certificate which would itself immediately qualify for reissuance.
    let lifetime = stamp.not_after.saturating_sub(stamp.not_before);
    stamp
        .not_after
        .saturating_sub((lifetime / 2).min(time::Duration::days(crate::pki::RENEW_DAYS).whole_seconds()))
}

fn grant_key(state: &Path, cert: &str) -> anyhow::Result<String> {
    for file in ["agent.key", "session-node.key"] {
        match std::fs::read_to_string(state.join(file)) {
            Ok(key) if crate::pki::cert_matches_key(cert, &key) => return Ok(key),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("no key matches the session-node grant")
}

pub(crate) fn renewal_request(state: &Path, machine: &str) -> anyhow::Result<Renewal> {
    let cert = std::fs::read_to_string(state.join("session-node.pem"))?;
    anyhow::ensure!(
        crate::pki::still_valid(&cert) && crate::util::now_unix() >= renew_at(&crate::pki::fingerprints(&cert)?),
        "session grant is outside its renewal window"
    );
    let key = grant_key(state, &cert)?;
    let csr = crate::pki::make_csr_with_key(machine, &key)?;
    let proof = sign_start(state, machine, &renewal_body(machine, &csr))?;
    Ok(Renewal { csr, proof })
}

pub(crate) async fn renew_grant(store: &crate::store::Store, pki: &crate::pki::Pki, machine: &str, request: Renewal) -> anyhow::Result<String> {
    let prior: StartProof = serde_json::from_str(&request.proof)?;
    anyhow::ensure!(
        crate::util::now_unix() >= renew_at(&crate::pki::fingerprints(&prior.cert)?),
        "session grant is outside its renewal window"
    );
    let revoked = crate::identities::revoked_current(store).await?;
    verify_current(&pki.ca_der, machine, &renewal_body(machine, &request.csr), &request.proof, &revoked)?;
    let old = crate::join::pem_der(&prior.cert)?;
    let stamp = crate::pki::fingerprints_der(&old)?;
    let mut ancestors = prior_serials(&old)?;
    ancestors.push(stamp.serial.clone());
    let pem = pki.session_node_leaf(&request.csr, machine, crate::pki::LEAF_DAYS, Some(&old), &ancestors)?;
    let (generation, subject, fp) = (revoked.stamp, machine.to_owned(), crate::pki::fingerprints(&pem)?);
    store
        .call(move |c| {
            let tx = c.savepoint()?;
            // A revoke ordered between verification and issuance wins. A descendant proof
            // also carries every ancestor serial, so a later revoke of an old grant still applies.
            anyhow::ensure!(
                crate::identities::revoked_stamp(&tx)? == generation,
                "revocations changed during session grant renewal"
            );
            anyhow::ensure!(crate::util::now_unix() < stamp.not_after, "session grant expired during renewal");
            use rusqlite::OptionalExtension as _;
            let existing: Option<(String, bool)> = tx
                .query_row(
                    "SELECT i.cert_pem, EXISTS(SELECT 1 FROM revocations r WHERE r.serial=i.serial AND r.at<=?2)
            FROM session_grant_renewals g JOIN identities i ON i.serial=g.issued WHERE g.prior=?1",
                    rusqlite::params![stamp.serial, crate::util::now_unix()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((cert, revoked)) = existing {
                anyhow::ensure!(!revoked, "the previous renewal was revoked");
                return Ok(cert);
            }
            tx.execute(
                "INSERT INTO identities(serial,kind,subject,key_sha256,not_before,not_after,issued_at,cert_pem) VALUES(?1,'session-node',?2,?3,?4,?5,?6,?7)",
                rusqlite::params![fp.serial, subject, fp.key, fp.not_before, fp.not_after, crate::util::now_unix(), pem],
            )?;
            tx.execute(
                "INSERT INTO session_grant_renewals(prior,issued) VALUES(?1,?2)",
                rusqlite::params![stamp.serial, fp.serial],
            )?;
            tx.commit()?;
            Ok(pem)
        })
        .await
}

pub(crate) fn save_renewal(state: &Path, machine: &str, root: &[u8], pem: &str) -> anyhow::Result<Option<i64>> {
    let old = std::fs::read_to_string(state.join("session-node.pem"))?;
    if pem == old {
        return Ok(renew_after(state));
    }
    let prior = crate::pki::fingerprints(&old)?;
    let key = grant_key(state, &old)?;
    let der = crate::join::pem_der(pem)?;
    anyhow::ensure!(
        crate::pki::verify_issued(root, &der)
            && has_start_uri(&der, machine)
            && crate::pki::cert_matches_key(pem, &key)
            && crate::pki::still_valid(pem)
            && crate::pki::fingerprints_der(&der)?.not_after > prior.not_after
            && prior_serials(&der)?.contains(&prior.serial),
        "returned certificate is not a renewal of this session grant"
    );
    crate::util::write_secret(&state.join("session-node.pem"), pem.as_bytes(), 0o600)?;
    crate::blobs::chunks::sync_dir(state)?;
    Ok(renew_after(state))
}

#[derive(Serialize, Deserialize)]
struct StartProof {
    cert: String,
    signature: String,
}

fn start_payload(body: &str) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"kiln delegated fleet session operation v1\0");
    hash.update(body.as_bytes());
    hash.finalize().into()
}

pub(crate) fn sign_start(state: &Path, machine: &str, body: &str) -> anyhow::Result<String> {
    use rcgen::SigningKey as _;
    let cert = std::fs::read_to_string(state.join("session-node.pem"))?;
    let key = grant_key(state, &cert)?;
    let der = crate::join::pem_der(&cert)?;
    anyhow::ensure!(
        has_start_uri(&der, machine) && crate::pki::cert_matches_key(&cert, &key),
        "this node has no matching session-node grant"
    );
    let signature = rcgen::KeyPair::from_pem(&key)?.sign(&start_payload(body))?;
    Ok(serde_json::to_string(&StartProof {
        cert,
        signature: hex::encode(signature),
    })?)
}

pub(crate) fn verify_current(root: &[u8], from: &str, body: &str, proof: &str, revoked: &crate::identities::Revoked) -> anyhow::Result<()> {
    verify_grant(root, from, body, proof, |stamp, now| revoked.covers(stamp, now))
}

#[cfg(test)]
pub(crate) fn verify_start(root: &[u8], from: &str, body: &str, proof: &str, revoked: &[kiln_ci::identity::chain::Revocation]) -> anyhow::Result<()> {
    verify_grant(root, from, body, proof, |stamp, now| {
        revoked.iter().any(|r| {
            r.at <= now
                && (r.key.as_deref().is_some_and(|key| key.eq_ignore_ascii_case(&stamp.key))
                    || r.serial.as_deref().is_some_and(|serial| serial.eq_ignore_ascii_case(&stamp.serial)))
        })
    })
}

fn verify_grant(root: &[u8], from: &str, body: &str, proof: &str, forbidden: impl Fn(&crate::pki::Fingerprints, i64) -> bool) -> anyhow::Result<()> {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
    let proof: StartProof = serde_json::from_str(proof)?;
    let der = crate::join::pem_der(&proof.cert)?;
    anyhow::ensure!(
        has_start_uri(&der, from) && crate::pki::verify_issued(root, &der),
        "untrusted session-node grant"
    );
    let now = crate::util::now_unix();
    for cert in [&der[..], root] {
        let stamp = crate::pki::fingerprints_der(cert)?;
        anyhow::ensure!(now >= stamp.not_before && now <= stamp.not_after, "session-node grant expired or not yet valid");
        anyhow::ensure!(!forbidden(&stamp, now), "revoked session-node grant");
    }
    let ancestors = prior_serials(&der)?;
    if !ancestors.is_empty() {
        let mut old = crate::pki::fingerprints_der(&der)?;
        for serial in ancestors {
            old.serial = serial;
            anyhow::ensure!(!forbidden(&old, now), "an authorizing session grant was revoked");
        }
    }
    let (_, cert) = x509_parser::parse_x509_certificate(&der).map_err(|e| anyhow::anyhow!("{e}"))?;
    let key = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, cert.public_key().subject_public_key.data.as_ref());
    key.verify(&start_payload(body), &hex::decode(proof.signature)?)
        .map_err(|_| anyhow::anyhow!("invalid session operation signature"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Member {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    pub key: String,
    pub addrs: Vec<SocketAddr>,
    pub v2: bool,
    #[serde(default)]
    pub raft: bool,
    #[serde(default)]
    pub executor: bool,
    #[serde(default)]
    pub charter: bool,
    #[serde(default)]
    pub liveness: bool,
    #[serde(default)]
    pub starts: bool,
    #[serde(default)]
    pub authority_mail: bool,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Bans {
    keys: BTreeMap<String, i64>,
    serials: BTreeMap<String, i64>,
    through: i64,
    complete: bool,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BansPage {
    since: i64,
    through: i64,
    complete: bool,
    rows: Vec<(Option<String>, Option<String>, i64)>,
}

/// The authenticated reach directory transports only additions this node has not observed.
/// A page's size follows its available wire room. A partial initial history cannot grant access.
async fn bans_page(store: &crate::store::Store, since: i64, base: i64, room: usize) -> anyhow::Result<BansPage> {
    store
        .read(move |db| {
            let mut q = db.prepare_cached("SELECT id,key_sha256,serial,at FROM revocations WHERE id>?1 ORDER BY id")?;
            let mut cursor = q.query([base.max(0)])?;
            let mut page = BansPage {
                since,
                through: base.max(0),
                complete: true,
                rows: vec![],
            };
            let mut row_bytes = 0usize;
            while let Some(r) = cursor.next()? {
                let entry = (r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?);
                // The cursor can gain a digit on this row and a partial page carries
                // "false". Count the exact metadata and separator without copying bodies.
                let through = r.get(0)?;
                row_bytes = row_bytes
                    .saturating_add(crate::memory::queue::json_bytes(&entry))
                    .saturating_add(usize::from(!page.rows.is_empty()));
                let header = BansPage {
                    since,
                    through,
                    complete: false,
                    rows: Vec::new(),
                };
                if row_bytes.saturating_add(crate::memory::queue::json_bytes(&header)) > room {
                    page.complete = false;
                    break;
                }
                page.through = through;
                page.rows.push(entry);
            }
            Ok(page)
        })
        .await
}

impl BansPage {
    pub(crate) fn next_cursor(&self) -> Option<i64> {
        (!self.complete && self.through > self.since.max(0)).then_some(self.through)
    }
}

pub(crate) async fn add_bans(store: &crate::store::Store, answer: &mut serde_json::Value, since: Option<i64>) {
    let Some(since) = since.filter(|r| *r >= -1) else { return };
    let reply = async {
        let stamp = store.read(|db| Ok(crate::identities::revoked_stamp(db)?)).await?;
        if stamp == since.max(0) {
            return Ok(BansPage {
                since,
                through: stamp,
                complete: true,
                rows: Vec::new(),
            });
        }
        let room = crate::reach::wire::MAX_MESSAGE
            .saturating_sub(crate::memory::queue::json_bytes(answer))
            .saturating_sub(r#","session_revocations":"#.len());
        let base = if stamp < since { 0 } else { since };
        bans_page(store, since, base, room).await
    }
    .await;
    let page = match reply {
        Ok(page) => page,
        Err(e) => {
            tracing::warn!(error=%e,"could not attest session revocations");
            BansPage {
                since,
                through: since.max(0),
                complete: false,
                rows: Vec::new(),
            }
        }
    };
    if let Some(obj) = answer.as_object_mut()
        && let Ok(raw) = serde_json::to_value(page)
    {
        obj.insert("session_revocations".into(), raw);
    }
}

impl Bans {
    fn apply(&mut self, page: BansPage) {
        self.through = page.through;
        self.complete = page.complete;
        for (key, serial, at) in page.rows {
            for (map, id) in [(&mut self.keys, key), (&mut self.serials, serial)] {
                if let Some(id) = id {
                    let at_old = map.entry(id.to_ascii_lowercase()).or_insert(at);
                    *at_old = (*at_old).min(at);
                }
            }
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Directory {
    node: String,
    ca: String,
    members: BTreeMap<String, Member>,
    #[serde(default)]
    bans: Option<Bans>,
}

pub(crate) struct Routes {
    // Serialize durable generations without blocking peer lookups on the filesystem.
    publishing: parking_lot::Mutex<()>,
    path: PathBuf,
    directory: parking_lot::RwLock<Directory>,
    pub(crate) names: super::charter_cache::Cache,
}

impl Routes {
    pub(crate) fn open(dir: &Path, node: &str, ca: &[u8]) -> anyhow::Result<Self> {
        use sha2::Digest as _;
        let path = dir.join("session-members.json");
        let mut directory = Directory {
            node: node.into(),
            ca: hex::encode(sha2::Sha256::digest(ca)),
            members: BTreeMap::new(),
            bans: None,
        };
        match std::fs::read(&path) {
            Ok(raw) => match serde_json::from_slice::<Directory>(&raw) {
                Ok(stored) if stored.node == node && stored.ca == directory.ca => directory = stored,
                Ok(_) => {}
                Err(e) => return Err(anyhow::anyhow!("saved session fleet directory is unreadable: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(!path.with_extension("invalid").try_exists()?,
                    "the last fleet directory could not be saved; recover the authenticated directory before founding sessions");
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            path,
            directory: parking_lot::RwLock::new(directory),
            names: super::charter_cache::Cache::open(dir,ca)?,
            publishing: parking_lot::Mutex::new(()),
        })
    }

    pub(crate) fn save_enrolled_registrar(state: &Path, ca: &[u8], cert: &super::charter::Certificate) -> anyhow::Result<()> {
        super::charter_cache::Cache::save_registrar(state,ca,cert)
    }

    pub(crate) fn member_names(&self) -> Vec<String> {
        self.directory
            .read()
            .members
            .iter()
            .filter(|(_, member)| member.executor)
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn resolve_alias(&self, selected: &str) -> anyhow::Result<Option<String>> {
        let directory = self.directory.read();
        if directory.members.contains_key(selected) { return Ok(Some(selected.to_owned())); }
        let mut names = directory.members.iter().filter(|(_,m)| m.aliases.iter().any(|a| a == selected));
        let one = names.next().map(|(name,_)| name.clone());
        anyhow::ensure!(names.next().is_none(), "runner name is ambiguous; select its machine id");
        Ok(one)
    }

    pub(crate) fn authority_mail(&self, machine: &str) -> bool {
        self.directory.read().members.get(machine).is_some_and(|m| m.authority_mail)
    }

    pub(crate) fn start_support(&self, machine: &str) -> Option<bool> {
        self.directory.read().members.get(machine).map(|m| m.starts)
    }

    pub(crate) fn contains(&self, machine: &str) -> bool {
        self.directory.read().members.contains_key(machine)
    }

    pub(crate) fn entries(&self) -> Vec<crate::reach::quic::PeerEntry> {
        self.directory
            .read()
            .members
            .iter()
            .map(|(name, member)| crate::reach::quic::PeerEntry {
                name: name.clone(),
                fingerprint: member.key.clone(),
                addrs: member.addrs.clone(),
            })
            .collect()
    }

    pub(crate) fn members(&self) -> BTreeMap<String, Member> {
        self.directory.read().members.clone()
    }

    pub(crate) fn bans_since(&self) -> i64 {
        self.directory.read().bans.as_ref().map_or(-1, |b| b.through)
    }

    pub(crate) fn verify_cached(&self, root: &[u8], from: &str, body: &str, proof: &str) -> anyhow::Result<()> {
        let directory = self.directory.read();
        anyhow::ensure!(directory.members.contains_key(from), "unknown fleet member");
        let bans = directory
            .bans
            .as_ref()
            .filter(|b| b.complete)
            .ok_or_else(|| anyhow::anyhow!("no complete authenticated revocation history"))?;
        verify_grant(root, from, body, proof, |stamp, now| {
            bans.keys.get(&stamp.key).is_some_and(|at| *at <= now)
                || bans.serials.get(&stamp.serial).is_some_and(|at| *at <= now)
        })
    }

    pub(crate) fn replace(&self, members: BTreeMap<String, Member>, page: Option<BansPage>) -> anyhow::Result<()> {
        let _publishing = self.publishing.lock();
        let next = {
            let directory = self.directory.read();
            let since = directory.bans.as_ref().map_or(-1, |b| b.through);
            let page = page.filter(|p| p.since == since && p.through >= 0);
            let changed = page
                .as_ref()
                .is_some_and(|p| directory.bans.as_ref().is_none_or(|b| b.complete != p.complete || b.through != p.through));
            if members == directory.members && !changed {
                return Ok(());
            }
            let mut bans = directory.bans.clone();
            if let Some(page) = page {
                bans.get_or_insert_with(Bans::default).apply(page);
            }
            Directory {
                members,
                bans,
                node: directory.node.clone(),
                ca: directory.ca.clone(),
            }
        };
        let bytes = serde_json::to_vec(&next)?;
        let dir = self.path.parent().ok_or_else(|| anyhow::anyhow!("fleet directory has no parent"))?;
        if let Err(e) = crate::util::write_secret(&self.path, &bytes, 0o600).and_then(|()| crate::blobs::chunks::sync_dir(dir)) {
            {
                let mut cached = self.directory.write();
                cached.members.clear();
                cached.bans = None;
            }
            // Fence stale grants on restart and preserve evidence that this node already
            // belonged to a fleet. Removing the only roster could found a second singleton.
            let invalid = self.path.with_extension("invalid");
            match std::fs::rename(&self.path, &invalid) {
                Ok(()) => {}
                Err(clean) if clean.kind() == std::io::ErrorKind::NotFound => {
                    crate::util::write_secret(&invalid, b"fleet directory update interrupted", 0o600)?;
                }
                Err(clean) => return Err(anyhow::anyhow!("could not persist or invalidate fleet directory: {e}; {clean}")),
            }
            crate::blobs::chunks::sync_dir(dir)?;
            return Err(e.into());
        }
        *self.directory.write() = next;
        Ok(())
    }

    pub(crate) fn restore(&self, manager: &crate::convo::raft::Manager) -> anyhow::Result<()> {
        self.names.restore(manager)?;
        let known = self.directory.read();
        manager.register_members(
            known
                .members
                .iter()
                .filter(|(_, m)| m.raft)
                .map(|(name, m)| (name.clone(), m.v2, m.executor && m.liveness))
                .collect(),
        );
        Ok(())
    }
}

/// W10. The bootstrap secret stays on the invited machine. Its enrollment request proves
/// possession over the exact requested identity and CSR. The invitation pins the answering CA.
pub(crate) struct Invite<'a> {
    id: &'a str,
    secret: &'a str,
    root: &'a str,
}

impl<'a> Invite<'a> {
    pub(crate) fn parse(token: &'a str) -> anyhow::Result<Option<Self>> {
        let Some(raw) = token.strip_prefix("session-invite.") else { return Ok(None) };
        let mut parts = raw.split('.');
        let (Some(id), Some(secret), Some(root), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            anyhow::bail!("invalid session node invite")
        };
        anyhow::ensure!(
            id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !secret.is_empty() && root.len() == 64,
            "incomplete session node invite"
        );
        Ok(Some(Self { id, secret, root }))
    }

    pub(crate) fn csr(&self, state: &Path, name: &str) -> anyhow::Result<(String, String)> {
        use std::io::Write as _;
        std::fs::create_dir_all(state)?;
        let path = state.join(format!("session-enrollment-{}.key", self.id));
        if !path.try_exists()? {
            let (_, key) = crate::pki::make_csr(name)?;
            let mut tmp = tempfile::NamedTempFile::new_in(state)?;
            tmp.write_all(key.as_bytes())?;
            tmp.as_file().sync_all()?;
            if let Err(e) = tmp.persist_noclobber(&path)
                && e.error.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e.into());
            }
            crate::blobs::chunks::sync_dir(state)?;
        }
        let key = std::fs::read_to_string(path)?;
        Ok((crate::pki::make_csr_with_key(name, &key)?, key))
    }

    pub(crate) fn completed(&self, state: &Path) {
        let _ = std::fs::remove_file(state.join(format!("session-enrollment-{}.key", self.id)));
    }

    pub(crate) fn proof(&self, request: &crate::agent::EnrollRequest) -> anyhow::Result<String> {
        let key =
            aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&hex::decode(self.secret)?).map_err(|_| anyhow::anyhow!("invalid session invite key"))?;
        Ok(format!(
            "session-proof.{}.{}",
            self.id,
            hex::encode(key.sign(&enrollment_body(request)?).as_ref())
        ))
    }

    pub(crate) fn verify(&self, reply: &crate::agent::EnrollResponse, key: &str) -> anyhow::Result<()> {
        let root = crate::join::pem_der(&reply.ca_pem)?;
        let grant = reply
            .session_node_pem
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("session node grant missing from enrollment"))?;
        let leaf = crate::join::pem_der(&reply.cert_pem)?;
        let der = crate::join::pem_der(grant)?;
        anyhow::ensure!(
            crate::util::sha256_hex(&root) == self.root
                && crate::pki::verify_issued(&root, &leaf)
                && crate::pki::verify_issued(&root, &der)
                && has_start_uri(&der, &reply.name)
                && crate::pki::cert_matches_key(grant, key)
                && crate::pki::cert_matches_key(&reply.cert_pem, key),
            "invalid session node enrollment"
        );
        Ok(())
    }
}

fn enrollment_body(r: &crate::agent::EnrollRequest) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        "kiln-session-node-enrollment-v1",
        &r.name,
        &r.hostname,
        &r.machine_id,
        &r.csr,
    ))?)
}

pub(crate) async fn mint_invite(store: &crate::store::Store, root: &[u8], now: i64) -> anyhow::Result<(String, i64)> {
    let id = crate::util::new_id();
    use aws_lc_rs::signature::KeyPair as _;
    let seed: [u8; 32] = rand::random();
    let key = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| anyhow::anyhow!("could not create invite key"))?;
    let public = key.public_key().as_ref().to_vec();
    let token = format!("session-invite.{id}.{}.{}", hex::encode(seed), crate::util::sha256_hex(root));
    let until = now
        .checked_add(i64::try_from(crate::hub::OUTPOST_INVITE_TTL.as_secs())?)
        .ok_or_else(|| anyhow::anyhow!("invalid invite expiry"))?;
    store
        .call(move |db| {
            db.execute("DELETE FROM session_node_invites WHERE expires_at<=?1", [now])?;
            db.execute(
                "INSERT INTO session_node_invites(id,public_key,expires_at) VALUES(?1,?2,?3)",
                rusqlite::params![id, public, until],
            )?;
            Ok(())
        })
        .await?;
    Ok((token, until))
}

pub(crate) async fn claim_invite(store: &crate::store::Store, r: &crate::agent::EnrollRequest, now: i64) -> anyhow::Result<bool> {
    use rcgen::PublicKeyData as _;
    use rusqlite::OptionalExtension as _;
    let Some((id, proof)) = r.token.strip_prefix("session-proof.").and_then(|token| token.split_once('.')) else {
        return Ok(false);
    };
    if id.len() != 32 || proof.len() != 128 {
        return Ok(false);
    }
    let id = id.to_owned();
    let lookup = id.clone();
    let public: Option<Vec<u8>> = store
        .read(move |db| {
            Ok(db
                .query_row(
                    "SELECT public_key FROM session_node_invites WHERE id=?1 AND expires_at>?2",
                    rusqlite::params![lookup, now],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await?;
    let Some(public) = public else { return Ok(false) };
    let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, public);
    let Ok(signature) = hex::decode(proof) else { return Ok(false) };
    if key.verify(&enrollment_body(r)?, &signature).is_err() {
        return Ok(false);
    }
    let csr = rcgen::CertificateSigningRequestParams::from_pem(&r.csr)?;
    let key = crate::util::sha256_hex(&csr.public_key.subject_public_key_info());
    let holder = crate::util::sha256_hex(&serde_json::to_vec(&(&r.name, &r.hostname, &r.machine_id, key))?);
    store
        .call(move |db| {
            Ok(db.execute(
                "UPDATE session_node_invites SET holder=?2 WHERE id=?1 AND expires_at>?3 AND (holder IS NULL OR holder=?2)",
                rusqlite::params![id, holder, now],
            )? == 1)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn w10_cold_session_node_uses_only_complete_persisted_revocations() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("ca"))?;
        let state = dir.path().join("west");
        enroll_session_node(&state)?;
        let (csr, key) = crate::pki::make_csr("west")?;
        let cert = pki.sign_session_node(&csr, "west", 1)?;
        crate::util::write_secret(&state.join("agent.key"), key.as_bytes(), 0o600)?;
        crate::util::write_secret(&state.join("session-node.pem"), cert.as_bytes(), 0o600)?;
        let proof = sign_start(&state, "west", "send here")?;
        let peers = BTreeMap::from([(
            "west".to_owned(),
            Member {
                aliases:Vec::new(),
                key: "peer".into(),
                addrs: Vec::new(),
                v2: true,
                raft: true,
                executor: true,
                charter: true,
                liveness: true,
                starts:true, authority_mail:true,
            },
        )]);
        let routes = Routes::open(dir.path(), "east", &pki.ca_der)?;
        routes.replace(
            peers.clone(),
            Some(BansPage {
                since: -1,
                through: 2,
                complete: false,
                rows: Vec::new(),
            }),
        )?;
        assert!(routes.verify_cached(&pki.ca_der, "west", "send here", &proof).is_err());
        routes.replace(
            peers.clone(),
            Some(BansPage {
                since: 2,
                through: 2,
                complete: true,
                rows: Vec::new(),
            }),
        )?;
        let reopened = Routes::open(dir.path(), "east", &pki.ca_der)?;
        reopened.verify_cached(&pki.ca_der, "west", "send here", &proof)?;
        let serial = crate::pki::fingerprints(&cert)?.serial;
        reopened.replace(
            peers,
            Some(BansPage {
                since: 2,
                through: 3,
                complete: true,
                rows: vec![(None, Some(serial), 0)],
            }),
        )?;
        let denied = Routes::open(dir.path(), "east", &pki.ca_der)?;
        assert!(denied.verify_cached(&pki.ca_der, "west", "send here", &proof).is_err());
        assert!(
            Routes::open(dir.path(), "east", b"other fleet")?
                .verify_cached(&pki.ca_der, "west", "send here", &proof)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn w10_session_invite_binds_one_machine_and_survives_a_lost_reply() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
            let store = crate::store::Store::open(&dir.path().join("kiln.db"))?;
            let (token, until) = mint_invite(&store, &pki.ca_der, 100).await?;
            let invite = Invite::parse(&token)?.ok_or_else(|| anyhow::anyhow!("invalid invite"))?;
            let state = dir.path().join("runner");
            let (csr, key) = invite.csr(&state, "west")?;
            let mut r = crate::agent::EnrollRequest {
                token: String::new(),
                name: "west".into(),
                csr,
                hostname: "west".into(),
                machine_id: None,
            };
            r.token = invite.proof(&r)?;
            assert!(!r.token.contains(invite.secret));
            assert!(claim_invite(&store, &r, 101).await?);
            let (csr, same) = invite.csr(&state, "west")?;
            assert!(crate::util::ct_eq(key.as_bytes(), same.as_bytes()), "restart changed the enrollment key");
            r.csr = csr;
            r.token = invite.proof(&r)?;
            assert!(claim_invite(&store, &r, 102).await?);
            r.hostname = "other".into();
            assert!(!claim_invite(&store, &r, 102).await?);
            r.token = invite.proof(&r)?;
            assert!(!claim_invite(&store, &r, 102).await?);
            r.hostname = "west".into();
            r.token = invite.proof(&r)?;
            assert!(!claim_invite(&store, &r, until).await?);
            let grant = pki.sign_session_node(&r.csr, "west", 1)?;
            let response = crate::agent::EnrollResponse {
                session_registrar: None,
                session_node_pem: Some(grant),
                cert_pem: pki.sign_csr(&r.csr, "west", 1)?,
                ca_pem: pki.ca_pem.clone(),
                name: "west".into(),
                quic: "".into(),
                label: None,
            };
            invite.verify(&response, &key)?;
            let (_, other) = crate::pki::make_csr("west")?;
            assert!(invite.verify(&response, &other).is_err());
            Ok(())
        })
    }

    #[test]
    fn w10_session_grant_renews_once_and_an_ancestor_revoke_still_fences_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let folder = tempfile::tempdir()?;
            let state = folder.path();
            let pki = crate::pki::Pki::open_scratch(&state.join("pki"))?;
            let store = crate::store::Store::open(&state.join("kiln.db"))?;
            let (csr, key) = crate::pki::make_csr("west")?;
            let old = pki.aging_session_grant(&csr, "west")?;
            crate::util::write_secret(&state.join("agent.key"), key.as_bytes(), 0o600)?;
            crate::util::write_secret(&state.join("session-node.pem"), old.as_bytes(), 0o600)?;
            let (other, _) = crate::pki::make_csr("west")?;
            let forged = Renewal {
                proof: sign_start(state, "west", &renewal_body("west", &other))?,
                csr: other,
            };
            assert!(renew_grant(&store, &pki, "west", forged).await.is_err());
            let retry = renewal_request(state, "west")?;
            let renewed = renew_grant(&store, &pki, "west", renewal_request(state, "west")?).await?;
            assert_ne!(crate::pki::fingerprints(&old)?.serial,crate::pki::fingerprints(&renewed)?.serial);
            assert_eq!(renew_grant(&store, &pki, "west", retry).await?, renewed);
            assert!(save_renewal(state, "west", &pki.ca_der, &renewed)?.is_some_and(|due| due > crate::util::now_unix()));
            assert!(renewal_request(state, "west").is_err());
            let proof = sign_start(state, "west", "send")?;
            assert!(verify_start(&pki.ca_der, "west", "send", &proof, &[]).is_ok());
            let revoke = kiln_ci::identity::chain::Revocation {
                serial: Some(crate::pki::fingerprints(&old)?.serial),
                key: None,
                at: 0,
                reason: "test".into(),
                by: "owner".into(),
            };
            assert!(verify_start(&pki.ca_der, "west", "send", &proof, &[revoke]).is_err());
            Ok(())
        })
    }

    #[test]
    fn cp_t3_2_session_grant_is_distinct_bound_and_revocable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open_scratch(&dir.path().join("pki"))?;
        let (csr, key) = crate::pki::make_csr("west")?;
        let state = dir.path().join("node");
        enroll_session_node(&state)?;
        crate::util::write_secret(&state.join("agent.key"), key.as_bytes(), 0o600)?;
        crate::util::write_secret(&state.join("session-node.pem"), pki.sign_csr(&csr, "west", 1)?.as_bytes(), 0o600)?;
        assert!(sign_start(&state, "west", "east:session:a").is_err());
        let cert = pki.sign_session_node(&csr, "west", 1)?;
        crate::util::write_secret(&state.join("session-node.pem"), cert.as_bytes(), 0o600)?;
        let proof = sign_start(&state, "west", "east:session:a")?;
        verify_start(&pki.ca_der, "west", "east:session:a", &proof, &[])?;
        assert!(verify_start(&pki.ca_der, "other", "east:session:a", &proof, &[]).is_err());
        assert!(verify_start(&pki.ca_der, "west", "east:session:b", &proof, &[]).is_err());
        let revoked = kiln_ci::identity::chain::Revocation {
            key: Some(crate::pki::fingerprints(&cert)?.key),
            ..Default::default()
        };
        assert!(verify_start(&pki.ca_der, "west", "east:session:a", &proof, &[revoked]).is_err());
        Ok(())
    }

    #[test]
    fn cp_t3_2_offline_routes_are_bound_to_node_and_fleet() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut peers = BTreeMap::from([(
            "east".into(),
            Member {
                aliases: vec!["laptop".into(), "build".into()],
                key: "pinned".into(),
                addrs: vec!["127.0.0.1:7071".parse()?],
                v2: true,
                raft: true,
                executor: true,
                charter: true,
                liveness: true,
                starts:true, authority_mail:true,
            },
        )]);
        Routes::open(dir.path(), "west", b"ca-a")?.replace(peers.clone(), None)?;
        let routes = Routes::open(dir.path(), "west", b"ca-a")?;
        assert_eq!(routes.members(), peers);
        assert_eq!(routes.resolve_alias("laptop")?.as_deref(), Some("east"));
        assert!(routes.resolve_alias("other")?.is_none());
        let mut second = peers.get("east").ok_or_else(|| anyhow::anyhow!("missing fixture member"))?.clone();
        second.aliases = vec!["build".into()];
        peers.insert("north".into(), second);
        routes.replace(peers, None)?;
        assert!(routes.resolve_alias("build").is_err());
        assert_eq!(routes.resolve_alias("north")?.as_deref(), Some("north"));
        assert!(Routes::open(dir.path(), "west", b"ca-b")?.members().is_empty());
        assert!(Routes::open(dir.path(), "other", b"ca-a")?.members().is_empty());
        Routes::open(dir.path(), "west", b"ca-a")?.replace(BTreeMap::new(), None)?;
        assert!(Routes::open(dir.path(), "west", b"ca-a")?.members().is_empty());
        std::fs::write(dir.path().join("session-members.json"), b"partial")?;
        assert!(Routes::open(dir.path(),"west",b"ca-a").is_err());
        std::fs::rename(dir.path().join("session-members.json"), dir.path().join("session-members.invalid"))?;
        assert!(Routes::open(dir.path(),"west",b"ca-a").is_err());
        Ok(())
    }
}
