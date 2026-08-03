//! Utilities for managing JDC communication, connection setup,
//! shutdown signaling, and upstream state tracking.
//!
//! This module provides:
//! - Construction of `SetupConnection` messages for mining, job declarator, and template
//!   distribution protocols.
//! - Helpers for parsing frames into typed Stratum messages.
//! - An async I/O task spawner for handling framed network communication with shutdown
//!   coordination.
//! - Deserialization of coinbase transaction outputs.
//! - Shutdown signaling types for orchestrating controlled shutdown of upstream, downstream, and
//!   job declarator components.
//! - An atomic wrapper for managing the upstream connection state safely across threads.
use std::{
    collections::BinaryHeap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32, Ordering},
    },
};
use stratum_apps::stratum_core::{
    binary_sv2::Str0255Owned,
    job_declaration_sv2::PushSolutionOwned,
    mining_sv2::{
        CloseChannelOwned, OpenExtendedMiningChannelOwned, OpenStandardMiningChannelOwned,
    },
};

use stratum_apps::{
    key_utils::Secp256k1PublicKey,
    stratum_core::{
        bitcoin::hashes::sha256d,
        channels_sv2::client::{self, extended::ExtendedChannel},
        common_messages_sv2::{Protocol, SetupConnectionOwned},
        mining_sv2::SubmitSharesExtendedOwned,
        parsers_sv2::{JobDeclarationOwned, MiningOwned, Tlv},
        template_distribution_sv2::SetNewPrevHashOwned as SetNewPrevHashTdp,
    },
    utils::types::{ChannelId, DownstreamId, Hashrate, JobId},
};
use tracing::{debug, info};

use crate::{
    channel_manager::downstream_message_handler::RouteMessageTo, error::JDCErrorKind,
    jd_mode::JDMode,
};

pub(crate) type DownstreamMessage = (MiningOwned, Option<Vec<Tlv>>);

/// Represents a single upstream entry (Pool + JDS pair) with raw address strings
/// that are resolved via DNS at connection time.
#[derive(Debug, Clone)]
pub struct UpstreamEntry {
    /// Pool host — can be an IP address or a hostname.
    pub pool_host: String,
    pub pool_port: u16,
    /// JDS host — can be an IP address or a hostname.
    pub jds_host: String,
    pub jds_port: u16,
    pub authority_pubkey: Secp256k1PublicKey,
    pub tried_or_flagged: bool,
    pub user_identity: String,
}

/// Constructs a `SetupConnection` message for the mining protocol.
pub fn get_setup_connection_message(
    min_version: u16,
    max_version: u16,
    address: &SocketAddr,
) -> Result<SetupConnectionOwned, JDCErrorKind> {
    let address_str = address.ip().to_string();
    let endpoint_host = address_str.as_str().try_into()?;
    let vendor = "".try_into()?;
    let hardware_version = "".try_into()?;
    let firmware = "".try_into()?;
    let device_id = "".try_into()?;
    let flags = 0b0000_0000_0000_0000_0000_0000_0000_0110;
    Ok(SetupConnectionOwned {
        protocol: Protocol::MiningProtocol,
        min_version,
        max_version,
        flags,
        endpoint_host,
        endpoint_port: address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    })
}

/// Constructs a `SetupConnection` message for the Job Declarator (JDS).
pub fn get_setup_connection_message_jds(
    proxy_address: &SocketAddr,
    mode: &JDMode,
) -> SetupConnectionOwned {
    let address_str = proxy_address.ip().to_string();
    let endpoint_host = address_str.as_str().try_into().unwrap();
    let vendor = "".try_into().unwrap();
    let hardware_version = "".try_into().unwrap();
    let firmware = "".try_into().unwrap();
    let device_id = "".try_into().unwrap();
    let mut setup_connection = SetupConnectionOwned {
        protocol: Protocol::JobDeclarationProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
        endpoint_host,
        endpoint_port: proxy_address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    };

    if mode.is_config_full_template() {
        setup_connection.allow_full_template_mode();
    }

    setup_connection
}

/// Constructs a `SetupConnection` message for the Template Provider (TP).
pub fn get_setup_connection_message_tp(address: SocketAddr) -> SetupConnectionOwned {
    let address_str = address.ip().to_string();
    let endpoint_host = address_str.as_str().try_into().unwrap();
    let vendor = "".try_into().unwrap();
    let hardware_version = "".try_into().unwrap();
    let firmware = "".try_into().unwrap();
    let device_id = "".try_into().unwrap();
    SetupConnectionOwned {
        protocol: Protocol::TemplateDistributionProtocol,
        min_version: 2,
        max_version: 2,
        flags: 0b0000_0000_0000_0000_0000_0000_0000_0000,
        endpoint_host,
        endpoint_port: address.port(),
        vendor,
        hardware_version,
        firmware,
        device_id,
    }
}

/// Represents the state of the upstream connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamState {
    /// No channel established with upstream.
    NoChannel = 0,
    /// Channel is being established undergoing.
    Pending = 1,
    /// Channel is active and connected.
    Connected = 2,
    /// Running in solo mining mode.
    SoloMining = 3,
}

/// Atomic wrapper for managing upstream connection state safely across threads.
#[derive(Clone)]
pub struct AtomicUpstreamState {
    inner: Arc<AtomicU8>,
}

impl AtomicUpstreamState {
    /// Creates a new atomic upstream state.
    pub fn new(state: UpstreamState) -> Self {
        Self {
            inner: Arc::new(AtomicU8::new(state as u8)),
        }
    }

    /// Returns the current upstream state.
    pub fn get(&self) -> UpstreamState {
        match self.inner.load(Ordering::SeqCst) {
            0 => UpstreamState::NoChannel,
            1 => UpstreamState::Pending,
            2 => UpstreamState::Connected,
            3 => UpstreamState::SoloMining,
            _ => unreachable!("invalid upstream state"),
        }
    }

    /// Updates the upstream state
    pub fn set(&self, state: UpstreamState) {
        self.inner.store(state as u8, Ordering::SeqCst);
    }

    /// Conditionally updates the upstream state if the current value matches.
    pub fn compare_and_set(
        &self,
        current: UpstreamState,
        new: UpstreamState,
    ) -> Result<(), UpstreamState> {
        self.inner
            .compare_exchange(current as u8, new as u8, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|v| match v {
                0 => UpstreamState::NoChannel,
                1 => UpstreamState::Pending,
                2 => UpstreamState::Connected,
                3 => UpstreamState::SoloMining,
                _ => unreachable!("invalid upstream state"),
            })
    }
}

/// Represents a pending channel request during the bootstrap phase
/// of the Job Declarator Client (JDC).  
///
/// These requests are created by downstreams that want to open
/// a mining channel but cannot proceed immediately.  
/// They remain queued until an upstream channel is successfully opened,
/// at which point they can be processed.
///
/// Two types of requests can be pending:
/// - [`OpenExtendedMiningChannel`] for extended mining channels
/// - [`OpenStandardMiningChannel`] for standard mining channels
pub enum PendingChannelRequest {
    /// A request to open an extended mining channel.
    ExtendedChannel {
        downstream_id: DownstreamId,
        message: OpenExtendedMiningChannelOwned,
    },
    /// A request to open a standard mining channel.
    StandardChannel {
        downstream_id: DownstreamId,
        message: OpenStandardMiningChannelOwned,
    },
}

impl From<(DownstreamId, OpenExtendedMiningChannelOwned)> for PendingChannelRequest {
    fn from(value: (DownstreamId, OpenExtendedMiningChannelOwned)) -> Self {
        PendingChannelRequest::ExtendedChannel {
            downstream_id: value.0,
            message: value.1,
        }
    }
}

impl From<(DownstreamId, OpenStandardMiningChannelOwned)> for PendingChannelRequest {
    fn from(value: (DownstreamId, OpenStandardMiningChannelOwned)) -> Self {
        PendingChannelRequest::StandardChannel {
            downstream_id: value.0,
            message: value.1,
        }
    }
}

impl PendingChannelRequest {
    pub fn downstream_id(&self) -> DownstreamId {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id,
                message: _,
            } => *downstream_id,
            PendingChannelRequest::StandardChannel {
                downstream_id,
                message: _,
            } => *downstream_id,
        }
    }

    pub fn message(self) -> MiningOwned {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id: _,
                message: open_channel_message,
            } => MiningOwned::OpenExtendedMiningChannel(open_channel_message),
            PendingChannelRequest::StandardChannel {
                downstream_id: _,
                message: open_channel_message,
            } => MiningOwned::OpenStandardMiningChannel(open_channel_message),
        }
    }

    pub fn hashrate(&self) -> Hashrate {
        match self {
            PendingChannelRequest::ExtendedChannel {
                downstream_id: _,
                message: m,
            } => m.nominal_hash_rate,
            PendingChannelRequest::StandardChannel {
                downstream_id: _,
                message: m,
            } => m.nominal_hash_rate,
        }
    }
}

/// Creates a [`CloseChannel`] message for the given channel ID and reason.
///
/// The `msg` is converted into a [`Str0255`] reason code.  
/// If conversion fails, this function will panic.
pub(crate) fn create_close_channel_msg(channel_id: ChannelId, msg: &str) -> CloseChannelOwned {
    CloseChannelOwned {
        channel_id,
        reason_code: Str0255Owned::try_from(msg).expect("Could not convert message."),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownstreamChannelJobId {
    pub downstream_id: DownstreamId,
    pub channel_id: ChannelId,
    pub job_id: JobId,
}

impl From<(DownstreamId, ChannelId, JobId)> for DownstreamChannelJobId {
    fn from(value: (DownstreamId, ChannelId, JobId)) -> Self {
        DownstreamChannelJobId {
            downstream_id: value.0,
            channel_id: value.1,
            job_id: value.2,
        }
    }
}

/// This method validates cached shares when a `SetCustomMiningJob.Success`
/// arrives. This method also appends response to route queue to be sent
/// to upstream.
pub fn validate_cached_share(
    mut upstream_message: SubmitSharesExtendedOwned,
    upstream_channel: &mut ExtendedChannel,
    prev_hash: &SetNewPrevHashTdp,
    sequence_number_factory: &AtomicU32,
    messages: &mut Vec<RouteMessageTo>,
) {
    match upstream_channel.validate_share(upstream_message.clone()) {
        Ok(client::share_accounting::ShareValidationResult::Valid(share_hash)) => {
            upstream_message.sequence_number =
                sequence_number_factory.fetch_add(1, Ordering::Relaxed);

            info!(
                "Cached SubmitSharesExtended: valid share, forwarding it to upstream | channel_id: {}, sequence_number: {}, share_hash: {}  ✅",
                upstream_message.channel_id, upstream_message.sequence_number, share_hash
            );

            messages.push(MiningOwned::SubmitSharesExtended(upstream_message).into());
        }

        Ok(client::share_accounting::ShareValidationResult::BlockFound(share_hash)) => {
            upstream_message.sequence_number =
                sequence_number_factory.fetch_add(1, Ordering::Relaxed);

            info!("💰 Block Found (cached extended)!!! 💰 {share_hash}");

            let mut channel_extranonce = upstream_channel.get_extranonce_prefix().to_vec();
            channel_extranonce.extend_from_slice(upstream_message.extranonce.as_bytes());

            let push_solution = PushSolutionOwned {
                extranonce: channel_extranonce.try_into().expect("extranonce"),
                ntime: upstream_message.ntime,
                nonce: upstream_message.nonce,
                version: upstream_message.version,
                nbits: prev_hash.n_bits,
                prev_hash: prev_hash.prev_hash.clone(),
            };

            messages.push(JobDeclarationOwned::PushSolution(push_solution).into());
            messages.push(MiningOwned::SubmitSharesExtended(upstream_message).into());
        }

        Err(err) => {
            let code = match err {
                client::share_accounting::ShareValidationError::Invalid(code) => code,
                client::share_accounting::ShareValidationError::Stale(code) => code,
                client::share_accounting::ShareValidationError::InvalidJobId(code) => code,
                client::share_accounting::ShareValidationError::DoesNotMeetTarget(code) => code,
                client::share_accounting::ShareValidationError::DuplicateShare(code) => code,
                client::share_accounting::ShareValidationError::BadExtranonceSize(code) => code,
                client::share_accounting::ShareValidationError::VersionRollingNotAllowed(code) => {
                    code
                }
                _ => unreachable!(),
            };

            debug!(
                "❌ Cached SubmitSharesExtended: SubmitSharesError, not forwarding it to upstream | channel_id={}, sequence_number={}, error={code}",
                upstream_message.channel_id, upstream_message.sequence_number
            );
        }
    }
}

/// Maximum number of shares cached per template
const CACHED_SHARES_CAPACITY: usize = 100;

/// A wrapper around [`SubmitSharesExtended`] that adds ordering by share difficulty.
#[derive(Clone, Debug)]
pub struct SharesOrderedByDiff {
    pub share: SubmitSharesExtendedOwned,
    share_hash: sha256d::Hash,
}

impl SharesOrderedByDiff {
    pub fn new(share: SubmitSharesExtendedOwned, share_hash: sha256d::Hash) -> Self {
        Self { share, share_hash }
    }
}

impl Ord for SharesOrderedByDiff {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.share_hash.cmp(&other.share_hash)
    }
}

impl PartialOrd for SharesOrderedByDiff {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SharesOrderedByDiff {
    fn eq(&self, other: &Self) -> bool {
        self.share_hash == other.share_hash
    }
}

impl Eq for SharesOrderedByDiff {}

/// Inserts a share into the cache, evicting the worst entry when
/// [`CACHED_SHARES_CAPACITY`] is reached.
///
/// The cache retains the best shares (lowest `share_hash`), since lower
/// hashes indicate higher-quality shares that are more likely to remain
/// valid if relayed later.
///
/// Internally implemented with a `BinaryHeap`, where the root represents
/// the current worst share (highest hash) and is replaced when a better
/// share arrives.
pub(crate) fn add_share_to_cache(
    heap: &mut BinaryHeap<SharesOrderedByDiff>,
    entry: SharesOrderedByDiff,
) {
    let len = heap.len();

    if len < CACHED_SHARES_CAPACITY {
        debug!(
            "Caching share (hash={:?}); cache size {}/{}",
            entry.share_hash,
            len + 1,
            CACHED_SHARES_CAPACITY
        );
        heap.push(entry);
        return;
    }

    if let Some(worst) = heap.peek() {
        if entry.share_hash < worst.share_hash {
            debug!(
                "Replacing worst cached share: old_hash={:?}, new_hash={:?}",
                worst.share_hash, entry.share_hash
            );
            heap.pop();
            heap.push(entry);
        } else {
            debug!(
                "Discarding share (hash={:?}); worse than cached worst={:?}",
                entry.share_hash, worst.share_hash
            );
        }
    }
}
