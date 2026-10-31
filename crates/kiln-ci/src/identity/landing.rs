//! A land, signed as the queue that tested it (docs/ci.md W27-R07), the pure part. When a land
//! queue moves a trunk to the candidate it tested (its compare-and-swap), kiln signs a statement of
//! the land as its server, whose queue it is. It's about the trunk at the candidate, the commit that
//! landed (`gitCommit`), and says where the trunk was (`base`); the batch's diffs, each at its
//! version with the commit that carries it on the candidate and the verdicts that let it land (by
//! their statements' digests); and the checks the queue ran on the candidate, each with its
//! conclusion and its legs' signed results (by their statements' digests). [`read`] takes one back.
//! The landed commits and the restacks after them are kiln's own, signed as kiln (W27-R03).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::statement::{Statement, Subject};

/// A land's payload type.
pub const PAYLOAD_LAND: &str = "application/vnd.kiln.land+json";

/// A diff that landed: its name, the version that landed, the commit that carries it on the
/// candidate, and the verdicts that let it land, by their statements' digests.
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
    /// Where the trunk was.
    pub base: String,
    /// The candidate tested, and landed: the trunk's head now.
    pub candidate: String,
    pub batch: Vec<Landed>,
    pub checks: Vec<Ran>,
}

impl Land {
    /// Its queue's name: the repository's trunk.
    pub fn queue(&self) -> String {
        format!("{}:{}", self.repo, self.trunk)
    }

    fn subject(&self) -> Subject {
        Subject { name: format!("{}@refs/heads/{}", self.repo, self.trunk), digest: BTreeMap::from([("gitCommit".to_owned(), self.candidate.clone())]), ..Subject::default() }
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
/// queue are its land's, its one subject the trunk at its candidate, and every commit and digest
/// is whole.
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
    if !is_sha(&land.base) || !is_sha(&land.candidate) {
        return Err("its base or candidate isn't a whole commit".into());
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
        return Err(format!("its subject isn't {} at its candidate", land.queue()));
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
            base: sha('a'),
            candidate: sha('7'),
            batch: vec![
                Landed { diff: "D41".into(), version: 5, commit: sha('4'), verdicts: vec![digest('b')] },
                Landed { diff: "D42".into(), version: 2, commit: sha('7'), verdicts: vec![digest('c')] },
            ],
            checks: vec![Ran { name: "ci/test".into(), conclusion: "success".into(), results: vec![digest('1'), digest('2')] }, Ran { name: "merge_group".into(), conclusion: "success".into(), results: Vec::new() }],
        }
    }

    /// W27-R07: D41 and D42 land on main as one batch: the land's statement names main's queue, the
    /// candidate that landed (main's new head) as its subject, each diff at its version with its
    /// verdicts, and the checks with their legs' signed results; it reads back as it was.
    #[test]
    fn w27_r07_a_lands_statement_reads_back() -> Result<(), String> {
        let land = d41_d42();
        let s = statement(&land);
        assert_eq!(s.subject.first().map(|x| (x.name.as_str(), x.digest.get("gitCommit").cloned())), Some(("hyper-light/kiln@refs/heads/main", Some(sha('7')))));
        assert_eq!(s.body.get("queue").and_then(|q| q.as_str()), Some("hyper-light/kiln:main"));
        assert_eq!(read(PAYLOAD_LAND, &s)?, land);
        Ok(())
    }

    /// W27-R07: a land statement whose subject isn't its candidate, whose queue isn't its trunk's,
    /// or whose digests or commits aren't whole isn't taken, saying why.
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
