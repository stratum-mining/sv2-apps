use std::future::Future;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Coordinates fallback across the components of a runtime generation.
///
/// Fallback is signalled through a [`CancellationToken`] and acknowledged by lifetime: a future
/// wrapped with [`FallbackCoordinator::track`] counts as pending until it is dropped, whether it
/// returned, was cancelled or panicked, so there is nothing a component has to remember to call
/// on its way out. [`FallbackCoordinator::trigger_fallback_and_wait`] cancels the token and
/// resolves once no tracked future is left, which it checks rather than infers from a wakeup: a
/// future tracked after the trigger, or after every other one already finished, is still waited
/// for.
///
/// This is meant to be used together with [`crate::task_manager::TaskManager`]:
///
/// ```ignore
/// task_manager.spawn(fallback_coordinator.track(async move {
///     tokio::select! {
///         _ = fallback_token.cancelled() => { /* clean up */ }
///         // ...
///     }
/// }));
/// ```
///
/// A coordinator serves one fallback cycle: once the wait returned, the runtime instantiates a
/// fresh one for the next generation.
#[derive(Debug, Clone, Default)]
pub struct FallbackCoordinator {
    signal: CancellationToken,
    tasks: TaskTracker,
    // The trackers of the coordinators this one is a child of, which wait for its futures too.
    ancestors: Vec<TaskTracker>,
}

impl FallbackCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the cancellation token that signals fallback.
    pub fn token(&self) -> CancellationToken {
        self.signal.clone()
    }

    /// A coordinator for work nested in this one's generation: fallback on this coordinator
    /// signals the child too and waits for the futures the child tracks, while fallback on the
    /// child leaves this one alone and waits for the child's futures only.
    pub fn child(&self) -> Self {
        let mut ancestors = self.ancestors.clone();
        ancestors.push(self.tasks.clone());
        Self {
            signal: self.signal.child_token(),
            tasks: TaskTracker::new(),
            ancestors,
        }
    }

    /// Wrap a fallback-relevant future so that fallback waits for it until it is dropped.
    ///
    /// Tracking starts here, not when the future is first polled, so a tracked future handed to a
    /// spawner is pending from the moment it exists. Track before fallback is triggered or from
    /// inside a future that is itself tracked: a future tracked after the wait has observed the
    /// tracker empty belongs to no generation and is not waited for.
    pub fn track<F: Future>(&self, fut: F) -> impl Future<Output = F::Output> + use<F> {
        let ancestors: Vec<_> = self.ancestors.iter().map(TaskTracker::token).collect();
        let fut = self.tasks.track_future(fut);
        async move {
            let _ancestors = ancestors;
            fut.await
        }
    }

    /// Trigger fallback and wait until every tracked future has been dropped.
    pub async fn trigger_fallback_and_wait(&self) {
        tracing::debug!(
            pending = self.tasks.len(),
            "FallbackCoordinator: triggering fallback"
        );
        self.signal.cancel();
        self.tasks.close();
        // `wait` resolves once the tracker has been empty at some point since the call, so work
        // tracked after that point would not be covered: look again, and wait again if there is
        // any. `wait` re-reads the closed-and-empty state when it is created and on every poll,
        // so a drop between the check and the next `wait` is seen by that `wait`, not missed.
        loop {
            self.tasks.wait().await;
            if self.tasks.is_empty() {
                break;
            }
            tracing::debug!(
                pending = self.tasks.len(),
                "FallbackCoordinator: work tracked after the tracker emptied, waiting again"
            );
        }
        tracing::debug!("FallbackCoordinator: all components finished fallback cleanup");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
    use std::pin::pin;

    #[tokio::test]
    async fn waits_for_work_tracked_after_an_earlier_one_finished() {
        let coordinator = FallbackCoordinator::new();
        let first = coordinator.track(std::future::pending::<()>());
        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert!(wait.as_mut().now_or_never().is_none());

        drop(first);
        let second = coordinator.track(std::future::pending::<()>());
        assert!(
            wait.as_mut().now_or_never().is_none(),
            "fallback completed while a component was still pending"
        );

        drop(second);
        assert!(wait.as_mut().now_or_never().is_some());
    }

    #[tokio::test]
    async fn tracked_future_counts_from_creation_until_dropped() {
        let coordinator = FallbackCoordinator::new();
        let never_polled = coordinator.track(std::future::pending::<()>());
        let mut wait = pin!(coordinator.trigger_fallback_and_wait());
        assert!(wait.as_mut().now_or_never().is_none());

        drop(never_polled);
        assert!(wait.as_mut().now_or_never().is_some());
    }

    #[tokio::test]
    async fn fallback_reaches_a_child_and_waits_for_it_but_not_the_other_way_round() {
        let parent = FallbackCoordinator::new();
        let parent_work = parent.track(std::future::pending::<()>());
        let child = parent.child();
        child.trigger_fallback_and_wait().await;
        assert!(!parent.token().is_cancelled());
        drop(parent_work);

        let child = parent.child();
        let child_work = child.track(std::future::pending::<()>());
        let mut wait = pin!(parent.trigger_fallback_and_wait());
        assert!(
            wait.as_mut().now_or_never().is_none(),
            "fallback completed while the child's work was still pending"
        );
        assert!(child.token().is_cancelled());

        drop(child_work);
        assert!(wait.as_mut().now_or_never().is_some());
    }

    #[tokio::test]
    async fn panicking_tracked_task_is_released() {
        let coordinator = FallbackCoordinator::new();
        let task = tokio::spawn(coordinator.track(async { panic!("task failed") }));
        assert!(task.await.unwrap_err().is_panic());

        coordinator.trigger_fallback_and_wait().await;
    }
}
