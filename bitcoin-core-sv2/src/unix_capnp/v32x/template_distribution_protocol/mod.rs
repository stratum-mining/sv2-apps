//! Module for interacting with Bitcoin Core v32.x via Sv2 Template Distribution Protocol via
//! capnp over UNIX socket.

use super::bitcoin_capnp_types;

#[allow(clippy::duplicate_mod)]
#[path = "../../v32x_v31x/template_distribution_protocol/mod.rs"]
mod shared;

pub use shared::*;
