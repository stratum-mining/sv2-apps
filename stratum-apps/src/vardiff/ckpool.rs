//! ckpool-style variable difficulty.
//!
//! Ported from ckpool `src/stratifier.c` `add_submit()` and the supporting
//! helpers `decay_client` / `decay_time` / `time_bias` / `sane_tdiff` in
//! `libckpool.c`.
//!
//! ## ckpool fixed endpoint (reference)
//!
//! ckpool targets **0.3 shares/second** (18 shares/minute). It re-evaluates
//! difficulty after **72 shares** or **240 seconds** (whichever first), using:
//!
//! - 1-minute rolling dsps when share count is high (`ssdc >= 72`)
//! - 5-minute rolling dsps otherwise
//! - hysteresis band on rate ratio `drr = dsps / difficulty` of `[0.15, 0.4]`
//! - `optimal_diff = round(dsps * 3.33)`  (`1 / 0.3`)
//!
//! ## Configurable shares-per-minute
//!
//! Let `R = shares_per_minute / 60` (target shares per second). Then:
//!
//! | ckpool constant | generalized form |
//! |---|---|
//! | 72 shares | `shares_per_minute * 4`  (shares expected in 240s) |
//! | 240 seconds | 240 seconds (unchanged) |
//! | drr band `[0.15, 0.4]` | `[0.5·R, (4/3)·R]` |
//! | `dsps * 3.33` | `dsps / R` |
//!
//! The result is expressed as a new **nominal hashrate** so existing
//! `hash_rate_to_target` / `SetTarget` call sites keep working.

use std::time::{Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

use stratum_core::{
    bitcoin::Target,
    channels_sv2::{vardiff::error::VardiffError, Vardiff},
};
use tracing::debug;

/// Default minimum hashrate (H/s) if not specified.
const DEFAULT_MIN_HASHRATE: f32 = 1.0;

/// Rolling-average window for the 1-minute dsps series (ckpool `MIN1`).
const MIN1_SECS: f64 = 60.0;
/// Rolling-average window for the 5-minute dsps series (ckpool `MIN5`).
const MIN5_SECS: f64 = 300.0;
/// Minimum elapsed time between decay updates (ckpool batches sub-50ms).
const MIN_DECAY_INTERVAL_SECS: f64 = 0.05;
/// ckpool re-check wall-clock window ("every 240 seconds…").
const CHECK_PERIOD_SECS: f64 = 240.0;
/// ckpool lower/upper drr hysteresis multipliers relative to the 0.3 sps target:
/// `0.15 / 0.3 = 0.5`, `0.4 / 0.3 = 4/3`.
const DRR_LOW_FACTOR: f64 = 0.5;
const DRR_HIGH_FACTOR: f64 = 4.0 / 3.0;
/// Floor under which a decaying average is zeroed (ckpool underflow guard).
const DSPS_UNDERFLOW: f64 = 2e-16;
/// Relative tolerance when comparing target difficulty to our last known value.
const DIFF_MATCH_EPSILON: f64 = 0.01;

/// ckpool-style variable difficulty state.
///
/// Tracks exponentially-weighted difficulty-shares-per-second (`dsps1` /
/// `dsps5`) and applies the `add_submit()` adjustment rules against a
/// configurable shares-per-minute target.
#[derive(Debug)]
pub struct VardiffState {
    /// 1-minute rolling difficulty-shares per second.
    dsps1: f64,
    /// 5-minute rolling difficulty-shares per second.
    dsps5: f64,
    /// Difficulty accumulated while shares arrive faster than the decay interval.
    uadiff: f64,
    /// Shares since the last difficulty change (`ssdc` in ckpool).
    ssdc: u32,
    /// Shares observed before `current_diff` was known (flushed on first evaluate).
    pending_shares: u32,
    /// Last difficulty we believe is active on the channel.
    current_diff: f64,
    /// Instant of the first recorded share (for bias / lifetime).
    first_share: Option<Instant>,
    /// Instant of the last difficulty change (`ldc` in ckpool).
    last_diff_change: Instant,
    /// Instant of the last decay update.
    last_decay: Instant,
    /// Unix timestamp of the last difficulty change (trait compatibility).
    timestamp_of_last_update: u64,
    /// Shares since last update (mirrors `ssdc` for the Vardiff trait).
    shares_since_last_update: u32,
    /// Lowest hashrate (H/s) the system will allow after an adjustment.
    min_allowed_hashrate: f32,
}

impl VardiffState {
    /// Creates a new `VardiffState` with the default minimum hashrate.
    pub fn new() -> Result<Self, VardiffError> {
        Self::new_with_min(DEFAULT_MIN_HASHRATE)
    }

    /// Creates a new `VardiffState` with a specific minimum hashrate.
    pub fn new_with_min(min_allowed_hashrate: f32) -> Result<Self, VardiffError> {
        let now_instant = Instant::now();
        let timestamp_secs = unix_now()?;

        Ok(Self {
            dsps1: 0.0,
            dsps5: 0.0,
            uadiff: 0.0,
            ssdc: 0,
            pending_shares: 0,
            current_diff: 0.0,
            first_share: None,
            last_diff_change: now_instant,
            last_decay: now_instant,
            timestamp_of_last_update: timestamp_secs,
            shares_since_last_update: 0,
            min_allowed_hashrate,
        })
    }

    /// Sets the count of shares since the last update.
    pub fn set_shares_since_last_update(&mut self, shares_since_last_update: u32) {
        self.shares_since_last_update = shares_since_last_update;
        self.ssdc = shares_since_last_update;
    }

    /// Records a share of the given difficulty into the rolling averages.
    ///
    /// Mirrors ckpool `decay_client()` + the `ssdc++` / first-share bookkeeping
    /// from `add_submit()`.
    fn on_share(&mut self, diff: f64, now: Instant) {
        if self.first_share.is_none() {
            self.first_share = Some(now);
            self.last_diff_change = now;
            self.last_decay = now;
        }

        self.ssdc = self.ssdc.saturating_add(1);
        self.shares_since_last_update = self.ssdc;
        self.decay_client(diff, now);
    }

    /// ckpool `decay_client`: exponential decay of dsps1/dsps5.
    fn decay_client(&mut self, mut diff: f64, now: Instant) {
        let tdiff = sane_tdiff(now.duration_since(self.last_decay).as_secs_f64());

        // Batch sub-50ms updates like ckpool.
        if tdiff < MIN_DECAY_INTERVAL_SECS {
            self.uadiff += diff;
            return;
        }

        self.last_decay = now;
        diff += self.uadiff;
        self.uadiff = 0.0;
        decay_time(&mut self.dsps1, diff, tdiff, MIN1_SECS);
        decay_time(&mut self.dsps5, diff, tdiff, MIN5_SECS);
    }

    /// Flush any unaccounted difficulty into the averages (before evaluating).
    fn flush_uadiff(&mut self, now: Instant) {
        if self.uadiff <= 0.0 {
            return;
        }
        let tdiff = sane_tdiff(now.duration_since(self.last_decay).as_secs_f64());
        // Force a decay even if under the 50ms gate so try_vardiff sees fresh data.
        self.last_decay = now;
        let diff = self.uadiff;
        self.uadiff = 0.0;
        decay_time(&mut self.dsps1, diff, tdiff, MIN1_SECS);
        decay_time(&mut self.dsps5, diff, tdiff, MIN5_SECS);
    }

    /// ckpool `add_submit` difficulty decision, returning a new nominal hashrate.
    fn evaluate(
        &mut self,
        hashrate: f32,
        target_diff: f64,
        shares_per_minute: f32,
        now: Instant,
    ) -> Option<f32> {
        if shares_per_minute <= 0.0 || !shares_per_minute.is_finite() {
            return None;
        }
        if target_diff <= 0.0 || !target_diff.is_finite() {
            return None;
        }
        if hashrate <= 0.0 || !hashrate.is_finite() {
            return None;
        }

        self.flush_uadiff(now);

        // Adopt the channel difficulty if we have never seen one yet, and
        // flush any shares that arrived before difficulty was known.
        //
        // `pending_shares` already bumped `ssdc` / `shares_since_last_update`,
        // so only fold their difficulty-work into the rolling averages here.
        if self.current_diff <= 0.0 {
            self.current_diff = target_diff;
            let pending = self.pending_shares;
            self.pending_shares = 0;
            if pending > 0 {
                if self.first_share.is_none() {
                    self.first_share = Some(now);
                    self.last_diff_change = now;
                    self.last_decay = now;
                }
                // Attribute the batched work as a single decay sample so we
                // don't re-increment ssdc.
                self.decay_client(target_diff * pending as f64, now);
            }
        }

        // Share difficulty diverged from the difficulty we last assigned —
        // ckpool resets `ssdc` and skips this evaluation.
        if !diffs_match(self.current_diff, target_diff) {
            debug!(
                target: "vardiff",
                "Channel difficulty changed externally ({:.6} -> {:.6}); resetting ssdc",
                self.current_diff, target_diff
            );
            self.current_diff = target_diff;
            self.ssdc = 0;
            self.shares_since_last_update = 0;
            return None;
        }

        let first_share = self.first_share?;

        let bdiff = sane_tdiff(now.duration_since(first_share).as_secs_f64());
        let tdiff = sane_tdiff(now.duration_since(self.last_diff_change).as_secs_f64());

        // Shares expected in the 240s check window at the configured rate.
        let share_threshold = shares_for_check_period(shares_per_minute);

        // ckpool: if (ssdc < 72 && tdiff < 240) return;
        if (self.ssdc as f64) < share_threshold && tdiff < CHECK_PERIOD_SECS {
            return None;
        }

        let target_sps = shares_per_minute as f64 / 60.0;

        // Diff rate ratio. Fast path uses 1-minute average once we've seen a
        // full check-window of shares; otherwise the 5-minute average.
        let (dsps, bias_period) = if (self.ssdc as f64) >= share_threshold {
            (self.dsps1, MIN1_SECS)
        } else {
            (self.dsps5, MIN5_SECS)
        };
        let bias = time_bias(bdiff, bias_period);
        if bias <= 0.0 {
            return None;
        }
        let dsps = dsps / bias;
        let drr = dsps / target_diff;

        let drr_low = target_sps * DRR_LOW_FACTOR;
        let drr_high = target_sps * DRR_HIGH_FACTOR;

        // Optimal rate product is R (= target_sps); allow hysteresis.
        if drr > drr_low && drr < drr_high {
            debug!(
                target: "vardiff",
                "drr {:.4} within hysteresis [{:.4}, {:.4}] (spm={:.2}); no change",
                drr, drr_low, drr_high, shares_per_minute
            );
            return None;
        }

        // optimal = dsps / R  (= dsps * 60 / spm). ckpool: lround(dsps * 3.33).
        //
        // ckpool difficulties are integer pool-diff units. Bitcoin
        // `difficulty_float()` values for share targets are often << 1, so
        // rounding to the nearest integer would collapse everything to 0/1.
        // Keep a floating optimal and only enforce a tiny positive floor.
        let mut optimal = dsps / target_sps;
        if optimal < 1e-12 {
            return None;
        }

        // No-op if the relative change is negligible.
        if (optimal - target_diff).abs() / target_diff.max(optimal) < DIFF_MATCH_EPSILON {
            return None;
        }

        // First share after a long idle: don't drop difficulty immediately.
        // ckpool: if (optimal < client->diff && client->ssdc == 1) { reset ldc; return; }
        if optimal < target_diff && self.ssdc == 1 {
            self.last_diff_change = now;
            if let Ok(ts) = unix_now() {
                self.timestamp_of_last_update = ts;
            }
            return None;
        }

        // Convert optimal difficulty → new nominal hashrate, preserving the
        // hashrate↔difficulty relationship established by hash_rate_to_target:
        //   hashrate ∝ difficulty  (at fixed shares_per_minute)
        let mut new_hashrate = hashrate as f64 * (optimal / target_diff);
        if new_hashrate < self.min_allowed_hashrate as f64 {
            new_hashrate = self.min_allowed_hashrate as f64;
            // Re-derive optimal so current_diff stays consistent with the clamp.
            optimal = target_diff * (new_hashrate / hashrate as f64);
        }

        debug!(
            target: "vardiff",
            "ckpool vardiff adjust: biased_dsps={:.4} drr={:.4} spm={:.2} \
             ssdc={} tdiff={:.1}s diff {:.6} -> {:.6} hashrate {:.2} -> {:.2} H/s",
            dsps,
            drr,
            shares_per_minute,
            self.ssdc,
            tdiff,
            target_diff,
            optimal,
            hashrate,
            new_hashrate
        );

        self.ssdc = 0;
        self.shares_since_last_update = 0;
        self.current_diff = optimal;
        self.last_diff_change = now;
        if let Ok(ts) = unix_now() {
            self.timestamp_of_last_update = ts;
        }

        Some(new_hashrate as f32)
    }
}

impl Vardiff for VardiffState {
    fn last_update_timestamp(&self) -> u64 {
        self.timestamp_of_last_update
    }

    fn shares_since_last_update(&self) -> u32 {
        self.shares_since_last_update
    }

    fn min_allowed_hashrate(&self) -> f32 {
        self.min_allowed_hashrate
    }

    fn set_timestamp_of_last_update(&mut self, timestamp_of_last_update: u64) {
        self.timestamp_of_last_update = timestamp_of_last_update;
    }

    fn increment_shares_since_last_update(&mut self) {
        // Use the last known channel difficulty. If try_vardiff has not run
        // yet (current_diff == 0), queue the share and flush it once evaluate()
        // learns the real target difficulty.
        if self.current_diff > 0.0 {
            self.on_share(self.current_diff, Instant::now());
        } else {
            self.pending_shares = self.pending_shares.saturating_add(1);
            self.ssdc = self.ssdc.saturating_add(1);
            self.shares_since_last_update = self.ssdc;
        }
    }

    fn reset_counter(&mut self) -> Result<(), VardiffError> {
        let timestamp_secs = unix_now()?;
        self.timestamp_of_last_update = timestamp_secs;
        self.shares_since_last_update = 0;
        self.ssdc = 0;
        self.last_diff_change = Instant::now();
        Ok(())
    }

    fn try_vardiff(
        &mut self,
        hashrate: f32,
        target: &Target,
        shares_per_minute: f32,
    ) -> Result<Option<f32>, VardiffError> {
        let target_diff = target.difficulty_float();
        let now = Instant::now();
        Ok(self.evaluate(hashrate, target_diff, shares_per_minute, now))
    }
}

/// Shares expected during [`CHECK_PERIOD_SECS`] at the configured rate.
///
/// ckpool: 18 spm × 4 min = 72.
fn shares_for_check_period(shares_per_minute: f32) -> f64 {
    shares_per_minute as f64 * (CHECK_PERIOD_SECS / 60.0)
}

/// ckpool `decay_time`: exponentially decaying average over `interval` seconds.
fn decay_time(f: &mut f64, fadd: f64, fsecs: f64, interval: f64) {
    if fsecs <= 0.0 {
        return;
    }
    let mut dexp = fsecs / interval;
    // Sanity bound matching ckpool.
    if dexp > 36.0 {
        dexp = 36.0;
    }
    let fprop = 1.0 - 1.0 / dexp.exp();
    let ftotal = 1.0 + fprop;
    *f += (fadd / fsecs) * fprop;
    *f /= ftotal;
    if *f < DSPS_UNDERFLOW {
        *f = 0.0;
    }
}

/// ckpool `time_bias`: `1 - exp(-tdiff/period)`, used to un-bias early averages.
fn time_bias(tdiff: f64, period: f64) -> f64 {
    let mut dexp = tdiff / period;
    if dexp > 36.0 {
        dexp = 36.0;
    }
    1.0 - 1.0 / dexp.exp()
}

/// ckpool `sane_tdiff`: clamp tiny/negative elapsed times.
fn sane_tdiff(tdiff: f64) -> f64 {
    if tdiff < 0.001 {
        0.001
    } else {
        tdiff
    }
}

fn diffs_match(a: f64, b: f64) -> bool {
    if a <= 0.0 || b <= 0.0 {
        return false;
    }
    let rel = (a - b).abs() / a.max(b);
    rel <= DIFF_MATCH_EPSILON
}

fn unix_now() -> Result<u64, SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use stratum_core::channels_sv2::target::hash_rate_to_target;

    const SPM_CKPOOL: f32 = 18.0;
    const SPM_CUSTOM: f32 = 6.0;

    fn target_for(hashrate: f32, spm: f32) -> Target {
        hash_rate_to_target(hashrate as f64, spm as f64)
            .expect("hash_rate_to_target")
            .into()
    }

    /// Drive N shares of the given difficulty into state with a fixed dt.
    fn feed_shares(state: &mut VardiffState, n: u32, diff: f64, dt_secs: f64) {
        // Advance last_decay into the past so each on_share decays cleanly.
        state.last_decay = Instant::now() - Duration::from_secs_f64(dt_secs.max(0.1));
        for _ in 0..n {
            // Space shares by dt_secs using last_decay manipulation.
            state.last_decay = Instant::now() - Duration::from_secs_f64(dt_secs.max(0.1));
            state.on_share(diff, Instant::now());
        }
    }

    #[test]
    fn share_threshold_matches_ckpool_at_18_spm() {
        assert!((shares_for_check_period(18.0) - 72.0).abs() < f64::EPSILON);
        assert!((shares_for_check_period(6.0) - 24.0).abs() < f64::EPSILON);
        assert!((shares_for_check_period(120.0) - 480.0).abs() < f64::EPSILON);
    }

    #[test]
    fn decay_time_and_time_bias_basic() {
        let mut f = 0.0;
        // One unit of work over 1 second into a 60s window.
        decay_time(&mut f, 1.0, 1.0, MIN1_SECS);
        assert!(f > 0.0);

        let bias = time_bias(60.0, 60.0);
        // 1 - e^{-1} ≈ 0.632
        assert!((bias - (1.0 - (-1.0f64).exp())).abs() < 1e-9);
    }

    #[test]
    fn no_update_before_threshold() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;

        // Far fewer than 72 shares and well under 240s.
        feed_shares(&mut state, 10, diff, 1.0);
        state.last_diff_change = Instant::now();

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        assert!(
            result.is_none(),
            "should not adjust before share/time threshold"
        );
    }

    #[test]
    fn hysteresis_holds_near_target_rate() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        state.first_share = Some(Instant::now() - Duration::from_secs(600));
        state.last_diff_change = Instant::now() - Duration::from_secs(300);

        // Target rate is 18 spm = 0.3 sps. Feed ~72 shares over 240s → 0.3 sps.
        // Each share contributes `diff` difficulty-work, so dsps ≈ 0.3 * diff,
        // drr ≈ 0.3 which sits inside [0.15, 0.4].
        let interval = 240.0 / 72.0;
        feed_shares(&mut state, 72, diff, interval);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        assert!(
            result.is_none(),
            "at target rate, drr should be inside hysteresis"
        );
    }

    #[test]
    fn fast_shares_raise_hashrate() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        state.first_share = Some(Instant::now() - Duration::from_secs(600));
        state.last_diff_change = Instant::now() - Duration::from_secs(300);

        // ~4× target rate: 72 shares in ~60s.
        feed_shares(&mut state, 72, diff, 60.0 / 72.0);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        let new_hr = result.expect("should raise difficulty/hashrate when shares are fast");
        assert!(
            new_hr > hashrate,
            "new hashrate {new_hr} should exceed previous {hashrate}"
        );
        assert_eq!(state.ssdc, 0);
    }

    #[test]
    fn slow_shares_lower_hashrate() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        state.first_share = Some(Instant::now() - Duration::from_secs(900));
        // Past the 240s window with few shares.
        state.last_diff_change = Instant::now() - Duration::from_secs(300);

        // Only a handful of shares over a long period → low drr.
        feed_shares(&mut state, 5, diff, 50.0);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        let new_hr = result.expect("should lower difficulty/hashrate when shares are slow");
        assert!(
            new_hr < hashrate,
            "new hashrate {new_hr} should be below previous {hashrate}"
        );
    }

    #[test]
    fn configurable_spm_scales_thresholds() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CUSTOM);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        state.first_share = Some(Instant::now() - Duration::from_secs(600));
        state.last_diff_change = Instant::now() - Duration::from_secs(300);

        // At 6 spm, share threshold is 24. Feed 24 shares at ~4× rate (24 in 60s).
        feed_shares(&mut state, 24, diff, 60.0 / 24.0);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CUSTOM)
            .expect("try_vardiff");
        let new_hr = result.expect("should adjust with configurable SPM threshold of 24");
        assert!(new_hr > hashrate);
    }

    #[test]
    fn first_share_after_idle_does_not_drop() {
        let mut state = VardiffState::new().unwrap();
        let hashrate = 1_000_000.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        // Establish some historical dsps, then go idle, then one slow share.
        state.first_share = Some(Instant::now() - Duration::from_secs(900));
        state.last_diff_change = Instant::now() - Duration::from_secs(300);
        // Seed a low dsps so optimal < current.
        state.dsps5 = diff * 0.01; // very low rate
        state.ssdc = 0;
        state.shares_since_last_update = 0;

        // Single share after long absence.
        feed_shares(&mut state, 1, diff, 300.0);
        assert_eq!(state.ssdc, 1);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        assert!(
            result.is_none(),
            "ckpool refuses to drop diff on the first share after absence"
        );
    }

    #[test]
    fn trait_increment_and_reset() {
        let mut state = VardiffState::new().unwrap();
        state.current_diff = 16.0;
        state.increment_shares_since_last_update();
        state.increment_shares_since_last_update();
        assert_eq!(state.shares_since_last_update(), 2);
        state.reset_counter().unwrap();
        assert_eq!(state.shares_since_last_update(), 0);
        assert_eq!(state.ssdc, 0);
    }

    #[test]
    fn clamps_to_min_allowed_hashrate() {
        let min_hr = 1000.0_f32;
        let mut state = VardiffState::new_with_min(min_hr).unwrap();
        let hashrate = 1500.0_f32;
        let target = target_for(hashrate, SPM_CKPOOL);
        let diff = target.difficulty_float();
        state.current_diff = diff;
        state.first_share = Some(Instant::now() - Duration::from_secs(900));
        state.last_diff_change = Instant::now() - Duration::from_secs(300);
        // Extremely low rate.
        state.dsps5 = diff * 1e-6;
        feed_shares(&mut state, 3, diff, 80.0);

        let result = state
            .try_vardiff(hashrate, &target, SPM_CKPOOL)
            .expect("try_vardiff");
        if let Some(new_hr) = result {
            assert!(
                new_hr >= min_hr,
                "hashrate {new_hr} must not fall below min {min_hr}"
            );
        }
    }
}
