//! Shared implementation modules reused by Bitcoin Core v31.x and v32.x runtimes.
//!
//! Shared JDP/TDP modules in this namespace are reused via `#[path = "..."]` from
//! `unix_capnp::v31x` and `unix_capnp::v32x`, so they compile in each version-local `super::*`
//! context; they are not exported from this module tree.
