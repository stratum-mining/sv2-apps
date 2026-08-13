use std::sync::Mutex as StdMutex;
use tokio::task::JoinSet;

/// Manages a collection of spawned tokio tasks.
///
/// This struct provides a centralized way to spawn, track, and manage the lifecycle
/// of async tasks in the translator. Tasks are tracked in a [`tokio::task::JoinSet`],
/// which can be used to wait for all tasks to complete or abort them during shutdown.
///
/// # Tracking cost
///
/// Tracking must stay cheap on the hot path, because callers spawn several tasks per
/// downstream connection. A `Vec<JoinHandle<()>>` pruned with `retain(|h|
/// !h.is_finished())` on every spawn costs a linear scan of every live handle per
/// spawn, which makes reaching N concurrent connections quadratic in N and serializes
/// that scan on one mutex.
///
/// A `JoinSet` inserts in constant time. Because a `JoinSet` only releases an entry
/// when that entry is joined, and [`Self::join_all`] runs only at shutdown, spawn
/// also drains already-finished tasks with the non-blocking
/// [`tokio::task::JoinSet::try_join_next`]. That drain touches only tasks the runtime
/// has already marked complete, never live ones, so it costs O(tasks finished since
/// the previous spawn) — amortized constant per task — and keeps the set bounded to
/// roughly the live task count. Without it the set would retain one task
/// control-block per task ever spawned, growing with cumulative rather than
/// concurrent tasks.
///
/// The mutex itself is retained, so spawns still serialize on it; what this removes
/// is the per-spawn scan over all live handles.
pub struct TaskManager {
    tasks: StdMutex<JoinSet<()>>,
}

impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskManager {
    /// Creates a new TaskManager instance.
    ///
    /// Initializes an empty task manager ready to spawn and track tasks.
    pub fn new() -> Self {
        Self {
            tasks: StdMutex::new(JoinSet::new()),
        }
    }

    /// Spawns a new async task and adds it to the managed collection.
    ///
    /// The task will be tracked by this manager and can be waited for or aborted
    /// using the other methods.
    ///
    /// # Arguments
    /// * `fut` - The future to spawn as a task
    #[track_caller]
    pub fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        use tracing::Instrument;
        let location = std::panic::Location::caller();
        let span = tracing::trace_span!(
            "task",
            file = location.file(),
            line = location.line(),
            column = location.column(),
        );

        // `JoinSet::spawn` schedules the future on the current runtime, as the prior
        // `tokio::spawn` did, and retains an abort handle internally. The lock is held
        // for the insert and the drain below and never across an `.await`, so `spawn`
        // stays synchronous.
        let mut tasks = self.tasks.lock().unwrap();
        tasks.spawn(fut.instrument(span));

        // Release entries for tasks that have already completed.
        while tasks.try_join_next().is_some() {}
    }

    /// Waits for all managed tasks to complete.
    ///
    /// This method will block until all tasks that were spawned through this
    /// manager have finished executing. Tasks are awaited in completion order.
    pub async fn join_all(&self) {
        // `JoinSet::join_next` borrows mutably and is awaited, and a
        // `std::sync::Mutex` guard must not be held across an `.await`. Move the set
        // out under the lock, leaving an empty one behind, then drain it unlocked.
        // Tasks spawned after this point land in the fresh set and are covered by a
        // later `join_all` or `abort_all`.
        let mut owned = {
            let mut tasks = self.tasks.lock().unwrap();
            std::mem::take(&mut *tasks)
        };

        while owned.join_next().await.is_some() {}
    }

    /// Aborts all managed tasks.
    ///
    /// This method immediately cancels all tasks that were spawned through this
    /// manager. The tasks will be terminated without waiting for them to complete.
    ///
    /// Aborted entries remain in the set until they are joined, so callers that need
    /// the tasks reaped should follow this with [`Self::join_all`].
    pub async fn abort_all(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        tasks.abort_all();
    }

    /// Number of tasks currently tracked, for tests asserting the set stays bounded.
    #[cfg(test)]
    fn tracked_len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::TaskManager;

    /// `join_all` must release every tracked entry. This is exact rather than
    /// approximate: after draining, nothing may remain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_all_drains_every_tracked_task() {
        let task_manager = TaskManager::new();
        for _ in 0..64 {
            task_manager.spawn(async {});
        }

        task_manager.join_all().await;

        assert_eq!(
            task_manager.tracked_len(),
            0,
            "join_all left entries behind"
        );
    }

    /// `spawn` must release finished entries, so the set tracks roughly the live task
    /// count rather than every task ever spawned. An implementation that never reaped
    /// would end at the full cumulative count.
    ///
    /// This runs on a current-thread runtime deliberately, so the reap is observable
    /// without depending on wall-clock time or on how many worker threads happen to be
    /// free — the suite runs tests in parallel, so a multi-threaded variant is not
    /// reliable. Each spawned future here is ready immediately, and the batch is kept
    /// well under the scheduler's per-tick poll budget so one `yield_now` drives the
    /// whole batch to completion before this task is polled again.
    ///
    /// The bound is asserted every round rather than once at the end: if reaping works,
    /// the tracked count returns to roughly zero each round no matter how many rounds
    /// run, so accumulation shows up immediately.
    #[tokio::test]
    async fn spawn_reaps_finished_tasks_and_stays_bounded() {
        const ROUNDS: usize = 40;
        const PER_ROUND: usize = 25;
        const TOLERANCE: usize = 8;

        let task_manager = TaskManager::new();
        for round in 0..ROUNDS {
            for _ in 0..PER_ROUND {
                task_manager.spawn(async {});
            }

            // Let the queued batch run to completion, then reap it on the next spawn.
            tokio::task::yield_now().await;
            task_manager.spawn(async {});

            let tracked = task_manager.tracked_len();
            assert!(
                tracked <= TOLERANCE,
                "round {round}: {tracked} tasks still tracked after spawning \
                 {PER_ROUND}, so finished tasks are not being released"
            );
        }
    }

    /// A panicking task must not poison tracking: the entry is still released, and the
    /// manager keeps working afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_task_is_reaped() {
        let task_manager = TaskManager::new();
        task_manager.spawn(async {
            panic!("intentional panic from a managed task");
        });

        task_manager.join_all().await;

        assert_eq!(
            task_manager.tracked_len(),
            0,
            "panicked task was not reaped"
        );
    }
}
