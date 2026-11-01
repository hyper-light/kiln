//! A land, signed as the queue that tested it (docs/ci.md W27-R07), the pure part. When a land
//! queue moves a trunk (its compare-and-swap), kiln signs a statement of the land as its server,
//! whose queue it is. It's about the trunk at the commit it moved to (`to`, its `gitCommit`), and
//! says the tip it was applied onto (`from`), the candidate the queue tested (where its checks
//! ran) and that candidate's base. `to` is the candidate itself, unless the trunk moved since the
//! candidate's base only for changes it shares nothing with and the queue applied the batch again
//! onto the tip (`rebased`). It names the batch's diffs, each at its version with the commit that
//! carries it on `to` and the verdicts that let it land (by their statements' digests), and the
//! checks the queue ran on the candidate, each with its conclusion and its legs' signed results (by
//! their statements' digests). [`read`] takes one back. The landed commits and the restacks after
//! them are kiln's own, signed as kiln (W27-R03).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::statement::{Statement, Subject};

/// A land's payload type.
pub const PAYLOAD_LAND: &str = "application/vnd.kiln.land+json";

/// A diff that landed: its name, the version that landed, the commit that carries it on the
/// commit the trunk moved to, and the verdicts that let it land, by their statements' digests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landed {
    pub diff: String,
    pub version: u32,
    pub commit: String,
    pub verdicts: Vec<String>,
}

/// A check the queue ran on the candidate: its name, its conclusion, and its legs' signed results,
/// by their statements' digests (none: its legs signed none).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ran {
    pub name: String,
    pub conclusion: String,
    pub results: Vec<String>,
}

/// A land, as its statement says it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Land {
    pub repo: String,
    pub trunk: String,
    /// The tip the trunk moved from: what the batch was applied onto.
    pub from: String,
    /// The commit the trunk moved to: its head now.
    pub to: String,
    /// The candidate the queue tested, where its checks ran: `to`, unless `rebased`.
    pub candidate: String,
    /// The candidate's base.
    pub base: String,
    /// The batch applied again onto `from`, a tip that moved since `base` only for changes the
    /// candidate shares nothing with.
    #[serde(default)]
    pub rebased: bool,
    pub batch: Vec<Landed>,
    pub checks: Vec<Ran>,
    /// The host's own queue that tested and landed it, where kiln's didn't (docs/ci.md W26-R19):
    /// `merge queue` (GitHub's) or `merge train` (GitLab's). kiln's server signs what it saw land,
    /// naming the tip it went onto (`from`, `base`) where kiln's mirror has it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub via: String,
}

impl Land {
    /// Its queue's name: the repository's trunk.
    pub fn queue(&self) -> String {
        format!("{}:{}", self.repo, self.trunk)
    }

    fn subject(&self) -> Subject {
        Subject { name: format!("{}@refs/heads/{}", self.repo, self.trunk), digest: BTreeMap::from([("gitCommit".to_owned(), self.to.clone())]), ..Subject::default() }
    }
}

/// What a land's statement's body is: the land, and its kind and queue.
#[derive(Serialize, Deserialize)]
struct Body {
    kind: String,
    queue: String,
    #[serde(flatten)]
    land: Land,
}

/// The statement of `land`.
pub fn statement(land: &Land) -> Statement {
    let body = Body { kind: "land".into(), queue: land.queue(), land: land.clone() };
    Statement { subject: vec![land.subject()], ..Statement::new(serde_json::to_value(&body).unwrap_or_default()) }
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The land statement `s` (signed as `payload_type`) says, or why it isn't one: its kind and
/// queue are its land's, its one subject the trunk at the commit it moved to, which is its
/// candidate unless the batch was applied again, and every commit and digest is whole.
pub fn read(payload_type: &str, s: &Statement) -> Result<Land, String> {
    if payload_type != PAYLOAD_LAND {
        return Err(format!("{payload_type} isn't a land's payload type"));
    }
    let b: Body = serde_json::from_value(s.body.clone()).map_err(|e| format!("its body isn't a land's: {e}"))?;
    if b.kind != "land" {
        return Err(format!("its body says it's a {}, not a land", b.kind));
    }
    let land = b.land;
    if b.queue != land.queue() {
        return Err(format!("it names queue {}, and lands on {}", b.queue, land.queue()));
    }
    if land.repo.is_empty() || land.trunk.is_empty() {
        return Err("it names no repository or trunk".into());
    }
    // A host's queue's land names the tip it went onto where kiln saw it, else nothing.
    let seen = |c: &String| is_sha(c) || (!land.via.is_empty() && c.is_empty());
    if !(is_sha(&land.to) && is_sha(&land.candidate) && seen(&land.from) && seen(&land.base)) {
        return Err("its from, to, candidate or base isn't a whole commit".into());
    }
    if !land.rebased && land.to != land.candidate {
        return Err(format!("it moved the trunk to {}, not the candidate it tested, {}, and wasn't applied again", land.to, land.candidate));
    }
    if land.batch.is_empty() {
        return Err("its batch is empty".into());
    }
    for d in &land.batch {
        if d.diff.is_empty() || d.version == 0 || !is_sha(&d.commit) {
            return Err(format!("{} isn't a whole diff, version and commit", d.diff));
        }
        if let Some(bad) = d.verdicts.iter().find(|v| !is_digest(v)) {
            return Err(format!("{}'s verdict {bad} isn't a statement's digest", d.diff));
        }
    }
    for c in &land.checks {
        if let Some(bad) = c.results.iter().find(|v| !is_digest(v)) {
            return Err(format!("check {}'s result {bad} isn't a statement's digest", c.name));
        }
    }
    if s.subject != [land.subject()] {
        return Err(format!("its subject isn't {} at the commit it moved to", land.queue()));
    }
    Ok(land)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn digest(c: char) -> String {
        format!("sha256:{}", c.to_string().repeat(64))
    }

    fn d41_d42() -> Land {
        Land {
            repo: "hyper-light/kiln".into(),
            trunk: "main".into(),
            from: sha('a'),
            to: sha('7'),
            candidate: sha('7'),
            base: sha('a'),
            rebased: false,
            batch: vec![
                Landed { diff: "D41".into(), version: 5, commit: sha('4'), verdicts: vec![digest('b')] },
                Landed { diff: "D42".into(), version: 2, commit: sha('7'), verdicts: vec![digest('c')] },
            ],
            checks: vec![Ran { name: "ci/test".into(), conclusion: "success".into(), results: vec![digest('1'), digest('2')] }, Ran { name: "merge_group".into(), conclusion: "success".into(), results: Vec::new() }],
            via: String::new(),
        }
    }

    /// W26-R19: a land through a host's own queue reads back naming that queue, its tip before
    /// named where kiln saw it and left out where it didn't; kiln's own lands say no `via` at all,
    /// so their statements are as they were.
    #[test]
    fn w26_r19_a_host_queues_land_reads_back() -> Result<(), String> {
        assert!(statement(&d41_d42()).body.get("via").is_none(), "kiln's queue's land names no host queue");
        let seen = Land { via: "merge queue".into(), ..d41_d42() };
        assert_eq!(read(PAYLOAD_LAND, &statement(&seen))?.via, "merge queue");
        let unseen = Land { from: String::new(), base: String::new(), ..seen.clone() };
        assert_eq!(read(PAYLOAD_LAND, &statement(&unseen))?, unseen);
        let kilns = Land { via: String::new(), ..unseen };
        assert!(read(PAYLOAD_LAND, &statement(&kilns)).is_err_and(|e| e.contains("isn't a whole commit")), "kiln's own queue always names its tip");
        Ok(())
    }

    /// W27-R07: D41 and D42 land on main as one batch: the land's statement names main's queue, the
    /// commit main moved to (the candidate it tested, main's new head) as its subject, each diff at
    /// its version with its verdicts, and the checks with their legs' signed results; it reads back
    /// as it was.
    #[test]
    fn w27_r07_a_lands_statement_reads_back() -> Result<(), String> {
        let land = d41_d42();
        let s = statement(&land);
        assert_eq!(s.subject.first().map(|x| (x.name.as_str(), x.digest.get("gitCommit").cloned())), Some(("hyper-light/kiln@refs/heads/main", Some(sha('7')))));
        assert_eq!(s.body.get("queue").and_then(|q| q.as_str()), Some("hyper-light/kiln:main"));
        assert_eq!(read(PAYLOAD_LAND, &s)?, land);
        Ok(())
    }

    /// W27-R07: a land the queue applied again onto a tip that moved since its candidate's base (for
    /// changes it shares nothing with) is about the commit the trunk moved to, and still names the
    /// candidate tested, where its checks ran; one not applied again must be its candidate.
    #[test]
    fn w27_r07_a_land_applied_again_is_about_the_commit_the_trunk_moved_to() -> Result<(), String> {
        let land = Land { from: sha('b'), to: sha('8'), rebased: true, ..d41_d42() };
        let s = statement(&land);
        assert_eq!(s.subject.first().and_then(|x| x.digest.get("gitCommit").cloned()), Some(sha('8')));
        let back = read(PAYLOAD_LAND, &s)?;
        assert_eq!((back.to.as_str(), back.candidate.as_str(), back.from.as_str(), back.rebased), (sha('8').as_str(), sha('7').as_str(), sha('b').as_str(), true));
        let unsaid = Land { rebased: false, ..land };
        assert!(read(PAYLOAD_LAND, &statement(&unsaid)).is_err_and(|e| e.contains("not the candidate it tested")));
        Ok(())
    }

    /// W27-R07: a land statement whose subject isn't the commit the trunk moved to, whose queue
    /// isn't its trunk's, or whose digests or commits aren't whole isn't taken, saying why.
    #[test]
    fn w27_r07_a_land_that_doesnt_hold_together_is_refused() {
        let s = statement(&d41_d42());
        let mut other = s.clone();
        if let Some(x) = other.subject.first_mut() {
            x.digest.insert("gitCommit".into(), sha('9'));
        }
        assert!(read(PAYLOAD_LAND, &other).is_err_and(|e| e.contains("subject")));
        let mut moved = s.clone();
        if let Some(o) = moved.body.as_object_mut() {
            o.insert("queue".into(), "hyper-light/kiln:release".into());
        }
        assert!(read(PAYLOAD_LAND, &moved).is_err_and(|e| e.contains("queue")));
        let mut torn = d41_d42();
        if let Some(c) = torn.checks.first_mut() {
            c.results.push("abc".into());
        }
        assert!(read(PAYLOAD_LAND, &statement(&torn)).is_err_and(|e| e.contains("isn't a statement's digest")));
        assert!(read("application/vnd.kiln.custody+json", &s).is_err());
    }
}
