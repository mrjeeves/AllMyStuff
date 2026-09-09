//! Bounded frame-completion pacing. Pure monotonic accounting, no IO.
use std::time::{Duration, Instant};

/// A route may spend this much immediately before shaping begins. Four 24 KiB
/// slices preserve a useful keyframe/scene-change kick without letting every
/// captured frame become a fresh unbounded burst.
const VIDEO_PACE_BURST_BYTES: u64 = 96 * 1024;
/// Balanced's 4 Mbps congestion floor still needs enough drain headroom that a
/// large recovery frame does not visibly drag across hundreds of milliseconds.
const VIDEO_PACE_WAN_FLOOR_BPS: u64 = 8_000_000;
const VIDEO_PACE_LAN_FLOOR_BPS: u64 = 16_000_000;
/// For routes whose own target is below this value, recovery headroom stops
/// here. A route explicitly targeting more is never shaped below its average.
const VIDEO_PACE_RECOVERY_CEILING_BPS: u64 = 32_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PacePolicy {
    pub(crate) drain_bps: u64,
    pub(crate) burst_bytes: u64,
}

/// The production shaping policy is rate-relative rather than encoder-CBR:
/// encoders retain the VBV/peak room that prevents blocky motion, while the
/// transport drains that quality burst through one bounded bucket. A target
/// above the recovery ceiling remains load-bearing — shaping below the stated
/// average would only build an endless queue — but a normal Balanced route can
/// no longer turn a 4 Mbps target into a repeated 50+ Mbps wall.
pub(crate) fn pace_policy(game: bool, wan: bool, rate_bps: u32, override_mbps: u64) -> PacePolicy {
    let drain_bps = if override_mbps > 0 {
        override_mbps.max(8) * 1_000_000
    } else if rate_bps == 0 {
        if wan {
            VIDEO_PACE_WAN_FLOOR_BPS
        } else {
            VIDEO_PACE_LAN_FLOOR_BPS
        }
    } else {
        let rate = u64::from(rate_bps);
        let headroom = if game {
            rate.saturating_mul(5) / 4
        } else {
            rate.saturating_mul(3) / 2
        };
        let floor = if wan {
            VIDEO_PACE_WAN_FLOOR_BPS
        } else {
            VIDEO_PACE_LAN_FLOOR_BPS
        };
        headroom
            .max(floor)
            .min(rate.max(VIDEO_PACE_RECOVERY_CEILING_BPS))
    };
    PacePolicy {
        drain_bps,
        burst_bytes: VIDEO_PACE_BURST_BYTES,
    }
}

#[derive(Debug)]
pub(crate) struct PaceRouteState {
    tokens: u64,
    accounted_at: Instant,
}

impl PaceRouteState {
    pub(crate) fn full(now: Instant, policy: PacePolicy) -> Self {
        Self {
            tokens: policy.burst_bytes,
            accounted_at: now,
        }
    }

    /// Reserve `bytes` against a token bucket and return how long the caller
    /// must wait before sending them. `accounted_at` may sit in the future when
    /// a prior reservation is outstanding, making consecutive calls preserve
    /// the same drain schedule instead of each resetting at a frame boundary.
    pub(crate) fn reserve(&mut self, now: Instant, bytes: usize, policy: PacePolicy) -> Duration {
        if now >= self.accounted_at {
            let elapsed_ns = now.duration_since(self.accounted_at).as_nanos();
            let refill =
                elapsed_ns.saturating_mul(u128::from(policy.drain_bps)) / 8_000_000_000u128;
            self.tokens = self
                .tokens
                .saturating_add(refill.min(u128::from(u64::MAX)) as u64)
                .min(policy.burst_bytes);
            self.accounted_at = now;
        }
        self.tokens = self.tokens.min(policy.burst_bytes);
        let bytes = bytes as u64;
        if bytes <= self.tokens {
            self.tokens -= bytes;
            return Duration::ZERO;
        }
        let deficit = bytes - self.tokens;
        self.tokens = 0;
        let wait_us = u128::from(deficit)
            .saturating_mul(8_000_000)
            .div_ceil(u128::from(policy.drain_bps.max(1)))
            .min(u128::from(u64::MAX)) as u64;
        let base = self.accounted_at.max(now);
        self.accounted_at = base + Duration::from_micros(wait_us);
        self.accounted_at.saturating_duration_since(now)
    }
}

/// Aggregate LAN media safety ceiling, shared by all paced LAN routes in a
/// node. This is a ceiling, not a claim about available link bandwidth.
pub(crate) const LAN_AGGREGATE_POLICY: PacePolicy = PacePolicy {
    drain_bps: 256_000_000,
    burst_bytes: VIDEO_PACE_BURST_BYTES,
};

/// A complete picture is the decode unit. Average encoder bitrate is not a
/// useful deadline for a large scene-change picture: the measured 546196-byte
/// AU spent 222ms sleeping at 16Mbps. Give a LAN AU one requested frame interval
/// to drain, without resetting its bucket or enlarging its burst allowance.
/// WAN/unknown paths and explicit diagnostic rate overrides retain their policy.
/// At the shared ceiling, oversized AUs can still take longer than one interval.
pub(crate) fn frame_policy(
    base: PacePolicy,
    wan: bool,
    override_mbps: u64,
    bytes: usize,
    fps: u32,
) -> PacePolicy {
    if wan || override_mbps != 0 {
        return base;
    }
    let frame_rate = (bytes as u64)
        .saturating_mul(8)
        .saturating_mul(u64::from(fps.clamp(1, 240)));
    PacePolicy {
        drain_bps: base
            .drain_bps
            .max(frame_rate)
            .min(LAN_AGGREGATE_POLICY.drain_bps),
        burst_bytes: base.burst_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(
        bucket: &mut PaceRouteState,
        start: Instant,
        bytes: usize,
        policy: PacePolicy,
    ) -> Duration {
        let mut now = start;
        let mut remaining = bytes;
        while remaining > 0 {
            let chunk = remaining.min(24 * 1024);
            now += bucket.reserve(now, chunk, policy);
            remaining -= chunk;
        }
        now.duration_since(start)
    }

    #[test]
    fn measured_scene_change_fits_60fps_without_a_larger_burst() {
        let now = Instant::now();
        let base = pace_policy(false, false, 8_000_000, 0);
        let bytes = 546_196 + 64;
        let old = drain(&mut PaceRouteState::full(now, base), now, bytes, base);
        assert!(old > Duration::from_millis(220));
        let policy = frame_policy(base, false, 0, bytes, 60);
        assert_eq!(policy.burst_bytes, base.burst_bytes);
        let new = drain(&mut PaceRouteState::full(now, policy), now, bytes, policy);
        assert!(new < Duration::from_micros(16_667));
    }

    #[test]
    fn frame_target_and_explicit_wan_limits_are_respected() {
        let base = pace_policy(false, false, 8_000_000, 0);
        assert!(
            frame_policy(base, false, 0, 300_000, 60).drain_bps
                > frame_policy(base, false, 0, 300_000, 30).drain_bps
        );
        assert_eq!(frame_policy(base, true, 0, 600_000, 60), base);
        assert_eq!(frame_policy(base, false, 16, 600_000, 60), base);
        assert_eq!(frame_policy(base, false, 0, 1000, 60), base);
    }

    #[test]
    fn simultaneous_routes_cannot_multiply_the_lan_ceiling() {
        let now = Instant::now();
        let mut shared = PaceRouteState::full(now, LAN_AGGREGATE_POLICY);
        let mut last = Duration::ZERO;
        // Two producers reserve alternating fragments without waiting, as
        // independent routes would. All reservations accumulate in one bucket.
        for _ in 0..80 {
            last = shared.reserve(now, 24 * 1024, LAN_AGGREGATE_POLICY);
        }
        assert_eq!(last, Duration::from_micros(58_368));
    }

    #[test]
    fn consecutive_large_frames_keep_debt_and_idle_refills_only_one_burst() {
        let now = Instant::now();
        let policy = frame_policy(
            pace_policy(false, false, 8_000_000, 0),
            false,
            0,
            546_196,
            60,
        );
        let mut bucket = PaceRouteState::full(now, policy);
        let first = drain(&mut bucket, now, 546_196, policy);
        let second = drain(&mut bucket, now + first, 546_196, policy);
        assert!(second > first);
        assert!(bucket
            .reserve(now + Duration::from_secs(1), 96 * 1024, policy)
            .is_zero());
        assert!(!bucket
            .reserve(now + Duration::from_secs(1), 1, policy)
            .is_zero());
    }
}
