//! Job Declaration Server (JDS) for the Stratum V2 protocol.
//!
//! This crate implements a server that validates custom mining jobs declared by downstream
//! clients via the [Job Declaration Protocol (JDP)]. It manages token allocation, coinbase
//! and merkle path validation, and delegates block-level validation to a modular Job Validation
//! Engine.
//!
//! The primary entry point is [`job_declarator::JobDeclarator`], which provides:
//! - [`JobDeclarator::new`](job_declarator::JobDeclarator::new) — creates the engine with a
//!   validation backend (e.g. Bitcoin Core IPC).
//! - [`JobDeclarator::start`](job_declarator::JobDeclarator::start) — launches the JDP message
//!   processing loop.
//! - [`JobDeclarator::start_downstream_server`](job_declarator::JobDeclarator::start_downstream_server)
//!   — listens for JDC connections on a Noise-encrypted TCP socket.
//! - [`JobDeclarator::handle_set_custom_mining_job`](job_declarator::JobDeclarator::handle_set_custom_mining_job)
//!   — validates a Mining Protocol `SetCustomMiningJob` message (for use when embedded in Pool).
//!
//! When used as a library (embedded in Pool), the caller is responsible for owning the
//! [`CancellationToken`](bitcoin_core_sv2::job_declaration_protocol::CancellationToken) and
//! [`TaskManager`](stratum_apps::task_manager::TaskManager).
//!
//! [Job Declaration Protocol (JDP)]: https://github.com/stratum-mining/sv2-spec/blob/main/07-Job-Declaration-Protocol.md

pub mod config;
pub mod error;
pub mod io_task;
pub mod job_declarator;
