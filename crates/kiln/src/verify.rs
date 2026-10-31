//! kiln verify (docs/ci.md W27-R10): the chain back from a deploy, an artifact, a commit or a
//! change to whoever made it, a link at a time, the first that doesn't hold named. Each link rests
//! on a statement kiln took: checked as of when kiln took it (its signer's chain to kiln's root,
//! the revocations as of then), and found in the trail by an inclusion proof against the newest
//! checkpoint kiln's server signed, one signed first when any entry came after the last. What
//! holds is `kiln_ci::identity::verify`'s; [`verify`] walks.
//!
//! - **A deploy**: its release as kiln's deploy records have it (deploys sign with Ship's plan),
//!   and each artifact it runs, by digest.
//! - **An artifact**: the provenance that names it, signed as the leg that built it, and the
//!   commit it was built from. For a deploy, every commit on the trunk that commit brings: since
//!   the commit the release before it ran, else since the trunk's first land; at most 1,000.
//! - **A commit**: on its trunk, its signature as kiln checked it; the land that put it there,
//!   signed as the queue that tested it (the candidate tested is what landed, every check
//!   passing); each verdict the land names, holding on the version that landed (given there, or
//!   carried by verbatim patch-id), people and agents apart, and as many as the trunk's policy
//!   asked where the land began; each revision those verdicts were given on, and the one that
//!   landed, signed by whoever made it.
//! - **A change**: the commit its land put on the trunk, as a commit. A change whose hands it
//!   changed are recorded (custody, by its change's identity) has that chain walked too.
//! - **kiln's root** ends each chain: every signature above verifies to it.

use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

use base64::Engine as _;
use http::StatusCode;
use kiln_ci::identity::chain::{Actor, Identity, Revocation};
use kiln_ci::identity::landing::{self, Land, Landed};
use kiln_ci::identity::statement::{self, Statement};
use kiln_ci::identity::trail;
use kiln_ci::identity::verdict::{self, Kept};
use kiln_ci::identity::verify::{self, Against, Held, Link, Proven};
use kiln_ci::land::Policy;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use crate::ci::Fail;
use crate::ci::diffs::{self, Diff};
use crate::deploy::model::{Release, State};
use crate::deploy::store::ReleaseQuery;
use crate::hub::Hub;
use crate::store::Repo;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The most commits a deploy's walk takes in for one artifact; past that, it says so.
const COMMITS_MAX: usize = 1000;

/// What `kiln verify` walks back from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A release: its number, or `<environment>/<app>@<version>`.
    Deploy(String),
    /// An artifact, by its digest (`sha256:<hex>`).
    Artifact(String),
    /// A commit of a repository kiln hosts.
    Commit { repo: String, sha: String },
    /// A change: its diff (`D12`) or the diff's id.
    Change(String),
}

impl Target {
    /// `id` as a `kind` of target: `deploy`, `artifact`, `commit` (`<repository>@<commit>`) or
    /// `change`.
    pub fn parse(kind: &str, id: &str) -> Result<Self, String> {
        let id = id.trim();
        if id.is_empty() {
            return Err(format!("say which {kind} to verify"));
        }
        let is_hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
        match kind {
            "deploy" => Ok(Self::Deploy(id.to_owned())),
            "artifact" => {
                let hex = id.strip_prefix("sha256:").unwrap_or(id);
                if hex.len() == 64 && is_hex(hex) { Ok(Self::Artifact(format!("sha256:{}", hex.to_ascii_lowercase()))) } else { Err(format!("{id} isn't an artifact's digest, sha256:<hex>")) }
            }
            "commit" => match id.rsplit_once('@') {
                Some((repo, sha)) if !repo.is_empty() && (4..=64).contains(&sha.len()) && is_hex(sha) => Ok(Self::Commit { repo: repo.to_owned(), sha: sha.to_ascii_lowercase() }),
                _ => Err(format!("{id} isn't <repository>@<commit>")),
            },
            "change" => Ok(Self::Change(id.to_owned())),
            other => Err(format!("kiln verifies a deploy, an artifact, a commit or a change, not {other:?}")),
        }
    }

    /// The one target `q` names (`deploy`, `artifact`, `commit` or `change`), as `GET /api/verify`
    /// and its operation take it.
    pub fn of(q: &HashMap<String, String>) -> Result<Self, Fail> {
        let asked: Vec<(&str, &String)> = ["deploy", "artifact", "commit", "change"].into_iter().filter_map(|k| q.get(k).map(|v| (k, v))).collect();
        match asked.as_slice() {
            [(kind, id)] => Self::parse(kind, id).map_err(|e| (StatusCode::BAD_REQUEST, e)),
            _ => Err((StatusCode::BAD_REQUEST, "say what to verify: one of `deploy`, `artifact`, `commit` (<repository>@<commit>) or `change`".into())),
        }
    }

    fn words(&self) -> String {
        match self {
            Self::Deploy(id) => format!("deploy {id}"),
            Self::Artifact(digest) => format!("artifact {digest}"),
            Self::Commit { repo, sha } => format!("commit {repo}@{sha}"),
            Self::Change(key) => format!("change {key}"),
        }
    }
}

/// Walks the chain back from `target` (docs/ci.md W27-R10): each link with whether it holds or why
/// not, the first that doesn't named, and how long the walk took.
pub async fn verify(hub: &Hub, target: &Target) -> Result<Value, Fail> {
    let started = Instant::now();
    // Every statement under a checkpoint: one signed now when any entry came after the last.
    let refused = crate::trail::checkpoint(hub, true).await.err().map(|e| format!("{e:#}"));
    let under = crate::trail::checkpoints(&hub.store, 1).await.map_err(internal)?.into_iter().next();
    let revocations = crate::identities::revocations(&hub.store).await.map_err(internal)?;
    let mut w = Walk { hub, org: hub.pki.organization(), revocations, under, refused, links: Vec::new(), lands: HashMap::new(), held_lands: HashMap::new(), policies: HashMap::new(), walked: BTreeSet::new() };
    match target {
        Target::Deploy(id) => Box::pin(w.deploy(id)).await?,
        Target::Artifact(digest) => {
            if let Some(built) = w.artifact(digest, digest).await.map_err(internal)? {
                Box::pin(w.commit(&built.repo, &built.path, &built.trunk, &built.commit)).await.map_err(internal)?;
            }
        }
        Target::Commit { repo, sha } => Box::pin(w.commit_of(repo, sha)).await?,
        Target::Change(key) => Box::pin(w.change(key)).await?,
    }
    let root = crate::util::sha256_hex(&hub.pki.ca_der);
    w.links.push(Link::holds(format!("kiln's root, {} (sha256 {}…): every signature above verifies to it", w.org, root.get(..16).unwrap_or(&root))));
    let broken = verify::first_broken(&w.links).map(|l| json!({ "link": l.words, "why": l.holds.as_ref().err() }));
    let links: Vec<Value> = w.links.iter().map(|l| json!({ "link": l.words, "holds": l.holds.is_ok(), "why": l.holds.as_ref().err() })).collect();
    let checkpoint = match (&w.under, &w.refused) {
        (Some(k), None) => json!({ "size": k.size, "signed_at": k.signed_at }),
        (k, why) => json!({ "size": k.as_ref().map(|k| k.size), "refused": why }),
    };
    Ok(json!({
        "target": target.words(),
        "verified": broken.is_none(),
        "broken": broken,
        "links": links,
        "checkpoint": checkpoint,
        "took_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

/// A trunk's lands, as their kept statements say, to find the one a commit or a diff landed by
/// (the trail holds each to what was signed).
#[derive(Default)]
struct Lands {
    /// Each commit a land put on the trunk (its candidate, each diff's commit): the land's digest.
    by_commit: HashMap<String, String>,
    /// Each diff landed: the commit that carries it on the trunk.
    by_diff: HashMap<String, String>,
    /// Where the trunk was when its first land began: kiln's queue landed none of what came before.
    first_base: Option<String>,
}

/// Where an artifact was built from: a repository kiln hosts, kept at `path`, its trunk and the
/// commit.
struct Built {
    repo: Repo,
    path: String,
    trunk: String,
    commit: String,
}

struct Walk<'h> {
    hub: &'h Hub,
    org: String,
    revocations: Vec<Revocation>,
    /// The newest checkpoint; and why kiln's server signs none now, when it doesn't.
    under: Option<crate::trail::Kept>,
    refused: Option<String>,
    links: Vec<Link>,
    /// Each (repository's id, trunk)'s lands, read once a walk.
    lands: HashMap<(String, String), Lands>,
    /// Each land held, by its digest, and each trunk's policy, by where a land began: a batch's
    /// commits share both.
    held_lands: HashMap<String, Result<Land, String>>,
    policies: HashMap<String, Policy>,
    /// The commits walked already.
    walked: BTreeSet<String>,
}

impl Walk<'_> {
    fn against(&self) -> Against<'_> {
        Against { root: &self.hub.pki.ca_der, revocations: &self.revocations, org: &self.org }
    }

    /// Statement `digest` as the trail holds it, under the newest checkpoint: what it says, and each
    /// signature kept beside it with when kiln took it; or why the trail doesn't hold it.
    async fn holding(&self, digest: &str) -> Result<(Held, Vec<(Value, i64)>), String> {
        let size = self.under.as_ref().map_or(0, |k| k.size);
        let h = crate::trail::holding(&self.hub.store, digest, size).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("the trail has no {}", short_digest(digest)))?;
        let (Some(k), Some(proof)) = (&self.under, h.proof) else {
            if h.entry.index < size {
                return Err(format!("the trail's tiles up to {size} aren't all kept: entry {} has no proof", h.entry.index));
            }
            let why = self.refused.as_deref().map_or_else(String::new, |r| format!(" (it signs none now: {r})"));
            return Err(format!("the trail has it at entry {}, under no checkpoint kiln's server signed{why}", h.entry.index));
        };
        let taken_at = h.entry.taken_at;
        let proven = Proven { entry: h.entry, proof, note: k.note.clone(), certificate: k.certificate.clone(), signed_at: k.signed_at };
        Ok((Held { payload_type: h.payload_type, payload: h.payload, bundle: None, taken_at, proven: Some(proven) }, h.signatures))
    }

    /// Statement `digest` held, and each of its signatures that holds: their signers, or why not.
    async fn signed(&self, digest: &str) -> Result<(Held, Vec<Identity>), String> {
        let (held, signatures) = self.holding(digest).await?;
        let signers = self.signers(&held, signatures)?;
        Ok((held, signers))
    }

    /// `held`'s statement in the trail, and each of `signatures` that holds as of when kiln took it
    /// (its entry's time or the signature's, the later): their signers, or the first why none does.
    fn signers(&self, held: &Held, signatures: Vec<(Value, i64)>) -> Result<Vec<Identity>, String> {
        let a = self.against();
        if signatures.is_empty() {
            verify::held(held, &a)?;
            return Err("it carries no signature".into());
        }
        let (mut signers, mut why) = (Vec::new(), None);
        for (bundle, at) in signatures {
            match verify::held(&Held { bundle: Some(bundle), taken_at: held.taken_at.max(at), ..held.clone() }, &a) {
                Ok(Some(id)) => signers.push(id),
                Ok(None) => {}
                Err(e) => {
                    why.get_or_insert(e);
                }
            }
        }
        if signers.is_empty() {
            return Err(why.unwrap_or_else(|| "it carries no signature".into()));
        }
        Ok(signers)
    }

    /// kiln's own statement `digest` held, the checkpoint over it vouching for it: what it says.
    async fn own(&self, digest: &str) -> Result<Statement, String> {
        let (held, _) = self.holding(digest).await?;
        verify::held(&held, &self.against())?;
        statement_of(&held.payload)
    }

    /// Who signed commit `sha`, in words, as kiln's own statements of what it checked say (a push's
    /// check of it, or kiln making it); or why it doesn't hold (the newest's).
    async fn commit_signed(&self, sha: &str) -> Result<String, String> {
        let digests = crate::trail::statements_about(&self.hub.store, &format!("gitCommit:{sha}"), trail::PAYLOAD_CHECKED).await.map_err(|e| format!("{e:#}"))?;
        let mut why = None;
        for d in digests.iter().rev() {
            let said = match self.own(d).await {
                Ok(st) if matches!(st.body.get("check").and_then(Value::as_str), Some("signature" | "made")) => verify::signed_commit(&st.body),
                Ok(_) => continue,
                Err(e) => Err(e),
            };
            match said {
                Ok(words) => return Ok(words),
                Err(e) => {
                    why.get_or_insert(e);
                }
            }
        }
        Err(why.unwrap_or_else(|| "kiln has no word on its signature: no push kiln checked brought it".into()))
    }

    /// `repo`'s lands on `trunk`.
    async fn lands(&mut self, repo: &Repo, trunk: &str) -> anyhow::Result<&Lands> {
        let key = (repo.id.clone(), trunk.to_owned());
        if !self.lands.contains_key(&key) {
            let (id, t) = key.clone();
            let rows: Vec<(String, String)> = self
                .hub
                .store
                .call(move |c| Ok(c.prepare("SELECT digest, bundle FROM land_signatures WHERE repo_id = ?1 AND trunk = ?2 ORDER BY signed_at, rowid")?.query_map(params![id, t], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?))
                .await?;
            let mut lands = Lands::default();
            for (digest, bundle) in rows {
                let Some(land) = land_of(&bundle) else { continue };
                lands.first_base.get_or_insert_with(|| land.base.to_ascii_lowercase());
                for b in &land.batch {
                    lands.by_commit.insert(b.commit.to_ascii_lowercase(), digest.clone());
                    lands.by_diff.insert(b.diff.clone(), b.commit.to_ascii_lowercase());
                }
                lands.by_commit.insert(land.candidate.to_ascii_lowercase(), digest);
            }
            self.lands.insert(key.clone(), lands);
        }
        self.lands.get(&key).ok_or_else(|| anyhow::anyhow!("{}'s lands on {trunk} weren't read", repo.name))
    }

    /// Land `digest`, held and signed as kiln's server (whose queue it is): what it says.
    async fn land(&mut self, digest: &str) -> Result<Land, String> {
        if let Some(land) = self.held_lands.get(digest) {
            return land.clone();
        }
        let land = self.land_held(digest).await;
        self.held_lands.insert(digest.to_owned(), land.clone());
        land
    }

    async fn land_held(&self, digest: &str) -> Result<Land, String> {
        let (held, signers) = self.signed(digest).await?;
        if !signers.iter().any(|s| matches!(s.actor, Actor::Server) && s.org == self.org) {
            return Err(format!("it's signed by {}, not kiln's server, whose queue it is", names(&signers)));
        }
        landing::read(&held.payload_type, &statement_of(&held.payload)?)
    }

    /// Commit `sha` of `repo` (kept at `path`) on `trunk`: on it, and signed; the land that put it
    /// there; for each diff it carries, the verdicts that let it land, the trunk's policy, and its
    /// revisions' authors.
    async fn commit(&mut self, repo: &Repo, path: &str, trunk: &str, sha: &str) -> anyhow::Result<()> {
        if !self.walked.insert(sha.to_owned()) {
            return Ok(());
        }
        let on = git(self.hub, path, &["merge-base", "--is-ancestor", sha, &format!("refs/heads/{trunk}")]).await?.ok;
        let words = format!("{} on {trunk}", short(sha));
        let signed = self.commit_signed(sha).await;
        self.links.push(match (on, signed) {
            (false, _) => Link::broken(words, format!("it isn't on {trunk}")),
            (true, Ok(by)) => Link::holds(format!("{words}, {by}")),
            (true, Err(why)) => Link::broken(words, why),
        });
        let words = format!("{} landed by {trunk}'s queue", short(sha));
        let Some(digest) = self.lands(repo, trunk).await?.by_commit.get(sha).cloned() else {
            self.links.push(Link::broken(words, format!("no land of {trunk}'s queue names it: it wasn't tested as it landed")));
            return Ok(());
        };
        let land = match self.land(&digest).await {
            Ok(land) => land,
            Err(why) => {
                self.links.push(Link::broken(words, format!("its land: {why}")));
                return Ok(());
            }
        };
        self.links.push(match verify::landed(sha, &land) {
            Ok(said) => Link::holds(format!("{} {said}", short(sha))),
            Err(why) => Link::broken(words, why),
        });
        // The diffs it carries: its own in the batch; the whole batch for a candidate that's none's.
        let mut carried: Vec<&Landed> = land.batch.iter().filter(|l| l.commit.eq_ignore_ascii_case(sha)).collect();
        if carried.is_empty() && land.candidate.eq_ignore_ascii_case(sha) {
            carried = land.batch.iter().collect();
        }
        if carried.is_empty() {
            self.links.push(Link::broken(format!("{}'s reviews", short(sha)), "its land names no diff it carries"));
        }
        let policy = match self.policies.get(&land.base) {
            Some(p) => p.clone(),
            None => {
                let p = policy_at(self.hub, path, &land.base).await;
                self.policies.insert(land.base.clone(), p.clone());
                p
            }
        };
        for l in carried {
            Box::pin(self.landed_diff(path, trunk, l, &policy)).await?;
        }
        Ok(())
    }

    /// Diff `l` as it landed: its custody where it changed hands; each verdict its land names,
    /// holding on the version that landed; the trunk's policy met by them; and each revision they
    /// were given on, and the one that landed, signed by whoever made it.
    async fn landed_diff(&mut self, path: &str, trunk: &str, l: &Landed, policy: &Policy) -> anyhow::Result<()> {
        let Some(d) = diffs::find(self.hub, &l.diff).await? else {
            self.links.push(Link::broken(format!("{} v{}", l.diff, l.version), "kiln has no such diff"));
            return Ok(());
        };
        self.custody(&d).await?;
        let versions = diffs::versions(self.hub, &d).await?;
        let Some(landed) = versions.iter().find(|v| v.n == l.version) else {
            self.links.push(Link::broken(format!("{} v{}", d.name, l.version), format!("v{} isn't one of {}'s versions", l.version, d.name)));
            return Ok(());
        };
        let patch = crate::ci::verdicts::verbatim(self.hub, path, &landed.commit).await;
        let kept: Vec<Kept> = versions.iter().map(|v| Kept { n: v.n, commit: v.commit.clone(), patch_id: (v.n == l.version).then(|| patch.clone()).flatten() }).collect();
        let author = kiln_ci::land::principal(d.author_for.as_deref().unwrap_or(&d.author)).to_ascii_lowercase();
        let (mut approvers, mut revisions) = (BTreeSet::new(), BTreeSet::from([l.version]));
        for digest in &l.verdicts {
            match self.verdict(digest, &kept, l.version).await {
                Ok((words, reviewer, given)) => {
                    self.links.push(Link::holds(words));
                    revisions.insert(given);
                    if let Some((who, person, agent)) = reviewer.filter(|(_, person, agent)| *person != author && (policy.land.agents_can_approve || !agent)) {
                        approvers.insert((who, person, agent));
                    }
                }
                Err((words, why)) => self.links.push(Link::broken(words, why)),
            }
        }
        let (asks, have) = (usize::try_from(policy.land.approvals).unwrap_or(usize::MAX), approvers.len());
        let words = format!("{} met {trunk}'s review policy", d.name);
        let whose = if policy.land.agents_can_approve { "people's or agents'" } else { "people's" };
        self.links.push(if have >= asks {
            Link::holds(format!("{words}: {have} of the {asks} approvals it asks ({whose}, none its author's)"))
        } else {
            Link::broken(words, format!("it asks for {asks} approvals ({whose}, none its author's): {have} hold"))
        });
        for n in revisions {
            let Some(v) = versions.iter().find(|v| v.n == n) else { continue };
            let words = format!("{} v{n} ({})", d.name, short(&v.commit));
            self.links.push(match self.commit_signed(&v.commit).await {
                Ok(by) => Link::holds(format!("{words}, {by}")),
                Err(why) => Link::broken(words, why),
            });
        }
        Ok(())
    }

    /// Verdict `digest`, as a land names it: held, signed by whom it says gave it, an accept, holding
    /// on version `n` of its diff (`kept`). Its words; who gave it as the trunk's policy counts
    /// reviewers (them, their person, and whether an agent); and the version it was given on. Or
    /// the link, and why not.
    #[allow(clippy::type_complexity)]
    async fn verdict(&self, digest: &str, kept: &[Kept], n: u32) -> Result<(String, Option<(String, String, bool)>, u32), (String, String)> {
        let unknown = || format!("verdict {}", short_digest(digest));
        let (held, signatures) = self.holding(digest).await.map_err(|why| (unknown(), why))?;
        let body = statement_of(&held.payload).and_then(|st| verdict::read(&held.payload_type, &st)).map_err(|why| (unknown(), why))?;
        // Named by whom it says gave it, until its signature says who did.
        let words = format!("approved by {}", Identity::parse(&body.by).map_or_else(|_| body.by.clone(), |id| verify::who(&id)));
        let at = format!("{} v{}", body.revision.diff, body.revision.version);
        let signers = self.signers(&held, signatures).map_err(|why| (words.clone(), format!("its verdict on {at}: {why}")))?;
        let Some(signer) = signers.iter().find(|s| s.uri().is_ok_and(|u| u == body.by)) else {
            return Err((words, format!("its verdict on {at} is signed by {}, not whom it names", names(&signers))));
        };
        if body.kind != "verdict" || body.action != "accept" {
            return Err((words, format!("its {} on {at} is {}, not an accept", body.kind, body.action)));
        }
        let said = verify::approved(&body, signer, kept, n).map_err(|why| (words, why))?;
        Ok((said, reviewer(signer), body.revision.version))
    }

    /// `d`'s custody where it changed hands (W27-R08): the chain from the newest record of it, by
    /// its change's identity.
    async fn custody(&mut self, d: &Diff) -> anyhow::Result<()> {
        let Some(name) = d.change.as_deref() else { return Ok(()) };
        let Some(head) = crate::custody::head_named(&self.hub.store, name).await? else { return Ok(()) };
        let Some(chain) = crate::custody::chain(&self.hub.store, &self.hub.pki.ca_der, &self.org, &head, crate::util::now_unix()).await? else { return Ok(()) };
        let words = format!("{}'s custody", d.name);
        self.links.push(if chain.get("unbroken").and_then(Value::as_bool) == Some(true) {
            let hops = chain.get("hops").and_then(Value::as_array).map_or(0, Vec::len);
            let by = chain.get("held_by").and_then(Value::as_str).map_or_else(String::new, |h| format!(", held by {h}"));
            Link::holds(format!("{words}: {hops} hops, each signed by both sides{by}"))
        } else {
            Link::broken(words, chain.pointer("/break/detail").and_then(Value::as_str).unwrap_or("its chain breaks").to_owned())
        });
        Ok(())
    }

    /// Artifact `digest` (`named`, in words): the provenance that names it, signed as the leg that
    /// built it; and where it was built from, when that's a repository kiln hosts.
    async fn artifact(&mut self, digest: &str, named: &str) -> anyhow::Result<Option<Built>> {
        let words = format!("artifact {named}");
        let Some(statement) = provenance_of(self.hub, digest).await? else {
            self.links.push(Link::broken(words, format!("kiln has no provenance of {}: its CI didn't build it, or it was rebuilt since", short_digest(digest))));
            return Ok(None);
        };
        let (held, signers) = match self.signed(&statement).await {
            Ok(s) => s,
            Err(why) => {
                self.links.push(Link::broken(words, format!("its provenance: {why}")));
                return Ok(None);
            }
        };
        let built = match verify::artifact(digest, &held.payload) {
            Ok(b) => b,
            Err(why) => {
                self.links.push(Link::broken(words, why));
                return Ok(None);
            }
        };
        self.links.push(Link::holds(format!("{words}, built from {} as its provenance says", short(&built.commit))));
        self.links.push(match signers.iter().find_map(|s| verify::built_by(s).ok()) {
            Some(leg) => Link::holds(leg),
            None => Link::broken(format!("{named} built by its leg"), format!("its provenance is signed by {} alone, not the leg that built it", names(&signers))),
        });
        let repos = self.hub.store.repos().await?;
        let Some(repo) = repo_of(repos, &built.source) else {
            self.links.push(Link::broken(format!("{} on its trunk", short(&built.commit)), format!("it's of {}, a repository kiln doesn't host", built.source)));
            return Ok(None);
        };
        let Some(path) = crate::ci::hosting::native_path(self.hub, &repo) else {
            self.links.push(Link::broken(format!("{} on its trunk", short(&built.commit)), format!("{} isn't hosted here any more", repo.name)));
            return Ok(None);
        };
        let trunk = repo.default_branch.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into());
        Ok(Some(Built { path: path.to_string_lossy().into_owned(), trunk, commit: built.commit, repo }))
    }

    /// The commit the provenance of artifact `digest` says it was built from, once it holds: where
    /// the commits a later release brings start.
    async fn built_from(&self, digest: &str) -> Option<String> {
        let statement = provenance_of(self.hub, digest).await.ok()??;
        let (held, _) = self.signed(&statement).await.ok()?;
        verify::artifact(digest, &held.payload).ok().map(|b| b.commit)
    }

    /// The commits on `trunk` that `sha` brings, the newest first: since `since` (the commit the
    /// release before ran) where it's under `sha`, else since the trunk's first land; `sha` alone
    /// when the trunk has had none. At most [`COMMITS_MAX`], saying so past that.
    async fn brought(&mut self, repo: &Repo, path: &str, trunk: &str, sha: &str, since: Option<String>) -> anyhow::Result<Vec<String>> {
        let since = match since {
            Some(s) if git(self.hub, path, &["merge-base", "--is-ancestor", &s, sha]).await?.ok => Some(s),
            _ => self.lands(repo, trunk).await?.first_base.clone(),
        };
        let Some(since) = since else { return Ok(vec![sha.to_owned()]) };
        let (range, most) = (format!("{since}..{sha}"), format!("--max-count={}", COMMITS_MAX.saturating_add(1)));
        let out = git(self.hub, path, &["rev-list", "--first-parent", &most, &range]).await?;
        anyhow::ensure!(out.ok, "git rev-list {range}: {}", String::from_utf8_lossy(&out.err).trim());
        let mut list: Vec<String> = String::from_utf8_lossy(&out.out).lines().map(str::to_owned).collect();
        if list.len() > COMMITS_MAX {
            list.truncate(COMMITS_MAX);
            self.links.push(Link::broken(format!("the commits {} brings", short(sha)), format!("more than {COMMITS_MAX} since {}: kiln verify commit walks the rest one at a time", short(&since))));
        }
        if list.is_empty() {
            list.push(sha.to_owned());
        }
        Ok(list)
    }

    /// Release `id`: the release, as kiln's deploy records have it; each artifact it runs; and the
    /// commits each brings since the release before it.
    async fn deploy(&mut self, id: &str) -> Result<(), Fail> {
        let r = release_of(self.hub, id).await?;
        let words = format!("deploy {}/{}@{}", r.environment, r.app, r.version.clone().unwrap_or_else(|| r.id.to_string()));
        if r.artifacts.is_empty() {
            self.links.push(Link::broken(words, "it names no artifact: kiln has nothing it runs to walk back from"));
            return Ok(());
        }
        // Deploys sign with Ship's plan (W27-R07): until then, the release is kiln's record of it.
        self.links.push(Link::holds(format!("{words} (release {}), as kiln's deploy records have it", r.id)));
        let before = previous(self.hub, &r).await.map_err(internal)?;
        for (name, a) in &r.artifacts {
            if a.digest.is_empty() {
                self.links.push(Link::broken(format!("artifact {name}"), format!("it's deployed by its tag ({}:{}), not its digest", a.repository, a.tag)));
                continue;
            }
            let Some(built) = self.artifact(&a.digest, &format!("{name} {}", short_digest(&a.digest))).await.map_err(internal)? else { continue };
            let since = match before.as_ref().and_then(|b| b.artifacts.get(name)).filter(|p| !p.digest.is_empty()) {
                Some(p) => self.built_from(&p.digest).await,
                None => None,
            };
            for c in self.brought(&built.repo, &built.path, &built.trunk, &built.commit, since).await.map_err(internal)? {
                Box::pin(self.commit(&built.repo, &built.path, &built.trunk, &c)).await.map_err(internal)?;
            }
        }
        Ok(())
    }

    /// Commit `sha` (whole or abbreviated) of repository `name`, on its trunk.
    async fn commit_of(&mut self, name: &str, sha: &str) -> Result<(), Fail> {
        let repos = self.hub.store.repos().await.map_err(internal)?;
        let repo = repos.into_iter().find(|r| r.name == name || r.id == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository {name}")))?;
        let path = crate::ci::hosting::native_path(self.hub, &repo).ok_or_else(|| (StatusCode::CONFLICT, format!("{name} isn't hosted here")))?.to_string_lossy().into_owned();
        let spec = format!("{sha}^{{commit}}");
        let found = git(self.hub, &path, &["rev-parse", "--verify", "--quiet", &spec]).await.map_err(internal)?;
        let full = String::from_utf8_lossy(&found.out).trim().to_ascii_lowercase();
        if !found.ok || full.is_empty() {
            return Err((StatusCode::NOT_FOUND, format!("{name} has no commit {sha}")));
        }
        let trunk = repo.default_branch.clone().filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into());
        Box::pin(self.commit(&repo, &path, &trunk, &full)).await.map_err(internal)
    }

    /// Change `key`: the commit its land put on its trunk, as a commit, after its custody.
    async fn change(&mut self, key: &str) -> Result<(), Fail> {
        let d = diffs::find(self.hub, key).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no change {key}")))?;
        let (repo, path) = diffs::home(self.hub, &d).await?;
        let landed = self.lands(&repo, &d.trunk).await.map_err(internal)?.by_diff.get(&d.name).cloned();
        match landed {
            Some(commit) => Box::pin(self.commit(&repo, &path, &d.trunk, &commit)).await.map_err(internal)?,
            None => {
                self.custody(&d).await.map_err(internal)?;
                self.links.push(Link::broken(format!("{} landed", d.name), format!("no land of {}'s queue names it: it hasn't landed", d.trunk)));
            }
        }
        Ok(())
    }
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn statement_of(payload: &[u8]) -> Result<Statement, String> {
    serde_json::from_slice(payload).map_err(|_| "its statement doesn't read".to_owned())
}

/// What a land's kept bundle says, read to find it by (the trail holds it to what was signed).
fn land_of(bundle: &str) -> Option<Land> {
    let v: Value = serde_json::from_str(bundle).ok()?;
    let payload = B64.decode(v.pointer("/dsseEnvelope/payload")?.as_str()?).ok()?;
    landing::read(landing::PAYLOAD_LAND, &serde_json::from_slice(&payload).ok()?).ok()
}

/// The digest of the statement the newest provenance kiln keeps of artifact `digest` signs.
async fn provenance_of(hub: &Hub, digest: &str) -> anyhow::Result<Option<String>> {
    let (d, kind) = (digest.to_ascii_lowercase(), kiln_ci::attest::PROVENANCE.to_owned());
    let bundle: Option<String> = hub
        .store
        .call(move |c| {
            let sql = "SELECT a.bundle FROM ci_attestations a JOIN ci_attestation_subjects s ON s.attestation_id = a.id WHERE s.digest = ?1 AND a.predicate_type = ?2 ORDER BY a.id DESC LIMIT 1";
            Ok(c.query_row(sql, params![d, kind], |r| r.get(0)).optional()?)
        })
        .await?;
    Ok(bundle.and_then(|b| {
        let v: Value = serde_json::from_str(&b).ok()?;
        let payload = B64.decode(v.pointer("/dsseEnvelope/payload")?.as_str()?).ok()?;
        Some(statement::digest(v.pointer("/dsseEnvelope/payloadType")?.as_str()?, &payload))
    }))
}

/// The repository kiln hosts that provenance source `source` (`git+<url>/<repo>@<ref>`) names.
fn repo_of(repos: Vec<Repo>, source: &str) -> Option<Repo> {
    let url = source.strip_prefix("git+").unwrap_or(source);
    let url = url.rsplit_once('@').map_or(url, |(u, _)| u);
    let url = url.strip_suffix(".git").unwrap_or(url);
    repos.into_iter().filter(|r| url.ends_with(&format!("/{}", r.name))).max_by_key(|r| r.name.len())
}

/// Release `id`: `<environment>/<app>@<version>` (or that app's release by number), or a number.
async fn release_of(hub: &Hub, id: &str) -> Result<Release, Fail> {
    let missing = || (StatusCode::NOT_FOUND, format!("no release {id}"));
    let Some((place, version)) = id.rsplit_once('@') else {
        let n: i64 = id.parse().map_err(|_| (StatusCode::BAD_REQUEST, format!("{id} isn't a release's number or <environment>/<app>@<version>")))?;
        return crate::deploy::store::release(&*hub.records, n).await.map_err(internal)?.ok_or_else(missing);
    };
    let (environment, app) = place.split_once('/').ok_or_else(|| (StatusCode::BAD_REQUEST, format!("{id} isn't <environment>/<app>@<version>")))?;
    let q = ReleaseQuery { app: Some(app.to_owned()), environment: Some(environment.to_owned()), version: Some(version.to_owned()), limit: 1, ..ReleaseQuery::default() };
    if let Some(r) = crate::deploy::store::releases(&*hub.records, q).await.map_err(internal)?.into_iter().next() {
        return Ok(r);
    }
    let by_number = match version.parse::<i64>() {
        Ok(n) => crate::deploy::store::release(&*hub.records, n).await.map_err(internal)?,
        Err(_) => None,
    };
    by_number.filter(|r| r.app == app && r.environment == environment).ok_or_else(missing)
}

/// The release of `r`'s app in its environment that ran before it: the newest earlier one that
/// went out.
async fn previous(hub: &Hub, r: &Release) -> anyhow::Result<Option<Release>> {
    let q = ReleaseQuery { app: Some(r.app.clone()), environment: Some(r.environment.clone()), limit: 1000, ..ReleaseQuery::default() };
    Ok(crate::deploy::store::releases(&*hub.records, q).await?.into_iter().find(|p| p.id < r.id && matches!(p.state, State::Succeeded | State::Superseded | State::RolledBack)))
}

/// The trunk's review policy as it stood at `base`, where a land began: its `.kiln/review.yaml`
/// there, else the defaults, as the queue read it.
async fn policy_at(hub: &Hub, path: &str, base: &str) -> Policy {
    let spec = format!("{base}:.kiln/review.yaml");
    let text = match git(hub, path, &["show", &spec]).await {
        Ok(r) if r.ok => String::from_utf8_lossy(&r.out).into_owned(),
        _ => String::new(),
    };
    Policy::parse(&text).unwrap_or_default()
}

async fn git(hub: &Hub, path: &str, args: &[&str]) -> anyhow::Result<diffs::Ran> {
    let mut all = vec!["-C", path];
    all.extend_from_slice(args);
    diffs::git_run(hub, &all, &[], Vec::new()).await
}

/// Who `id` is as a trunk's policy counts reviewers: who they are, the person they are or act for
/// (`user:kim`), and whether an agent. None for what isn't a reviewer.
fn reviewer(id: &Identity) -> Option<(String, String, bool)> {
    match &id.actor {
        Actor::Person { name } => Some((verify::who(id), kiln_ci::land::principal(name).to_ascii_lowercase(), false)),
        Actor::Agent { person, .. } => Some((verify::who(id), kiln_ci::land::principal(person).to_ascii_lowercase(), true)),
        _ => None,
    }
}

fn names(ids: &[Identity]) -> String {
    ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(" and ")
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn short_digest(digest: &str) -> String {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("sha256:{}…", hex.get(..12).unwrap_or(hex))
}

/// `kiln verify`: the chain back from a deploy, an artifact, a commit or a change, each link with
/// whether it holds, the first that doesn't named (docs/ci.md W27-R10), over the API.
#[derive(clap::Args)]
pub struct VerifyArgs {
    /// What to walk back from: `deploy`, `artifact`, `commit` or `change`.
    kind: String,
    /// A release (its number, or `<environment>/<app>@<version>`), an artifact's digest
    /// (`sha256:<hex>`), `<repository>@<commit>`, or a diff (`D12`).
    id: String,
    #[arg(long, env = "KILN_URL")]
    url: Option<String>,
    /// The admin token, or a personal access token with `audit.read`.
    #[arg(long, env = "KILN_ADMIN_TOKEN", hide_env_values = true, allow_hyphen_values = true)]
    token: Option<String>,
    /// The whole answer, as JSON.
    #[arg(long)]
    json: bool,
}

/// `kiln verify <kind> <id>`: each link on a line; an error naming the first that doesn't hold.
pub async fn cli(args: VerifyArgs, cfg: &crate::config::Config) -> anyhow::Result<()> {
    use crate::ci::cli::{Api, enc};
    use crate::http::client::Method;
    Target::parse(&args.kind, &args.id).map_err(anyhow::Error::msg)?;
    let api = Api::connect(cfg, args.url.as_deref(), args.token.as_deref()).await?;
    let v = api.call_api(Method::Get, &format!("/api/verify?{}={}", args.kind, enc(&args.id)), None).await?;
    let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for l in v.get("links").and_then(Value::as_array).into_iter().flatten() {
            match l.get("why").and_then(Value::as_str) {
                Some(why) => println!("✗ {}: {why}", text(l, "link")),
                None => println!("→ {}", text(l, "link")),
            }
        }
    }
    let took = v.get("took_ms").and_then(Value::as_u64).unwrap_or_default();
    match v.get("broken").filter(|b| !b.is_null()) {
        Some(b) => anyhow::bail!("the chain breaks at {}: {} ({took} ms)", text(b, "link"), text(b, "why")),
        None => {
            if !args.json {
                println!("verified ({took} ms)");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W27-R10: what `kiln verify` takes, as the CLI, the API and the operation read it: one target,
    /// an artifact by its digest, a commit as `<repository>@<commit>`.
    #[test]
    fn w27_r10_a_target_reads_as_kiln_verify_takes_it() {
        let hex = "AB".repeat(32);
        assert_eq!(Target::parse("artifact", &hex), Ok(Target::Artifact(format!("sha256:{}", hex.to_ascii_lowercase()))));
        assert!(Target::parse("artifact", "sha256:abc").is_err(), "a digest that isn't whole");
        assert_eq!(Target::parse("commit", "acme/app@1A2B3C4"), Ok(Target::Commit { repo: "acme/app".into(), sha: "1a2b3c4".into() }));
        assert!(Target::parse("commit", "acme/app").is_err() && Target::parse("commit", "@1a2b3c4").is_err() && Target::parse("commit", "acme/app@main").is_err());
        assert_eq!(Target::parse("deploy", " prod/api@812 "), Ok(Target::Deploy("prod/api@812".into())));
        assert!(Target::parse("release", "1").is_err() && Target::parse("change", " ").is_err());
        let q = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<HashMap<_, _>>();
        assert_eq!(Target::of(&q(&[("change", "D12"), ("limit", "3")])), Ok(Target::Change("D12".into())));
        assert!(Target::of(&q(&[])).is_err_and(|(s, _)| s == StatusCode::BAD_REQUEST), "nothing to verify");
        assert!(Target::of(&q(&[("change", "D12"), ("deploy", "7")])).is_err(), "two at once");
    }

    #[test]
    fn w27_r10_a_provenance_source_names_the_repository_kiln_hosts() {
        let repo = |name: &str| Repo {
            id: name.into(),
            name: name.into(),
            clone_url: String::new(),
            branches: Vec::new(),
            poll_seconds: 0,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "native".into(),
            default_branch: None,
            mirror: None,
        };
        let repos = vec![repo("acme/app"), repo("app"), repo("acme/app-web")];
        let of = |source: &str| repo_of(repos.clone(), source).map(|r| r.name);
        assert_eq!(of("git+https://kiln.home/acme/app@refs/heads/main"), Some("acme/app".into()), "the longest name that ends it");
        assert_eq!(of("git+https://kiln.home/acme/app.git@refs/heads/main"), Some("acme/app".into()));
        assert_eq!(of("git+https://github.com/other/thing@v1"), None);
    }
}
