//! The gates a release passes before its commit (docs/deploy-api.md §9.1): a freeze, the
//! environment's windows, built-in checks, the upstream environments it must run in first (`after`,
//! txst's required environments) and how long (`soak`), and people's approvals. Each gate's result
//! is recorded on the release with its evidence. A gate that can pass by waiting holds the release
//! (`waiting`, or `waiting_approval` for people); one that can't refuses it, at once.

use serde_json::{Value, json};

use super::model::{App, Environment, GateResult, Kind, Release, State, Window};
use super::store;
use super::Deploy;

/// What the gates say of a release now.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Every gate passed: commit it.
    Go(Vec<GateResult>),
    /// A gate holds it: look again at `until` (unix seconds) or when something moves; `approval`:
    /// it waits for people.
    Hold { results: Vec<GateResult>, why: String, approval: bool, until: Option<i64> },
    /// A gate it can't pass by waiting.
    Refuse { results: Vec<GateResult>, why: String },
}

/// How soon a held release is looked at again, when nothing says sooner.
const AGAIN_SECS: i64 = 15;

fn result(gate: &str, passed: bool, detail: impl Into<String>, evidence: Value) -> GateResult {
    GateResult { gate: gate.to_owned(), passed, detail: detail.into(), evidence }
}

/// The person a caller is or acts for, as reviewers name people: `user:ada`, a token of hers
/// (`pat:<id> of user:ada`) and an agent acting for her (`agent:fix for user:ada`) are `ada`; the
/// owner is `admin`.
pub fn person(name: &str) -> &str {
    let name = name.rsplit_once(" for ").or_else(|| name.rsplit_once(" of ")).map_or(name, |(_, who)| who);
    name.strip_prefix("user:").unwrap_or(name)
}

/// Whether a deny window holds now, or no allow window is open: when it opens again, and why.
fn closed(windows: &[Window], now: i64) -> Result<Option<(i64, String)>, String> {
    let mut allow_open = None::<bool>;
    let mut next_allow: Option<i64> = None;
    for w in windows {
        let (schedule, deny) = match (&w.allow, &w.deny) {
            (Some(s), None) => (s, false),
            (None, Some(s)) => (s, true),
            _ => return Err("a window is either `allow: <cron>` or `deny: <cron>`".into()),
        };
        let cron = kiln_ci::trigger::cron::Cron::parse(schedule).map_err(|e| format!("window `{schedule}`: {e}"))?;
        let tz = kiln_ci::trigger::cron::zone(w.timezone.as_deref())?;
        let secs = kiln_ci::trigger::cron::seconds(&w.duration).and_then(|s| i64::try_from(s).ok()).filter(|s| *s > 0).ok_or_else(|| format!("window `{schedule}`: `{}` isn't a duration (`90m`, `2h`, `1d`)", w.duration))?;
        // Open now when it opened within its duration.
        let opened = cron.fires_in(now - secs, now, &tz, 10_000).last().copied();
        match (deny, opened) {
            (true, Some(at)) => return Ok(Some((at + secs, format!("a deny window (`{schedule}`, {}) holds releases until {}", w.duration, super::repo::rfc3339(at + secs))))),
            (true, None) => {}
            (false, Some(_)) => allow_open = Some(true),
            (false, None) => {
                allow_open.get_or_insert(false);
                if let Some(at) = cron.fires_in(now, now + 60 * 24 * 3600, &tz, 1).first().copied() {
                    next_allow = Some(next_allow.map_or(at, |n| n.min(at)));
                }
            }
        }
    }
    Ok(match allow_open {
        Some(false) => {
            let until = next_allow.unwrap_or(now + 3600);
            Some((until, format!("no allow window is open: the next opens at {}", super::repo::rfc3339(until))))
        }
        _ => None,
    })
}

/// What the gates say of `r` now.
pub async fn evaluate(d: &Deploy, r: &Release, env: &Environment, app: &App) -> anyhow::Result<Verdict> {
    let g = &env.gates;
    let now = crate::util::now_unix();
    let mut results = Vec::new();

    if let Some(why) = &g.freeze {
        results.push(result("freeze", false, format!("frozen: {why}"), json!({ "reason": why })));
        return Ok(Verdict::Hold { results, why: format!("{} is frozen: {why}", env.name), approval: false, until: Some(now + 60) });
    }

    match closed(&g.windows, now) {
        Ok(Some((until, why))) => {
            results.push(result("windows", false, why.clone(), json!({ "until": until })));
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(until) });
        }
        Ok(None) if !g.windows.is_empty() => results.push(result("windows", true, "a window lets it go", Value::Null)),
        Ok(None) => {}
        Err(why) => {
            results.push(result("windows", false, why.clone(), Value::Null));
            return Ok(Verdict::Refuse { results, why });
        }
    }

    for check in &g.checks {
        let (passed, detail) = match check.as_str() {
            "no-snapshots" => match r.version.as_deref() {
                Some(v) if v.to_ascii_lowercase().contains("snapshot") => (false, format!("{v} is a snapshot")),
                _ => (true, "not a snapshot".to_owned()),
            },
            "owners-set" if app.owners.is_empty() => (false, format!("{} has no owner team", app.name)),
            "owners-set" => (true, format!("owned by {}", app.owners.join(", "))),
            other => (false, format!("kiln has no check `{other}` (no-snapshots, owners-set)")),
        };
        results.push(result(&format!("check:{check}"), passed, detail.clone(), Value::Null));
        if !passed {
            return Ok(Verdict::Refuse { results, why: detail });
        }
    }

    // A new version runs upstream first; rollbacks, config and enabling run what passed before.
    if r.kind == Kind::Deploy && !g.after.is_empty() {
        let soak = match g.soak.as_deref() {
            Some(s) => Some(kiln_ci::trigger::cron::seconds(s).and_then(|x| i64::try_from(x).ok()).ok_or_else(|| anyhow::anyhow!("soak `{s}` isn't a duration (`30m`, `2h`, `1d`)"))?),
            None => None,
        };
        let version = r.version.clone().unwrap_or_default();
        let mut soaking: Option<(i64, String)> = None;
        for upstream in &g.after {
            let reg = store::registration(&d.hub.store, &r.app, upstream).await?;
            let running = match reg.as_ref().and_then(|x| x.running) {
                Some(id) => store::release(&d.hub.store, id).await?,
                None => None,
            };
            let same = running.as_ref().filter(|x| x.version.as_deref() == Some(version.as_str()) && same_artifacts(x, r));
            match same {
                Some(x) => {
                    let since = x.ended_at.unwrap_or(x.created_at);
                    results.push(result(&format!("after:{upstream}"), true, format!("{version} runs in {upstream} (release {})", x.id), json!({ "environment": upstream, "release": x.id, "since": since })));
                    if let Some(s) = soak
                        && now - since < s
                        && soaking.as_ref().is_none_or(|(until, _)| since + s > *until)
                    {
                        soaking = Some((since + s, upstream.clone()));
                    }
                }
                None => {
                    // On its way there: wait for it.
                    let coming = store::in_flight(&d.hub.store, &r.app, upstream).await?.filter(|x| x.version.as_deref() == Some(version.as_str()));
                    if let Some(x) = coming {
                        let why = format!("waits for {upstream} to run {version} (release {} is {})", x.id, x.state.as_str());
                        results.push(result(&format!("after:{upstream}"), false, why.clone(), json!({ "environment": upstream, "release": x.id })));
                        return Ok(Verdict::Hold { results, why, approval: false, until: Some(now + AGAIN_SECS) });
                    }
                    let there = running.as_ref().and_then(|x| x.version.clone()).unwrap_or_else(|| "nothing".to_owned());
                    let why = if reg.is_none() {
                        format!("{} isn't registered in {upstream}, which {} releases must run in first", r.app, env.name)
                    } else if running.as_ref().is_some_and(|x| x.version.as_deref() == Some(version.as_str())) {
                        format!("{version} runs in {upstream} with other artifacts: release the same build there first")
                    } else {
                        format!("{version} doesn't run in {upstream} ({there} does): release it there first")
                    };
                    results.push(result(&format!("after:{upstream}"), false, why.clone(), json!({ "environment": upstream, "runs": there })));
                    return Ok(Verdict::Refuse { results, why });
                }
            }
        }
        if let Some((until, upstream)) = soaking {
            let why = format!("soaking in {upstream} until {}", super::repo::rfc3339(until));
            results.push(result("soak", false, why.clone(), json!({ "until": until })));
            return Ok(Verdict::Hold { results, why, approval: false, until: Some(until) });
        }
        if soak.is_some() {
            results.push(result("soak", true, "soaked long enough upstream", Value::Null));
        }
    }

    if let Some(a) = &g.approvals {
        let count = a.count.max(1);
        let valid: Vec<&super::model::Approval> = r
            .approvals
            .iter()
            .filter(|x| a.self_approve || person(&x.by) != person(&r.requested_by))
            .filter(|x| a.reviewers.is_empty() || a.reviewers.iter().any(|rv| rv == person(&x.by)))
            .collect();
        let mut people: Vec<&str> = valid.iter().map(|x| person(&x.by)).collect();
        people.sort_unstable();
        people.dedup();
        let have = u32::try_from(people.len()).unwrap_or(u32::MAX);
        let evidence = json!({ "count": count, "approved_by": people, "reviewers": a.reviewers, "self_approve": a.self_approve });
        if have < count {
            let who = if a.reviewers.is_empty() { "anyone who may approve releases here".to_owned() } else { a.reviewers.join(", ") };
            let why = format!("waits for {} more approval{} from {who}", count - have, if count - have == 1 { "" } else { "s" });
            results.push(result("approvals", false, why.clone(), evidence));
            return Ok(Verdict::Hold { results, why, approval: true, until: None });
        }
        results.push(result("approvals", true, format!("approved by {}", people.join(", ")), evidence));
    }
    Ok(Verdict::Go(results))
}

/// Whether two releases deploy the same build: each artifact both name has the same digest, or
/// (without digests) the same repository and tag.
fn same_artifacts(a: &Release, b: &Release) -> bool {
    b.artifacts.iter().all(|(name, x)| match a.artifacts.get(name) {
        Some(y) if !x.digest.is_empty() && !y.digest.is_empty() => x.digest == y.digest,
        Some(y) => x.repository == y.repository && x.tag == y.tag,
        None => false,
    })
}

/// Whether a release's state is one the gates hold it in.
pub fn holding(s: State) -> bool {
    matches!(s, State::Waiting | State::WaitingApproval)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny(cron: &str, duration: &str) -> Window {
        Window { allow: None, deny: Some(cron.into()), duration: duration.into(), timezone: None }
    }

    fn allow(cron: &str, duration: &str) -> Window {
        Window { allow: Some(cron.into()), deny: None, duration: duration.into(), timezone: None }
    }

    #[test]
    fn windows_hold_releases_while_they_deny_and_until_one_allows() -> Result<(), String> {
        // Saturday 2026-10-17 12:00 UTC.
        let sat_noon = 1_792_238_400;
        let weekend = deny("0 0 * * 6", "48h");
        let held = closed(std::slice::from_ref(&weekend), sat_noon)?.ok_or("held on Saturday")?;
        assert_eq!(held.0, sat_noon - 12 * 3600 + 48 * 3600, "until Monday 00:00");
        assert!(closed(&[weekend], sat_noon + 3 * 86_400)?.is_none(), "free on Tuesday");
        // Weekdays 9 to 17 only.
        let office = allow("0 9 * * 1-5", "8h");
        let (until, why) = closed(std::slice::from_ref(&office), sat_noon)?.ok_or("not on Saturday")?;
        assert!(why.contains("next opens") && until > sat_noon);
        assert!(closed(&[office], sat_noon + 2 * 86_400 - 12 * 3600 + 10 * 3600)?.is_none(), "Monday 10:00 is open");
        assert!(closed(&[Window { allow: Some("0 0 * * *".into()), deny: Some("0 0 * * *".into()), duration: "1h".into(), timezone: None }], sat_noon).is_err());
        Ok(())
    }

    #[test]
    fn a_reviewer_is_the_person_a_caller_is_or_acts_for() {
        assert_eq!(person("user:ada"), "ada");
        assert_eq!(person("agent:fix-it for user:ada"), "ada");
        assert_eq!(person("pat:01a15248f432 of user:ada"), "ada");
        assert_eq!(person("pat:01a15248f432 of admin"), "admin");
        assert_eq!(person("admin"), "admin");
    }
}
