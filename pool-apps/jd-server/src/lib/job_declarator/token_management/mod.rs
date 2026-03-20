//! Module for managing the tokens used in the Job Declaration process.
//!
//! There's two types of tokens:
//! - Allocated tokens: allocated into a `AllocateMiningJobToken.Success` message.
//! - Active tokens: tokens that were previously allocated and are then activated into a
//!   `DeclareMiningJob.Success` message.
//!
//! The process of "activating" an "allocated" token consists of:
//! - Removing the allocated token from the allocated tokens set.
//! - Creating a corresponding active token and adding it to the active tokens set.
//! - Returning the active token.
//!
//! Both kinds of token are managed via [`TokenManager`]. It is responsible for:
//! - Allocating new tokens.
//! - Deallocating allocated tokens after a configurable timeout.
//! - Activating tokens that are allocated.
//! - Deactivating active tokens after a configurable timeout.
//! - Checking if a token is allocated.
//! - Checking if a token is active.

use super::{ACTIVE_TOKEN_TIMEOUT_SECS, ALLOCATED_TOKEN_TIMEOUT_SECS, JANITOR_INTERVAL_SECS};
use bitcoin_core_sv2::job_declaration_protocol::CancellationToken;
use dashmap::DashMap;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use stratum_apps::{
    task_manager::TaskManager,
    utils::types::{DownstreamId, JdToken},
};

/// Data associated with an allocated token.
/// - Instant is the allocation timestamp
/// - DownstreamId is the downstream ID that allocated the token
pub type AllocatedTokenData = (Instant, DownstreamId);
/// Data associated with an active token.
/// - JdToken is the corresponding allocated token
/// - Instant is the activation timestamp
/// - DownstreamId is the downstream ID that activated the token
pub type ActiveTokenData = (JdToken, Instant, DownstreamId);

/// Manager for the tokens used in the Job Declaration process.
#[derive(Clone)]
pub struct TokenManager {
    token_factory: Arc<AtomicU64>,
    allocated_tokens: Arc<DashMap<JdToken, AllocatedTokenData>>,
    active_tokens: Arc<DashMap<JdToken, ActiveTokenData>>,
    cancellation_token: CancellationToken,
    task_manager: Arc<TaskManager>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl TokenManager {
    /// Constructor of [`TokenManager`]. Also spawns the janitor task.
    ///
    /// Please note that "token" in `CancellationToken` has a different meaning in this context.
    /// It is meant to kill the janitor tasks, and has nothing to do with the `JdToken`s.
    pub fn new(cancellation_token: CancellationToken, task_manager: Arc<TaskManager>) -> Self {
        let token_manager = Self {
            token_factory: Arc::new(AtomicU64::new(0)),
            allocated_tokens: Arc::new(DashMap::new()),
            active_tokens: Arc::new(DashMap::new()),
            cancellation_token,
            task_manager,
        };
        token_manager.spawn_janitor_task();
        token_manager
    }

    /// Allocates a new token and adds it to the allocated tokens set.
    pub fn allocate(&self, downstream_id: DownstreamId) -> JdToken {
        let token = self.token_factory.fetch_add(1, Ordering::Relaxed);
        self.allocated_tokens
            .insert(token, (Instant::now(), downstream_id));
        token
    }

    /// Removes a token from the allocated tokens set.
    pub fn deallocate(&self, token: JdToken) {
        self.allocated_tokens.remove(&token);
    }

    /// Checks if a token is allocated.
    pub fn is_allocated(&self, token: JdToken, downstream_id: DownstreamId) -> bool {
        if let Some(allocation_info) = self.allocated_tokens.get(&token) {
            allocation_info.1 == downstream_id
        } else {
            false
        }
    }

    /// Takes an allocated token and removes it from the internal set.
    /// Creates a corresponding active token and adds it to the internal set.
    pub fn activate(&self, allocated_token: JdToken, downstream_id: DownstreamId) -> JdToken {
        self.allocated_tokens.remove(&allocated_token);
        let activated_token = self.token_factory.fetch_add(1, Ordering::Relaxed);
        self.active_tokens.insert(
            activated_token,
            (allocated_token, Instant::now(), downstream_id),
        );
        activated_token
    }

    /// Removes an active token from the internal set.
    pub fn deactivate(&self, active_token: JdToken) {
        self.active_tokens.remove(&active_token);
    }

    /// Returns the allocated token that corresponds to an active token.
    /// Returns `None` if the active token is not found.
    pub fn allocated_from_active(&self, active_token: JdToken) -> Option<JdToken> {
        self.active_tokens.get(&active_token).map(|entry| entry.0)
    }

    /// Clears all allocated and active tokens.
    pub fn clear(&self) {
        self.allocated_tokens.clear();
        self.active_tokens.clear();
    }

    /// Removes all allocated and active tokens belonging to a given downstream.
    pub fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.allocated_tokens
            .retain(|_, (_, owner)| *owner != downstream_id);
        self.active_tokens
            .retain(|_, (_, _, owner)| *owner != downstream_id);
    }

    /// Spawns a janitor task that removes expired allocated and active tokens.
    fn spawn_janitor_task(&self) {
        let cancellation_token = self.cancellation_token.clone();
        let allocated_tokens = Arc::clone(&self.allocated_tokens);
        let active_tokens = Arc::clone(&self.active_tokens);
        let allocated_token_timeout = Duration::from_secs(ALLOCATED_TOKEN_TIMEOUT_SECS);
        let active_token_timeout = Duration::from_secs(ACTIVE_TOKEN_TIMEOUT_SECS);
        let janitor_interval = Duration::from_secs(JANITOR_INTERVAL_SECS);
        self.task_manager.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        break;
                    }
                    _ = tokio::time::sleep(janitor_interval) => {
                        // Avoid removing while iterating the same DashMap, which can block.
                        let now = Instant::now();
                        allocated_tokens.retain(|_, (timestamp, _)| {
                            now.duration_since(*timestamp) <= allocated_token_timeout
                        });
                        active_tokens.retain(|_, (_, timestamp, _)| {
                            now.duration_since(*timestamp) <= active_token_timeout
                        });
                    }
                }
            }
        });
    }
}
