//! Version-agnostic Job Declaration Protocol runtime API.
//!
//! This module exposes a runtime handle that receives [`io::JdRequest`] messages and bridges them
//! to the selected Bitcoin Core IPC backend.
//!
//! The request channel covers the two base JDP flows:
//! - `DeclareMiningJob`
//! - `PushSolution`
//!
//! Token lifecycle and higher-level protocol state remain the caller responsibility (for example,
//! associating `AllocateMiningJobToken`/`DeclareMiningJob`/`SetCustomMiningJob` state).

pub mod io;

use crate::{
    common::{BitcoinCoreSv2Error, BitcoinCoreSv2Protocol, BitcoinCoreVersion},
    unix_capnp::{v30x, v31x, v32x},
};
use async_channel::Receiver;
use io::JdRequest;
use std::path::Path;
use stratum_core::bitcoin::{TxMerkleNode, Txid, consensus::Encodable, hashes::Hash};
pub use tokio_util::sync::CancellationToken;

/// Version-agnostic JDP runtime handle.
///
/// Instances are created with [`new`], which selects the concrete backend for the requested
/// [`BitcoinCoreVersion`].
pub enum BitcoinCoreSv2JDP {
    V30X(v30x::job_declaration_protocol::BitcoinCoreSv2JDP),
    V31X(v31x::job_declaration_protocol::BitcoinCoreSv2JDP),
    V32X(v32x::job_declaration_protocol::BitcoinCoreSv2JDP),
}

impl BitcoinCoreSv2JDP {
    pub async fn run(&self) {
        match self {
            Self::V30X(runtime) => runtime.run().await,
            Self::V31X(runtime) => runtime.run().await,
            Self::V32X(runtime) => runtime.run().await,
        }
    }
}

pub type BitcoinCoreSv2JDPError = BitcoinCoreSv2Error;

/// Builds a version-agnostic JDP runtime from the selected Bitcoin Core major version.
pub async fn new<P>(
    version: BitcoinCoreVersion,
    bitcoin_core_unix_socket_path: P,
    incoming_requests: Receiver<JdRequest>,
    cancellation_token: CancellationToken,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) -> Result<BitcoinCoreSv2JDP, BitcoinCoreSv2JDPError>
where
    P: AsRef<Path>,
{
    match version {
        BitcoinCoreVersion::V30X => v30x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V30X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
        BitcoinCoreVersion::V31X => v31x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V31X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
        BitcoinCoreVersion::V32X => v32x::job_declaration_protocol::BitcoinCoreSv2JDP::new(
            bitcoin_core_unix_socket_path,
            incoming_requests,
            cancellation_token,
            ready_tx,
        )
        .await
        .map(BitcoinCoreSv2JDP::V32X)
        .map_err(|error| {
            BitcoinCoreSv2JDPError::from_debug(version, BitcoinCoreSv2Protocol::JDP, error)
        }),
    }
}

/// Computes the coinbase merkle branch in the txid merkle tree for a block whose
/// non-coinbase transactions have the given txids (in block order).
///
/// Returns the sibling hashes at each level from leaf to root, needed to reconstruct the
/// block header's merkle root from the coinbase position (index 0). The branch does not
/// depend on the coinbase transaction itself, so a placeholder leaf is used.
///
/// Used to compare with a `SetCustomMiningJob.merkle_path`.
pub fn coinbase_merkle_branch(txids: &[Txid]) -> Vec<TxMerkleNode> {
    let mut hashes: Vec<TxMerkleNode> = Vec::with_capacity(1 + txids.len());
    // placeholder for the coinbase txid; position-0 branches never include their own leaf
    hashes.push(TxMerkleNode::all_zeros());
    for txid in txids {
        hashes.push((*txid).into());
    }

    if hashes.len() == 1 {
        return Vec::new();
    }

    let mut branch = Vec::new();

    while hashes.len() > 1 {
        branch.push(hashes[1]);

        let half = hashes.len().div_ceil(2);
        let mut next_level = Vec::with_capacity(half);
        for idx in 0..half {
            let left = hashes[2 * idx];
            let right = hashes[std::cmp::min(2 * idx + 1, hashes.len() - 1)];
            let mut engine = TxMerkleNode::engine();
            left.consensus_encode(&mut engine)
                .expect("in-memory writers don't error");
            right
                .consensus_encode(&mut engine)
                .expect("in-memory writers don't error");
            next_level.push(TxMerkleNode::from_engine(engine));
        }
        hashes = next_level;
    }

    branch
}
