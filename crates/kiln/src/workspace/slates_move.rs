//! An agent moved to another runner with its Slates volume (kiln goal 95101ef32ffb8b099942fe076619c310, K4; Slates'
//! A-291 planned move): the volume goes by Slates' own move inside kiln's Slates fleet, its head handed to the target's
//! node and served there, the old owner's copy frozen and then let go, never copied through a disk.
//!
//! - **The server's plan** ([`plan`], `harness::relocate`): both runners in the fleet (`[slates_fleet]`), the target's
//!   node serving (its member id said with its runner's word, [`super::slates_fleet::report_member`]), and no other
//!   agent working in the volume; else refused by kind ([`MoveRefused`]), the agent left where it is.
//! - **The source runner** ([`move_out`], `harness::session`'s hand-over, once the agent's turn is over and its harness
//!   is down): the workspace's mount let go, the move asked of Slates and its answers followed until the configuration
//!   names the target (`MoveProgress::Moved`), the session handed over after. A refused move mounts the workspace again
//!   and the agent goes on where it was; the refusal is said by kind ([`MoveRefused::of`]).
//! - **The target runner** (`workspace::arrived_here`, `workspace::prepare`): the moved volume mounted under its own
//!   place for workspaces, the agent's folder there, before its harness starts.
//!
//! An overlay (a workspace laid over its machine's folder, as kiln makes them today) is refused `NotPlain` by Slates: its
//! base stays on its source host. A workspace made as a clone of a captured base moves (K5).

use serde::{Deserialize, Serialize};

use super::slates::Made;
use super::slates_fleet::Members;

/// What the server asks the source runner to move with the agent: its volume, to the target runner's Slates node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMove {
    /// The volume's id (Slates' 32 hexadecimal digits).
    pub volume: String,
    /// The target's fleet member id, as its runner last said it.
    pub member: u64,
    /// The target runner's machine.
    pub to: String,
    /// The Slates user the target's kiln acts as on its node, which the source shares the volume with before the move
    /// (`slates_fleet::NodeUser`): none from a target that didn't say it, whose kiln then can't mount what moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<super::slates_fleet::NodeUser>,
}

/// Why an agent's volume doesn't move with it, by kind: kiln's plan's refusals and Slates' typed answers to the move
/// (its `Error` mapped by [`MoveRefused::of`]). The agent stays where it was, running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MoveRefused {
    /// A runner isn't in kiln's Slates fleet: `[slates_fleet]` doesn't list it.
    NotInFleet { machine: String },
    /// The server's `[slates_fleet]` can't make a fleet, in its words.
    Setting { said: String },
    /// The target's Slates hasn't said it serves as the fleet's node (it hasn't started since it joined).
    TargetNotServing { machine: String },
    /// A runner's kiln is older than the move: it can't move a volume (or take one).
    OlderRunner { machine: String },
    /// Another agent works in the volume: it stays with them.
    Shared { with: Vec<String> },
    /// Its machine is away: only the runner that holds the volume moves it.
    SourceAway { machine: String },
    /// Something still holds the volume (a mount, a lease): Slates moves it once it's let go.
    Attached,
    /// The volume is moving already.
    AlreadyMoving,
    /// It's an overlay, whose base stays on its source host: a workspace made as a clone of a captured base moves.
    NotPlain,
    /// It has clones, or is a green's origin, which depend on it where it is.
    Lineage,
    /// It has snapshots besides its head, which a move would lose.
    Snapshots,
    /// The target isn't another member of the volume's region.
    NotAMember,
    /// The target is outside its owner's neighbourhood.
    OutsideNeighbourhood,
    /// Slates couldn't place its records in time and thawed it where it was: asking again may move it.
    Unplaced,
    /// The agent is isolated: its microVM and what its harness keeps there are its machine's (A-750).
    Isolated,
    /// Anything else Slates said, in its words.
    Slates { said: String },
}

/// Every kind a move is refused by ([`MoveRefused::kind`]), as a refusal names it (`[kind: x]`): what a mover, or a test of
/// kiln's Slates fleet under faults (`super::slates_faults`), may take a refusal for.
pub const KINDS: [&str; 16] = [
    "not_in_fleet",
    "setting",
    "target_not_serving",
    "older_runner",
    "shared",
    "source_away",
    "attached",
    "already_moving",
    "not_plain",
    "lineage",
    "snapshots",
    "not_a_member",
    "outside_neighbourhood",
    "unplaced",
    "isolated",
    "slates",
];

impl MoveRefused {
    /// Slates' typed answer to a move, by kind.
    pub fn of(e: &slates_api::Error) -> MoveRefused {
        use slates_api::{Busy, Error, Invalid, MoveTarget, RetryCause, Unmovable, Unusable};
        match e {
            Error::Busy(Busy::Attached) => MoveRefused::Attached,
            Error::Busy(Busy::Moving) => MoveRefused::AlreadyMoving,
            Error::Unusable(Unusable::Unmovable(Unmovable::NotPlain)) => MoveRefused::NotPlain,
            Error::Unusable(Unusable::Unmovable(Unmovable::Lineage)) => MoveRefused::Lineage,
            Error::Unusable(Unusable::Unmovable(Unmovable::Snapshots)) => MoveRefused::Snapshots,
            Error::Invalid(Invalid::MoveTarget(MoveTarget::NotMember)) => MoveRefused::NotAMember,
            Error::Invalid(Invalid::MoveTarget(MoveTarget::OutsideNeighbourhood)) => MoveRefused::OutsideNeighbourhood,
            Error::Retry { cause: RetryCause::MoveUnplaced, .. } => MoveRefused::Unplaced,
            other => MoveRefused::Slates { said: other.to_string() },
        }
    }

    /// The refusal's kind, as its serialized tag names it: what a tool matches a refusal by.
    pub fn kind(&self) -> &'static str {
        match self {
            MoveRefused::NotInFleet { .. } => "not_in_fleet",
            MoveRefused::Setting { .. } => "setting",
            MoveRefused::TargetNotServing { .. } => "target_not_serving",
            MoveRefused::OlderRunner { .. } => "older_runner",
            MoveRefused::Shared { .. } => "shared",
            MoveRefused::SourceAway { .. } => "source_away",
            MoveRefused::Attached => "attached",
            MoveRefused::AlreadyMoving => "already_moving",
            MoveRefused::NotPlain => "not_plain",
            MoveRefused::Lineage => "lineage",
            MoveRefused::Snapshots => "snapshots",
            MoveRefused::NotAMember => "not_a_member",
            MoveRefused::OutsideNeighbourhood => "outside_neighbourhood",
            MoveRefused::Unplaced => "unplaced",
            MoveRefused::Isolated => "isolated",
            MoveRefused::Slates { .. } => "slates",
        }
    }

    /// The refusal as kiln reports it: its words, then its kind (`[kind: not_plain]`).
    pub fn reported(&self) -> String {
        format!("{} [kind: {}]", self.said(), self.kind())
    }

    /// The refusal in words, for the agent's timeline and its mover.
    pub fn said(&self) -> String {
        match self {
            MoveRefused::NotInFleet { machine } => format!("{machine} isn't in kiln's Slates fleet ([slates_fleet] doesn't list it), so its workspace can't move with it"),
            MoveRefused::Setting { said } => format!("kiln's Slates fleet can't move workspaces: {said}"),
            MoveRefused::TargetNotServing { machine } => format!("{machine}'s Slates hasn't said it serves as a node of kiln's Slates fleet yet"),
            MoveRefused::OlderRunner { machine } => format!("{machine}'s runner is older than moving a workspace volume: upgrade it"),
            MoveRefused::Shared { with } => format!("other agents work in its workspace too ({}): it stays with them", with.join(", ")),
            MoveRefused::SourceAway { machine } => format!("{machine} is away, and only the runner that holds its workspace moves it"),
            MoveRefused::Attached => "something still holds its workspace (a mount, a lease), so Slates doesn't move it".to_owned(),
            MoveRefused::AlreadyMoving => "its workspace is moving already".to_owned(),
            MoveRefused::NotPlain => "its workspace is laid over its machine's folder, which stays there: only a workspace made as a clone of a captured base moves".to_owned(),
            MoveRefused::Lineage => "its workspace has clones that depend on it where it is".to_owned(),
            MoveRefused::Snapshots => "its workspace has snapshots besides its head, which a move would lose".to_owned(),
            MoveRefused::NotAMember => "the target isn't a member of its workspace's Slates region".to_owned(),
            MoveRefused::OutsideNeighbourhood => "the target is outside its workspace's Slates neighbourhood".to_owned(),
            MoveRefused::Unplaced => "Slates couldn't place its workspace's records in time and kept it where it was: move it again".to_owned(),
            MoveRefused::Isolated => crate::sandbox::isolated::NO_MOVE.to_owned(),
            MoveRefused::Slates { said } => format!("Slates refused to move its workspace: {said}"),
        }
    }
}

/// The server's plan for moving the agent whose workspace is `made` from runner `from` to runner `to` (module docs):
/// both in `setting`'s fleet, the target's node serving as `members` says, no other agent among `sharers` working in it.
pub fn plan(setting: &crate::config::SlatesFleet, members: &Members, from: &str, to: &str, made: &Made, sharers: &[String]) -> Result<VolumeMove, MoveRefused> {
    if !sharers.is_empty() {
        return Err(MoveRefused::Shared { with: sharers.to_vec() });
    }
    for machine in [from, to] {
        match super::slates_fleet::place_of(setting, machine) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(MoveRefused::NotInFleet { machine: machine.to_owned() }),
            Err(said) => return Err(MoveRefused::Setting { said }),
        }
    }
    let target = members.members.iter().find(|m| m.machine == to);
    let member = target.and_then(|m| m.member).ok_or_else(|| MoveRefused::TargetNotServing { machine: to.to_owned() })?;
    Ok(VolumeMove { volume: made.volume.clone(), member, to: to.to_owned(), user: target.and_then(|m| m.user) })
}

/// The first pause before asking a move under way again: a move whose head is placed already switches at once.
const ASK_FIRST: std::time::Duration = std::time::Duration::from_millis(10);
/// The longest pause between two asks of a move under way, the pause doubling up to it: Slates places a moved head and
/// switches within its fleet's failover bound (10 s by default), so a slow move costs an ask a second.
const ASK_MOST: std::time::Duration = std::time::Duration::from_secs(1);

/// What the source's next step is, from Slates' answer to one ask of the move ([`move_out`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The configuration names the target: the volume moved.
    Moved,
    /// Under way (`None`: ask again after kiln's doubling pause), or refused for now with the pause Slates derived.
    Again { pause_ns: Option<u64> },
    /// Refused: the agent stays.
    Refused(MoveRefused),
}

/// The step Slates' answer `answer` to a move to member `to` makes.
pub fn step(answer: &Result<slates_api::MoveProgress, slates_api::Error>, to: u64) -> Step {
    match answer {
        Ok(slates_api::MoveProgress::Moved { to: there }) if *there == to => Step::Moved,
        Ok(slates_api::MoveProgress::Moved { .. }) => Step::Refused(MoveRefused::AlreadyMoving),
        Ok(slates_api::MoveProgress::Moving { to: there }) if *there == to => Step::Again { pause_ns: None },
        Ok(slates_api::MoveProgress::Moving { .. }) => Step::Refused(MoveRefused::AlreadyMoving),
        Err(e) => match MoveRefused::of(e) {
            MoveRefused::Slates { .. } => match e {
                slates_api::Error::Retry { pause_ns, .. } => Step::Again { pause_ns: Some(*pause_ns) },
                _ => Step::Refused(MoveRefused::of(e)),
            },
            refused => Step::Refused(refused),
        },
    }
}

/// The source runner's side (module docs): workspace `m`'s mount let go and its volume moved by Slates to `to`'s node,
/// following Slates' answers until it says the configuration names the target. Slates bounds the move itself (an
/// unplaced move thaws within its failover bound and says so), so no clock of kiln's ends it. Refused: the workspace
/// is mounted again where it was, and the refusal comes back by kind.
pub async fn move_out(m: &Made, to: &VolumeMove) -> Result<(), MoveRefused> {
    // The target's kiln is given every right on the volume first: its owner is this host's user, whom no other node's
    // kiln is (`slates_fleet::NodeUser`).
    if let Some(user) = to.user {
        super::slates::share_with(&m.volume, user).await.map_err(|e| MoveRefused::Slates { said: format!("{e:#}") })?;
    }
    super::slates::unmount_for_move(m).await.map_err(|e| MoveRefused::Slates { said: format!("{e:#}") })?;
    let mut pause = ASK_FIRST;
    let refused = loop {
        let answer = match super::slates::ask_move(&to.volume, to.member).await {
            Ok(answer) => answer,
            Err(e) => break MoveRefused::Slates { said: format!("{e:#}") },
        };
        match step(&answer, to.member) {
            Step::Moved => return Ok(()),
            Step::Again { pause_ns: Some(ns) } => super::slates::pause_for_move(std::time::Duration::from_nanos(ns)).await,
            Step::Again { pause_ns: None } => {
                super::slates::pause_for_move(pause).await;
                pause = (pause * 2).min(ASK_MOST);
            }
            Step::Refused(refused) => break refused,
        }
    };
    if let Err(e) = super::slates::mounted(m).await {
        tracing::warn!(volume = %m.volume, error = %format!("{e:#}"), "a workspace whose move was refused couldn't be mounted again");
    }
    Err(refused)
}

#[cfg(test)]
mod tests {
    /// A-580. Do: take every refusal a move can end in, one of each, and their kinds. Expect each kind in [`KINDS`], and
    /// [`KINDS`] to hold those kinds alone, each once: the list a test judges a refusal by never drifts from the kinds a
    /// refusal names.
    #[test]
    fn every_refusals_kind_is_listed_and_the_list_holds_no_other() {
        let machine = || "m".to_owned();
        let every = [
            MoveRefused::NotInFleet { machine: machine() },
            MoveRefused::Setting { said: String::new() },
            MoveRefused::TargetNotServing { machine: machine() },
            MoveRefused::OlderRunner { machine: machine() },
            MoveRefused::Shared { with: Vec::new() },
            MoveRefused::SourceAway { machine: machine() },
            MoveRefused::Attached,
            MoveRefused::AlreadyMoving,
            MoveRefused::NotPlain,
            MoveRefused::Lineage,
            MoveRefused::Snapshots,
            MoveRefused::NotAMember,
            MoveRefused::OutsideNeighbourhood,
            MoveRefused::Unplaced,
            MoveRefused::Isolated,
            MoveRefused::Slates { said: String::new() },
        ];
        let named: std::collections::BTreeSet<&str> = every.iter().map(MoveRefused::kind).collect();
        let listed: std::collections::BTreeSet<&str> = KINDS.iter().copied().collect();
        assert_eq!(listed.len(), KINDS.len(), "each kind listed once");
        assert_eq!(named, listed, "the kinds refusals name are the kinds listed");
    }

    use super::*;
    use crate::workspace::slates_fleet::Member;

    fn made() -> Made {
        Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/state/workspaces/kiln-a".into(), ..Default::default() }
    }

    fn fleet() -> crate::config::SlatesFleet {
        crate::config::SlatesFleet { f: 1, nodes: [("bench-a", "10.0.0.1:17300"), ("bench-b", "10.0.0.2:17300"), ("bench-c", "10.0.0.3:17300")].into_iter().map(|(m, a)| (m.to_owned(), a.to_owned())).collect(), ..Default::default() }
    }

    fn members(serving: &[(&str, Option<u64>)]) -> Members {
        Members {
            fleet: crate::workspace::slates_fleet::FLEET.to_owned(),
            members: serving.iter().map(|(machine, member)| Member { machine: (*machine).to_owned(), address: "10.0.0.9:17300".into(), domain: 1, certificate: String::new(), member: *member, user: None }).collect(),
        }
    }

    /// K4: Slates' typed answers to a move, each by kind, and anything else in Slates' words.
    #[test]
    fn k4_slates_refusals_of_a_move_are_reported_by_kind() {
        use slates_api::{Busy, Error, Invalid, MoveTarget, RetryCause, Unmovable, Unusable};
        let cases = [
            (Error::Busy(Busy::Attached), MoveRefused::Attached),
            (Error::Busy(Busy::Moving), MoveRefused::AlreadyMoving),
            (Error::Unusable(Unusable::Unmovable(Unmovable::NotPlain)), MoveRefused::NotPlain),
            (Error::Unusable(Unusable::Unmovable(Unmovable::Lineage)), MoveRefused::Lineage),
            (Error::Unusable(Unusable::Unmovable(Unmovable::Snapshots)), MoveRefused::Snapshots),
            (Error::Invalid(Invalid::MoveTarget(MoveTarget::NotMember)), MoveRefused::NotAMember),
            (Error::Invalid(Invalid::MoveTarget(MoveTarget::OutsideNeighbourhood)), MoveRefused::OutsideNeighbourhood),
            (Error::Retry { cause: RetryCause::MoveUnplaced, pause_ns: 5 }, MoveRefused::Unplaced),
        ];
        for (error, kind) in cases {
            assert_eq!(MoveRefused::of(&error), kind, "{error:?}");
            assert!(!kind.said().is_empty());
        }
        let other = Error::Unusable(Unusable::Destroying);
        assert!(matches!(MoveRefused::of(&other), MoveRefused::Slates { ref said } if said == &other.to_string()));
        assert!(MoveRefused::NotPlain.said().contains("captured base"), "an overlay's refusal says what moves");
        for refused in [MoveRefused::NotPlain, MoveRefused::Shared { with: vec!["a".into()] }, MoveRefused::Slates { said: "x".into() }] {
            let tagged = serde_json::to_value(&refused).ok().and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned));
            assert_eq!(tagged.as_deref(), Some(refused.kind()), "the kind kiln reports is the one its wire names");
            assert!(refused.reported().ends_with(&format!("[kind: {}]", refused.kind())));
        }
    }

    /// K4: Slates' answers followed: under way to the target asks again at once, a pause asked for is kept, the move to
    /// the target done is done, and an answer naming another target, or a refusal, ends the move refused by kind.
    #[test]
    fn k4_the_source_follows_slates_answers_to_the_move() {
        use slates_api::{Error, MoveProgress, RetryCause, Unmovable, Unusable};
        assert_eq!(step(&Ok(MoveProgress::Moving { to: 7 }), 7), Step::Again { pause_ns: None });
        assert_eq!(step(&Ok(MoveProgress::Moved { to: 7 }), 7), Step::Moved);
        assert_eq!(step(&Ok(MoveProgress::Moving { to: 8 }), 7), Step::Refused(MoveRefused::AlreadyMoving));
        assert_eq!(step(&Ok(MoveProgress::Moved { to: 8 }), 7), Step::Refused(MoveRefused::AlreadyMoving));
        assert_eq!(step(&Err(Error::Retry { cause: RetryCause::Overloaded { shard: 1 }, pause_ns: 40 }), 7), Step::Again { pause_ns: Some(40) });
        assert_eq!(step(&Err(Error::Retry { cause: RetryCause::MoveUnplaced, pause_ns: 40 }), 7), Step::Refused(MoveRefused::Unplaced));
        assert_eq!(step(&Err(Error::Unusable(Unusable::Unmovable(Unmovable::NotPlain))), 7), Step::Refused(MoveRefused::NotPlain));
    }

    /// K4: the server's plan moves a workspace only inside kiln's Slates fleet, to a target whose node said it serves,
    /// and never one another agent works in; each refusal by kind.
    #[test]
    fn k4_the_plan_moves_a_workspace_only_inside_the_fleet_to_a_serving_node() {
        let serving = members(&[("bench-a", Some(11)), ("bench-b", Some(22))]);
        assert_eq!(plan(&fleet(), &serving, "bench-a", "bench-b", &made(), &[]), Ok(VolumeMove { volume: made().volume, member: 22, to: "bench-b".into(), user: None }));
        assert_eq!(plan(&fleet(), &serving, "bench-a", "bench-c", &made(), &[]), Err(MoveRefused::TargetNotServing { machine: "bench-c".into() }));
        assert_eq!(plan(&fleet(), &members(&[("bench-b", None)]), "bench-a", "bench-b", &made(), &[]), Err(MoveRefused::TargetNotServing { machine: "bench-b".into() }));
        assert_eq!(plan(&fleet(), &serving, "bench-a", "bench-d", &made(), &[]), Err(MoveRefused::NotInFleet { machine: "bench-d".into() }));
        assert_eq!(plan(&fleet(), &serving, "bench-d", "bench-b", &made(), &[]), Err(MoveRefused::NotInFleet { machine: "bench-d".into() }));
        assert_eq!(plan(&crate::config::SlatesFleet::default(), &serving, "bench-a", "bench-b", &made(), &[]), Err(MoveRefused::NotInFleet { machine: "bench-a".into() }), "no fleet declared");
        assert_eq!(plan(&fleet(), &serving, "bench-a", "bench-b", &made(), &["other".into()]), Err(MoveRefused::Shared { with: vec!["other".into()] }));
        // The target's user, as its runner said it, goes with the move: what the source shares the volume with.
        let user = crate::workspace::slates_fleet::NodeUser { anchor: 7, uid: 501 };
        let mut said = members(&[("bench-a", Some(11)), ("bench-b", Some(22))]);
        said.members.iter_mut().filter(|m| m.machine == "bench-b").for_each(|m| m.user = Some(user));
        assert_eq!(plan(&fleet(), &said, "bench-a", "bench-b", &made(), &[]).map(|v| v.user), Ok(Some(user)));
        let mut too_few = fleet();
        too_few.f = 2;
        assert!(matches!(plan(&too_few, &serving, "bench-a", "bench-b", &made(), &[]), Err(MoveRefused::Setting { .. })));
    }
}
