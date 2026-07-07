//! Bitcoin Core master IPC implementation modules.
//!
//! This namespace reuses the v31.x backend source with Bitcoin Core master capnp bindings. If
//! master diverges, add only the changed module here and keep unchanged modules path-imported.

#![allow(clippy::duplicate_mod)]

pub(crate) use bitcoin_capnp_types_master as capnp_types;

#[path = "../v31x/job_declaration_protocol/mod.rs"]
pub mod job_declaration_protocol;

#[path = "../v31x/template_distribution_protocol/mod.rs"]
pub mod template_distribution_protocol;
