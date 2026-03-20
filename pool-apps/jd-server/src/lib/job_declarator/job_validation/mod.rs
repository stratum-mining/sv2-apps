//! Module for validating Custom Jobs.

use stratum_apps::{
    stratum_core::{
        bitcoin::Wtxid,
        job_declaration_sv2::{DeclareMiningJob, ProvideMissingTransactionsSuccess, PushSolution},
        mining_sv2::SetCustomMiningJob,
    },
    utils::types::JdToken,
};

pub mod bitcoin_core_ipc;

/// The trait that JDS will use to validate and propagate solutions for Custom Jobs.
/// This allows for modularity with regards to:
/// - different Bitcoin Node implementations.
/// - different ways to connect to the Bitcoin Node.
///
/// Please note that while this is a trait with some similarities with
/// `handlers_sv2::job_declaration::HandleJobDeclarationMessagesFromClientAsync`,
/// this has a different purpose.
///
/// More specifically, we diverge from
/// `handlers_sv2::job_declaration::HandleJobDeclarationMessagesFromClientAsync` in the following
/// ways:
/// - we do not handle the `AllocateMiningJobToken` message
/// - we handle `SetCustomMiningJob` message
#[async_trait::async_trait]
pub trait JobValidationEngine: Send + Sync {
    /// Handles a declare mining job request.
    async fn handle_declare_mining_job(
        &self,
        declare_mining_job: DeclareMiningJob<'_>,
        provide_missing_transactions_success: Option<ProvideMissingTransactionsSuccess<'_>>,
    ) -> DeclareMiningJobResult;

    /// Submits a mining solution to the backend.
    async fn handle_push_solution(&self, push_solution: PushSolution<'_>);

    /// Validates a `SetCustomMiningJob` (Mining Protocol) against the previously declared job
    /// identified by `allocated_token`.
    async fn handle_set_custom_mining_job(
        &self,
        set_custom_mining_job: SetCustomMiningJob<'_>,
        allocated_token: JdToken,
    ) -> SetCustomMiningJobResult;

    /// Performs backend-specific shutdown work.
    ///
    /// Default implementation is a no-op so non-threaded engines do not need to
    /// implement custom teardown.
    fn shutdown(&self) {}
}

/// Result of a [`JobValidationEngine::handle_declare_mining_job`] call.
pub enum DeclareMiningJobResult {
    Success,
    Error(String), // error_code string
    MissingTransactions(Vec<Wtxid>),
}

/// Result of a [`JobValidationEngine::handle_set_custom_mining_job`] call.
pub enum SetCustomMiningJobResult {
    Success,
    Error(String), // error_code string
}
