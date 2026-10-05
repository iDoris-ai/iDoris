use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const FAILURE_THRESHOLD: u8 = 3;
const COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct State {
    failures: u8,
    cooldown_until: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct HealthTracker {
    state: Mutex<HashMap<String, State>>,
}

impl HealthTracker {
    pub fn is_cooling_down(&self, id: &str, now: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .and_then(|s| s.cooldown_until)
            .is_some_and(|until| until > now)
    }

    pub fn record(&self, id: &str, ok: bool, now: Instant) {
        let mut states = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = states.entry(id.to_owned()).or_default();
        if ok {
            *state = State::default();
        } else {
            state.failures = state.failures.saturating_add(1);
            if state.failures >= FAILURE_THRESHOLD {
                state.cooldown_until = Some(now + COOLDOWN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HealthTracker;
    use std::time::{Duration, Instant};

    #[test]
    fn cooldown_boundary_and_repeated_failure() {
        let tracker = HealthTracker::default();
        let now = Instant::now();
        for _ in 0..3 {
            tracker.record("p", false, now);
        }

        assert!(tracker.is_cooling_down("p", now + Duration::from_millis(29_999)));
        assert!(!tracker.is_cooling_down("p", now + Duration::from_millis(30_000)));
        tracker.record("p", false, now + Duration::from_secs(30));
        assert!(tracker.is_cooling_down("p", now + Duration::from_millis(59_999)));
        assert!(!tracker.is_cooling_down("p", now + Duration::from_secs(60)));
    }

    #[test]
    fn success_resets_failures_and_cooldown() {
        let tracker = HealthTracker::default();
        let now = Instant::now();
        for _ in 0..3 {
            tracker.record("p", false, now);
        }
        tracker.record("p", true, now + Duration::from_secs(1));
        assert!(!tracker.is_cooling_down("p", now + Duration::from_secs(1)));
        for _ in 0..2 {
            tracker.record("p", false, now + Duration::from_secs(2));
        }
        assert!(!tracker.is_cooling_down("p", now + Duration::from_secs(2)));
    }

    #[test]
    fn provider_states_are_independent() {
        let tracker = HealthTracker::default();
        let now = Instant::now();
        for _ in 0..3 {
            tracker.record("p", false, now);
        }
        tracker.record("q", true, now);
        assert!(tracker.is_cooling_down("p", now));
        assert!(!tracker.is_cooling_down("q", now));
    }

    #[test]
    fn first_two_failures_do_not_cool_down() {
        let tracker = HealthTracker::default();
        let now = Instant::now();
        tracker.record("p", false, now);
        assert!(!tracker.is_cooling_down("p", now));
        tracker.record("p", false, now);
        assert!(!tracker.is_cooling_down("p", now));
    }
}
