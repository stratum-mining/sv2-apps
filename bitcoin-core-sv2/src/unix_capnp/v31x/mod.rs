//! Bitcoin Core v31.x IPC implementation modules.
//!
//! This namespace contains the concrete v31.x runtime implementations used when
//! [`crate::runtime_api::BitcoinCoreVersion::V31X`] is selected.
//!
//! It is wired against `bitcoin_capnp_types_v31`, which re-exports the matching `capnp`
//! and `capnp-rpc` APIs.

pub(crate) use bitcoin_capnp_types_v31 as bitcoin_capnp_types;

pub mod job_declaration_protocol;
pub mod template_distribution_protocol;
