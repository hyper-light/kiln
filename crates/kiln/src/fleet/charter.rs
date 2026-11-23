//! CP-T3.2. A fleet's original name registrar delegates new names exactly once. A missing
//! peer or a partial directory can never constitute a new electorate for the same namespace.
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, ensure};
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::convo::raft::Manager;
use crate::hub::Hub;
use crate::pki::Pki;
use crate::reach::directory::RunnerInfo;
use crate::store::Store;

/// Three voters are the minimum electorate that can lose one node and retain a majority.
const FIRST_QUORUM: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct Certificate {
    pub registrar: String,
    pub voters: Vec<String>,
    pub reserved_count: u64,
    pub reserved_digest: [u8; 32],
    signature: Vec<u8>,
}

pub(crate) fn digest_names<'a>(names: impl IntoIterator<Item = &'a str>) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"kiln session name reservations v1\0");
    for name in names {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
    }
    hash.finalize().into()
}

impl Certificate {
    pub(crate) fn digest(&self, ca: &[u8]) -> Result<[u8; 32]> {
        let bytes = serde_json::to_vec(&(
            "kiln session name authority v1",
            Sha256::digest(ca).as_slice(),
            &self.registrar,
            &self.voters,
            self.reserved_count,
            self.reserved_digest,
        ))?;
        Ok(Sha256::digest(bytes).into())
    }

    pub(crate) fn verify(&self, ca: &[u8]) -> Result<[u8; 32]> {
        ensure!(!self.registrar.is_empty(), "empty session name registrar");
        if self.voters.is_empty() {
            ensure!(
                self.reserved_count == 0 && self.reserved_digest == digest_names(std::iter::empty()),
                "invalid registrar certificate"
            );
        } else {
            ensure!(
                self.voters.len() == FIRST_QUORUM && self.voters.iter().all(|v| !v.is_empty()) && self.voters.windows(2).all(|w| w.first() < w.last()),
                "invalid founding electorate"
            );
        }
        let digest = self.digest(ca)?;
        let (_, root) = x509_parser::parse_x509_certificate(ca).map_err(|e| anyhow::anyhow!("invalid fleet CA: {e}"))?;
        let key = aws_lc_rs::signature::UnparsedPublicKey::new(
            &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
            root.public_key().subject_public_key.data.as_ref(),
        );
        key.verify(&digest, &self.signature)
            .map_err(|_| anyhow::anyhow!("invalid fleet name authority signature"))?;
        Ok(digest)
    }

    fn sign(pki: &Pki, registrar: String, voters: Vec<String>, names: &BTreeSet<String>) -> Result<Self> {
        let mut cert = Self {
            registrar,
            voters,
            reserved_count: u64::try_from(names.len())?,
            reserved_digest: digest_names(names.iter().map(String::as_str)),
            signature: Vec::new(),
        };
        cert.signature = pki.sign_fleet_statement(&cert.digest(&pki.ca_der)?)?;
        Ok(cert)
    }
}

#[cfg(test)]
pub(super) fn signed_fixture(pki: &Pki, registrar: &str, mut voters: Vec<String>, names: &[&str]) -> Result<Certificate> {
    voters.sort_unstable();
    Certificate::sign(pki, registrar.to_owned(), voters, &names.iter().map(|s| (*s).to_owned()).collect())
}

#[derive(Default)]
pub(crate) struct Issuer {
    initial: OnceLock<Certificate>,
    sealed: OnceLock<Certificate>,
    plan: OnceLock<Vec<String>>,
    sealing: futures::lock::Mutex<()>,
    waiting: std::sync::atomic::AtomicBool,
    refresh_due: Arc<std::sync::atomic::AtomicBool>,
}

impl Issuer {
    pub(crate) fn waiting_for_legacy(&self) -> bool {
        self.waiting.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn sealed(&self) -> Option<&Certificate> {
        self.sealed.get()
    }
    pub(crate) fn registrar(&self) -> Result<Certificate> {
        self.initial
            .get()
            .cloned()
            .context("the fleet's authenticated name registrar is unavailable")
    }
}

async fn certificate(db: &Store, kind: i64) -> Result<Option<Certificate>> {
    db.read(move |c| {
        let bytes: Option<Vec<u8>> = c
            .query_row("SELECT certificate FROM session_name_authority WHERE kind=?1", [kind], |r| r.get(0))
            .optional()?;
        Ok(bytes.map(|b| serde_json::from_slice(&b)).transpose()?)
    })
    .await
}

/// Run before the bootstrap's front door or peer manager accepts work. A sealed charter
/// survives a bootstrap outage, and a restart mid-freeze finishes precisely its recorded plan.
pub(crate) async fn initialize(hub: &Arc<Hub>, manager: &Arc<Manager>) -> Result<()> {
    let initial = match certificate(&hub.store, 0).await? {
        Some(cert) => cert,
        None => {
            let cert = Certificate::sign(&hub.pki, manager.node_name().to_owned(), Vec::new(), &BTreeSet::new())?;
            let bytes = serde_json::to_vec(&cert)?;
            hub.store
                .call(move |c| {
                    c.execute(
                        "INSERT INTO session_name_authority(kind,certificate) VALUES(0,?1) ON CONFLICT(kind) DO NOTHING",
                        [bytes],
                    )?;
                    Ok(())
                })
                .await?;
            certificate(&hub.store, 0).await?.context("registrar was not persisted")?
        }
    };
    initial.verify(&hub.pki.ca_der)?;
    ensure!(
        initial.voters.is_empty() && initial.registrar == manager.node_name(),
        "name authority belongs to another registrar"
    );
    let sealed = certificate(&hub.store, 1).await?;
    let (m, registrar, legacy) = (Arc::clone(manager), initial.registrar.clone(), sealed.is_none());
    crate::rt::blocking(move || -> Result<()> {
        if legacy {
            m.guard_legacy_names(true, BTreeSet::new())?;
        }
        m.set_name_registrar(&registrar)
    })
    .await??;
    let _ = hub.name_issuer.initial.set(initial);
    if let Some(c) = sealed {
        restore_sealed(hub, manager, c).await?;
    } else {
        seed_original_history(hub, manager).await?;
        refresh_guard(hub, manager).await?;
        if let Some(voters) = stored_plan(&hub.store).await? {
            finish_plan(hub, manager, voters).await?;
        }
    }
    Ok(())
}

async fn seed_original_history(hub: &Arc<Hub>, mgr: &Arc<Manager>) -> Result<()> {
    let (seeded,prior)=hub.store.read(|c|Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_name_bootstrap_inventory),(EXISTS(SELECT 1 FROM session_name_legacy_holders) OR EXISTS(SELECT 1 FROM agent_mail) OR EXISTS(SELECT 1 FROM agentenv_sessions) OR EXISTS(SELECT 1 FROM agent_def_sessions))", [],
        |r|Ok((r.get::<_,bool>(0)?,r.get::<_,bool>(1)?)))?)).await?;
    if seeded {
        return Ok(());
    }
    let root = Arc::clone(hub);
    let manager = Arc::clone(mgr);
    let (names, original) = crate::rt::blocking(move || -> Result<_> {
        let db = root.harness.convo.as_ref().context("missing conversation catalog")?;
        let mut names: BTreeSet<_> = manager.historical_name_inventory()?.into_iter().collect();
        names.extend(db.historical_names()?);
        names.extend(root.harness.ids());
        root.harness.local.historical_session_names(&mut names)?;
        if let Some(dir) = root.harness.local.archive_dir() {
            archive_names(&dir, &mut names)?;
        }
        Ok((names, db.has_legacy_name_catalog()?))
    })
    .await??;
    let registrar = mgr.node_name().to_owned();
    hub.store
        .call(move |c| {
            let tx = c.savepoint()?;
            {
                let mut q = tx.prepare("INSERT OR IGNORE INTO session_name_legacy_rows(name) VALUES(?1)")?;
                for n in names {
                    q.execute([n])?;
                }
            }
            if prior && !original {
                // The old catalog is missing even though the global server has earlier holders.
                // Keep established sessions available while blocking a new namespace until recovery.
                tx.execute(
                    "INSERT INTO session_name_legacy_holders(name) VALUES(?1) ON CONFLICT(name) DO NOTHING",
                    [&registrar],
                )?;
            } else {
                tx.execute("UPDATE session_name_legacy_holders SET complete=1 WHERE name=?1", [registrar])?;
                tx.execute("INSERT OR IGNORE INTO session_name_bootstrap_inventory(id) VALUES(1)", [])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

async fn refresh_guard(hub: &Arc<Hub>, mgr: &Arc<Manager>) -> Result<()> {
    let (waiting,names)=hub.store.read(|c| {
        let waiting:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM session_name_legacy_holders WHERE complete=0)",[],|r|r.get(0))?;
        if waiting {return Ok((true,BTreeSet::new()));}
        let mut q=c.prepare("SELECT name FROM session_name_legacy_rows UNION SELECT session FROM agentenv_sessions UNION SELECT session_id FROM agent_def_sessions UNION SELECT recipient FROM agent_mail")?;
        Ok((false,q.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<BTreeSet<String>>>()?))
    }).await?;
    let mgr = Arc::clone(mgr);
    crate::rt::blocking(move || mgr.guard_legacy_names(waiting, names)).await??;
    hub.name_issuer.waiting.store(waiting, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// A successful last legacy page releases the gate even in one- and two-executor fleets.
pub(crate) async fn observe_legacy(hub: &Arc<Hub>, machine: String, page: Option<InventoryPage>) -> Result<Option<String>> {
    let refresh = &hub.name_issuer.refresh_due;
    let after = advance_legacy(&hub.store, machine, page, Some(refresh.clone())).await?;
    if refresh.load(std::sync::atomic::Ordering::Acquire) && hub.name_issuer.waiting_for_legacy() {
        let _one = hub.name_issuer.sealing.lock().await;
        if hub.name_issuer.sealed().is_none()
            && let Some(mgr) = hub.harness.session_consensus()?
        {
            refresh_guard(hub, &mgr).await?;
            refresh.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    Ok(after)
}

async fn stored_plan(store: &Store) -> Result<Option<Vec<String>>> {
    store
        .read(|c| {
            let raw: Option<String> = c
                .query_row("SELECT voters FROM session_name_plan WHERE id=1", [], |r| r.get(0))
                .optional()?;
            Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
        })
        .await
}

async fn install(hub: &Arc<Hub>, mgr: &Arc<Manager>, cert: Certificate, names: BTreeSet<String>) -> Result<()> {
    let (hub, mgr) = (Arc::clone(hub), Arc::clone(mgr));
    crate::rt::blocking(move || -> Result<()> {
        let digest = cert.verify(&hub.pki.ca_der)?;
        ensure!(cert.registrar == mgr.node_name() && !cert.voters.is_empty(), "foreign name charter");
        ensure!(
            u64::try_from(names.len())? == cert.reserved_count && digest_names(names.iter().map(String::as_str)) == cert.reserved_digest,
            "historic session reservations differ from the charter"
        );
        mgr.install_name_charter(digest, cert.voters.clone(), names)?;
        let _ = hub.name_issuer.sealed.set(cert);
        Ok(())
    })
    .await?
}

async fn restore_sealed(hub: &Arc<Hub>, mgr: &Arc<Manager>, cert: Certificate) -> Result<Certificate> {
    let digest = cert.verify(&hub.pki.ca_der)?;
    let manager = Arc::clone(mgr);
    crate::rt::blocking(move || manager.require_name_charter(digest)).await??;
    let names = hub
        .store
        .read(|db| {
            let mut q = db.prepare("SELECT name FROM session_name_reservations ORDER BY name")?;
            Ok(q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<BTreeSet<String>>>()?)
        })
        .await?;
    install(hub, mgr, cert.clone(), names).await?;
    Ok(cert)
}

// Keep the recorded decision even if taking the local cutoff or installing it fails.
// Retry the same plan independently of which agents are currently connected.
async fn finish_plan(hub: &Arc<Hub>, mgr: &Arc<Manager>, voters: Vec<String>) -> Result<Certificate> {
    let plan = hub.name_issuer.plan.get_or_init(|| voters.clone());
    ensure!(plan == &voters, "durable session name charter voters changed");
    if let Some(cert) = certificate(&hub.store, 1).await? {
        return restore_sealed(hub, mgr, cert).await;
    }
    let (g, convos, registry) = (
        Arc::clone(mgr),
        hub.harness.convo.clone().context("conversation catalog unavailable")?,
        Arc::clone(hub),
    );
    let names = crate::rt::blocking(move || -> Result<BTreeSet<String>> {
        // This durable cut fences both future starts and in-flight provisional reservations.
        let mut names: BTreeSet<_> = g.freeze_name_registrar()?.into_iter().collect();
        names.extend(convos.historical_names()?);
        names.extend(registry.harness.ids());
        registry.harness.local.historical_session_names(&mut names)?;
        if let Some(dir) = registry.harness.local.archive_dir() {
            archive_names(&dir, &mut names)?;
        }
        ensure!(!names.contains(""), "empty name in the session archive");
        Ok(names)
    })
    .await??;
    let mut names = names;
    let historical=hub.store.read(|c| {
        let mut q=c.prepare("SELECT name FROM session_name_legacy_rows UNION SELECT session FROM agentenv_sessions UNION SELECT session_id FROM agent_def_sessions UNION SELECT recipient FROM agent_mail")?;
        Ok(q.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<BTreeSet<String>>>()?)
    }).await?;
    names.extend(historical);
    let (pki, registrar) = (Arc::clone(&hub.pki), mgr.node_name().to_owned());
    let (cert, names) = crate::rt::blocking(move || -> Result<_> { Ok((Certificate::sign(&pki, registrar, voters, &names)?, names)) }).await??;
    let bytes = serde_json::to_vec(&cert)?;
    let (actual, names) = hub
        .store
        .call(move |c| {
            let tx = c.savepoint()?;
            let old: Option<Vec<u8>> = tx
                .query_row("SELECT certificate FROM session_name_authority WHERE kind=1", [], |r| r.get(0))
                .optional()?;
            if let Some(old) = old {
                let committed = serde_json::from_slice::<Certificate>(&old)?;
                let mut q = tx.prepare("SELECT name FROM session_name_reservations ORDER BY name")?;
                let persisted = q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<BTreeSet<String>>>()?;
                return Ok((committed, persisted));
            }
            {
                let mut q = tx.prepare("INSERT INTO session_name_reservations(name) VALUES(?1)")?;
                for name in &names {
                    q.execute([name])?;
                }
            }
            tx.execute("INSERT INTO session_name_authority(kind,certificate) VALUES(1,?1)", [bytes])?;
            tx.commit()?;
            Ok((cert, names))
        })
        .await?;
    install(hub, mgr, actual.clone(), names).await?;
    Ok(actual)
}

/// Only a complete authenticated directory can call this. Queue ordering and transient routing
/// never amend an issued charter. Before there are three capable members the registrar witnesses
/// every new name. The plan is persisted before freezing, including across interrupted issuance.
pub(crate) async fn issue(hub: &Arc<Hub>, advertised: &[RunnerInfo]) -> Result<Certificate> {
    if let Some(cert) = hub.name_issuer.sealed.get() {
        return Ok(cert.clone());
    }
    let initial = hub.name_issuer.registrar()?;
    let mgr = hub.harness.session_consensus()?.context("session name manager is unavailable")?;
    let supports = |r: &RunnerInfo| {
        !r.stale
            && !r.labels.iter().any(|s| s == crate::proto::OUTPOST_LABEL)
            && [
                crate::proto::feature::CONVO_RAFT_V2,
                crate::proto::feature::CONVO_EXECUTOR,
                crate::proto::feature::CONVO_NAME_CHARTER,
            ]
            .iter()
            .all(|f| crate::proto::feature::speaks(&r.features, f))
    };
    let mut candidates: BTreeSet<_> = advertised
        .iter()
        .filter(|r| supports(r))
        .map(|r| r.name.clone())
        .filter(|n| !n.is_empty() && n != mgr.node_name())
        .collect();
    if mgr.can_execute() {
        candidates.insert(mgr.node_name().to_owned());
    }
    if candidates.len() < FIRST_QUORUM && hub.name_issuer.plan.get().is_none() {
        return Ok(initial);
    }
    let _one = hub.name_issuer.sealing.lock().await;
    if let Some(cert) = hub.name_issuer.sealed.get() {
        return Ok(cert.clone());
    }
    if let Some(plan) = hub.name_issuer.plan.get() {
        return finish_plan(hub, &mgr, plan.clone()).await;
    }
    let registrar = initial.registrar.clone();
    let issuer = hub.clone();
    let voters = hub
        .store
        .call(move |c| {
            let tx = c.savepoint()?;
            let old: Option<String> = tx
                .query_row("SELECT voters FROM session_name_plan WHERE id=1", [], |r| r.get(0))
                .optional()?;
            if let Some(old) = old {
                return Ok(Some(serde_json::from_str(&old)?));
            }
            let waiting: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_name_legacy_holders WHERE complete=0)", [], |r| {
                r.get(0)
            })?;
            if waiting {
                return Ok(None);
            }
            let mut ranked = Vec::with_capacity(candidates.len());
            {
                let mut q = tx.prepare(
                    "SELECT MIN(i.rowid) FROM identities i JOIN enrollments e ON e.machine=i.subject
                JOIN agents a ON a.name=i.subject AND a.ephemeral=0 WHERE i.subject=?1 AND i.kind='machine'
                AND i.not_before<=?2 AND i.not_after>?2
                AND NOT EXISTS(SELECT 1 FROM revocations r WHERE r.serial=i.serial AND r.at<=?2)
                AND NOT EXISTS(SELECT 1 FROM revocations r WHERE r.key_sha256=i.key_sha256 AND r.at<=?2)",
                )?;
                let now = crate::util::now_unix();
                for name in candidates {
                    if name == registrar {
                        ranked.push((false, 0, name));
                    } else if let Some(first) = q.query_row(rusqlite::params![&name, now], |r| r.get::<_, Option<i64>>(0))? {
                        ranked.push((true, first, name));
                    }
                }
            }
            if ranked.len() < FIRST_QUORUM {
                return Ok(None);
            }
            ranked.sort_unstable();
            let mut voters = ranked.into_iter().take(FIRST_QUORUM).map(|(_, _, n)| n).collect::<Vec<_>>();
            voters.sort_unstable();
            tx.execute("INSERT INTO session_name_plan(id,voters) VALUES(1,?1)", [serde_json::to_string(&voters)?])?;
            tx.commit()?;
            // A dropped directory request must still leave the durable plan discoverable by
            // the next request, even if all three original voters have gone offline.
            let _ = issuer.name_issuer.plan.set(voters.clone());
            Ok(Some(voters))
        })
        .await?;
    match voters {
        Some(voters) => finish_plan(hub, &mgr, voters).await,
        None => Ok(initial),
    }
}

/// Sorted immutable historical names packed to the caller's remaining wire bytes.
pub(crate) async fn page(store: &Store, after: &str, room: usize) -> Result<Vec<String>> {
    let after = after.to_owned();
    store
        .read(move |db| {
            let mut q = db.prepare_cached("SELECT name FROM session_name_reservations WHERE name>?1 ORDER BY name")?;
            let mut rows = q.query([after])?;
            let mut out = Vec::new();
            let mut bytes = 2usize;
            while let Some(row) = rows.next()? {
                let name: String = row.get(0)?;
                let next = bytes
                    .saturating_add(crate::memory::queue::json_bytes(&name))
                    .saturating_add(usize::from(!out.is_empty()));
                if next > room {
                    ensure!(!out.is_empty(), "no room for the next charter reservation");
                    break;
                }
                out.push(name);
                bytes = next;
            }
            Ok(out)
        })
        .await
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InventoryPage {
    after: String,
    names: Vec<String>,
    complete: bool,
}

fn archive_names(dir: &std::path::Path, names: &mut BTreeSet<String>) -> Result<()> {
    let files = match std::fs::read_dir(dir) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in files {
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            names.insert(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .context("invalid archived session name")?
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// A legacy holder records a stable positive inventory before uploading any part of it. New
/// sessions after this point need a witness at the original registrar. A lost reply or restart
/// returns exactly the same ordered suffix instead of trusting a previously missing name.
pub(crate) fn local_inventory(dir: &std::path::Path, ca: &[u8], machine: &str) -> Result<Arc<BTreeSet<String>>> {
    let name = crate::util::sha256_hex(&serde_json::to_vec(&(crate::util::sha256_hex(ca), machine))?);
    let path = dir.join(format!("legacy-names-{name}.json"));
    let read = || -> Result<_> {
        Ok(Arc::new(serde_json::from_reader::<_, BTreeSet<String>>(std::io::BufReader::new(
            std::fs::File::open(&path)?,
        ))?))
    };
    if path.try_exists()? {
        return read();
    }
    let local = crate::frontdoor::replica::local_node().filter(|n| n.machine == machine);
    let mgr = crate::convo::raft::runner_global()
        .filter(|m| m.node_name() == machine)
        .or_else(|| crate::convo::raft::server_global().filter(|m| m.node_name() == machine))
        .or_else(|| local.as_ref()?.convo_raft.clone())
        .context("cannot attest an unopened session manager")?;
    let mut names: BTreeSet<_> = mgr.historical_name_inventory()?.into_iter().collect();
    let store = mgr.store().context("legacy conversation store unavailable")?;
    ensure!(
        store.has_legacy_name_catalog()?,
        "a freshly created or replaced conversation store cannot attest its previous history"
    );
    names.extend(store.historical_names()?);
    let base = dir.parent().context("missing fleet state directory")?;
    let root = base
        .parent()
        .filter(|p| p.file_name().is_some_and(|s| s == "instances"))
        .and_then(std::path::Path::parent)
        .unwrap_or(base);
    archive_names(&root.join("archived"), &mut names)?;
    crate::harness::host::Host::read_session_names(&root.join("harness.json"), &mut names)?;
    // Records from either side of a blue/green handover can survive a partial upgrade.
    let instances = match std::fs::read_dir(root.join("instances")) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if let Some(entries) = instances {
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                crate::harness::host::Host::read_session_names(&entry.path().join("harness.json"), &mut names)?;
            }
        }
    }
    if let Some(host) = local.and_then(|n| n.host.clone()) {
        host.historical_session_names(&mut names)?;
        if let Some(archives) = host.archive_dir() {
            archive_names(&archives, &mut names)?;
        }
    }
    ensure!(!names.contains(""), "empty historic session name");
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    {
        use std::io::Write as _;
        let mut buf = std::io::BufWriter::new(tmp.as_file_mut());
        serde_json::to_writer(&mut buf, &names)?;
        buf.flush()?;
    }
    tmp.as_file().sync_all()?;
    match tmp.persist_noclobber(&path) {
        Ok(_) => {}
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => return read(),
        Err(e) => return Err(e.into()),
    }
    crate::blobs::chunks::sync_dir(dir)?;
    Ok(Arc::new(names))
}

pub(crate) fn inventory_page(names: &BTreeSet<String>, after: &str, room: usize) -> Result<InventoryPage> {
    use std::ops::Bound::{Excluded, Unbounded};
    let mut page = InventoryPage {
        after: after.to_owned(),
        names: Vec::new(),
        complete: false,
    };
    let mut bytes = crate::memory::queue::json_bytes(&page);
    for name in names.range::<str, _>((Excluded(after), Unbounded)) {
        let next = bytes
            .saturating_add(usize::from(!page.names.is_empty()))
            .saturating_add(crate::memory::queue::json_bytes(name));
        if next > room {
            ensure!(!page.names.is_empty(), "no room for a legacy name inventory");
            return Ok(page);
        }
        bytes = next;
        page.names.push(name.clone());
    }
    page.complete = true;
    Ok(page)
}

/// The authenticated machine link supplies the subject. A later page cannot complete a prefix
/// that the registrar has not durably received. Retries get the current committed cursor.
#[cfg(test)]
async fn legacy_cursor(db: &Store, machine: String, page: Option<InventoryPage>) -> Result<Option<String>> {
    advance_legacy(db, machine, page, None).await
}

async fn advance_legacy(
    db: &Store,
    machine: String,
    page: Option<InventoryPage>,
    refresh: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<Option<String>> {
    let query = "SELECT cursor FROM session_name_legacy_holders WHERE name=?1 AND complete=0";
    let Some(page) = page else {
        return db.read(move |c| Ok(c.query_row(query, [machine], |r| r.get(0)).optional()?)).await;
    };
    db.call(move |c| {
        let tx = c.savepoint()?;
        let cursor: Option<String> = tx.query_row(query, [&machine], |r| r.get(0)).optional()?;
        if cursor.as_deref() != Some(page.after.as_str()) {
            return Ok(cursor);
        }
        ensure!(page.complete || !page.names.is_empty(), "nonfinal legacy inventory with no progress");
        let mut last = page.after.as_str();
        {
            let mut q = tx.prepare("INSERT OR IGNORE INTO session_name_legacy_rows(name) VALUES(?1)")?;
            for name in &page.names {
                ensure!(name.as_str() > last, "unordered legacy inventory");
                q.execute([name])?;
                last = name;
            }
        }
        tx.execute(
            "UPDATE session_name_legacy_holders SET cursor=?2,complete=?3 WHERE name=?1",
            rusqlite::params![machine, last, page.complete],
        )?;
        let finished = page.complete
            && refresh.is_some()
            && !tx.query_row("SELECT EXISTS(SELECT 1 FROM session_name_legacy_holders WHERE complete=0)", [], |r| {
                r.get::<_, bool>(0)
            })?;
        tx.commit()?;
        // The directory waiter may disappear after SQLite commits its final page. Any
        // later directory poll retries loading the guard until the Raft owner acknowledges.
        if finished && let Some(refresh) = refresh {
            refresh.store(true, std::sync::atomic::Ordering::Release);
        }
        Ok((!page.complete).then(|| last.to_owned()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executor(name: &str) -> RunnerInfo {
        RunnerInfo {
            name: name.into(),
            connected: true,
            features: [
                crate::proto::feature::CONVO_RAFT_V2,
                crate::proto::feature::CONVO_EXECUTOR,
                crate::proto::feature::CONVO_NAME_CHARTER,
            ]
            .map(str::to_owned)
            .to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn cp_t3_2_missing_bootstrap_catalog_keeps_new_names_fenced_after_other_holders_report() -> Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let db = hub.harness.convo.clone().context("missing test catalog")?;
            assert!(!db.has_legacy_name_catalog()?);
            hub.store
                .call(|c| {
                    c.execute("INSERT INTO session_name_legacy_holders(name) VALUES('old-runner')", [])?;
                    Ok(())
                })
                .await?;
            let wal = tempfile::tempdir()?;
            let mgr = Manager::open_for_host("origin", wal.path(), Some(db), crate::convo::raft::Policy::default(), true)?;
            initialize(&hub, &mgr).await?;
            hub.harness.bind_session_consensus(&mgr)?;
            let report = InventoryPage {
                after: String::new(),
                names: vec!["remote-history".into()],
                complete: true,
            };
            assert!(observe_legacy(&hub, "old-runner".into(), Some(report)).await?.is_none());
            assert_eq!(observe_legacy(&hub, "origin".into(), None).await?.as_deref(), Some(""));
            assert!(hub.name_issuer.waiting_for_legacy());
            assert!(mgr.open_leader("missing-owner", 1, Vec::new()).is_err());
            assert!(mgr.freeze_name_registrar().is_err());
            mgr.shutdown();
            Ok(())
        })
    }

    #[test]
    fn cp_t3_2_first_charter_requires_valid_enrollment_and_complete_positive_inventory() -> Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let db = hub.harness.convo.clone().context("missing conversation catalog in test")?;
            let dir = tempfile::tempdir()?;
            let mgr = Manager::open_for_host("origin", dir.path(), Some(db.clone()), crate::convo::raft::Policy::default(), true)?;
            initialize(&hub, &mgr).await?;
            hub.harness.bind_session_consensus(&mgr)?;
            let original = hub.name_issuer.registrar()?;
            let held = hub.cfg.data("harness").join("local.json");
            std::fs::write(&held, br#"[{"id":"asleep-on-bootstrap","unneeded":{"large":[1,2,3]}}]"#)?;
            let old = executor("old");
            let worker = executor("worker");
            let ghost = executor("no-enrollment");
            let revoked = executor("revoked");
            for peer in [&old, &worker, &revoked] {
                let (csr, _key) = crate::pki::make_csr(&peer.name)?;
                let cert = hub.pki.sign_csr(&csr, &peer.name, crate::pki::LEAF_DAYS)?;
                crate::identities::record(&hub.store, "machine", &peer.name, &cert).await?;
                crate::identities::enrolled(&hub.store, &peer.name, "admin").await?;
                let subject = peer.name.clone();
                let serial = crate::pki::fingerprints(&cert)?.serial;
                hub.store.call(move |c| {
                    c.execute("INSERT OR IGNORE INTO agents(name,labels,slots,hostname,os,arch,version,ephemeral,connected,last_seen,enrolled_at) VALUES(?1,'[]',1,?1,'linux','amd64','test',0,1,0,0)",[&subject])?;
                    if subject=="revoked" {c.execute("INSERT INTO revocations(serial,subject,at,reason,by) VALUES(?1,?2,0,'test','admin')",rusqlite::params![serial,subject])?;}
                    Ok(())
                }).await?;
            }
            assert_eq!(issue(&hub, &[old.clone(), ghost.clone(), revoked.clone()]).await?, original);
            hub.store
                .call(|c| {
                    c.execute("INSERT INTO session_name_legacy_holders(name) VALUES('old')", [])?;
                    Ok(())
                })
                .await?;
            refresh_guard(&hub, &mgr).await?;
            assert!(mgr.open_leader("unknown-legacy-owner", 1, Vec::new()).is_err());
            let members = [old, worker, ghost, revoked];
            assert_eq!(issue(&hub, &members).await?, original);
            let partial = InventoryPage {
                after: String::new(),
                names: vec!["archive".into()],
                complete: false,
            };
            assert_eq!(
                legacy_cursor(&hub.store, "old".into(), Some(partial.clone())).await?.as_deref(),
                Some("archive")
            );
            assert_eq!(legacy_cursor(&hub.store, "old".into(), Some(partial)).await?.as_deref(), Some("archive"));
            assert_eq!(issue(&hub, &members).await?, original);
            assert!(
                observe_legacy(
                    &hub,
                    "old".into(),
                    Some(InventoryPage {
                        after: "archive".into(),
                        names: vec!["offline".into()],
                        complete: true
                    })
                )
                .await?
                .is_none()
            );
            assert!(mgr.open_leader("offline", 1, Vec::new()).is_err());
            mgr.open_leader("new-after-inventory", 1, Vec::new())?;
            db.put(vec![crate::convo::rows::Row::Session {
                session: "bootstrap-history".into(),
                info: Some("{}".into()),
                meta: None,
                state: None,
                changes: None,
                epoch: Some(0),
                open: true,
                at: 1,
            }]);
            db.sync()?;
            let cert = issue(&hub, &members).await?;
            assert_eq!(cert.voters, ["old", "origin", "worker"]);
            assert_eq!(issue(&hub, &[]).await?, cert);
            let names = page(&hub.store, "", usize::MAX).await?;
            assert_eq!(
                names,
                ["archive", "asleep-on-bootstrap", "bootstrap-history", "new-after-inventory", "offline"]
            );
            assert_eq!(digest_names(names.iter().map(String::as_str)), cert.reserved_digest);
            let mut tampered = cert.clone();
            tampered.reserved_count += 1;
            assert!(tampered.verify(&hub.pki.ca_der).is_err());
            let other = crate::pki::Pki::open_scratch(&dir.path().join("other"))?;
            assert!(cert.verify(&other.ca_der).is_err());
            assert_eq!(mgr.name_charter_digest(), Some(cert.verify(&hub.pki.ca_der)?));
            assert_eq!(certificate(&hub.store, 1).await?, Some(cert));
            mgr.shutdown();
            Ok(())
        })
    }
}
