//! ## Downstream SV1 Module
//!
//! This module defines the structures, messages, and utility functions
//! used for handling the downstream connection with SV1 mining clients.
//!
//! It includes definitions for messages exchanged with a Bridge component,
//! structures for submitting shares and updating targets, and constants
//! and functions for managing client interactions.
//!
//! The module is organized as follows:
//! - The [`Downstream`] struct handles the state and communication for a single downstream SV1
//!   miner.
//! - The [`Sv1Server`] struct is the main server that listens for and manages all downstream miner
//!   connections.

mod downstream;
mod job_store;
#[cfg(feature = "monitoring")]
mod monitoring;
mod sv1_server;
pub use downstream::Downstream;
pub use sv1_server::Sv1Server;
