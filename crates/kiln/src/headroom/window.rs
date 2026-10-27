//! The control: a window of how many heavy groups may run at full speed, grown while every target
//! meets its goal and halved when one misses (TCP's additive increase, multiplicative decrease),
//! and from it, each group's share of time. Pure: times are milliseconds handed in, so it's tested
//! without waiting.

use super::settings::Settings;

/// Who yields first: the lowest class is slowed first, and a person is never held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Background work: benches, sync.
    Background,
    /// What an agent runs on its own (its turns, its tools' commands).
    Agent,
    /// A person's own message (sent "now"): never held.
    Person,
}

/// The window and how long targets have been healthy or not.
#[derive(Debug, Clone)]
pub struct Control {
    w: f64,
    last_cut: Option<u64>,
    last_seen: Option<u64>,
    /// Since when every target has met its goal (`None`: one misses now).
    healthy_since: Option<u64>,
    /// Since when a target has missed (`None`: all meet their goals).
    missing_since: Option<u64>,
}

impl Control {
    pub fn new(s: &Settings) -> Self {
        Self { w: f64::from(s.window.start), last_cut: None, last_seen: None, healthy_since: None, missing_since: None }
    }

    /// One look at the targets at `now`: `healthy` when each met its goal. True when the window
    /// was cut.
    pub fn observe(&mut self, s: &Settings, now: u64, healthy: bool) -> bool {
        let (min, max) = (f64::from(s.window.min), f64::from(s.window.max));
        let dt = self.last_seen.map_or(0, |t| now.saturating_sub(t));
        self.last_seen = Some(now);
        let mut cut = false;
        if healthy {
            self.missing_since = None;
            self.healthy_since.get_or_insert(now);
            self.w += dt as f64 / (f64::from(s.window.grow_secs) * 1000.0);
        } else {
            self.healthy_since = None;
            self.missing_since.get_or_insert(now);
            if self.last_cut.is_none_or(|t| now.saturating_sub(t) >= u64::from(s.window.cooldown_ms)) {
                self.w /= 2.0;
                self.last_cut = Some(now);
                cut = true;
            }
        }
        self.w = self.w.clamp(min, max);
        cut
    }

    /// Heavy groups that may run at full speed now.
    pub fn allowed(&self) -> usize {
        self.w.floor().max(1.0) as usize
    }

    pub fn window(&self) -> f64 {
        self.w
    }

    pub fn healthy(&self) -> bool {
        self.missing_since.is_none()
    }

    /// A target has missed this long without a break.
    pub fn missing_for(&self, now: u64) -> u64 {
        self.missing_since.map_or(0, |t| now.saturating_sub(t))
    }

    /// Every target has met its goal this long.
    pub fn healthy_for(&self, now: u64) -> u64 {
        self.healthy_since.map_or(0, |t| now.saturating_sub(t))
    }

    /// Restart from the settings' start (new settings).
    pub fn reset(&mut self, s: &Settings) {
        self.w = f64::from(s.window.start).clamp(f64::from(s.window.min), f64::from(s.window.max));
    }
}

/// A group of processes, as the planner sees it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub class: Class,
    pub heavy: bool,
    /// Bytes written a second (for which to slow first: the heaviest writer).
    pub write_rate: f64,
    /// Its share now, in percent (100: not paced).
    pub duty: u8,
}

/// Each group's share of time from here (percent), in `groups`' order. Groups past the window are
/// slowed, the lowest class and then the heaviest writer first; the rest run at full speed. A
/// paced group gets back to full speed only once targets have been healthy for
/// `release_after_secs` (no flapping); it goes to 0% once they've missed for `freeze_after_secs`
/// with the window at its least.
pub fn plan(s: &Settings, c: &Control, now: u64, groups: &[Seen]) -> Vec<u8> {
    if !s.enabled || !s.pacing.enabled {
        return vec![100; groups.len()];
    }
    let released = c.healthy_for(now) >= u64::from(s.pacing.release_after_secs) * 1000;
    // Heavy work, in the order it's slowed in.
    let mut heavy: Vec<(usize, &Seen)> = groups.iter().enumerate().filter(|(_, g)| g.heavy && g.class != Class::Person).collect();
    heavy.sort_by(|(_, a), (_, b)| a.class.cmp(&b.class).then(b.write_rate.total_cmp(&a.write_rate)));
    let excess = heavy.len().saturating_sub(c.allowed());
    let mut rank: Vec<Option<usize>> = vec![None; groups.len()];
    for (r, (i, _)) in heavy.iter().enumerate() {
        if let Some(slot) = rank.get_mut(*i) {
            *slot = Some(r);
        }
    }
    let frozen = !c.healthy() && c.window() <= f64::from(s.window.min) && c.missing_for(now) >= u64::from(s.pacing.freeze_after_secs) * 1000;
    let duty = if frozen {
        0
    } else if c.healthy() {
        50.max(s.pacing.min_duty_pct)
    } else {
        s.pacing.min_duty_pct
    };
    groups
        .iter()
        .zip(rank)
        .map(|(g, r)| match r {
            // Past the window: slowed further while targets miss; never sped up until released.
            Some(r) if r < excess => {
                if released { 100 } else { g.duty.min(duty) }
            }
            // Within it: what was slowed stays so until released.
            Some(_) => {
                if released || g.duty == 100 { 100 } else { g.duty }
            }
            // Not heavy (any more), or a person's: runs once released, a person's always.
            None => {
                if released || g.class == Class::Person { 100 } else { g.duty }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heavy(class: Class, rate: f64, duty: u8) -> Seen {
        Seen { class, heavy: true, write_rate: rate, duty }
    }

    #[test]
    fn healthy_targets_grow_the_window_and_a_miss_halves_it_once_per_cooldown() {
        let s = Settings::default();
        let mut c = Control::new(&s);
        assert_eq!(c.allowed(), 2);
        let mut t = 0;
        for _ in 0..84 {
            t += 250;
            c.observe(&s, t, true);
        }
        // Over 20 s healthy at one per 10 s: two more.
        assert_eq!(c.allowed(), 4);
        assert!(c.observe(&s, t + 250, false));
        assert_eq!(c.allowed(), 2);
        // Within the cooldown, another miss doesn't cut again.
        assert!(!c.observe(&s, t + 500, false));
        assert_eq!(c.allowed(), 2);
        assert!(c.observe(&s, t + 2500, false));
        assert_eq!(c.allowed(), 1, "never under the least");
        assert!(c.observe(&s, t + 5000, false));
        assert_eq!(c.allowed(), 1);
    }

    #[test]
    fn past_the_window_the_heaviest_agent_work_is_slowed_first_and_people_never() {
        let s = Settings::default();
        let mut c = Control::new(&s);
        c.observe(&s, 0, false);
        let groups = [heavy(Class::Agent, 1e6, 100), heavy(Class::Agent, 9e6, 100), heavy(Class::Person, 50e6, 100), heavy(Class::Background, 1.0, 100)];
        let d = plan(&s, &c, 100, &groups);
        // Window 1, three heavy non-person groups: two slowed, background first, then the heavier agent.
        assert_eq!(d, vec![100, 10, 100, 10]);
    }

    #[test]
    fn long_misses_at_the_least_window_stop_paced_work_and_health_releases_it_slowly() {
        let s = Settings::default();
        let mut c = Control::new(&s);
        let groups = vec![heavy(Class::Agent, 1.0, 100), heavy(Class::Agent, 2.0, 100)];
        let mut t = 0;
        let mut d = plan(&s, &c, t, &groups);
        while t < 25_000 {
            t += 250;
            c.observe(&s, t, false);
            let g: Vec<Seen> = groups.iter().zip(&d).map(|(g, &duty)| Seen { duty, ..g.clone() }).collect();
            d = plan(&s, &c, t, &g);
        }
        assert_eq!(d, vec![100, 0], "frozen after 20 s of misses at the least window");
        // Healthy again: not released before 30 s.
        for _ in 0..40 {
            t += 250;
            c.observe(&s, t, true);
            let g: Vec<Seen> = groups.iter().zip(&d).map(|(g, &duty)| Seen { duty, ..g.clone() }).collect();
            d = plan(&s, &c, t, &g);
        }
        assert_eq!(d.get(1), Some(&0), "10 s of health isn't enough");
        for _ in 0..100 {
            t += 250;
            c.observe(&s, t, true);
            let g: Vec<Seen> = groups.iter().zip(&d).map(|(g, &duty)| Seen { duty, ..g.clone() }).collect();
            d = plan(&s, &c, t, &g);
        }
        assert_eq!(d, vec![100, 100]);
    }

    #[test]
    fn off_means_full_speed() {
        let s = Settings { enabled: false, ..Settings::default() };
        let c = Control::new(&s);
        assert_eq!(plan(&s, &c, 0, &[heavy(Class::Agent, 1.0, 10)]), vec![100]);
    }
}
