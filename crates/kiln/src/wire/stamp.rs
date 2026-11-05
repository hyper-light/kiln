//! A runner's stamp on an agent session's update (goal 10, plan 01a1a87b2dae step 5): how long the runner held
//! the update, from taking it in (its session handing it on, `crate::harness::session::Sink`) to making its
//! frame, said at the end of the update's JSON as `taken_us`, a whole number of microseconds.
//!
//! In g9p7's fifth round frames lagged (74 ms at the median) while every leg at the server was short: the time
//! went before the server had them, on the runner or on the wire, and nothing said which. With the stamp the
//! server observes the runner's hold of each update beside its own time with it
//! (`crate::route_latency::RunnerLeg::Runner` and `Server`), and what a frame took past both is the wire's.
//!
//! It's a duration on the runner's monotonic clock, never a time of day. The runner's clock and the server's
//! differ (another machine's, or one NTP stepped), so a time read on one set against a time read on the other
//! says as much about the clocks as about the update. Each leg is two readings of one clock: the runner's hold
//! of the runner's, the server's time of the server's.
//!
//! The field goes last, the runner writing `,"taken_us":<digits>}` in place of the object's closing brace, so
//! it's added without moving what's before it, and read without reading the rest: the server takes it off the
//! end before it reads the update (`crate::wire::payload`), so the update is read, and its unknown fields
//! looked for, as a runner without the stamp would have sent it. A server from before the stamp reads the
//! update as it always did: serde lets the field go, and it's counted (`kiln_wire_ignored_fields_total`) and
//! said once an hour, as docs/wire.md says a newer peer's field is.

use std::fmt::Write as _;
use std::time::Duration;

/// The field's name and its colon, as it ends an update's JSON.
const NAME: &str = "\"taken_us\":";

/// `json`, an update's JSON object, saying that the runner took the update in `taken` before its frame was
/// made: the field appended, last. What isn't an object (none is, as `crate::harness::core::Update` is
/// written) is left as it is.
pub fn put(json: &mut String, taken: Duration) {
    // An empty object takes the field without a comma before it.
    let empty = match json.strip_suffix('}') {
        Some(inner) => inner.trim_end().ends_with('{'),
        None => return,
    };
    let us = u64::try_from(taken.as_micros()).unwrap_or(u64::MAX);
    json.pop();
    if !empty {
        json.push(',');
    }
    json.push_str(NAME);
    let _ = write!(json, "{us}");
    json.push('}');
}

/// How long before its frame was made the runner took in the update `json` carries, read off its end where
/// a runner said it ([`put`]), and `json` left as the update alone, as a runner without the stamp sends it.
/// `None`, and `json` as it was, when it doesn't end so. Nothing is copied or moved: the field is cut off the
/// end and the object's brace put back.
pub fn take(json: &mut String) -> Option<Duration> {
    let (at, us) = field(json)?;
    json.truncate(at);
    json.push('}');
    Some(Duration::from_micros(us))
}

/// Where in `json` the field [`put`] appends starts (its comma, or its name in an object that had nothing
/// else), and the microseconds it says. Only the object's last member, exactly as [`put`] writes it, is the
/// field: `"taken_us":` after a comma or the object's opening brace, then a whole number that fits a `u64`,
/// then the brace that ends the object, nothing after it. A member of a nested object, or a string that
/// says the same words, ends otherwise (`}}`, `"}`), so it's never taken for the field.
fn field(json: &str) -> Option<(usize, u64)> {
    let inner = json.strip_suffix('}')?;
    let digits = inner.bytes().rev().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (head, number) = inner.split_at_checked(inner.len().saturating_sub(digits))?;
    let us = number.bytes().try_fold(0u64, |n, d| n.checked_mul(10)?.checked_add(u64::from(d.checked_sub(b'0')?)))?;
    let before = head.strip_suffix(NAME)?;
    if let Some(rest) = before.strip_suffix(',') {
        return Some((rest.len(), us));
    }
    (before == "{").then_some((before.len(), us))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event};

    fn said(n: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: format!("e{n}"), seq: n, at: 1, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    /// Goal 10, plan 01a1a87b2dae step 5: an update the runner stamped ends with the field, the rest of it as it
    /// was; read off, it says the runner's hold to the microsecond and leaves the update as it was written, byte
    /// for byte; and a server from before the stamp reads the stamped update as the same update.
    #[test]
    fn g10_s5_a_stamped_update_says_the_runners_hold_and_reads_as_the_update() -> anyhow::Result<()> {
        for u in [said(1, "the retry loop reads its deadline"), Update::Closed, Update::Reset { from: Some(3) }] {
            let written = serde_json::to_string(&u)?;
            let mut json = written.clone();
            put(&mut json, Duration::from_micros(1_234));
            anyhow::ensure!(json.starts_with(written.trim_end_matches('}')) && json.ends_with(",\"taken_us\":1234}"), "appended, last: {json}");
            // A server from before it: the update it always read.
            anyhow::ensure!(serde_json::from_str::<Update>(&json)? == u, "an older reader takes it as the update: {json}");
            anyhow::ensure!(take(&mut json) == Some(Duration::from_micros(1_234)), "read off: {json}");
            anyhow::ensure!(json == written, "left as it was written: {json}");
        }
        // As long as a u64 of microseconds says; past it, the most.
        let mut json = serde_json::to_string(&Update::Closed)?;
        put(&mut json, Duration::MAX);
        anyhow::ensure!(take(&mut json) == Some(Duration::from_micros(u64::MAX)));
        // An empty object takes it without a comma, and gives it back.
        let mut empty = "{}".to_owned();
        put(&mut empty, Duration::from_micros(7));
        anyhow::ensure!(empty == "{\"taken_us\":7}", "{empty}");
        anyhow::ensure!(take(&mut empty) == Some(Duration::from_micros(7)) && empty == "{}", "{empty}");
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: an update with no stamp (an older runner's, or one not taken in from its
    /// harness: a stream's picture) says none and is left as it came; so is what only looks like the field (a
    /// nested object's member, words in a string, a number too large for a u64, the field not last, spaces), and
    /// what isn't an object is never stamped.
    #[test]
    fn g10_s5_an_update_without_the_stamp_says_none_and_is_left_as_it_came() -> anyhow::Result<()> {
        let unstamped = serde_json::to_string(&said(2, "no stamp"))?;
        let lookalikes = [
            unstamped.clone(),
            r#"{"t":"reply","client":"c","value":{"taken_us":5}}"#.to_owned(),
            r#"{"t":"reply","client":"c","value":{"a":1,"taken_us":5}}"#.to_owned(),
            r#"{"t":"stream","id":"e1","text":",\"taken_us\":5}"}"#.to_owned(),
            r#"{"t":"closed","my_taken_us":5}"#.to_owned(),
            r#"{"t":"closed","taken_us":18446744073709551616}"#.to_owned(),
            r#"{"taken_us":5,"t":"closed"}"#.to_owned(),
            r#"{"t":"closed", "taken_us": 5}"#.to_owned(),
            r#"{"t":"closed","taken_us":}"#.to_owned(),
            "{\"t\":\"closed\",\"taken_us\":5}\n".to_owned(),
            String::new(),
        ];
        for came in lookalikes {
            let mut json = came.clone();
            anyhow::ensure!(take(&mut json).is_none(), "taken from {came}");
            anyhow::ensure!(json == came, "changed: {came} became {json}");
        }
        for not_an_object in ["[1,2]", "\"x}\"", "7", ""] {
            let mut json = not_an_object.to_owned();
            put(&mut json, Duration::from_micros(9));
            anyhow::ensure!(json == not_an_object, "stamped: {json}");
        }
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a runner's stamped updates, on its session's channel stream, reach their
    /// owner as the updates they were written as; at the owner's take the runner's hold of each is its `runner` leg,
    /// as the runner said it. Handed over with its frame's arrival (as the hub's loop hands one), the server's time
    /// with it is its `server` leg, from that arrival to the owner done with it.
    #[test]
    fn g10_s5_an_owner_observes_the_runners_hold_and_the_servers_time() -> anyhow::Result<()> {
        use crate::route_latency::{RunnerLeg, legs};
        use std::time::Instant;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::hub_with_runner(crate::rt::clock(), "fedora", crate::proto::CAP_UNREAD | crate::proto::CAP_WIRE).await?;
            // Each leg's count and sum (µs): other tests' observations only add to them.
            let leg = |l: RunnerLeg| legs().of(l).map_or((0, 0), |h| (h.count(), h.sum_us()));
            let stamped = |u: &Update, held: Duration| -> anyhow::Result<crate::proto::AgentMsg> {
                let mut json = serde_json::to_string(u)?;
                put(&mut json, held);
                Ok(crate::proto::AgentMsg::Harness { session: "s1".into(), json })
            };
            let runner0 = leg(RunnerLeg::Runner);
            let open = Update::Open { info: Box::new(crate::harness::session::Info { id: "s1".into(), kind: "claude".into(), ..Default::default() }), since: None };
            crate::hub::tests::hear_item(&hub, 1, "s:s1#a1", 1, &stamped(&open, Duration::from_micros(700))?).await?;
            crate::hub::tests::hear_item(&hub, 1, "s:s1#a1", 2, &stamped(&said(1, "the retry loop reads its deadline"), Duration::from_micros(700))?).await?;
            hub.harness.applied("s1").await;
            anyhow::ensure!(hub.harness.located_on("s1", "fedora"), "opened on fedora");
            let events = hub.harness.events_json("s1", None, 20).ok_or_else(|| anyhow::anyhow!("no copy of s1"))?;
            let seen: serde_json::Value = serde_json::from_slice(&events)?;
            let ids: Vec<&str> = seen.get("events").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|e| e.get("id").and_then(serde_json::Value::as_str)).collect();
            anyhow::ensure!(ids == ["e1"] && !String::from_utf8_lossy(&events).contains("taken_us"), "taken as written: {}", String::from_utf8_lossy(&events));
            let runner = leg(RunnerLeg::Runner);
            anyhow::ensure!(runner.0 >= runner0.0 + 2 && runner.1 >= runner0.1 + 1_400, "two held 700 µs each: {runner0:?} to {runner:?}");
            // As the hub's loop hands one over, its frame's arrival with it: here 5 ms before.
            let arrived = Instant::now().checked_sub(Duration::from_millis(5)).ok_or_else(|| anyhow::anyhow!("no time 5 ms ago"))?;
            let server0 = leg(RunnerLeg::Server);
            let update = said(2, "and its budget");
            hub.harness.from_runner(crate::harness::copies::RunnerUpdate { peer: 1, agent: "fedora".into(), session: "s1".into(), update, shipped: false, arrived: Some(arrived), handed: Some(Instant::now()), taken: Some(Duration::from_micros(300)) }).await;
            hub.harness.applied("s1").await;
            let server = leg(RunnerLeg::Server);
            anyhow::ensure!(server.0 > server0.0 && server.1 >= server0.1 + 5_000, "from its arrival 5 ms before: {server0:?} to {server:?}");
            Ok(())
        })
    }
}
