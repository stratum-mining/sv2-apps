//! Upstream selection shared by the miner-side applications.
//!
//! An [`crate::upstream_selector::UpstreamSelector`] holds the configured upstreams in priority
//! order, the first being the primary, and decides which of them may be tried and when:
//!
//! - An upstream fails when a walk spends all [`crate::upstream_selector::ATTEMPTS_PER_UPSTREAM`]
//!   attempts on it, when a probe of it fails, or when it drops less than 10 minutes after it
//!   connected. Dropping after that long, and being left on purpose, are not failures.
//! - After a failure the upstream waits before it is tried again: 10 seconds, doubled on each
//!   consecutive failure, at most 10 minutes, and at most 1 minute while no upstream is connected.
//!   Each wait is stretched or shortened by up to 20 % within those limits, so clients of one pool
//!   do not retry in lockstep. Staying connected for 10 minutes clears the failure count.
//! - No upstream is excluded for good: once its wait is over it is eligible again.
//!
//! [`crate::upstream_selector::UpstreamSelector::walk`] goes through the upstreams in priority
//! order and returns the first connection made.
//! [`crate::upstream_selector::UpstreamSelector::start_probe`] checks the upstreams ranked above
//! the active one, or all of them while none is connected, as their waits run out, and triggers
//! fallback once one answers. Connecting and probing are left to the application, which passes
//! them in.

use std::{
    fmt::{Debug, Display},
    future::Future,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use rand::Rng;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::{fallback_coordinator::FallbackCoordinator, task_manager::TaskManager};
use tokio_util::sync::CancellationToken;

/// Connection attempts a walk makes on one upstream before it moves on to the next.
pub const ATTEMPTS_PER_UPSTREAM: usize = 3;

/// Time allowed for one probe, connecting and setting up included.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(30);

const PAUSE_BEFORE_ATTEMPT: Duration = Duration::from_secs(1);
const FIRST_WAIT: Duration = Duration::from_secs(10);
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);
const MAX_WAIT_WHILE_DISCONNECTED: Duration = Duration::from_secs(60);
const STABLE_CONNECTION: Duration = Duration::from_secs(10 * 60);
const JITTER: f64 = 0.2;

/// The outcome of a connection attempt, and of a whole walk.
#[derive(Debug)]
pub enum Attempt<T, X> {
    /// Connected; carries whatever the application's connect step produced.
    Connected(T),
    /// Not connected. For one attempt the walk tries again or moves on; for a walk, no upstream
    /// could be connected.
    Failed,
    /// An error that ends the walk at once, such as a shutdown.
    Abort(X),
}

/// The upstreams of an application in priority order, with the state that decides which of them
/// may be tried. Cloning it shares that state.
#[derive(Debug)]
pub struct UpstreamSelector<E>(Arc<Mutex<Upstreams<E>>>);

impl<E> Clone for UpstreamSelector<E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

#[derive(Debug)]
struct Upstreams<E> {
    slots: Vec<Slot<E>>,
    active: Option<Active>,
}

#[derive(Debug)]
struct Slot<E> {
    entry: E,
    failures: u32,
    wait: Option<Wait>,
}

#[derive(Debug, Clone, Copy)]
struct Wait {
    since: Instant,
    length: Duration,
    jitter: f64,
}

#[derive(Debug, Clone, Copy)]
struct Active {
    rank: usize,
    since: Instant,
}

impl<E> Upstreams<E> {
    // When the upstream at `rank` may be tried again, `None` if it is not waiting. The limit that
    // applies while nothing is connected is taken here rather than when the wait starts, so an
    // upstream that failed while another one was active is back within it once that one is gone
    // too. The length is capped before the jitter applies, at a value the longest jitter keeps
    // within the limit, so waits at the limit stay spread out.
    fn ready_at(&self, rank: usize) -> Option<Instant> {
        let wait = self.slots[rank].wait?;
        let limit = match self.active {
            Some(_) => MAX_WAIT,
            None => MAX_WAIT_WHILE_DISCONNECTED,
        };
        let length = wait.length.min(limit.div_f64(1.0 + JITTER));
        Some(wait.since + length.mul_f64(wait.jitter))
    }

    fn fail(&mut self, rank: usize, now: Instant) {
        let slot = &mut self.slots[rank];
        slot.failures = slot.failures.saturating_add(1);
        let length = FIRST_WAIT
            .saturating_mul(2_u32.saturating_pow(slot.failures - 1))
            .min(MAX_WAIT);
        slot.wait = Some(Wait {
            since: now,
            length,
            jitter: rand::thread_rng().gen_range(1.0 - JITTER..=1.0 + JITTER),
        });
    }

    fn connect(&mut self, rank: usize, now: Instant) {
        self.slots[rank].wait = None;
        self.active = Some(Active { rank, since: now });
    }

    // Ends the active connection and returns its rank. A connection that lasted long enough clears
    // the failure count; a shorter one is a failure when `short_is_failure` holds.
    fn release(&mut self, now: Instant, short_is_failure: bool) -> Option<usize> {
        let active = self.active.take()?;
        if now.duration_since(active.since) >= STABLE_CONNECTION {
            self.slots[active.rank].failures = 0;
        } else if short_is_failure {
            self.fail(active.rank, now);
        }
        Some(active.rank)
    }

    // The upstream to probe next and when: among those ranked above the active one, or all of
    // them while none is connected, the one that is or becomes eligible first, the higher-ranked
    // when several become eligible at the same instant.
    fn next_probe(&self, now: Instant) -> Option<(usize, Instant)> {
        let above = self.active.map_or(self.slots.len(), |active| active.rank);
        (0..above)
            .map(|rank| (rank, self.ready_at(rank).unwrap_or(now)))
            .min_by_key(|&(_, at)| at)
    }
}

impl<E> UpstreamSelector<E> {
    /// Creates a selector over `entries`, in priority order.
    pub fn new(entries: impl IntoIterator<Item = E>) -> Self {
        let slots = entries
            .into_iter()
            .map(|entry| Slot {
                entry,
                failures: 0,
                wait: None,
            })
            .collect();
        Self(Arc::new(Mutex::new(Upstreams {
            slots,
            active: None,
        })))
    }

    // Every operation leaves the state consistent before anything that could panic, so a poisoned
    // lock still holds usable state.
    fn lock(&self) -> MutexGuard<'_, Upstreams<E>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether no upstream is configured.
    pub fn is_empty(&self) -> bool {
        self.lock().slots.is_empty()
    }

    /// The active upstream dropped. It fails if it had been connected for less than 10 minutes.
    /// Returns whether an upstream was active.
    pub fn disconnected(&self) -> bool {
        self.lock().release(Instant::now(), true).is_some()
    }

    /// The application leaves the active upstream on purpose, which is not a failure. Returns the
    /// rank it had, if one was active.
    pub fn leave(&self) -> Option<usize> {
        self.lock().release(Instant::now(), false)
    }

    /// Records a connection made outside a walk as the upstream at `rank`.
    pub fn connected(&self, rank: usize) {
        self.lock().connect(rank, Instant::now());
    }

    /// Resolves once some upstream may be tried again, at once if one already may. Never resolves
    /// when no upstream is configured.
    pub async fn next_ready(&self) {
        let next = self.lock().next_probe(Instant::now());
        match next {
            Some((_, at)) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    }
}

impl<E: Clone + Display> UpstreamSelector<E> {
    /// Goes through the upstreams in priority order, skipping those still waiting, and gives each
    /// [`ATTEMPTS_PER_UPSTREAM`] attempts with `connect`. Returns the first connection made, with
    /// that upstream active, or [`Attempt::Failed`] once every upstream failed or was skipped.
    pub async fn walk<T, X, F, Fut>(&self, mut connect: F) -> Attempt<T, X>
    where
        F: FnMut(E) -> Fut,
        Fut: Future<Output = Attempt<T, X>>,
    {
        let count = self.lock().slots.len();
        for rank in 0..count {
            let (entry, ready_at) = {
                let upstreams = self.lock();
                (
                    upstreams.slots[rank].entry.clone(),
                    upstreams.ready_at(rank),
                )
            };
            let now = Instant::now();
            if let Some(at) = ready_at.filter(|&at| at > now) {
                info!(
                    "Skipping upstream {} of {count} ({entry}), waiting {}s after failing",
                    rank + 1,
                    at.duration_since(now).as_secs()
                );
                continue;
            }
            info!("Trying upstream {} of {count}: {entry}", rank + 1);
            match attempts(&entry, &mut connect).await {
                Attempt::Connected(connection) => {
                    self.lock().connect(rank, Instant::now());
                    return Attempt::Connected(connection);
                }
                Attempt::Failed => self.lock().fail(rank, Instant::now()),
                Attempt::Abort(error) => return Attempt::Abort(error),
            }
        }
        Attempt::Failed
    }

    /// Starts the probe of a runtime generation: a task, tracked by `generation`, that probes the
    /// upstreams ranked above the active one, or all of them while none is connected, each once
    /// its wait has run out, and triggers `generation`'s fallback once one answers, so the walk
    /// that follows connects to it. The task ends with the generation or at shutdown, and never
    /// probes while the primary is active.
    ///
    /// `probe` connects to an upstream and sets the connection up, tracking whatever it starts on
    /// the coordinator it gets. That coordinator is a [`FallbackCoordinator::child`] of
    /// `generation`: its fallback is triggered and awaited once `probe` returned or ran out of
    /// [`PROBE_DEADLINE`], and `generation`'s fallback reaches it when that ends the probe first.
    /// The connections `probe` opens are expected to close on `cancellation_token`, which ends the
    /// probe otherwise.
    pub fn start_probe<F, Fut, R>(
        &self,
        task_manager: &TaskManager,
        cancellation_token: CancellationToken,
        generation: &FallbackCoordinator,
        probe: F,
    ) where
        E: Send + 'static,
        F: FnMut(E, FallbackCoordinator) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), R>> + Send,
        R: Debug + Send,
    {
        let upstreams = self.clone();
        let generation_coordinator = generation.clone();
        let fallback_token = generation.token();
        task_manager.spawn(generation.track(async move {
            tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => {}
                _ = fallback_token.cancelled() => {}
                _ = upstreams.probe(&generation_coordinator, probe) => fallback_token.cancel(),
            }
        }));
    }

    // The probe loop of `start_probe`. Returns the rank of the upstream that answered, which is
    // eligible again, with the active upstream left; a probe that does not answer is a failure.
    async fn probe<F, Fut, R>(&self, generation: &FallbackCoordinator, mut probe: F) -> usize
    where
        F: FnMut(E, FallbackCoordinator) -> Fut,
        Fut: Future<Output = Result<(), R>>,
        R: Debug,
    {
        loop {
            let next = self.lock().next_probe(Instant::now());
            let Some((rank, at)) = next else {
                return std::future::pending().await;
            };
            tokio::time::sleep_until(at).await;

            let entry = self.lock().slots[rank].entry.clone();
            info!("Probing upstream {} ({entry})", rank + 1);
            let coordinator = generation.child();
            let outcome =
                tokio::time::timeout(PROBE_DEADLINE, probe(entry.clone(), coordinator.clone()))
                    .await;
            coordinator.trigger_fallback_and_wait().await;

            let mut upstreams = self.lock();
            match outcome {
                Ok(Ok(())) => {
                    info!(
                        "Upstream {} ({entry}) answered the probe, switching to it",
                        rank + 1
                    );
                    upstreams.slots[rank].wait = None;
                    upstreams.release(Instant::now(), false);
                    return rank;
                }
                Ok(Err(error)) => {
                    warn!(
                        "Upstream {} ({entry}) did not answer the probe: {error:?}",
                        rank + 1
                    )
                }
                Err(_) => warn!(
                    "Upstream {} ({entry}) did not answer the probe within {PROBE_DEADLINE:?}",
                    rank + 1
                ),
            }
            upstreams.fail(rank, Instant::now());
        }
    }
}

/// Makes up to [`ATTEMPTS_PER_UPSTREAM`] attempts on `entry` with `connect`, pausing a second
/// before each, and returns the first that does not fail. Why an attempt failed is for `connect`
/// to log.
pub async fn attempts<E, T, X, F, Fut>(entry: &E, connect: &mut F) -> Attempt<T, X>
where
    E: Clone + Display,
    F: FnMut(E) -> Fut,
    Fut: Future<Output = Attempt<T, X>>,
{
    for attempt in 1..=ATTEMPTS_PER_UPSTREAM {
        tokio::time::sleep(PAUSE_BEFORE_ATTEMPT).await;
        info!("Connection attempt {attempt}/{ATTEMPTS_PER_UPSTREAM} to {entry}");
        match connect(entry.clone()).await {
            Attempt::Failed => {}
            done => return done,
        }
    }
    Attempt::Failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn selector(count: usize) -> UpstreamSelector<String> {
        UpstreamSelector::new((0..count).map(|rank| format!("upstream-{rank}")))
    }

    fn wait_length(upstreams: &Upstreams<String>, rank: usize, now: Instant) -> Duration {
        upstreams
            .ready_at(rank)
            .expect("waiting")
            .duration_since(now)
    }

    fn within_jitter(actual: Duration, nominal: Duration) -> bool {
        actual >= nominal.mul_f64(1.0 - JITTER) && actual <= nominal.mul_f64(1.0 + JITTER)
    }

    // A connect step answering from a fixed set of reachable upstreams, counting its calls.
    fn reachable<'a>(
        up: &'a [&'a str],
        calls: &'a AtomicUsize,
    ) -> impl FnMut(String) -> std::future::Ready<Attempt<String, ()>> + 'a {
        move |entry| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(if up.contains(&entry.as_str()) {
                Attempt::Connected(entry)
            } else {
                Attempt::Failed
            })
        }
    }

    #[test]
    fn waits_double_from_ten_seconds_up_to_ten_minutes() {
        let selector = selector(2);
        let mut upstreams = selector.lock();
        upstreams.connect(1, Instant::now());
        let now = Instant::now();
        let capped = MAX_WAIT.div_f64(1.0 + JITTER);
        for nominal in [10, 20, 40, 80, 160, 320, 640, 1280].map(Duration::from_secs) {
            upstreams.fail(0, now);
            let nominal = nominal.min(capped);
            let actual = wait_length(&upstreams, 0, now);
            assert!(
                within_jitter(actual, nominal) && actual <= MAX_WAIT,
                "wait {actual:?}, expected {nominal:?} ±20 %, at most {MAX_WAIT:?}"
            );
        }
    }

    #[test]
    fn waits_are_capped_at_a_minute_while_nothing_is_connected() {
        let selector = selector(2);
        let mut upstreams = selector.lock();
        let now = Instant::now();
        upstreams.connect(1, now);
        for _ in 0..5 {
            upstreams.fail(0, now);
        }
        assert!(within_jitter(
            wait_length(&upstreams, 0, now),
            Duration::from_secs(160)
        ));

        upstreams.release(now, false);
        let actual = wait_length(&upstreams, 0, now);
        assert!(
            within_jitter(actual, MAX_WAIT_WHILE_DISCONNECTED.div_f64(1.0 + JITTER))
                && actual <= MAX_WAIT_WHILE_DISCONNECTED,
            "wait {actual:?}, expected at most {MAX_WAIT_WHILE_DISCONNECTED:?}"
        );
    }

    // Upstreams that fail together keep retrying apart even once their waits reach a limit.
    #[test]
    fn waits_at_a_limit_keep_their_jitter() {
        let selector = selector(2);
        let mut upstreams = selector.lock();
        let now = Instant::now();
        let wait_with = |upstreams: &mut Upstreams<String>, jitter: f64| {
            upstreams.slots[0].wait = Some(Wait {
                since: now,
                length: MAX_WAIT,
                jitter,
            });
            wait_length(upstreams, 0, now)
        };
        for limit in [MAX_WAIT_WHILE_DISCONNECTED, MAX_WAIT] {
            if limit == MAX_WAIT {
                upstreams.connect(1, now);
            }
            let shortest = wait_with(&mut upstreams, 1.0 - JITTER);
            let nominal = wait_with(&mut upstreams, 1.0);
            let longest = wait_with(&mut upstreams, 1.0 + JITTER);
            assert!(
                shortest < nominal && nominal < longest && longest <= limit,
                "waits {shortest:?} < {nominal:?} < {longest:?} <= {limit:?}"
            );
        }
    }

    #[test]
    fn a_drop_after_ten_minutes_is_not_a_failure_and_clears_the_count() {
        let selector = selector(1);
        let start = Instant::now();
        let mut upstreams = selector.lock();
        upstreams.fail(0, start);
        upstreams.fail(0, start);
        upstreams.connect(0, start);

        upstreams.release(start + STABLE_CONNECTION, true);
        assert_eq!(upstreams.slots[0].failures, 0);
        assert!(upstreams.ready_at(0).is_none());
    }

    #[test]
    fn leaving_on_purpose_is_never_a_failure() {
        let selector = selector(1);
        selector.connected(0);
        assert_eq!(selector.leave(), Some(0));
        assert_eq!(selector.lock().slots[0].failures, 0);
        assert!(selector.lock().ready_at(0).is_none());
    }

    #[test]
    fn probes_go_to_higher_ranks_only_and_to_all_while_disconnected() {
        let selector = selector(3);
        let now = Instant::now();
        let mut upstreams = selector.lock();
        upstreams.fail(0, now);
        upstreams.fail(1, now);
        upstreams.fail(1, now);

        upstreams.active = Some(Active {
            rank: 0,
            since: now,
        });
        assert_eq!(
            upstreams.next_probe(now),
            None,
            "nothing ranks above the primary"
        );

        upstreams.active = Some(Active {
            rank: 2,
            since: now,
        });
        let (rank, at) = upstreams.next_probe(now).expect("a higher rank to probe");
        assert_eq!(rank, 0, "the upstream whose wait ends first");
        assert_eq!(Some(at), upstreams.ready_at(0));

        upstreams.release(now, false);
        upstreams.slots[2].wait = None;
        assert_eq!(
            upstreams.next_probe(now),
            Some((2, now)),
            "with nothing connected every upstream counts, and one not waiting is due now"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn walk_connects_to_the_first_eligible_upstream_in_priority_order() {
        let selector = selector(3);
        let calls = AtomicUsize::new(0);

        let connected = selector
            .walk(reachable(&["upstream-1", "upstream-2"], &calls))
            .await;

        assert!(matches!(connected, Attempt::Connected(ref entry) if entry == "upstream-1"));
        assert_eq!(calls.load(Ordering::Relaxed), ATTEMPTS_PER_UPSTREAM + 1);
        let upstreams = selector.lock();
        assert_eq!(upstreams.active.map(|active| active.rank), Some(1));
        assert_eq!(upstreams.slots[0].failures, 1);
        assert!(upstreams.ready_at(0).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn walk_skips_upstreams_that_are_still_waiting() {
        let selector = selector(2);
        selector.lock().fail(0, Instant::now());
        let calls = AtomicUsize::new(0);

        let connected = selector
            .walk(reachable(&["upstream-0", "upstream-1"], &calls))
            .await;

        assert!(matches!(connected, Attempt::Connected(ref entry) if entry == "upstream-1"));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn walk_stops_at_an_abort() {
        let selector = selector(2);
        let calls = AtomicUsize::new(0);

        let outcome: Attempt<(), &str> = selector
            .walk(|_| {
                calls.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Attempt::Abort("shutdown"))
            })
            .await;

        assert!(matches!(outcome, Attempt::Abort("shutdown")));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    // An upstream that connected once stays eligible: the only upstream connects, drops after a
    // stable connection, cannot be reached for a while and comes back.
    #[tokio::test(start_paused = true)]
    async fn an_upstream_that_connected_before_is_connected_again() {
        let selector = selector(1);
        let calls = AtomicUsize::new(0);

        let first = selector.walk(reachable(&["upstream-0"], &calls)).await;
        assert!(matches!(first, Attempt::Connected(_)));

        tokio::time::advance(STABLE_CONNECTION).await;
        selector.disconnected();
        let tried_before = calls.load(Ordering::Relaxed);
        let while_down = selector.walk(reachable(&[], &calls)).await;
        assert!(matches!(while_down, Attempt::Failed));
        assert_eq!(
            calls.load(Ordering::Relaxed) - tried_before,
            ATTEMPTS_PER_UPSTREAM,
            "the upstream that connected before was tried, not skipped"
        );

        let down_since = Instant::now();
        tokio::time::timeout(FIRST_WAIT.mul_f64(1.0 + JITTER), selector.next_ready())
            .await
            .expect("it is tried again after the first wait, not excluded");
        assert!(down_since.elapsed() >= FIRST_WAIT.mul_f64(1.0 - JITTER));
        let back = selector.walk(reachable(&["upstream-0"], &calls)).await;
        assert!(matches!(back, Attempt::Connected(_)));
    }

    // The only upstream drops shortly after connecting. It waits out its first failure, then it
    // is tried again.
    #[tokio::test(start_paused = true)]
    async fn a_short_drop_is_a_failure_and_the_upstream_is_tried_again_after_its_wait() {
        let selector = selector(1);
        let calls = AtomicUsize::new(0);
        let first = selector.walk(reachable(&["upstream-0"], &calls)).await;
        assert!(matches!(first, Attempt::Connected(_)));

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(selector.disconnected());
        let tried_before = calls.load(Ordering::Relaxed);
        let during_wait = selector.walk(reachable(&["upstream-0"], &calls)).await;
        assert!(matches!(during_wait, Attempt::Failed));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            tried_before,
            "skipped while it waits"
        );

        tokio::time::timeout(FIRST_WAIT.mul_f64(1.0 + JITTER), selector.next_ready())
            .await
            .expect("eligible again after its first wait");
        let back = selector.walk(reachable(&["upstream-0"], &calls)).await;
        assert!(matches!(back, Attempt::Connected(_)));
    }

    fn answers(yes: bool) -> std::future::Ready<Result<(), &'static str>> {
        std::future::ready(if yes { Ok(()) } else { Err("refused") })
    }

    #[tokio::test(start_paused = true)]
    async fn the_upstream_that_answers_a_probe_is_left_for_without_a_failure() {
        let selector = selector(2);
        let now = Instant::now();
        selector.lock().fail(0, now);
        selector.lock().connect(1, now);

        let answered = selector
            .probe(&FallbackCoordinator::new(), |entry, _| {
                answers(entry == "upstream-0")
            })
            .await;

        assert_eq!(answered, 0);
        assert!(
            !selector.disconnected(),
            "the fallback that follows finds no active upstream to count a failure against"
        );
        let upstreams = selector.lock();
        assert!(upstreams.ready_at(0).is_none());
        assert_eq!(upstreams.slots[1].failures, 0);
        assert!(upstreams.ready_at(1).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_that_does_not_answer_is_a_failure() {
        let selector = selector(2);
        let now = Instant::now();
        selector.lock().fail(0, now);
        selector.lock().connect(1, now);
        let probes = AtomicUsize::new(0);

        let answered = selector
            .probe(&FallbackCoordinator::new(), |_, _| {
                answers(probes.fetch_add(1, Ordering::Relaxed) == 1)
            })
            .await;

        assert_eq!(answered, 0);
        assert_eq!(probes.load(Ordering::Relaxed), 2);
        assert_eq!(selector.lock().slots[0].failures, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_running_past_its_deadline_does_not_answer() {
        let selector = selector(2);
        selector.lock().connect(1, Instant::now());
        let probes = AtomicUsize::new(0);

        let answered = selector
            .probe(&FallbackCoordinator::new(), |_, _| {
                let first = probes.fetch_add(1, Ordering::Relaxed) == 0;
                async move {
                    if first {
                        std::future::pending::<()>().await;
                    }
                    Ok::<(), ()>(())
                }
            })
            .await;

        assert_eq!(answered, 0);
        assert_eq!(probes.load(Ordering::Relaxed), 2);
        assert_eq!(
            selector.lock().slots[0].failures,
            1,
            "the probe that ran out counted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_triggers_the_generations_fallback() {
        let selector = selector(2);
        selector.lock().connect(1, Instant::now());
        let generation = FallbackCoordinator::new();

        selector.start_probe(
            &TaskManager::new(),
            CancellationToken::new(),
            &generation,
            |_, _| answers(true),
        );

        tokio::time::timeout(Duration::from_secs(1), generation.token().cancelled())
            .await
            .expect("the answer triggers the generation's fallback");
    }

    #[tokio::test(start_paused = true)]
    async fn the_generations_fallback_ends_and_waits_for_what_a_running_probe_opened() {
        let selector = selector(2);
        selector.lock().connect(1, Instant::now());
        let generation = FallbackCoordinator::new();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (opened_sender, mut opened) = tokio::sync::mpsc::unbounded_channel();

        let probe_closed = Arc::clone(&closed);
        selector.start_probe(
            &TaskManager::new(),
            CancellationToken::new(),
            &generation,
            move |_, coordinator: FallbackCoordinator| {
                // A connection the probe opened, which takes a while to close once told to.
                let token = coordinator.token();
                let closed = Arc::clone(&probe_closed);
                tokio::spawn(coordinator.track(async move {
                    token.cancelled().await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    closed.store(true, Ordering::Relaxed);
                }));
                _ = opened_sender.send(());
                std::future::pending::<Result<(), ()>>()
            },
        );
        opened.recv().await.expect("the probe runs");

        generation.trigger_fallback_and_wait().await;
        assert!(closed.load(Ordering::Relaxed));
    }
}
