use std::sync::Mutex as StdMutex;
use tokio::task::JoinHandle;

/// Manages a collection of spawned tokio tasks.
///
/// This struct provides a centralized way to spawn, track, and manage the lifecycle
/// of async tasks in the translator. It maintains a list of join handles that can
/// be used to wait for all tasks to complete or abort them during shutdown.
pub struct TaskManager {
    // The handles, and the length at which `spawn` next prunes finished ones.
    tasks: StdMutex<(Vec<JoinHandle<()>>, usize)>,
}

/// Smallest prune threshold, so a nearly empty manager doesn't scan on every spawn.
const MIN_PRUNE_AT: usize = 64;

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
            tasks: StdMutex::new((Vec::new(), MIN_PRUNE_AT)),
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

        let handle = tokio::spawn(fut.instrument(span));
        let mut guard = self.tasks.lock().unwrap();
        let (tasks, prune_at) = &mut *guard;
        // Pruning only once the list has doubled since the last prune keeps `spawn` amortised
        // O(1) and the list within about 2× the live tasks.
        if tasks.len() >= *prune_at {
            tasks.retain(|h| !h.is_finished());
            *prune_at = (2 * tasks.len()).max(MIN_PRUNE_AT);
        }
        tasks.push(handle);
    }

    /// Waits for all managed tasks to complete.
    ///
    /// This method will block until all tasks that were spawned through this
    /// manager have finished executing. Tasks are joined in reverse order
    /// (most recently spawned first).
    pub async fn join_all(&self) {
        let handles = {
            let mut guard = self.tasks.lock().unwrap();
            std::mem::take(&mut guard.0)
        };

        for handle in handles {
            let _ = handle.await;
        }
    }

    /// Aborts all managed tasks.
    ///
    /// This method immediately cancels all tasks that were spawned through this
    /// manager. The tasks will be terminated without waiting for them to complete.
    pub async fn abort_all(&self) {
        let mut guard = self.tasks.lock().unwrap();
        for handle in guard.0.drain(..) {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Finished tasks are pruned, so the list stays near the live count instead of
    // growing with every task ever spawned; and spawn scans it only when it has doubled.
    #[tokio::test]
    async fn spawn_keeps_the_list_bounded() {
        let manager = TaskManager::new();
        for _ in 0..10_000 {
            manager.spawn(async {});
            tokio::task::yield_now().await;
        }
        let tracked = manager.tasks.lock().unwrap().0.len();
        assert!(
            tracked <= 2 * MIN_PRUNE_AT,
            "{tracked} handles kept for ~0 live tasks"
        );
        manager.join_all().await;
    }

    /// Spawn cost with many live tasks:
    /// `cargo test --release -p stratum-apps --lib spawn_cost -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn spawn_cost() {
        const N: usize = 50_000;
        let manager = TaskManager::new();
        let hold = std::sync::Arc::new(tokio::sync::Notify::new());
        let t = std::time::Instant::now();
        for _ in 0..N {
            let hold = hold.clone();
            manager.spawn(async move { hold.notified().await });
        }
        println!("spawn: {N} live tasks in {:?}", t.elapsed());
        manager.abort_all().await;
    }
}
