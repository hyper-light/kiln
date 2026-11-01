//! Whether a comment on a change reaches an agent (docs/ci.md W26-R18), the pure part. A person's
//! words always go to the change's agent while it runs. Agents' words (an agent's, a CI job's) go
//! only where the repository lets agents fix code from agents' feedback, its switch on its page in
//! Repos, off until turned on; and then only until agents have started [`FIXES_PER_THREAD`] fixes
//! in one thread since a person last spoke there, so two agents can't keep each other going. An
//! agent's own words never go back to it, and an agent that has ended is sent nothing. Every input
//! is passed in; each refusal says why.

/// Who said it, as review keeps principals: `user:kim` (a person, by any of their tokens),
/// `agent:<session>` (an agent), `job:<leg>` (a CI job).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Person,
    Agent,
    Job,
}

impl Speaker {
    pub fn of(principal: &str) -> Speaker {
        let p = principal.split(" for ").next().unwrap_or(principal).trim();
        if p.starts_with("agent:") {
            Speaker::Agent
        } else if p.starts_with("job:") {
            Speaker::Job
        } else {
            Speaker::Person
        }
    }
}

/// Agent-started fixes one thread takes before it waits for a person.
pub const FIXES_PER_THREAD: u32 = 3;

/// Why a version exists when it answers threads and an agent pushed it (docs/ci.md W26-R04): an
/// agent's fix, its thread the first it answers.
pub const AGENT_FIX: &str = "agent's fix";

/// What's known as a comment on an agent's change comes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heard {
    pub speaker: Speaker,
    /// It's the words of the agent it would go to.
    pub own: bool,
    /// The repository lets agents fix code from agents' feedback.
    pub agents_fix: bool,
    /// Fixes agents' feedback has started in its thread since a person last spoke there.
    pub fixes: u32,
    /// The agent it would go to still runs.
    pub runs: bool,
}

/// Why a comment doesn't go to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Its own words.
    Own,
    /// Its agent has ended.
    Ended,
    /// The repository doesn't let agents fix from agents' feedback.
    Switch,
    /// Agents have started their fixes in this thread: a person speaks next.
    Bound,
}

impl Why {
    pub fn words(self) -> &'static str {
        match self {
            Why::Own => "its own words",
            Why::Ended => "the change's agent has ended: a person starts or resumes it",
            Why::Switch => "its repository doesn't let agents fix code from agents' feedback: the switch on its page in Repos is off",
            Why::Bound => "agents have started 3 fixes in this thread since a person spoke in it: it waits for a person",
        }
    }
}

/// Where a comment goes: to the agent, or nowhere, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Tell,
    Hold(Why),
}

/// Whether `h`'s comment reaches its change's agent. Agents' words with the switch off go nowhere
/// whether or not the agent runs: starting it again wouldn't send them.
pub fn route(h: Heard) -> Route {
    let agents = h.speaker != Speaker::Person;
    if h.own {
        Route::Hold(Why::Own)
    } else if agents && !h.agents_fix {
        Route::Hold(Why::Switch)
    } else if !h.runs {
        Route::Hold(Why::Ended)
    } else if agents && h.fixes >= FIXES_PER_THREAD {
        Route::Hold(Why::Bound)
    } else {
        Route::Tell
    }
}

/// Fixes agents' feedback has started in a thread since a person last spoke there: of `fixes` (when
/// each version was made whose why is an agent's fix answering the thread), those made at or after
/// the newest of `said` (each of its comments' principal and when) a person wrote. Every fix counts
/// while no person has spoken; one made the second a person spoke counts.
pub fn fixes_since_a_person(said: &[(&str, i64)], fixes: &[i64]) -> u32 {
    let since = said.iter().filter(|(p, _)| Speaker::of(p) == Speaker::Person).map(|(_, at)| *at).max();
    u32::try_from(fixes.iter().filter(|at| since.is_none_or(|s| **at >= s)).count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(principal: &str, agents_fix: bool, fixes: u32) -> Heard {
        Heard { speaker: Speaker::of(principal), own: false, agents_fix, fixes, runs: true }
    }

    #[test]
    fn w26_r18_speakers_by_principal() {
        assert_eq!(["user:kim", "pat:01ab of user:kim", "admin", "agent:01a19c0f@box-a", "agent:review for user:kim", "job:build-3"].map(Speaker::of), [Speaker::Person, Speaker::Person, Speaker::Person, Speaker::Agent, Speaker::Agent, Speaker::Job]);
    }

    /// The switch off (the default): a person's words go; an agent's or a job's don't, saying so.
    #[test]
    fn w26_r18_agents_feedback_starts_nothing_with_the_switch_off() {
        assert_eq!(route(heard("user:kim", false, 0)), Route::Tell);
        assert_eq!(route(heard("agent:01a19c0f@box-a", false, 0)), Route::Hold(Why::Switch));
        assert_eq!(route(heard("job:lint", false, 0)), Route::Hold(Why::Switch));
        assert!(Why::Switch.words().contains("switch on its page in Repos is off"));
    }

    /// The switch on: agents' words go, until the thread's agents have started their fixes; a
    /// person's always go, however many.
    #[test]
    fn w26_r18_agents_feedback_goes_with_the_switch_on_until_the_threads_limit() {
        for fixes in 0..FIXES_PER_THREAD {
            assert_eq!(route(heard("agent:01a19c0f@box-a", true, fixes)), Route::Tell, "{fixes} fixes so far");
        }
        assert_eq!(route(heard("agent:01a19c0f@box-a", true, FIXES_PER_THREAD)), Route::Hold(Why::Bound));
        assert_eq!(route(heard("job:lint", true, FIXES_PER_THREAD)), Route::Hold(Why::Bound));
        assert_eq!(route(heard("user:kim", true, 7)), Route::Tell);
        assert!(Why::Bound.words().contains("waits for a person"));
    }

    /// Its own words never go back to it, and an ended agent is sent nothing, whoever spoke.
    #[test]
    fn w26_r18_own_words_and_ended_agents_go_nowhere() {
        assert_eq!(route(Heard { own: true, ..heard("agent:01a19c0f@box-a", true, 0) }), Route::Hold(Why::Own));
        assert_eq!(route(Heard { runs: false, ..heard("user:kim", true, 0) }), Route::Hold(Why::Ended));
        assert_eq!(route(Heard { runs: false, ..heard("agent:01a19c0f@box-a", true, 0) }), Route::Hold(Why::Ended));
    }

    /// W26-AC10: with the switch off, agents' words go nowhere for the switch, not for an ended
    /// agent (starting it again wouldn't send them); with it on, an ended agent holds them.
    #[test]
    fn w26_ac10_the_switch_comes_before_an_ended_agent() {
        assert_eq!(route(Heard { runs: false, ..heard("agent:01a19c0f@box-a", false, 0) }), Route::Hold(Why::Switch));
        assert_eq!(route(Heard { runs: false, ..heard("job:lint", false, 0) }), Route::Hold(Why::Switch));
        assert_eq!(route(Heard { runs: false, ..heard("agent:01a19c0f@box-a", true, FIXES_PER_THREAD) }), Route::Hold(Why::Ended));
    }

    /// W26-AC10's bound, counted: agents' fixes in a thread since a person last spoke there, a fix
    /// the same second counting; with no person in it, every fix counts.
    #[test]
    fn w26_ac10_a_person_starts_the_threads_count_again() {
        let said = [("agent:review@box-a", 10), ("user:kim", 20), ("agent:review@box-a", 30)];
        assert_eq!(fixes_since_a_person(&said, &[15, 25, 35]), 2);
        assert_eq!(fixes_since_a_person(&said, &[20]), 1, "made the second kim spoke");
        assert_eq!(fixes_since_a_person(&[("agent:review@box-a", 1), ("job:lint", 2)], &[3, 4, 5]), 3);
        assert_eq!(fixes_since_a_person(&[("pat:01ab of user:kim", 50)], &[10, 20]), 0, "a person's token is the person");
        assert_eq!(route(heard("agent:review@box-a", true, fixes_since_a_person(&said, &[25, 35, 45]))), Route::Hold(Why::Bound));
    }
}
