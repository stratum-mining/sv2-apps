#[cfg(feature = "monitoring")]
use std::net::IpAddr;
use std::{
    collections::{BinaryHeap, VecDeque},
    net::SocketAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicUsize, Ordering},
    },
};
use stratum_apps::stratum_core::{
    job_declaration_sv2::{AllocateMiningJobTokenOwned, AllocateMiningJobTokenSuccessOwned},
    mining_sv2::{OpenExtendedMiningChannelOwned, SetTargetOwned, UpdateChannelOwned},
    template_distribution_sv2::RequestTransactionDataOwned,
};

use async_channel::{Receiver, Sender, unbounded};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    coinbase_output_constraints::coinbase_output_constraints_message,
    fallback_coordinator::FallbackCoordinator,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::accept_noise_connection,
    stratum_core::{
        bitcoin::{Amount, Target, TxOut},
        channels_sv2::{
            Vardiff, VardiffState,
            client::extended::ExtendedChannel,
            extranonce_manager::{ExtranonceAllocator, bytes_needed},
            outputs::deserialize_outputs,
            server::{group::GroupChannel, jobs::factory::JobFactory, standard::StandardChannel},
        },
        handlers_sv2::{
            HandleExtensionsFromServerOwnedAsync, HandleJobDeclarationMessagesFromServerOwnedAsync,
            HandleMiningMessagesFromClientOwnedAsync, HandleMiningMessagesFromServerOwnedAsync,
            HandleTemplateDistributionMessagesFromServerOwnedAsync,
        },
        job_declaration_sv2::DeclareMiningJobOwned,
        mining_sv2::SetCustomMiningJobOwned,
        parsers_sv2::{
            AnyMessageOwned, JobDeclarationOwned, MiningOwned, TemplateDistributionOwned, Tlv,
        },
        template_distribution_sv2::{NewTemplateOwned, SetNewPrevHashOwned as SetNewPrevHashTdp},
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{
            ChannelId, DownstreamId, OutboundFrame, RequestId, SerializedFrame, SharesBatchSize,
            SharesPerMinute, Sv2Frame, TemplateId, UpstreamJobId, VardiffKey,
        },
    },
};
use tokio::{net::TcpListener, select};
use tracing::{debug, error, info, warn};

use crate::{
    channel_manager::downstream_message_handler::RouteMessageTo,
    config::JobDeclaratorClientConfig,
    downstream::Downstream,
    error::{self, Action, JDCError, JDCErrorKind, JDCResult, LoopControl},
    jd_mode::JDMode,
    utils::{
        AtomicUpstreamState, DownstreamChannelJobId, DownstreamMessage, PendingChannelRequest,
        SharesOrderedByDiff, UpstreamState,
    },
};
#[cfg(feature = "monitoring")]
use stratum_apps::monitoring::{MinerTelemetry, MinerTelemetryStatus};
pub mod downstream_message_handler;
mod extensions_message_handler;
mod jd_message_handler;
mod template_message_handler;
mod upstream_message_handler;

// ============================================================================
// JDC extranonce layout
// ============================================================================
//
// JDC multiplexes many downstream channels (standard + extended) over a
// **single** upstream extended channel whose `extranonce_size` is fixed at
// opening time — JDC cannot grow it later. Every SV2 extranonce JDC
// produces is therefore laid out as:
//
//     | upstream_prefix | local_index | downstream rollable |
//
// * `upstream_prefix`: pool-assigned (empty in solo mining).
// * `local_index`: JDC's per-channel slot; width = [`JDC_LOCAL_PREFIX_BYTES`].
// * `downstream rollable`: what an extended downstream rolls (absent for standard downstreams).
//
// The size JDC asks the pool for at open time must cover **every** future
// downstream, not just the one that triggered the open. The floor on the
// rollable region is configured by
// [`JobDeclaratorClientConfig::reserved_downstream_rollable_extranonce_size`];
// see `handle_downstream_message` for the exact formula.

/// Maximum number of concurrent downstream channels JDC can allocate.
/// Determines [`JDC_LOCAL_PREFIX_BYTES`] via [`bytes_needed`].
const JDC_MAX_CHANNELS: u32 = 65_536;

/// Bytes consumed by JDC's per-channel `local_index`. Derived from
/// [`JDC_MAX_CHANNELS`] so the two stay in sync.
const JDC_LOCAL_PREFIX_BYTES: u8 = bytes_needed(JDC_MAX_CHANNELS);

/// Total extranonce length used by JDC in **solo mining** mode (no upstream
/// pool). Mirrors the Pool's layout so both modes produce consistent shapes.
///
/// ```text
/// | local_index (2) | downstream rollable (18) |
/// |<----- SOLO_FULL_EXTRANONCE_SIZE = 20 ---------->|
/// ```
pub const SOLO_FULL_EXTRANONCE_SIZE: u8 = 20;

/// A `DeclaredJob` encapsulates all the relevant data associated with a single
/// job declaration, including its template, optional messages, coinbase output,
/// and transaction list.
#[derive(Clone, Debug)]
pub struct DeclaredJob {
    // The original `DeclareMiningJob` message associated with this job,
    // if one was sent.
    declare_mining_job: Option<DeclareMiningJobOwned>,
    // The template associated with the declared job.
    template: NewTemplateOwned,
    // The `SetNewPrevHashTdp` message associated with this job, if available.
    prev_hash: Option<SetNewPrevHashTdp>,
    // The `SetCustomMiningJob` message associated with this job,
    // if a custom job was created.
    set_custom_mining_job: Option<SetCustomMiningJobOwned>,
    // The coinbase output for this job.
    coinbase_output: Vec<u8>,
    // The list of transactions included in the job’s template.
    tx_list: Vec<Vec<u8>>,
}

/// Represents all communication channels managed by the Channel Manager.
///
/// The `ChannelManagerIo` holds all the asynchronous communication primitives
/// required for message exchange between the **Channel Manager** and other subsystems.
/// It ensures decoupled, structured communication between upstreams, downstreams,
/// the Job Dispatcher Service (JDS), and the Template Provider (TP).
///
/// # Channels
/// 1. **Upstream**:
///    - `(upstream_sender, upstream_receiver)` Used to send and receive messages from the upstream
///      subsystem.
///
/// 2. **JDS**:
///    - `(jd_sender, jd_receiver)` Handles communication with JDS.
///
/// 3. **Template Provider**:
///    - `(tp_sender, tp_receiver)` Manages communication with the Template Provider.
///
/// 4. **Downstream**:
///    - `(downstream_sender, downstream_receiver)` Broadcasts messages to all downstream clients
///      and receives messages from them.
///
/// 5. **Status**:
///    - `status_sender` Allows the Channel Manager to notify the main status loop of critical state
///      changes.

#[derive(Clone)]
pub struct ChannelManagerIo {
    upstream_sender: Sender<OutboundFrame>,
    upstream_receiver: Receiver<SerializedFrame>,
    jd_sender: Sender<JobDeclarationOwned>,
    jd_receiver: Receiver<JobDeclarationOwned>,
    tp_sender: Sender<TemplateDistributionOwned>,
    tp_receiver: Receiver<TemplateDistributionOwned>,
    downstream_sender: SharedMap<DownstreamId, Sender<DownstreamMessage>>,
    downstream_receiver: Receiver<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
}

impl ChannelManagerIo {
    fn close(&self, close_template_provider: bool) {
        self.upstream_sender.close();
        self.jd_sender.close();
        self.upstream_receiver.close_and_drain();
        self.jd_receiver.close_and_drain();
        if close_template_provider {
            self.tp_sender.close();
            self.tp_receiver.close_and_drain();
        }
        self.downstream_receiver.close_and_drain();
        self.downstream_sender.for_each(|_, sender| sender.close());
        self.downstream_sender.clear();
    }
}

#[cfg(feature = "monitoring")]
#[derive(Clone)]
pub(crate) struct MinerTelemetryState {
    /// Latest telemetry fetched for each matched downstream connection.
    pub(crate) telemetry: SharedMap<DownstreamId, MinerTelemetry>,
    /// Miner management IP selected for each matched downstream connection.
    pub(crate) management_ips: SharedMap<DownstreamId, IpAddr>,
    /// Latest telemetry matching status for each active downstream connection.
    pub(crate) statuses: SharedMap<DownstreamId, MinerTelemetryStatus>,
    /// LAN CIDR ranges scanned for miner management interfaces.
    pub(crate) cidrs: Vec<String>,
    /// JDC listening port used to match discovered pool entries to this instance.
    pub(crate) pool_port: u16,
}

#[cfg(feature = "monitoring")]
impl MinerTelemetryState {
    fn new(cidrs: Vec<String>, pool_port: u16) -> Self {
        Self {
            telemetry: SharedMap::new(),
            management_ips: SharedMap::new(),
            statuses: SharedMap::new(),
            cidrs,
            pool_port,
        }
    }

    fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.telemetry.remove(&downstream_id);
        self.management_ips.remove(&downstream_id);
        self.statuses.remove(&downstream_id);
    }

    pub(crate) fn telemetry_for(&self, downstream_id: DownstreamId) -> Option<MinerTelemetry> {
        self.telemetry.get_cloned(&downstream_id)
    }

    pub(crate) fn management_ip_for(&self, downstream_id: DownstreamId) -> Option<IpAddr> {
        self.management_ips.get_cloned(&downstream_id)
    }

    pub(crate) fn status_for(&self, downstream_id: DownstreamId) -> Option<MinerTelemetryStatus> {
        self.statuses.get_cloned(&downstream_id)
    }
}

/// Contains all the state of mutable and immutable data required
/// by channel manager to process its task along with channels
/// to perform message traversal.
#[derive(Clone)]
pub struct ChannelManager {
    channel_manager_io: ChannelManagerIo,
    // Mapping of `downstream_id` -> `Downstream` object,
    // used by the channel manager to locate and interact with downstream clients.
    pub downstream: SharedMap<DownstreamId, Downstream>,
    // Unified extranonce prefix allocator shared by standard and extended
    // downstream channels. Rebuilt with the upstream-assigned prefix whenever
    // the upstream connection is (re)negotiated or `SetExtranoncePrefix` is
    // received. The allocated prefix is stored on the channel itself, so
    // dropping the channel automatically releases the slot.
    pub extranonce_allocator: SharedLock<ExtranonceAllocator>,
    // Factory that generates monotonically increasing request IDs
    // for messages sent from the JDC.
    pub request_id_factory: Arc<AtomicU32>,
    // Factory that assigns a unique ID to each new downstream connection.
    pub downstream_id_factory: Arc<AtomicUsize>,
    // Factory that assigns a unique sequence number to each share
    // submitted from the JDC to the upstream.
    pub sequence_number_factory: Arc<AtomicU32>,
    // The last future template received from the template provider.
    pub last_future_template: SharedLock<Option<NewTemplateOwned>>,
    // The last new-prevhash received from the template provider.
    pub last_new_prev_hash: SharedLock<Option<SetNewPrevHashTdp>>,
    // FIFO buffer of allocation tokens received from the JDS.
    // Oldest token is consumed first to minimize risk of JDS-side expiration.
    pub allocate_tokens: SharedLock<VecDeque<AllocateMiningJobTokenSuccessOwned>>,
    // Stores templates as they arrive, keyed by template ID.
    pub template_store: SharedMap<TemplateId, NewTemplateOwned>,
    // Stores the last declared job, keyed by the request ID used
    // when declaring the job to the JDS.
    pub last_declare_job_store: SharedMap<RequestId, DeclaredJob>,
    // Maps a template ID to the corresponding upstream job ID.
    pub template_id_to_upstream_job_id: SharedMap<TemplateId, UpstreamJobId>,
    // Maps a downstream ID + channel ID + job ID to the corresponding template ID.
    pub downstream_channel_id_and_job_id_to_template_id:
        SharedMap<DownstreamChannelJobId, TemplateId>,
    // The coinbase outputs currently in use.
    pub coinbase_outputs: SharedLock<Vec<u8>>,
    // The active upstream extended channel (client-side instance), if any.
    pub upstream_channel: SharedLock<Option<ExtendedChannel>>,
    // Optional "pool tag" string identifying the upstream pool.
    pub pool_tag_string: SharedLock<Option<String>>,
    // Pending downstream open-channel requests buffered while JDC is
    // bringing up or re-establishing the upstream channel.
    pub pending_downstream_requests: SharedLock<VecDeque<PendingChannelRequest>>,
    // Factory used to create custom mining jobs, when available.
    pub job_factory: SharedLock<Option<JobFactory>>,
    // Mapping of `(downstream_id, channel_id)` -> vardiff controller.
    // Each entry manages variable difficulty for a specific downstream channel.
    pub vardiff: SharedMap<VardiffKey, VardiffState>,
    /// Extensions that have been successfully negotiated with the upstream server.
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Extensions that the JDC supports.
    pub supported_extensions: Vec<u16>,
    /// Extensions that the JDC requires.
    pub required_extensions: Vec<u16>,
    /// Cached shares waiting for `SetCustomMiningJob.Success` before they can be
    /// validated again and propagated upstream.
    pub cached_shares: SharedMap<TemplateId, BinaryHeap<SharesOrderedByDiff>>,
    miner_tag_string: String,
    share_batch_size: SharesBatchSize,
    shares_per_minute: SharesPerMinute,
    /// Past jobs retained per channel; `None` uses the `channels_sv2` default.
    max_past_jobs: Option<usize>,
    user_identity: Arc<OnceLock<String>>,
    reserved_downstream_rollable_extranonce_size: u8,
    /// This represent the current state of Upstream channel
    /// 1. NoChannel: No active upstream connection.
    /// 2. Pending: A channel request has been sent, awaiting response.
    /// 3. Connected: An upstream channel is successfully established.
    /// 4. SoloMining: No upstream is available; the JDC operates in solo mining mode. case.
    pub upstream_state: AtomicUpstreamState,
    pub mode: JDMode,
    #[cfg(feature = "monitoring")]
    pub(crate) miner_telemetry: MinerTelemetryState,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManager {
    fn handle_error_action(
        &self,
        context: &str,
        e: &JDCError<error::ChannelManager>,
        cancellation_token: &CancellationToken,
        fallback_token: &CancellationToken,
    ) -> LoopControl {
        if cancellation_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error after shutdown was requested"
            );
            return LoopControl::Continue;
        }

        if fallback_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error during fallback"
            );
            return LoopControl::Continue;
        }

        match e.action {
            Action::Log => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            Action::Fallback => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} requested fallback"
                );
                fallback_token.cancel();
                LoopControl::Break
            }
            Action::Shutdown => {
                warn!(
                    error_kind = ?e.kind,
                    "{context} requested shutdown"
                );
                cancellation_token.cancel();
                LoopControl::Break
            }
            Action::Disconnect(downstream_id) => {
                warn!(
                    downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested downstream disconnect"
                );
                self.remove_downstream(downstream_id);
                LoopControl::Continue
            }
        }
    }

    /// Constructor method used to instantiate the Channel Manager
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: JobDeclaratorClientConfig,
        upstream_sender: Sender<OutboundFrame>,
        upstream_receiver: Receiver<SerializedFrame>,
        jd_sender: Sender<JobDeclarationOwned>,
        jd_receiver: Receiver<JobDeclarationOwned>,
        tp_sender: Sender<TemplateDistributionOwned>,
        tp_receiver: Receiver<TemplateDistributionOwned>,
        downstream_receiver: Receiver<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
        coinbase_outputs: Vec<u8>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        mode: JDMode,
    ) -> JDCResult<Self, error::ChannelManager> {
        // Start with a solo-mining allocator (no upstream prefix). Once the
        // upstream channel is opened in `handle_open_extended_mining_channel_success`
        // this allocator is replaced with one built from the upstream prefix.
        let extranonce_allocator =
            ExtranonceAllocator::new(Vec::new(), SOLO_FULL_EXTRANONCE_SIZE, JDC_MAX_CHANNELS)
                .map_err(JDCError::<error::ChannelManager>::shutdown)?;

        // Record the cap actually in force. When unset it comes from the `channels_sv2`
        // default, so it appears nowhere in this application's own config and is
        // otherwise not recoverable from a running instance.
        match config.max_past_jobs() {
            Some(n) if n > 0 => info!(max_past_jobs = n, "past-jobs retention cap configured"),
            _ => info!("past-jobs retention cap: using channels_sv2 default"),
        }

        let channel_manager_io = ChannelManagerIo {
            upstream_sender,
            upstream_receiver,
            jd_sender,
            jd_receiver,
            tp_sender,
            tp_receiver,
            downstream_sender: SharedMap::new(),
            downstream_receiver,
        };

        let channel_manager = ChannelManager {
            channel_manager_io,
            downstream: SharedMap::new(),
            extranonce_allocator: SharedLock::new(extranonce_allocator),
            request_id_factory: Arc::new(AtomicU32::new(0)),
            downstream_id_factory: Arc::new(AtomicUsize::new(0)),
            sequence_number_factory: Arc::new(AtomicU32::new(1)),
            last_future_template: SharedLock::new(None),
            last_new_prev_hash: SharedLock::new(None),
            allocate_tokens: SharedLock::new(VecDeque::new()),
            template_store: SharedMap::new(),
            last_declare_job_store: SharedMap::new(),
            template_id_to_upstream_job_id: SharedMap::new(),
            downstream_channel_id_and_job_id_to_template_id: SharedMap::new(),
            coinbase_outputs: SharedLock::new(coinbase_outputs),
            upstream_channel: SharedLock::new(None),
            pool_tag_string: SharedLock::new(None),
            pending_downstream_requests: SharedLock::new(VecDeque::new()),
            job_factory: SharedLock::new(None),
            vardiff: SharedMap::new(),
            negotiated_extensions: SharedLock::new(vec![]),
            supported_extensions,
            required_extensions,
            cached_shares: SharedMap::new(),
            share_batch_size: config.share_batch_size(),
            shares_per_minute: config.shares_per_minute(),
            max_past_jobs: config.max_past_jobs(),
            miner_tag_string: config.jdc_signature().to_string(),
            user_identity: Arc::new(OnceLock::new()),
            reserved_downstream_rollable_extranonce_size: config
                .reserved_downstream_rollable_extranonce_size(),
            upstream_state: AtomicUpstreamState::new(UpstreamState::SoloMining),
            mode,
            #[cfg(feature = "monitoring")]
            miner_telemetry: MinerTelemetryState::new(
                config.miner_telemetry_cidrs().to_vec(),
                config.listening_address().port(),
            ),
        };

        Ok(channel_manager)
    }

    pub fn set_user_identity(&self, identity: String) {
        self.user_identity
            .set(identity)
            .expect("upstream identity already set");
    }

    fn user_identity(&self) -> &str {
        self.user_identity.get().expect("identity should be set")
    }

    // Bootstraps a group channel with the given parameters.
    // Returns a `GroupChannel` if successful, otherwise returns `None`.
    //
    // To be called before calling Downstream::new.
    fn bootstrap_group_channel(&self, channel_id: ChannelId) -> Option<GroupChannel> {
        let full_extranonce_size = match self.upstream_channel.with(|channel| {
            channel
                .as_ref()
                .map(|channel| channel.get_full_extranonce_size())
                .unwrap_or(SOLO_FULL_EXTRANONCE_SIZE as usize)
        }) {
            Ok(size) => size,
            Err(e) => {
                error!(error = ?JDCErrorKind::from(e), "Failed to access upstream channel state");
                return None;
            }
        };
        let pool_tag_string = match self.pool_tag_string.get() {
            Ok(pool_tag_string) => pool_tag_string,
            Err(e) => {
                error!(error = ?JDCErrorKind::from(e), "Failed to access pool tag state");
                return None;
            }
        };
        let last_future_template = match self.last_future_template.get() {
            Ok(Some(template)) => template,
            Ok(None) => unreachable!("No future template found after readiness check"),
            Err(e) => {
                error!(error = ?JDCErrorKind::from(e), "Failed to access template state");
                return None;
            }
        };
        let last_new_prev_hash = match self.last_new_prev_hash.get() {
            Ok(Some(prev_hash)) => prev_hash,
            Ok(None) => unreachable!("No new prevhash found after readiness check"),
            Err(e) => {
                error!(error = ?JDCErrorKind::from(e), "Failed to access prevhash state");
                return None;
            }
        };
        let miner_tag_string = self.miner_tag_string.clone();
        let mut group_channel = match GroupChannel::new_for_job_declaration_client(
            channel_id,
            full_extranonce_size,
            pool_tag_string.clone(),
            miner_tag_string.clone(),
        ) {
            Ok(channel) => channel,
            Err(e) => {
                error!(error = ?e, "Failed to create group channel");
                return None;
            }
        };

        let coinbase_outputs = match self.coinbase_outputs.get() {
            Ok(outputs) => outputs,
            Err(e) => {
                error!(error = ?JDCErrorKind::from(e), "Failed to access channel manager state");
                return None;
            }
        };
        let mut coinbase_outputs = match deserialize_outputs(coinbase_outputs) {
            Ok(outputs) => outputs,
            Err(e) => {
                error!(error = ?e, "Failed to deserialize coinbase outputs");
                return None;
            }
        };

        coinbase_outputs[0].value =
            Amount::from_sat(last_future_template.coinbase_tx_value_remaining);

        if let Err(e) =
            group_channel.on_new_template(last_future_template, coinbase_outputs.clone())
        {
            error!(error = ?e, "Failed to add template to group channel");
            return None;
        }

        if let Err(e) = group_channel.on_set_new_prev_hash(last_new_prev_hash) {
            error!(error = ?e, "Failed to set new prevhash for group channel");
            return None;
        }

        Some(group_channel)
    }

    /// Starts the downstream server, and accepts new connection request.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_downstream_server(
        self,
        authority_public_key: Secp256k1PublicKey,
        authority_secret_key: Secp256k1SecretKey,
        cert_validity_sec: u64,
        listening_address: SocketAddr,
        task_manager: Arc<TaskManager>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        channel_manager_sender: Sender<(DownstreamId, MiningOwned, Option<Vec<Tlv>>)>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> JDCResult<(), error::ChannelManager> {
        // todo: let start downstream accept channel manager as `Arc`, instead of clone
        let this = Arc::new(self);

        let fallback_token = fallback_coordinator.token();

        // Bind before spawning, so that a bind failure is a startup failure the caller can
        // propagate. Everything after this point (waiting for the first template, then accepting)
        // runs in a background task, so bootstrap is never held hostage by the Template Provider.
        info!("Starting downstream server at {listening_address}");
        let server = TcpListener::bind(listening_address).await.map_err(|e| {
            error!(error = ?e, "Failed to bind downstream server at {listening_address}");
            JDCError::shutdown(e)
        })?;

        let task_manager_clone = task_manager.clone();
        // Register the listener task in fallback coordination, so fallback waits
        // for this accept loop to stop before attempting to re-bind the same port.
        let fallback_handler = fallback_coordinator.register();
        task_manager.spawn(async move {
            // Wait for initial template and prevhash before accepting connections
            let ready = loop {
                let has_required_data = match (
                    this.last_future_template.with(|template| template.is_some()),
                    this.last_new_prev_hash.with(|prev_hash| prev_hash.is_some()),
                ) {
                    (Ok(has_template), Ok(has_prev_hash)) => has_template && has_prev_hash,
                    _ => {
                        error!("Channel Manager: shared state poisoned while waiting for templates");
                        cancellation_token.cancel();
                        break false;
                    }
                };

                if has_required_data {
                    info!("Required template data received, ready to accept connections");
                    break true;
                }

                warn!("Waiting for initial template and prevhash from Template Provider...");
                warn!("Is the Bitcoin node undergoing IBD?");
                select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Channel Manager: received shutdown while waiting for templates");
                        break false;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Channel Manager: received fallback while waiting for templates");
                        break false;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            };

            if !ready {
                fallback_handler.done();
                return;
            }

            loop {
                select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Downstream Server: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Downstream Server: received fallback signal");
                        break;
                    }
                    res = server.accept() => {
                        match res {
                            Ok((stream, socket_address)) => {
                                info!(%socket_address, "New downstream connection");

                                let this = Arc::clone(&this);
                                let cancellation_token_inner = cancellation_token.clone();
                                let fallback_coordinator_inner = fallback_coordinator.clone();
                                let channel_manager_sender_inner = channel_manager_sender.clone();
                                let task_manager_inner = task_manager_clone.clone();
                                let supported_extensions_inner = supported_extensions.clone();
                                let required_extensions_inner = required_extensions.clone();

                                task_manager_clone.spawn(async move {
                                    let noise_stream = tokio::select! {
                                        biased;
                                        _ = cancellation_token_inner.cancelled() => {
                                            info!("Shutdown received during handshake, dropping connection");
                                            return;
                                        }
                                        result = accept_noise_connection(stream, authority_public_key, authority_secret_key, cert_validity_sec) => {
                                            match result {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    error!(error = ?e, "Noise handshake failed");
                                                    return;
                                                }
                                            }
                                        }
                                    };

                                    let downstream_id = this.downstream_id_factory.fetch_add(1, Ordering::Relaxed);

                                    let channel_id_factory = AtomicU32::new(1);
                                    let group_channel_id = channel_id_factory.fetch_add(1, Ordering::SeqCst);

                                    let group_channel = match this.bootstrap_group_channel(group_channel_id) {
                                        Some(group_channel) => group_channel,
                                        None => {
                                            error!("Failed to bootstrap group channel - disconnecting downstream {downstream_id}");
                                            cancellation_token_inner.cancel();
                                            return;
                                        }
                                    };

                                    let (channel_manager_sender_downstream, channel_manager_receiver_downstream) = unbounded();

                                    let downstream = Downstream::new(
                                        downstream_id,
                                        channel_id_factory,
                                        group_channel,
                                        channel_manager_sender_inner,
                                        channel_manager_receiver_downstream,
                                        noise_stream,
                                        cancellation_token_inner.clone(),
                                        fallback_coordinator_inner.clone(),
                                        task_manager_inner.clone(),
                                        supported_extensions_inner,
                                        required_extensions_inner,
                                    );

                                    this.channel_manager_io
                                        .downstream_sender
                                        .insert(downstream_id, channel_manager_sender_downstream);

                                    this.downstream.insert(downstream_id, downstream.clone());

                                    downstream
                                        .start(
                                            cancellation_token_inner,
                                            fallback_coordinator_inner,
                                            task_manager_inner,
                                            move |downstream_id| this.remove_downstream(downstream_id)
                                        )
                                        .await;
                                });
                                }

                                Err(e) => {
                                    error!(error = ?e, "Failed to accept new downstream connection");
                                }
                            }
                    }
                }
            }
            info!("Downstream server: Unified loop break");
            // Release the port before signalling the fallback coordinator, so a subsequent
            // fallback can re-bind the same listening address.
            drop(server);
            fallback_handler.done();
        });
        Ok(())
    }

    /// The central orchestrator of the Channel Manager.  
    ///  
    /// Responsible for receiving messages from all subsystems, processing them,  
    /// and either forwarding them to the appropriate subsystem or updating  
    /// the internal state of the Channel Manager as needed.
    pub async fn start(
        self,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
        coinbase_outputs: Vec<TxOut>,
    ) -> JDCResult<(), error::ChannelManager> {
        self.coinbase_output_constraints(coinbase_outputs).await?;

        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();
            let cm = self.clone();
            let vd = self.clone();
            let vardiff_future = vd.run_vardiff_loop();
            tokio::pin!(vardiff_future);
            loop {
                let mut cm_jds = cm.clone();
                let mut cm_pool = cm.clone();
                let mut cm_template = cm.clone();
                let mut cm_downstreams = cm.clone();
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        info!("Channel Manager: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("Channel Manager: fallback triggered");
                        break;
                    }
                    res = &mut vardiff_future => {
                        info!("Vardiff loop completed with: {res:?}");
                    }
                    res = cm_jds.handle_jds_message(),
                        if !cm.mode.is_solo_mining()
                            && !cm.channel_manager_io.jd_receiver.is_closed() =>
                    {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling JDS message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_jds_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = cm_pool.handle_pool_message_frame(),
                        if !cm.mode.is_solo_mining()
                            && !cm.channel_manager_io.upstream_receiver.is_closed() =>
                    {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Pool message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_pool_message_frame",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = cm_template.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Template Receiver message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_template_provider_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = cm_downstreams.handle_downstream_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Downstreams message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_downstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    }
                }
            }

            let close_template_provider =
                cancellation_token.is_cancelled() || !fallback_token.is_cancelled();
            self.channel_manager_io.close(close_template_provider);
            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });

        Ok(())
    }

    // Removes a downstream entry from the Channel Manager’s state.
    //
    // Given a `downstream_id`, this method:
    // 1. Removes the corresponding downstream from the `downstream` map.
    #[allow(clippy::result_large_err)]
    pub fn remove_downstream(&self, downstream_id: DownstreamId) {
        if let Some((_, downstream)) = self.downstream.remove(&downstream_id) {
            downstream.downstream_cancellation_token.cancel();
        }
        self.downstream_channel_id_and_job_id_to_template_id
            .retain(|key, _| key.downstream_id != downstream_id);
        self.vardiff
            .retain(|key, _| key.downstream_id != downstream_id);
        #[cfg(feature = "monitoring")]
        self.miner_telemetry.remove_downstream(downstream_id);
        self.channel_manager_io
            .downstream_sender
            .remove(&downstream_id);
    }

    /// Handles messages received from the JDS subsystem.  
    ///  
    /// This method listens for incoming frames on the `jd_receiver` channel.  
    /// - If the frame contains a JobDeclaration message, it forwards it to the   job declaration
    ///   message handler.
    /// - If the frame contains any unsupported message type, an error is returned.
    async fn handle_jds_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        let message = self
            .channel_manager_io
            .jd_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;

        self.handle_job_declaration_message_from_server(None, message, None)
            .await?;
        Ok(())
    }

    /// Handles messages received from the Upstream subsystem.  
    ///  
    /// This method listens for incoming frames on the `upstream_receiver` channel.  
    /// - If the frame contains a **Mining** message, it forwards it to the   mining message
    ///   handler.
    /// - If the frame contains any unsupported message type, an error is returned.
    async fn handle_pool_message_frame(&mut self) -> JDCResult<(), error::ChannelManager> {
        let mut sv2_frame = self
            .channel_manager_io
            .upstream_receiver
            .recv()
            .await
            .map_err(JDCError::fallback)?;
        let header = sv2_frame.header();
        let message_type = header.msg_type();
        let extension_type = header.ext_type();
        let payload = sv2_frame.payload();
        match protocol_message_type(extension_type, message_type) {
            MessageType::Mining => {
                self.handle_mining_message_frame_from_server(None, header, payload)
                    .await?;
            }
            MessageType::Extensions => {
                self.handle_extensions_message_frame_from_server(None, header, payload)
                    .await?;
            }
            _ => {
                warn!("Received unsupported message type from upstream: {message_type}");
                return Err(JDCError::log(JDCErrorKind::UnexpectedMessage(
                    extension_type,
                    message_type,
                )));
            }
        }
        Ok(())
    }

    // Handles messages received from the TP subsystem.
    //
    // This method listens for incoming frames on the `tp_receiver` channel.
    // - If the frame contains a TemplateDistribution message, it forwards it to the   template
    //   distribution message handler.
    // - If the frame contains any unsupported message type, an error is returned.
    async fn handle_template_provider_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        let message = self
            .channel_manager_io
            .tp_receiver
            .recv()
            .await
            .map_err(JDCError::shutdown)?;

        self.handle_template_distribution_message_from_server(None, message, None)
            .await?;
        Ok(())
    }

    // Handles messages received from downstream clients and routes them appropriately.
    //
    // # Overview
    // This method is similar to the upstream JDS message handler, but introduces additional
    // logic for handling OpenChannel requests (both standard and extended).
    //
    // # Message Flow
    // - For most mining messages: The message is forwarded directly to
    //   `handle_mining_message_from_client`.
    // - For OpenChannel messages: At the time of request, the `channel_id` is not yet assigned, so
    //   we cannot map the message back to the downstream. To solve this:
    //   1. The `downstream_id` is appended to the `user_identity` (e.g.,
    //      `"identity#downstream_id"`).
    //   2. Later, the appended downstream ID is stripped and used by the message handler to
    //      correctly attribute the request.
    //
    // # Channel Establishment Logic
    // - NoChannel → Pending:
    //   - The first downstream OpenChannel request is stored in `pending_downstream_requests`.
    //   - The upstream state transitions from `NoChannel` to `Pending`.
    //   - A single channel request is then sent to the upstream (JDC → upstream).
    //
    // - Pending:
    //   - Additional downstream OpenChannel requests are stored in `pending_downstream_requests`
    //     until the upstream connection is established.
    //
    // - Connected / SoloMining:
    //   - Downstream OpenChannel requests are immediately forwarded to the mining handler.
    //
    // # Notes
    // - Only one upstream channel is created per JDC instance.
    // - After the upstream channel is established, all new downstream requests bypass the pending
    //   mechanism and are sent directly to the mining handler.
    async fn handle_downstream_message(&mut self) -> JDCResult<(), error::ChannelManager> {
        let (downstream_id, message, tlvs) = self
            .channel_manager_io
            .downstream_receiver
            .recv()
            .await
            .map_err(JDCError::shutdown)?;

        match message {
            MiningOwned::OpenExtendedMiningChannel(downstream_channel_request) => {
                let downstream_msg = downstream_channel_request.clone();

                match self.upstream_state.get() {
                    UpstreamState::NoChannel => {
                        self.pending_downstream_requests
                            .with(|data| {
                                data.push_front((downstream_id, downstream_msg).into());
                            })
                            .map_err(JDCError::shutdown)?;

                        if self
                            .upstream_state
                            .compare_and_set(UpstreamState::NoChannel, UpstreamState::Pending)
                            .is_ok()
                        {
                            let mut upstream_message = downstream_channel_request;
                            let identity = self.user_identity().to_string();
                            upstream_message.user_identity =
                                identity.as_str().try_into().map_err(JDCError::shutdown)?;
                            upstream_message.request_id = 1;
                            // The upstream extended channel is opened once and its
                            // `extranonce_size` is fixed. Size its rollable region to fit:
                            //   - JDC's own `local_index` (JDC_LOCAL_PREFIX_BYTES), plus
                            //   - the larger of the downstream's request `M` and JDC's retroactive
                            //     commitment to future downstreams
                            //     (`reserved_downstream_rollable_extranonce_size`).
                            // Equivalently:
                            //   JDC_LOCAL_PREFIX_BYTES +
                            //     max(reserved_downstream_rollable, M).
                            let reserved_downstream_rollable =
                                self.reserved_downstream_rollable_extranonce_size as usize;
                            let downstream_min = upstream_message.min_extranonce_size as usize;
                            let upstream_min = (JDC_LOCAL_PREFIX_BYTES as usize).saturating_add(
                                std::cmp::max(reserved_downstream_rollable, downstream_min),
                            );
                            upstream_message.min_extranonce_size = upstream_min as u16;
                            let upstream_message =
                                MiningOwned::OpenExtendedMiningChannel(upstream_message);
                            let sv2_frame: Sv2Frame = AnyMessageOwned::Mining(upstream_message)
                                .try_into()
                                .map_err(JDCError::shutdown)?;
                            self.channel_manager_io
                                .upstream_sender
                                .send(sv2_frame.into())
                                .await
                                .map_err(|_| {
                                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                })?;
                        }
                    }
                    UpstreamState::Pending => {
                        self.pending_downstream_requests
                            .with(|data| {
                                data.push_back((downstream_id, downstream_msg).into());
                            })
                            .map_err(JDCError::shutdown)?;
                    }
                    UpstreamState::Connected => {
                        self.handle_mining_message_from_client(
                            Some(downstream_id),
                            MiningOwned::OpenExtendedMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                    UpstreamState::SoloMining => {
                        self.handle_mining_message_from_client(
                            Some(downstream_id),
                            MiningOwned::OpenExtendedMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                }
            }
            MiningOwned::OpenStandardMiningChannel(downstream_channel_request) => {
                let downstream_msg = downstream_channel_request.clone();

                match self.upstream_state.get() {
                    UpstreamState::NoChannel => {
                        self.pending_downstream_requests
                            .with(|data| data.push_front((downstream_id, downstream_msg).into()))
                            .map_err(JDCError::shutdown)?;

                        if self
                            .upstream_state
                            .compare_and_set(UpstreamState::NoChannel, UpstreamState::Pending)
                            .is_ok()
                        {
                            // The first downstream is a standard channel, which doesn't
                            // roll the extranonce itself. Ask the pool for
                            // JDC_LOCAL_PREFIX_BYTES +
                            // `reserved_downstream_rollable_extranonce_size` so we
                            // still honor our retroactive commitment to any later
                            // extended downstream that attaches to this upstream.
                            let upstream_min_extranonce_size = (JDC_LOCAL_PREFIX_BYTES as u16)
                                + self.reserved_downstream_rollable_extranonce_size as u16;
                            let identity = self.user_identity().to_string();
                            let upstream_open = OpenExtendedMiningChannelOwned {
                                user_identity: identity.as_str().try_into().unwrap(),
                                request_id: 1,
                                nominal_hash_rate: downstream_channel_request.nominal_hash_rate,
                                max_target: downstream_channel_request.max_target,
                                min_extranonce_size: upstream_min_extranonce_size,
                            };

                            let message = MiningOwned::OpenExtendedMiningChannel(upstream_open);
                            let sv2_frame: Sv2Frame = AnyMessageOwned::Mining(message)
                                .try_into()
                                .map_err(JDCError::shutdown)?;
                            self.channel_manager_io
                                .upstream_sender
                                .send(sv2_frame.into())
                                .await
                                .map_err(|_| {
                                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                })?;
                        }
                    }
                    UpstreamState::Pending => {
                        self.pending_downstream_requests
                            .with(|data| data.push_back((downstream_id, downstream_msg).into()))
                            .map_err(JDCError::shutdown)?;
                    }
                    UpstreamState::Connected => {
                        self.handle_mining_message_from_client(
                            Some(downstream_id),
                            MiningOwned::OpenStandardMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                    UpstreamState::SoloMining => {
                        self.handle_mining_message_from_client(
                            Some(downstream_id),
                            MiningOwned::OpenStandardMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                }
            }
            _ => {
                self.handle_mining_message_from_client(
                    Some(downstream_id),
                    message,
                    tlvs.as_deref(),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Sends a transaction-data request for the template.
    ///
    /// Only call this when the response can be consumed (i.e., `UpstreamState::Connected`
    /// in full-template mode). Templates received before the upstream channel opens are
    /// covered by the re-request in `handle_open_extended_mining_channel_success`.
    async fn request_transaction_data(
        &self,
        template_id: TemplateId,
    ) -> JDCResult<(), error::ChannelManager> {
        let message =
            TemplateDistributionOwned::RequestTransactionData(RequestTransactionDataOwned {
                template_id,
            });
        self.channel_manager_io
            .tp_sender
            .send(message)
            .await
            .map_err(|_e| JDCError::shutdown(JDCErrorKind::ChannelErrorSender))?;
        Ok(())
    }

    /// Utility method to request for more token to JDS.
    pub async fn allocate_tokens(
        &self,
        token_to_allocate: u32,
    ) -> JDCResult<(), error::ChannelManager> {
        debug!("Allocating {} job tokens", token_to_allocate);

        for i in 0..token_to_allocate {
            let request_id = self.request_id_factory.fetch_add(1, Ordering::Relaxed);

            debug!(
                request_id,
                "Allocating token {}/{}",
                i + 1,
                token_to_allocate
            );

            let identifier = self.user_identity().to_string();
            let message =
                JobDeclarationOwned::AllocateMiningJobToken(AllocateMiningJobTokenOwned {
                    user_identifier: identifier
                        .as_str()
                        .try_into()
                        .expect("Static string should always convert"),
                    request_id,
                });

            self.channel_manager_io
                .jd_sender
                .send(message)
                .await
                .map_err(|e| {
                    info!(error = ?e, "Failed to send AllocateMiningJobToken frame");
                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                })?;
        }

        info!("Requested allocation of {token_to_allocate} mining job tokens to JDS");
        Ok(())
    }

    // Runs the vardiff on extended channel.
    fn run_vardiff_on_extended_channel(
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        channel_state: &mut stratum_apps::stratum_core::channels_sv2::server::extended::ExtendedChannel,
        vardiff_state: &mut VardiffState,
        updates: &mut Vec<RouteMessageTo>,
    ) {
        let (hashrate, target, shares_per_minute) = (
            channel_state.get_nominal_hashrate(),
            channel_state.get_target(),
            channel_state.get_shares_per_minute(),
        );

        let Ok(new_hashrate_opt) = vardiff_state.try_vardiff(hashrate, target, shares_per_minute)
        else {
            debug!("Vardiff computation failed for extended channel {channel_id}");
            return;
        };

        let Some(new_hashrate) = new_hashrate_opt else {
            channel_state.set_stable_hashrate(true);
            return;
        };

        channel_state.set_stable_hashrate(false);

        match channel_state.update_channel(new_hashrate, None) {
            Ok(()) => {
                let updated_target = channel_state.get_target();
                updates.push(
                    (
                        downstream_id,
                        MiningOwned::SetTarget(SetTargetOwned {
                            channel_id,
                            maximum_target: updated_target.to_le_bytes().into(),
                        }),
                    )
                        .into(),
                );
                debug!("Updated target for extended channel_id={channel_id} to {updated_target:?}",);
            }
            Err(e) => warn!(
                "Failed to update extended channel channel_id={channel_id} during vardiff {e:?}"
            ),
        }
    }

    // Runs the vardiff on the standard channel.
    fn run_vardiff_on_standard_channel(
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        channel: &mut StandardChannel,
        vardiff_state: &mut VardiffState,
        updates: &mut Vec<RouteMessageTo>,
    ) {
        let hashrate = channel.get_nominal_hashrate();
        let target = channel.get_target();
        let shares_per_minute = channel.get_shares_per_minute();

        let Ok(new_hashrate_opt) = vardiff_state.try_vardiff(hashrate, target, shares_per_minute)
        else {
            debug!("Vardiff computation failed for standard channel {channel_id}");
            return;
        };

        let Some(new_hashrate) = new_hashrate_opt else {
            channel.set_stable_hashrate(true);
            return;
        };

        channel.set_stable_hashrate(false);
        match channel.update_channel(new_hashrate, None) {
            Ok(()) => {
                let updated_target = channel.get_target();
                updates.push(
                    (
                        downstream_id,
                        MiningOwned::SetTarget(SetTargetOwned {
                            channel_id,
                            maximum_target: updated_target.to_le_bytes().into(),
                        }),
                    )
                        .into(),
                );
                debug!(
                    "Updated target for standard channel channel_id={channel_id} to {updated_target:?}"
                );
            }
            Err(e) => warn!(
                "Failed to update standard channel channel_id={channel_id} during vardiff {e:?}"
            ),
        }
    }

    // Periodic vardiff task loop.
    //
    // # Purpose
    // - Executes the vardiff cycle every 60 seconds for all downstreams.
    // - Delegates to [`Self::run_vardiff`] on each tick.
    async fn run_vardiff_loop(&self) -> JDCResult<(), error::ChannelManager> {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            info!("Starting vardiff loop for downstreams");

            if let Err(e) = self.run_vardiff().await {
                error!(error = ?e, "Vardiff iteration failed");
            }
        }
    }

    // Runs vardiff across **all channels** and generates updates.
    //
    // # Purpose
    // - Iterates through all downstream channels (both standard and extended).
    // - Runs vardiff for each channel and collects the resulting updates.
    // - Propagates difficulty changes to downstreams and also sends an `UpdateChannel` message
    //   upstream if applicable.
    async fn run_vardiff(&self) -> JDCResult<(), error::ChannelManager> {
        let mut messages: Vec<RouteMessageTo> = vec![];
        for vardiff_key in self.vardiff.keys() {
            let channel_id = vardiff_key.channel_id;
            let downstream_id = vardiff_key.downstream_id;

            if self
                .downstream
                .with(&downstream_id, |downstream| {
                    self.vardiff.with_mut(&vardiff_key, |vardiff_state| {
                        downstream
                            .standard_channels
                            .with_mut(&channel_id, |standard_channel| {
                                Self::run_vardiff_on_standard_channel(
                                    downstream_id,
                                    channel_id,
                                    standard_channel,
                                    vardiff_state,
                                    &mut messages,
                                );
                            });
                        downstream
                            .extended_channels
                            .with_mut(&channel_id, |extended_channel| {
                                Self::run_vardiff_on_extended_channel(
                                    downstream_id,
                                    channel_id,
                                    extended_channel,
                                    vardiff_state,
                                    &mut messages,
                                );
                            });
                    });
                })
                .is_none()
            {
                self.vardiff.remove(&vardiff_key);
                continue;
            }
        }

        if !messages.is_empty() {
            let mut downstream_hashrate = 0.0;
            let mut min_target = [0xff; 32];

            self.downstream.for_each(|_, downstream| {
                let mut update_from_channel = |hashrate: f32, target: &Target| {
                    downstream_hashrate += hashrate;
                    min_target = std::cmp::min(target.to_le_bytes(), min_target);
                };

                downstream.standard_channels.for_each(|_, channel| {
                    update_from_channel(channel.get_nominal_hashrate(), channel.get_target());
                });

                downstream.extended_channels.for_each(|_, channel| {
                    update_from_channel(channel.get_nominal_hashrate(), channel.get_target());
                });
            });

            self.upstream_channel
                .with(|maybe_upstream_channel| {
                    if let Some(upstream_channel) = maybe_upstream_channel.as_mut() {
                        debug!(
                            "Checking upstream channel {} with hashrate {} and target {:?}",
                            upstream_channel.get_channel_id(),
                            upstream_channel.get_nominal_hashrate(),
                            upstream_channel.get_target()
                        );

                        upstream_channel.set_nominal_hashrate(downstream_hashrate);

                        info!("Sending update channel message upstream");
                        messages.push(
                            MiningOwned::UpdateChannel(UpdateChannelOwned {
                                channel_id: upstream_channel.get_channel_id(),
                                nominal_hash_rate: downstream_hashrate,
                                maximum_target: min_target.into(),
                            })
                            .into(),
                        );
                    }
                })
                .map_err(JDCError::shutdown)?;
        }

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        info!("Vardiff update cycle complete");
        Ok(())
    }

    /// Runs `f` while holding the downstream map entry guard.
    ///
    /// Use this when mutations must only happen if the downstream is still
    /// registered in the ChannelManager. Keep `f` short: do not perform blocking
    /// work, send/forward messages, or re-enter `self.downstream` inside it.
    ///
    /// Returns the closure result if the downstream is registered. Returns
    /// `DownstreamNotFound` with a disconnect action if the downstream is no
    /// longer registered.
    #[allow(clippy::result_large_err)]
    pub(crate) fn with_registered_downstream<R, F>(
        &self,
        downstream_id: DownstreamId,
        f: F,
    ) -> JDCResult<R, error::ChannelManager>
    where
        F: FnOnce(&Downstream) -> JDCResult<R, error::ChannelManager>,
    {
        match self
            .downstream
            .with(&downstream_id, |downstream| f(downstream))
        {
            Some(result) => result,
            None => Err(JDCError::disconnect(
                JDCErrorKind::DownstreamNotFound(downstream_id),
                downstream_id,
            )),
        }
    }

    /// Sends a CoinbaseOutputConstraints message to the template provider.
    ///
    /// # Purpose
    /// - Calculates the max coinbase output size and sigops for the coinbase outputs.
    /// - Sends the CoinbaseOutputConstraints message to the template provider.
    ///
    /// # Parameters
    /// - `coinbase_outputs`: The coinbase outputs to calculate the max coinbase output size and
    ///   sigops for.
    pub async fn coinbase_output_constraints(
        &self,
        coinbase_outputs: Vec<TxOut>,
    ) -> JDCResult<(), error::ChannelManager> {
        let msg = coinbase_output_constraints_message(coinbase_outputs);

        self.channel_manager_io
            .tp_sender
            .send(TemplateDistributionOwned::CoinbaseOutputConstraints(msg))
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
                JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
            })?;

        Ok(())
    }
}
