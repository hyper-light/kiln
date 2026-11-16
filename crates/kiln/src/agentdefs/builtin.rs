//! The agent definitions kiln ships (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P5.1, the person,
//! 2026-11-14: "One per ci skill and one for each harness we support. Also don't forget the ask/about
//! popup agents"): listed read only beside a person's own, run as they are, and copied to make one
//! theirs (`agentdefs.copy`). Each is made from what kiln itself has, so none drifts from it: one per
//! CI skill kiln ships (`crate::mcp::skills`, `kiln-ci-*`: its description its instructions, the
//! skill its own), one per harness (`crate::harness::kinds::Kind`, in kiln's Agentfile for it,
//! `crate::agentfiles::builtin`), and one per agent kiln starts from its UI with a brief of its own:
//! the about card (a conversation's "ask about this code", `crate::harness::ops::about_prompt`), the
//! same asked of code no agent made (`crate::ci::about`), an attachment's agent woken by an incident
//! and a review agent (`crate::ci::attach::bundle`), and the agent that installs a tool a job lacks on
//! a runner (`crate::toolfix::brief`), each brief with what kiln fills in at its start shown `<like
//! this>`. A person may have their own copy of a UI agent that's `replaceable` started in its place
//! for them (`super::uses`).

use serde::Serialize;

use super::{Limits, Settings};
use crate::ci::attach::Attachment;

/// What kiln's own are named by: `kiln:<name>`, never an id kiln.db makes (`crate::util::new_id`).
pub const PREFIX: &str = "kiln:";

/// One of kiln's own agent definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Builtin {
    /// `kiln:<name>`.
    pub id: String,
    pub name: String,
    /// What it's for, in a line.
    pub about: String,
    /// A UI agent kiln starts for a person, which their own copy may stand in for, for them alone
    /// (`agentdefs.use`).
    pub replaceable: bool,
    /// A UI agent's: what kiln tells it at each start, after the instructions of whichever definition
    /// it starts (kiln's own have none), with what that start fills in shown `<like this>`. Kiln's to
    /// make; a copy has its own instructions and gets the same brief.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    #[serde(flatten)]
    pub settings: Settings,
}

fn settings(harness: &str, instructions: String, skills: Vec<String>, agentfile: Option<String>) -> Settings {
    Settings { harness: harness.to_owned(), model: None, instructions, tools: Vec::new(), mcp: Vec::new(), skills, limits: Limits::default(), agentfile, volumes: Vec::new() }
}

fn one(name: &str, about: String, settings: Settings) -> Builtin {
    Builtin { id: format!("{PREFIX}{name}"), name: name.to_owned(), about, replaceable: false, brief: None, settings }
}

/// One of kiln's UI agents, on `harness`, whose start tells it `brief`: replaceable, its own
/// instructions none.
fn ui(name: &str, about: &str, brief: String) -> Builtin {
    Builtin { replaceable: true, brief: Some(brief), ..one(name, about.to_owned(), settings("claude", String::new(), Vec::new(), None)) }
}

/// The about card, in an agent's conversation (`agents.about`): "ask about this code".
pub const ABOUT: &str = "kiln:kiln-about";
/// The about card, in a change no agent made (`ci.repos.about`).
pub const ABOUT_CODE: &str = "kiln:kiln-about-code";
/// An attachment's agent, woken by an incident (`crate::ci::attach`).
pub const ATTACH: &str = "kiln:kiln-attach";
/// An attachment's agent asked to review a change (`crate::ci::attach`, wake `review`).
pub const REVIEW: &str = "kiln:kiln-review";
/// The agent that installs a tool a job lacks on a runner (`crate::toolfix`).
pub const TOOLFIX: &str = "kiln:kiln-toolfix";

/// Every one, in the order they're listed: the CI skills', the harnesses', then kiln's own agents'.
pub fn all() -> Vec<Builtin> {
    let mut out = Vec::new();
    for s in crate::mcp::skills::list() {
        let name = s.get("name").and_then(serde_json::Value::as_str).unwrap_or_default();
        if !name.starts_with("kiln-ci-") {
            continue;
        }
        let description = s.get("description").and_then(serde_json::Value::as_str).unwrap_or_default();
        let instructions = format!("Work as kiln's {name} skill says: read it first (skill://{name}/SKILL.md), then follow it.\n\nIt's for this: {description}");
        out.push(one(name, format!("kiln's {name} skill, as an agent"), settings("claude", instructions, vec![name.to_owned()], None)));
    }
    for k in crate::harness::kinds::Kind::all() {
        let agentfile = crate::agentfiles::builtin::ALL.iter().find(|b| b.harness == k.id).map(|b| b.name.to_owned());
        out.push(one(&format!("harness-{}", k.id), format!("{} as it is, in kiln's Agentfile for it", k.name), settings(k.id, String::new(), Vec::new(), agentfile)));
    }
    let tapped = crate::harness::ops::Tapped { path: "<path>", line: None, end: None, selection: Some("<selection>"), change: false, question: Some("<question>") };
    let about = crate::harness::ops::about_prompt(&tapped, "<the code around it>");
    out.push(ui("kiln-about", "The card that answers \"ask about this code\" in an agent's conversation: a child of that agent, reading and changing nothing", about.clone()));
    let repo = crate::store::Repo {
        id: String::new(),
        name: "<owner/name>".into(),
        clone_url: "<its clone URL>".into(),
        branches: Vec::new(),
        poll_seconds: 0,
        enabled: true,
        created_at: 0,
        last_polled_at: None,
        last_error: None,
        mode: "upstream".into(),
        default_branch: None,
        mirror: None,
    };
    let fetch = crate::ci::about::fetch_brief("", &repo, "<commit>", "<a folder of its own>");
    out.push(ui("kiln-about-code", "The card that answers \"ask about this code\" in a change no agent made: kiln starts it with the repository at that commit", format!("{fetch}\n\n{about}")));
    let attachment = crate::ci::attach::Attachment { name: "<attachment>".into(), on: "<what it looks after>".into(), can: vec!["<what it may do>".into()], playbook: "<its playbook>".into(), ..Default::default() };
    let incident = crate::ci::attach::Incident { id: "<incident>".into(), ..Default::default() };
    let woke = crate::ci::attach::Trigger { wake: "<what woke it>", title: "<what happened>".into(), detail: "<what kiln gives it of that>".into(), ..Default::default() };
    out.push(ui("kiln-attach", "An attachment's agent: woken by what it looks after (a failure, a stuck job, a mention, a schedule), it finds out what happened, acts within what it may do and reports", crate::ci::attach::bundle(&attachment, &incident, &woke, &[])));
    let detail = format!("<who> asked you to review <diff>, \"<its title>\" by <its author>, on <its trunk> (<its page>), its versions and open threads.\n\n{}", crate::ci::attach::review_how("<diff>"));
    let review = crate::ci::attach::Trigger { wake: "review", title: "<who> asked you to review <diff>".into(), detail, ..Default::default() };
    out.push(ui("kiln-review", "A review agent: an attachment asked to review a change (`git push -o reviewer=agent:review`), which comments, answers threads and gives its verdict, signed as itself", crate::ci::attach::bundle(&Attachment { name: "review".into(), ..attachment }, &incident, &review, &[])));
    let tools = ["<tool>".to_owned()];
    let fix = crate::toolfix::brief("<runner>", "<job>", "<owner/name>", "<number>", &tools, Some("<the command that wasn't there>"), None);
    out.push(ui("kiln-toolfix", "The agent kiln starts on a runner to install a tool a job lacks there, then runs the job again", fix));
    out
}

/// Kiln's own named `id` (`kiln:<name>`), if it's one.
pub fn get(id: &str) -> Option<Builtin> {
    id.strip_prefix(PREFIX)?;
    all().into_iter().find(|b| b.id == id)
}
