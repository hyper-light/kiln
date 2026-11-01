//! Whether the commits a push brings to a repository kiln hosts are signed as its policy says
//! (docs/ci.md W27-R03), the pure part. What kiln found of each new commit's signature ([`Seen`],
//! checked against its root and revocations at the time of the push), who pushes ([`Pusher`]) and
//! the repository's `signatures:` ([`crate::land::Signatures`]) decide each ref: taken, refused
//! saying which commit and why, or, where signatures aren't required, taken with a warning for
//! each commit that doesn't verify. The server reads the commits and checks their signatures.

use crate::identity::chain::{Actor, Identity};
use crate::land::{SignatureMode, Signatures};
use crate::trigger::pattern::Patterns;

/// What kiln found of a commit's signature, checked at the time of the push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// It has none.
    Unsigned,
    /// It has one that doesn't verify, and why (another root, revoked, another commit's).
    Unverified(String),
    /// It verifies as this identity.
    By(Identity),
}

/// Who pushes, as far as signatures go: an agent, for the person it acts for, or anyone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pusher {
    Agent { person: String },
    Other,
}

/// A ref's judgement: refused, saying why, or taken, with a warning for each commit that doesn't
/// verify (or isn't one the ref takes) where signatures aren't required.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Judged {
    pub refused: Option<String>,
    pub warnings: Vec<String>,
}

/// The most warnings one ref says; the rest are counted.
const MOST_WARNINGS: usize = 20;

/// Whose signatures a ref no rule names takes.
const ANYONE: [&str; 3] = ["people", "agents", "kiln"];

/// The commits `commits` (each its id and what was seen of its signature) that a push brings to
/// `refname` of a repository of organization `org`, pushed by `pusher`, judged by its `policy`;
/// `required`: whether the repository requires signatures where its policy doesn't say.
pub fn judge(policy: &Signatures, required: bool, org: &str, refname: &str, pusher: &Pusher, commits: &[(String, Seen)]) -> Judged {
    let mode = policy.mode.unwrap_or(if required { SignatureMode::Required } else { SignatureMode::Warn });
    let name = rule_name(refname);
    let takes = takes(policy, &name);
    let problems: Vec<Problem> = commits.iter().filter_map(|(sha, seen)| problem(short(sha), seen, &takes, org, pusher)).collect();
    match mode {
        SignatureMode::Required => {
            let Some(first) = problems.first() else { return Judged::default() };
            let mut why = match first {
                Problem::Unsigned(sha) => format!("commit {sha} isn't signed: {name} takes commits signed by {} of {org}", words(&takes)),
                Problem::Unverified(sha, why) => format!("commit {sha}'s signature doesn't verify ({why}): {name} takes commits signed by {} of {org}", words(&takes)),
                Problem::AsItsPerson(sha, person) => format!("commit {sha} is signed as {person}, whom the agent pushing it acts for: an agent pushes the commits it signed itself"),
                Problem::NotTaken(sha, who) => format!("commit {sha} is signed by {who}: {name} takes commits signed by {} of {org}", words(&takes)),
            };
            if problems.len() > 1 {
                why.push_str(&format!(" (and {} more {})", problems.len() - 1, if problems.len() == 2 { "commit" } else { "commits" }));
            }
            Judged { refused: Some(why), warnings: Vec::new() }
        }
        SignatureMode::Warn => {
            let mut warnings: Vec<String> = problems
                .iter()
                .take(MOST_WARNINGS)
                .map(|p| match p {
                    Problem::Unsigned(sha) => format!("warning: {sha} isn't signed (.kiln/review.yaml can require signatures)"),
                    Problem::Unverified(sha, why) => format!("warning: {sha}'s signature doesn't verify: {why} (.kiln/review.yaml can require signatures)"),
                    Problem::AsItsPerson(sha, person) => format!("warning: {sha} is signed as {person}, whom the agent pushing it acts for (.kiln/review.yaml can require signatures)"),
                    Problem::NotTaken(sha, who) => format!("warning: {sha} is signed by {who}, whom {name} doesn't take (.kiln/review.yaml can require signatures)"),
                })
                .collect();
            if problems.len() > MOST_WARNINGS {
                warnings.push(format!("warning: and {} more commits like them", problems.len() - MOST_WARNINGS));
            }
            Judged { refused: None, warnings }
        }
    }
}

/// What's wrong with one commit's signature, for the ref it goes to.
enum Problem {
    Unsigned(String),
    Unverified(String, String),
    AsItsPerson(String, String),
    NotTaken(String, String),
}

fn problem(sha: String, seen: &Seen, takes: &[String], org: &str, pusher: &Pusher) -> Option<Problem> {
    let id = match seen {
        Seen::Unsigned => return Some(Problem::Unsigned(sha)),
        Seen::Unverified(why) => return Some(Problem::Unverified(sha, why.clone())),
        Seen::By(id) => id,
    };
    if id.org != org {
        return Some(Problem::NotTaken(sha, format!("{} of {}", who(id), id.org)));
    }
    // An agent's push carries what it signed itself, never what's signed as its person.
    if let (Pusher::Agent { person }, Actor::Person { name }) = (pusher, &id.actor)
        && person == name
    {
        return Some(Problem::AsItsPerson(sha, name.clone()));
    }
    (!takes.iter().any(|t| taken(t, id))).then(|| Problem::NotTaken(sha, who(id)))
}

/// Whether signer `rule` (`people`, `agents`, `kiln`, `<name>`, `<name>/agents`) takes `id`.
fn taken(rule: &str, id: &Identity) -> bool {
    match (rule.trim(), &id.actor) {
        ("people", Actor::Person { .. }) | ("agents", Actor::Agent { .. }) | ("kiln", Actor::Server | Actor::Cd) => true,
        (r, Actor::Agent { person, .. }) => r.strip_suffix("/agents").is_some_and(|p| p == person),
        (r, Actor::Person { name }) => r == name,
        _ => false,
    }
}

/// The name a rule's globs match for `refname`: a branch's own name, a review submission's trunk
/// (`refs/for/<trunk>[/branch/<b>][%options]`), a tag as `tags/<name>`.
fn rule_name(refname: &str) -> String {
    if let Some(rest) = refname.strip_prefix("refs/for/") {
        let target = rest.split_once('%').map_or(rest, |(t, _)| t);
        return target.split_once("/branch/").map_or(target, |(t, _)| t).to_owned();
    }
    if let Some(tag) = refname.strip_prefix("refs/tags/") {
        return format!("tags/{tag}");
    }
    refname.strip_prefix("refs/heads/").unwrap_or(refname).to_owned()
}

/// Whose signatures the ref named `name` takes: the first rule naming it, else anyone's.
fn takes(policy: &Signatures, name: &str) -> Vec<String> {
    policy
        .signers
        .iter()
        .find(|r| Patterns::new(&r.refs).is_ok_and(|p| p.matches(name)))
        .map_or_else(|| ANYONE.iter().map(|s| (*s).to_owned()).collect(), |r| r.by.clone())
}

/// `takes` in words: "people, agents or kiln", "ada or kim's agents".
fn words(takes: &[String]) -> String {
    let each: Vec<String> = takes.iter().map(|t| t.trim().strip_suffix("/agents").map_or_else(|| t.trim().to_owned(), |p| format!("{p}'s agents"))).collect();
    match each.split_last() {
        None => "no one".to_owned(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
    }
}

/// Who signed, in a few words.
fn who(id: &Identity) -> String {
    match &id.actor {
        Actor::Person { name } => name.clone(),
        Actor::Agent { person, agent, .. } => format!("{person}'s agent {}", agent.get(..8).unwrap_or(agent)),
        Actor::Runner { machine, .. } => format!("the runner on {machine}"),
        Actor::Leg { run, leg, .. } => format!("leg {leg} of run {run}"),
        Actor::Server => "kiln's server".to_owned(),
        Actor::Cd => "kiln's CD".to_owned(),
    }
}

/// A commit's id as git shortens it.
fn short(sha: &str) -> String {
    sha.get(..7).unwrap_or(sha).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::land::Signers;

    const ORG: &str = "hyper-light";

    fn person(name: &str) -> Seen {
        Seen::By(Identity::new(ORG, Actor::Person { name: name.into() }))
    }

    fn agent(of: &str) -> Seen {
        Seen::By(Identity::new(ORG, Actor::Agent { person: of.into(), agent: "01a18b42matrix".into(), epoch: 3, held_by: None }))
    }

    fn commits(seen: Vec<Seen>) -> Vec<(String, Seen)> {
        seen.into_iter().enumerate().map(|(i, s)| (format!("3f2a1c{i}0123456789abcdef0123456789abcdef01"), s)).collect()
    }

    fn policy(yaml: &str) -> Signatures {
        crate::land::Policy::parse(yaml).map(|p| p.signatures).unwrap_or_default()
    }

    #[test]
    fn w27_r03_a_required_ref_refuses_an_unsigned_commit_saying_which_and_why() {
        let j = judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![person("sam"), Seen::Unsigned]));
        assert_eq!(j.refused.as_deref(), Some("commit 3f2a1c1 isn't signed: main takes commits signed by people, agents or kiln of hyper-light"));
        let j = judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![Seen::Unsigned, Seen::Unsigned, Seen::Unsigned]));
        assert!(j.refused.is_some_and(|r| r.starts_with("commit 3f2a1c0 isn't signed") && r.ends_with("(and 2 more commits)")));
        // Every commit signed by someone it takes: taken, nothing said.
        let ok = judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![person("sam"), agent("sam"), Seen::By(Identity::new(ORG, Actor::Server))]));
        assert_eq!(ok, Judged::default());
        assert_eq!(judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &[]), Judged::default(), "nothing new");
    }

    #[test]
    fn w27_r03_a_repository_that_only_warns_takes_the_push_and_names_each_commit() {
        let j = judge(&Signatures::default(), false, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![Seen::Unsigned, person("sam")]));
        assert_eq!(j, Judged { refused: None, warnings: vec!["warning: 3f2a1c0 isn't signed (.kiln/review.yaml can require signatures)".into()] });
        // Its policy says which, whatever the repository was made with.
        let warn = policy("signatures:\n  mode: warn\n");
        assert!(judge(&warn, true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![Seen::Unsigned])).refused.is_none());
        let required = policy("signatures:\n  mode: required\n");
        assert!(judge(&required, false, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![Seen::Unsigned])).refused.is_some());
        let many = judge(&Signatures::default(), false, ORG, "refs/heads/main", &Pusher::Other, &commits((0..25).map(|_| Seen::Unsigned).collect()));
        assert_eq!((many.warnings.len(), many.warnings.last().map(String::as_str)), (21, Some("warning: and 5 more commits like them")));
    }

    #[test]
    fn w27_r03_a_signature_that_doesnt_verify_is_refused_saying_why() {
        let revoked = Seen::Unverified("the signer's certificate was revoked at 2026-10-30 14:02 (key compromise)".into());
        let j = judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![revoked]));
        assert_eq!(j.refused.as_deref(), Some("commit 3f2a1c0's signature doesn't verify (the signer's certificate was revoked at 2026-10-30 14:02 (key compromise)): main takes commits signed by people, agents or kiln of hyper-light"));
        let other = Seen::By(Identity::new("elsewhere", Actor::Person { name: "sam".into() }));
        assert!(judge(&Signatures::default(), true, ORG, "refs/heads/main", &Pusher::Other, &commits(vec![other])).refused.is_some_and(|r| r.contains("sam of elsewhere")));
    }

    #[test]
    fn w27_r03_each_ref_takes_the_signers_its_rule_names() {
        let p = policy("signatures:\n  signers:\n    - refs: [\"release/*\", \"tags/*\"]\n      by: [ada, kim/agents]\n    - refs: [main]\n      by: [people, kiln]\n");
        let j = judge(&p, true, ORG, "refs/heads/release/1", &Pusher::Other, &commits(vec![person("sam")]));
        assert_eq!(j.refused.as_deref(), Some("commit 3f2a1c0 is signed by sam: release/1 takes commits signed by ada or kim's agents of hyper-light"));
        assert!(judge(&p, true, ORG, "refs/heads/release/1", &Pusher::Other, &commits(vec![person("ada"), agent("kim")])).refused.is_none());
        assert!(judge(&p, true, ORG, "refs/tags/v1", &Pusher::Other, &commits(vec![agent("sam")])).refused.is_some_and(|r| r.contains("sam's agent 01a18b42")));
        // A trunk's rule is its review submissions' too; an agent's commit isn't one main takes.
        assert!(judge(&p, true, ORG, "refs/for/main%topic=x", &Pusher::Other, &commits(vec![agent("sam")])).refused.is_some_and(|r| r.contains(": main takes commits signed by people or kiln")));
        assert!(judge(&p, true, ORG, "refs/for/main/branch/topic", &Pusher::Other, &commits(vec![person("sam")])).refused.is_none());
        // A ref no rule names takes anyone of the organization.
        assert!(judge(&p, true, ORG, "refs/heads/topic", &Pusher::Other, &commits(vec![agent("sam")])).refused.is_none());
    }

    #[test]
    fn w27_r03_an_agents_push_of_commits_signed_as_its_person_is_refused() {
        let by_sam_agent = Pusher::Agent { person: "sam".into() };
        let j = judge(&Signatures::default(), true, ORG, "refs/heads/topic", &by_sam_agent, &commits(vec![agent("sam"), person("sam")]));
        assert_eq!(j.refused.as_deref(), Some("commit 3f2a1c1 is signed as sam, whom the agent pushing it acts for: an agent pushes the commits it signed itself"));
        // Its own, and another person's (a branch it builds on): taken.
        assert!(judge(&Signatures::default(), true, ORG, "refs/heads/topic", &by_sam_agent, &commits(vec![agent("sam"), person("kim")])).refused.is_none());
        // sam pushing their own: taken.
        assert!(judge(&Signatures::default(), true, ORG, "refs/heads/topic", &Pusher::Other, &commits(vec![person("sam")])).refused.is_none());
        let warned = judge(&Signatures::default(), false, ORG, "refs/heads/topic", &by_sam_agent, &commits(vec![person("sam")]));
        assert_eq!(warned.warnings, vec!["warning: 3f2a1c0 is signed as sam, whom the agent pushing it acts for (.kiln/review.yaml can require signatures)".to_owned()]);
    }

    #[test]
    fn w27_r03_signers_read_from_review_yaml() {
        let p = policy("trunks: [main]\nsignatures:\n  mode: required\n  signers:\n    - refs: [main]\n      by: [people]\n");
        assert_eq!(p, Signatures { mode: Some(SignatureMode::Required), signers: vec![Signers { refs: vec!["main".into()], by: vec!["people".into()] }] });
        assert_eq!(policy("trunks: [main]\n"), Signatures::default(), "unset: the repository's own");
        assert!(crate::land::Policy::parse("signatures:\n  mode: sometimes\n").is_err(), "a mode it doesn't know is an error, not a default");
    }
}
