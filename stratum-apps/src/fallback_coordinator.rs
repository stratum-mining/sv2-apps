use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// coordinates fallback operations across multiple components with acknowledgement.
///
/// this is meant to be used together with [`crate::task_manager::TaskManager`],
/// as it allows triggering a fallback event (via [`CancellationToken`]) and waiting
/// until all registered components have completed their cleanup.
///
/// in summary, every time we spawn a fallback-relevant task inside the manager, we MUST:
/// - call [`FallbackCoordinator::register`] at task bootstrap
/// - call [`FallbackHandler::done`] at task completion
///
/// when a fallback trigger arrives to the main status loop, we MUST call
/// [`FallbackCoordinator::trigger_fallback_and_wait`] to wait for all registered components to
/// complete their cleanup before re-initializing them under the new upstream server.
///
/// finally, a new [`FallbackCoordinator`] must be instantiated for the next fallback cycle.
///
/// # fallback generations
///
/// one [`FallbackCoordinator::new`] is one fallback generation, shared by every clone: a handler
/// belongs to the generation of the coordinator it was registered on, and a fresh coordinator
/// starts the next generation.
///
/// registration stays legal after fallback has been triggered. a component that registers while
/// [`FallbackCoordinator::trigger_fallback_and_wait`] is still waiting joins the generation and
/// the wait covers it, because the wait resolves only once it observes no pending handler and
/// rechecks that after every wakeup. a component that registers after the wait has returned is
/// covered only by a later wait on this coordinator.
#[derive(Debug, Clone)]
pub struct FallbackCoordinator {
    signal: CancellationToken,
    pending_tasks: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Default for FallbackCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl FallbackCoordinator {
    pub fn new() -> Self {
        Self {
            signal: CancellationToken::new(),
            pending_tasks: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// register a component that will participate in fallback coordination
    /// returns a [`FallbackHandler`] that must be called when the component is done
    #[must_use]
    pub fn register(&self) -> FallbackHandler {
        tracing::debug!("FallbackCoordinator: registering component");
        self.pending_tasks.fetch_add(1, Ordering::Relaxed);

        FallbackHandler {
            coordinator: self.clone(),
            done: AtomicBool::new(false),
        }
    }

    /// get the cancellation token that signals fallback
    pub fn token(&self) -> CancellationToken {
        self.signal.clone()
    }

    /// trigger fallback and wait until no registered component is pending
    ///
    /// the pending count is the completion condition; a wakeup only means it is worth checking
    /// again. a handler that finishes between the check and the await is not missed, because
    /// [`Notify`] keeps that wakeup as a permit until the next wait consumes it.
    ///
    /// dropping the returned future keeps the coordinator consistent: the fallback stays
    /// triggered and a later call resumes waiting on the same pending count. one wait at a time:
    /// the wakeup reaches a single waiter.
    pub async fn trigger_fallback_and_wait(&self) {
        tracing::debug!("FallbackCoordinator: triggering fallback");
        self.signal.cancel();
        while self.pending_tasks.load(Ordering::Acquire) > 0 {
            self.notify.notified().await;
        }
        tracing::debug!("FallbackCoordinator: finished waiting for components to complete cleanup");
    }
}

pub struct FallbackHandler {
    coordinator: FallbackCoordinator,
    done: AtomicBool,
}

/// Handler for a component that will participate in fallback coordination
///
/// ⚠️ Warning: dropping this handler without calling [`FallbackHandler::done`] will result in a
/// panic.
impl FallbackHandler {
    /// Mark this handler as finished
    /// Takes ownership of `self`, preventing double-calling
    pub fn done(self) {
        tracing::debug!("FallbackHandler: done called");
        self.done.store(true, Ordering::Release);

        // every transition to zero wakes the waiter, whether or not fallback has been triggered
        // yet: an early wakeup only makes the waiter recheck the count
        if self
            .coordinator
            .pending_tasks
            .fetch_sub(1, Ordering::AcqRel)
            == 1
        {
            self.coordinator.notify.notify_one();
        }
    }
}

impl Drop for FallbackHandler {
    fn drop(&mut self) {
        if !self.done.load(Ordering::Acquire) {
            panic!("FallbackHandler dropped without calling done()");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        pin::{Pin, pin},
        task::{Context, Poll, Wake, Waker},
    };

    // polls once with a no-op waker, so each test step interleaves exactly where written
    fn poll<F: Future>(fut: &mut Pin<&mut F>) -> Poll<F::Output> {
        fut.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    }

    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    // the wait must be woken when the last handler goes away, or fallback would hang
    #[test]
    fn last_handler_done_wakes_the_waiter() {
        let coordinator = FallbackCoordinator::new();
        let handler = coordinator.register();
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);

        handler.done();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn wait_returns_immediately_without_pending_handlers() {
        let coordinator = FallbackCoordinator::new();
        coordinator.register().done();

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Ready(()));
        assert!(coordinator.token().is_cancelled());
    }

    #[test]
    fn wait_completes_when_the_last_handler_is_done() {
        let coordinator = FallbackCoordinator::new();
        let first = coordinator.register();
        let second = coordinator.register();

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Pending);

        first.done();
        assert_eq!(poll(&mut wait), Poll::Pending);

        second.done();
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }

    // Loupe #145 / sv2-apps #793: a handler finishing and another registering before the waiter
    // resumes
    #[test]
    fn stale_wakeup_does_not_end_the_wait_while_a_late_handler_is_pending() {
        let coordinator = FallbackCoordinator::new();
        let early = coordinator.register();

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Pending);

        early.done();
        let late = coordinator.register();
        assert_eq!(poll(&mut wait), Poll::Pending);

        late.done();
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }

    #[test]
    fn handler_finished_before_the_trigger_is_not_mistaken_for_completion() {
        let coordinator = FallbackCoordinator::new();
        coordinator.register().done();
        let pending = coordinator.register();

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Pending);

        pending.done();
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }

    #[test]
    fn dropped_wait_can_be_resumed() {
        let coordinator = FallbackCoordinator::new();
        let handler = coordinator.register();

        {
            let mut wait = pin!(coordinator.trigger_fallback_and_wait());
            assert_eq!(poll(&mut wait), Poll::Pending);
        }
        assert!(coordinator.token().is_cancelled());

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Pending);

        handler.done();
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }

    #[test]
    fn handler_done_while_no_wait_is_polled_is_not_lost() {
        let coordinator = FallbackCoordinator::new();
        let handler = coordinator.register();

        {
            let mut wait = pin!(coordinator.trigger_fallback_and_wait());
            assert_eq!(poll(&mut wait), Poll::Pending);
        }
        handler.done();

        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }

    #[test]
    fn registration_after_completion_is_covered_by_a_later_wait() {
        let coordinator = FallbackCoordinator::new();
        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Ready(()));

        let late = coordinator.register();
        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert_eq!(poll(&mut wait), Poll::Pending);

        late.done();
        assert_eq!(poll(&mut wait), Poll::Ready(()));
    }
}
