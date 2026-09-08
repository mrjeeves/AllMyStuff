//! Local monotonic durations only: never subtract clocks on different hosts.
//! Matching AU identities let sender/receiver samples describe the same frame.
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct AssemblyClock {
    first: Instant,
    last: Instant,
    max_gap: Duration,
}

impl AssemblyClock {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            first: now,
            last: now,
            max_gap: Duration::ZERO,
        }
    }

    pub(crate) fn observe(&mut self, now: Instant) {
        self.max_gap = self.max_gap.max(now.saturating_duration_since(self.last));
        self.last = now;
    }

    pub(crate) fn finish(mut self, now: Instant) -> (Duration, Duration) {
        self.observe(now); // Include the explicit end marker's wait.
        (now.saturating_duration_since(self.first), self.max_gap)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SendBreakdown {
    pub requested_us: u64,
    pub slept_us: u64,
    pub other_us: u64,
}

pub(crate) fn send_breakdown(total_us: u64, gaps: &[(u64, u64)], write_us: u64) -> SendBreakdown {
    let (requested_us, slept_us) = gaps.iter().fold((0u64, 0u64), |(r, s), &(a, b)| {
        (r.saturating_add(a), s.saturating_add(b))
    });
    SendBreakdown {
        requested_us,
        slept_us,
        other_us: total_us.saturating_sub(slept_us).saturating_sub(write_us),
    }
}

/// Deterministic cross-host sampling, approximately one frame/sec at 60fps.
/// Slow exceptions are separately rate-limited by the route's existing gate.
pub(crate) fn periodic_sample(sequence: Option<u64>) -> bool {
    sequence.is_some_and(|sequence| sequence % 60 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_fragments_can_hide_a_slow_complete_frame() {
        let start = Instant::now();
        let mut clock = AssemblyClock::new(start);
        for ms in (10..=250).step_by(10) {
            clock.observe(start + Duration::from_millis(ms));
        }
        assert_eq!(
            clock.finish(start + Duration::from_millis(260)),
            (Duration::from_millis(260), Duration::from_millis(10))
        );
    }

    #[test]
    fn closing_marker_delay_is_not_lost() {
        let start = Instant::now();
        let mut clock = AssemblyClock::new(start);
        clock.observe(start + Duration::from_millis(10));
        assert_eq!(
            clock.finish(start + Duration::from_millis(180)),
            (Duration::from_millis(180), Duration::from_millis(170))
        );
    }

    #[test]
    fn breakdown_separates_intentional_wait_from_writes_and_other_work() {
        assert_eq!(
            send_breakdown(260_000, &[(100_000, 101_000), (150_000, 151_000)], 5_000),
            SendBreakdown {
                requested_us: 250_000,
                slept_us: 252_000,
                other_us: 3_000
            }
        );
        assert_eq!(send_breakdown(1, &[(5, 6)], 7).other_us, 0);
        assert!(periodic_sample(Some(120)));
        assert!(!periodic_sample(Some(121)));
        assert!(!periodic_sample(None));
    }
}
