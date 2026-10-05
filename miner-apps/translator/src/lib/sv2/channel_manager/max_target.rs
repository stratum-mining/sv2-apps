//! Tracks the `max_target` an upstream channel must respect when it sends `SetTarget`.
//!
//! `SetTarget.target` must not exceed the `max_target` of the most recent `UpdateChannel` the
//! upstream accepted for the channel, or of the request that opened it if there has been none.
//! An accepted `UpdateChannel` has no response, so tProxy cannot tell when a new `max_target`
//! starts to apply. It gives the upstream a grace period to process each `UpdateChannel`: a value
//! sent longer ago than that is in force, unless the upstream rejected it with
//! `UpdateChannel.Error`. A `SetTarget` is checked against the easiest of the value in force and
//! every value sent within the grace period, since the upstream may not have processed those yet.
//! A `SetTarget` within a newer value does not change that, since it may also have been sent
//! before the upstream processed the change. A target above all of them is a violation.
//!
//! The spec says a client SHOULD NOT change `max_target` again before the grace period has
//! passed, otherwise it cannot tell which value a `SetTarget` is bound by. tProxy sends every
//! change right away instead, so the upstream learns each new value without waiting, and resolves
//! that ambiguity by checking against the easiest value the `SetTarget` may be bound by. A
//! compliant `SetTarget` is therefore never treated as a violation.
//!
//! `UpdateChannel.Error` does not say which `UpdateChannel` it rejects. With a single value sent
//! within the grace period, the error rejects that value and the one in force stays. With several,
//! any of them may have been rejected, so the easiest of them stays in force until the next change.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use stratum_apps::stratum_core::bitcoin::Target;

/// Time the upstream is given to process an `UpdateChannel`.
///
/// It covers the delivery and processing of the `UpdateChannel` itself: a `SetTarget` the
/// upstream sends after accepting it must already respect the new value.
pub(super) const MAX_TARGET_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// The `max_target` bound of one upstream channel.
#[derive(Clone, Debug)]
pub(super) struct UpstreamMaxTarget {
    /// The `max_target` known to be in force.
    in_force: Target,
    /// `max_target` values sent within the grace period, oldest first, with when they were sent.
    recent: VecDeque<(Target, Instant)>,
}

/// A `SetTarget` easier than every `max_target` the upstream can be bound by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MaxTargetViolation {
    pub(super) target: Target,
    pub(super) bound: Target,
}

impl UpstreamMaxTarget {
    /// Starts tracking a channel opened with `max_target`.
    pub(super) fn new(max_target: Target) -> Self {
        Self {
            in_force: max_target,
            recent: VecDeque::new(),
        }
    }

    /// Records the `max_target` of an `UpdateChannel` sent to the upstream.
    pub(super) fn on_update_channel(&mut self, max_target: Target, now: Instant, grace: Duration) {
        self.expire(now, grace);
        self.recent.push_back((max_target, now));
    }

    /// Checks a `SetTarget` against the bound.
    pub(super) fn on_set_target(
        &mut self,
        target: Target,
        now: Instant,
        grace: Duration,
    ) -> Result<(), MaxTargetViolation> {
        self.expire(now, grace);
        let bound = self.bound();
        if target > bound {
            return Err(MaxTargetViolation { target, bound });
        }
        Ok(())
    }

    /// Takes back the change the upstream rejected.
    pub(super) fn on_update_channel_error(&mut self, now: Instant, grace: Duration) {
        self.expire(now, grace);
        if self.recent.len() > 1 {
            // Any of them may have been rejected.
            self.in_force = self.bound();
        }
        self.recent.clear();
    }

    /// The easiest `max_target` a `SetTarget` may be bound by.
    fn bound(&self) -> Target {
        self.recent
            .iter()
            .map(|(max_target, _)| *max_target)
            .fold(self.in_force, Target::max)
    }

    /// Puts the values the upstream had the grace period to process in force.
    fn expire(&mut self, now: Instant, grace: Duration) {
        while let Some(&(max_target, sent_at)) = self.recent.front() {
            if now.duration_since(sent_at) < grace {
                break;
            }
            self.in_force = max_target;
            self.recent.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(30);

    /// A larger value is an easier target.
    fn target(value: u8) -> Target {
        let mut bytes = [0; 32];
        bytes[31] = value;
        Target::from_le_bytes(bytes)
    }

    fn violation(target_value: u8, bound_value: u8) -> MaxTargetViolation {
        MaxTargetViolation {
            target: target(target_value),
            bound: target(bound_value),
        }
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn targets_above_the_open_max_target_violate_the_bound() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));

        assert_eq!(bound.on_set_target(target(100), now, GRACE), Ok(()));
        assert_eq!(bound.on_set_target(target(10), now, GRACE), Ok(()));
        assert_eq!(
            bound.on_set_target(target(101), now, GRACE),
            Err(violation(101, 100))
        );
    }

    #[test]
    fn a_crossing_set_target_is_tolerated_during_the_grace_period() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        bound.on_update_channel(target(50), now, GRACE);

        // Possibly sent before the upstream accepted 50.
        assert_eq!(bound.on_set_target(target(80), now, GRACE), Ok(()));
        // Above the previous value, so a violation even during the grace period.
        assert_eq!(
            bound.on_set_target(target(150), now, GRACE),
            Err(violation(150, 100))
        );
    }

    #[test]
    fn a_set_target_within_the_lower_value_does_not_end_the_grace_period() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        bound.on_update_channel(target(50), now, GRACE);

        // Both may have been sent before the upstream accepted 50, while its bound was 100.
        assert_eq!(bound.on_set_target(target(40), now, GRACE), Ok(()));
        assert_eq!(bound.on_set_target(target(80), now, GRACE), Ok(()));
        // Once the grace period runs out, 50 is in force.
        assert_eq!(
            bound.on_set_target(target(80), now + GRACE, GRACE),
            Err(violation(80, 50))
        );
    }

    #[test]
    fn a_rejected_change_keeps_the_previous_value() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        bound.on_update_channel(target(50), now, GRACE);

        bound.on_update_channel_error(now, GRACE);
        assert_eq!(bound.on_set_target(target(80), now + GRACE, GRACE), Ok(()));
        assert_eq!(
            bound.on_set_target(target(101), now + GRACE, GRACE),
            Err(violation(101, 100))
        );
    }

    #[test]
    fn a_higher_value_is_in_force_once_the_grace_period_runs_out() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(50));
        bound.on_update_channel(target(80), now, GRACE);

        // The upstream may already have accepted 80.
        assert_eq!(bound.on_set_target(target(70), now, GRACE), Ok(()));
        assert_eq!(bound.on_set_target(target(80), now + GRACE, GRACE), Ok(()));
        assert_eq!(
            bound.on_set_target(target(81), now + GRACE, GRACE),
            Err(violation(81, 80))
        );
    }

    #[test]
    fn a_rejected_higher_value_keeps_the_previous_one() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(50));
        bound.on_update_channel(target(80), now, GRACE);

        bound.on_update_channel_error(now, GRACE);
        assert_eq!(
            bound.on_set_target(target(70), now, GRACE),
            Err(violation(70, 50))
        );
    }

    #[test]
    fn changes_within_the_grace_period_are_bound_by_the_easiest() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        bound.on_update_channel(target(80), now, GRACE);
        bound.on_update_channel(target(50), now + secs(10), GRACE);

        // The upstream may not have processed either change yet.
        assert_eq!(
            bound.on_set_target(target(100), now + secs(20), GRACE),
            Ok(())
        );
        // It had the grace period to process 80, but maybe not 50.
        assert_eq!(
            bound.on_set_target(target(81), now + secs(30), GRACE),
            Err(violation(81, 80))
        );
        // It had the grace period to process both.
        assert_eq!(
            bound.on_set_target(target(51), now + secs(40), GRACE),
            Err(violation(51, 50))
        );
    }

    #[test]
    fn frequent_changes_do_not_keep_values_older_than_the_grace_period() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        for (sent_after, value) in [(0, 80), (10, 60), (20, 70), (30, 50), (40, 65)] {
            bound.on_update_channel(target(value), now + secs(sent_after), GRACE);
        }

        // 80 and 60 were sent a grace period ago, so 60 is in force; 70, 50 and 65 may not be
        // processed yet. The 100 the channel started from no longer applies.
        assert_eq!(
            bound.on_set_target(target(95), now + secs(40), GRACE),
            Err(violation(95, 70))
        );
    }

    #[test]
    fn a_rejection_among_several_changes_keeps_the_easiest_until_the_next_change() {
        let now = Instant::now();
        let mut bound = UpstreamMaxTarget::new(target(100));
        bound.on_update_channel(target(80), now, GRACE);
        bound.on_update_channel(target(50), now + secs(10), GRACE);

        // Either change may have been rejected.
        bound.on_update_channel_error(now + secs(15), GRACE);
        assert_eq!(
            bound.on_set_target(target(100), now + secs(60), GRACE),
            Ok(())
        );

        bound.on_update_channel(target(40), now + secs(60), GRACE);
        assert_eq!(
            bound.on_set_target(target(41), now + secs(90), GRACE),
            Err(violation(41, 40))
        );
    }
}
