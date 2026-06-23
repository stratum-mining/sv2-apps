use std::{
    collections::{BinaryHeap, HashMap, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
};

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::CancellationToken;
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    coinbase_output_constraints::coinbase_output_constraints_message,
    custom_mutex::Mutex,
    fallback_coordinator::FallbackCoordinator,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::accept_noise_connection,
    stratum_core::{
        bitcoin::{consensus, Amount, Target, TxOut},
        channels_sv2::{
            client::extended::ExtendedChannel,
            extranonce_manager::{bytes_needed, ExtranonceAllocator},
            outputs::deserialize_outputs,
            server::{group::GroupChannel, jobs::factory::JobFactory, standard::StandardChannel},
            Vardiff, VardiffState,
        },
        handlers_sv2::{
            HandleExtensionsFromServerAsync, HandleJobDeclarationMessagesFromServerAsync,
            HandleMiningMessagesFromClientAsync, HandleMiningMessagesFromServerAsync,
            HandleTemplateDistributionMessagesFromServerAsync,
        },
        job_declaration_sv2::{
            AllocateMiningJobToken, AllocateMiningJobTokenSuccess, DeclareMiningJob,
        },
        mining_sv2::{OpenExtendedMiningChannel, SetCustomMiningJob, SetTarget, UpdateChannel},
        parsers_sv2::{AnyMessage, JobDeclaration, Mining, TemplateDistribution, Tlv},
        template_distribution_sv2::{NewTemplate, SetNewPrevHash as SetNewPrevHashTdp},
    },
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{protocol_message_type, MessageType},
        types::{
            ChannelId, DownstreamId, RequestId, SharesBatchSize, SharesPerMinute, Sv2Frame,
            TemplateId, UpstreamJobId, VardiffKey,
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
use stratum_apps::monitoring::MinerTelemetry;
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
    declare_mining_job: Option<DeclareMiningJob<'static>>,
    // The template associated with the declared job.
    template: NewTemplate<'static>,
    // The `SetNewPrevHashTdp` message associated with this job, if available.
    prev_hash: Option<SetNewPrevHashTdp<'static>>,
    // The `SetCustomMiningJob` message associated with this job,
    // if a custom job was created.
    set_custom_mining_job: Option<SetCustomMiningJob<'static>>,
    // The coinbase output for this job.
    coinbase_output: Vec<u8>,
    // The list of transactions included in the job’s template.
    tx_list: Vec<Vec<u8>>,
}

/// Central state container for the **Channel Manager**.
///
/// `ChannelManagerData` holds all runtime state that the JDC
/// needs to manage downstream clients, upstream connections, extranonce allocation,
/// job tracking, and various ID factories.  
pub struct ChannelManagerData {
    // Mapping of `downstream_id` → `Downstream` object,
    // used by the channel manager to locate and interact with downstream clients.
    pub downstream: HashMap<DownstreamId, Downstream>,
    // Unified extranonce prefix allocator shared by standard and extended
    // downstream channels. Rebuilt with the upstream-assigned prefix whenever
    // the upstream connection is (re)negotiated or `SetExtranoncePrefix` is
    // received. The allocated [`ExtranoncePrefix`] is stored on the channel
    // itself, so dropping the channel automatically releases the slot.
    extranonce_allocator: ExtranonceAllocator,
    // Factory that generates **monotonically increasing request IDs**
    // for messages sent from the JDC.
    request_id_factory: AtomicU32,
    // Factory that assigns a unique ID to each new **downstream connection**.
    downstream_id_factory: AtomicUsize,
    // Factory that assigns a unique **sequence number** to each share
    // submitted from the JDC to the upstream.
    pub sequence_number_factory: AtomicU32,
    // The last **future template** received from the upstream.
    last_future_template: Option<NewTemplate<'static>>,
    // The last **new prevhash** received from the upstream.
    pub last_new_prev_hash: Option<SetNewPrevHashTdp<'static>>,
    // FIFO buffer of allocation tokens received from the JDS.
    // Oldest token is consumed first to minimize risk of JDS-side expiration.
    allocate_tokens: VecDeque<AllocateMiningJobTokenSuccess<'static>>,
    // Stores new templates as they arrive, mapped by their **template ID**.
    template_store: HashMap<TemplateId, NewTemplate<'static>>,
    // Stores the last declared job, keyed by the `request_id` used when
    // declaring the job to the JDS.
    // This is later used to send a `SetCustomMiningJob`.
    last_declare_job_store: HashMap<RequestId, DeclaredJob>,
    // Maps a template ID → corresponding upstream job ID.
    template_id_to_upstream_job_id: HashMap<TemplateId, UpstreamJobId>,
    // Maps a downstream ID + channel_id + job ID → corresponding template ID.
    downstream_channel_id_and_job_id_to_template_id: HashMap<DownstreamChannelJobId, TemplateId>,
    // The coinbase outputs currently in use.
    coinbase_outputs: Vec<u8>,
    // The active upstream extended channel (client-side instance), if any.
    pub upstream_channel: Option<ExtendedChannel<'static>>,
    // Optional "pool tag" string, identifying the pool.
    pool_tag_string: Option<String>,
    // List of pending downstream connection requests,
    // persisted while the JDC is opening a channel with the upstream.
    pending_downstream_requests: VecDeque<PendingChannelRequest>,
    // Factory for creating **custom mining jobs**, if available.
    job_factory: Option<JobFactory>,
    // Mapping of `(downstream_id, channel_id)` → vardiff controller.
    // Each entry manages variable difficulty for a specific downstream channel.
    vardiff: HashMap<VardiffKey, VardiffState>,
    /// Extensions that have been successfully negotiated with the upstream server
    negotiated_extensions: Vec<u16>,
    /// Extensions that the JDC supports
    supported_extensions: Vec<u16>,
    /// Extensions that the JDC requires
    required_extensions: Vec<u16>,
    /// Cached shares waiting for `SetCustomMiningJob.Success` to be propagated upstream
    cached_shares: HashMap<TemplateId, BinaryHeap<SharesOrderedByDiff>>,
}

impl ChannelManagerData {
    /// Resets the internal state of the Channel Manager.
    ///
    /// This method is primarily used during **fallback scenarios** to clear and
    /// reinitialize all internal data structures. It ensures that the Channel Manager
    /// returns to a clean state, ready to handle fresh upstream or downstream connections.
    pub fn reset(&mut self, coinbase_outputs: Vec<u8>) {
        self.downstream.clear();
        self.template_store.clear();
        self.last_declare_job_store.clear();
        self.template_id_to_upstream_job_id.clear();
        self.downstream_channel_id_and_job_id_to_template_id.clear();
        self.pending_downstream_requests.clear();
        self.cached_shares.clear();

        self.downstream_id_factory = AtomicUsize::new(0);
        self.request_id_factory = AtomicU32::new(0);

        // Reset the allocator to its solo-mining default. When upstream
        // reconnects with a new extranonce prefix it will be rebuilt via
        // [`ExtranonceAllocator::from_upstream_prefix`] in the upstream handler.
        self.extranonce_allocator =
            ExtranonceAllocator::new(Vec::new(), SOLO_FULL_EXTRANONCE_SIZE, JDC_MAX_CHANNELS)
                .expect("Failed to create ExtranonceAllocator with valid parameters");

        self.allocate_tokens.clear();
        self.upstream_channel = None;
        self.pool_tag_string = None;

        self.coinbase_outputs = coinbase_outputs;
    }
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
    upstream_sender: Sender<Sv2Frame>,
    upstream_receiver: Receiver<Sv2Frame>,
    jd_sender: Sender<JobDeclaration<'static>>,
    jd_receiver: Receiver<JobDeclaration<'static>>,
    tp_sender: Sender<TemplateDistribution<'static>>,
    tp_receiver: Receiver<TemplateDistribution<'static>>,
    downstream_sender: Arc<Mutex<HashMap<DownstreamId, Sender<DownstreamMessage>>>>,
    downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
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
        self.downstream_sender.super_safe_lock(|downstreams| {
            for sender in downstreams.values() {
                sender.close();
            }
            downstreams.clear();
        });
    }
}

/// Contains all the state of mutable and immutable data required
/// by channel manager to process its task along with channels
/// to perform message traversal.
#[derive(Clone)]
pub struct ChannelManager {
    pub channel_manager_data: Arc<Mutex<ChannelManagerData>>,
    channel_manager_io: ChannelManagerIo,
    miner_tag_string: String,
    share_batch_size: SharesBatchSize,
    shares_per_minute: SharesPerMinute,
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
    pub(crate) miner_telemetry: Arc<Mutex<HashMap<DownstreamId, MinerTelemetry>>>,
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
        upstream_sender: Sender<Sv2Frame>,
        upstream_receiver: Receiver<Sv2Frame>,
        jd_sender: Sender<JobDeclaration<'static>>,
        jd_receiver: Receiver<JobDeclaration<'static>>,
        tp_sender: Sender<TemplateDistribution<'static>>,
        tp_receiver: Receiver<TemplateDistribution<'static>>,
        downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
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

        let channel_manager_data = Arc::new(Mutex::new(ChannelManagerData {
            downstream: HashMap::new(),
            extranonce_allocator,
            downstream_id_factory: AtomicUsize::new(0),
            request_id_factory: AtomicU32::new(0),
            sequence_number_factory: AtomicU32::new(1),
            last_future_template: None,
            last_new_prev_hash: None,
            allocate_tokens: VecDeque::new(),
            template_store: HashMap::new(),
            last_declare_job_store: HashMap::new(),
            template_id_to_upstream_job_id: HashMap::new(),
            downstream_channel_id_and_job_id_to_template_id: HashMap::new(),
            coinbase_outputs,
            upstream_channel: None,
            pool_tag_string: None,
            pending_downstream_requests: VecDeque::new(),
            job_factory: None,
            vardiff: HashMap::new(),
            negotiated_extensions: vec![],
            supported_extensions,
            required_extensions,
            cached_shares: HashMap::new(),
        }));

        let channel_manager_io = ChannelManagerIo {
            upstream_sender,
            upstream_receiver,
            jd_sender,
            jd_receiver,
            tp_sender,
            tp_receiver,
            downstream_sender: Arc::new(Mutex::new(HashMap::new())),
            downstream_receiver,
        };

        let channel_manager = ChannelManager {
            channel_manager_data,
            channel_manager_io,
            share_batch_size: config.share_batch_size(),
            shares_per_minute: config.shares_per_minute(),
            miner_tag_string: config.jdc_signature().to_string(),
            user_identity: Arc::new(OnceLock::new()),
            reserved_downstream_rollable_extranonce_size: config
                .reserved_downstream_rollable_extranonce_size(),
            upstream_state: AtomicUpstreamState::new(UpstreamState::SoloMining),
            mode,
            #[cfg(feature = "monitoring")]
            miner_telemetry: Arc::new(Mutex::new(HashMap::new())),
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
    fn bootstrap_group_channel(&self, channel_id: ChannelId) -> Option<GroupChannel<'static>> {
        let (full_extranonce_size, pool_tag_string, last_future_template, last_new_prev_hash) =
            self.channel_manager_data.super_safe_lock(|data| {
                (
                    data.upstream_channel
                        .as_ref()
                        .map(|channel| channel.get_full_extranonce_size())
                        .unwrap_or(SOLO_FULL_EXTRANONCE_SIZE as usize), /* Default to
                                                                         * SOLO_FULL_EXTRANONCE_SIZE if
                                                                         * upstream channel is
                                                                         * not
                                                                         * present
                                                                         * (solo mining mode) */
                    data.pool_tag_string.clone(),
                    data.last_future_template
                        .clone()
                        .expect("No future template found after readiness check"),
                    data.last_new_prev_hash
                        .clone()
                        .expect("No new prevhash found after readiness check"),
                )
            });
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

        let coinbase_outputs = self
            .channel_manager_data
            .super_safe_lock(|data| data.coinbase_outputs.clone());
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
        channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
    ) -> JDCResult<(), error::ChannelManager> {
        // todo: let start downstream accept channel manager as `Arc`, instead of clone
        let this = Arc::new(self);

        // Wait for initial template and prevhash before accepting connections
        let fallback_token = fallback_coordinator.token();
        loop {
            let has_required_data = this.channel_manager_data.super_safe_lock(|data| {
                data.last_future_template.is_some() && data.last_new_prev_hash.is_some()
            });

            if has_required_data {
                info!("Required template data received, ready to accept connections");
                break;
            }

            warn!("Waiting for initial template and prevhash from Template Provider...");
            warn!("Is the Bitcoin node undergoing IBD?");
            select! {
                _ = cancellation_token.cancelled() => {
                    info!("Channel Manager: received shutdown while waiting for templates");
                    return Ok(());
                }
                _ = fallback_token.cancelled() => {
                    info!("Channel Manager: received fallback while waiting for templates");
                    return Ok(());
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
        }

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

                                    let downstream_id = this.channel_manager_data
                                        .super_safe_lock(|data| data.downstream_id_factory.fetch_add(1, Ordering::Relaxed));

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
                                        #[cfg(feature = "monitoring")]
                                        socket_address.ip(),
                                    );

                                    this.channel_manager_io.downstream_sender.super_safe_lock(|map| map.insert(downstream_id, channel_manager_sender_downstream));

                                    this.channel_manager_data.super_safe_lock(|data| {
                                        data.downstream.insert(downstream_id, downstream.clone());
                                    });

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
    ) {
        // Serialize coinbase outputs before moving into async block
        // todo: should we really be serializing here?
        let serialized_coinbase_outputs = consensus::serialize(&coinbase_outputs);

        if let Err(e) = self.coinbase_output_constraints(coinbase_outputs).await {
            error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
            if let Action::Shutdown = e.action {
                warn!(
                    error_kind = ?e.kind,
                    "CoinbaseOutputConstraints requested shutdown; cancelling global token"
                );
                cancellation_token.cancel();
            }
            return;
        }

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
                        info!("Channel Manager: fallback triggered, resetting state");
                        self.upstream_state.set(UpstreamState::SoloMining);
                        self.channel_manager_data.super_safe_lock(|data| data.reset(serialized_coinbase_outputs.clone()));

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
    }

    // Removes a downstream entry from the Channel Manager’s state.
    //
    // Given a `downstream_id`, this method:
    // 1. Removes the corresponding downstream from the `downstream` map.
    #[allow(clippy::result_large_err)]
    pub fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.channel_manager_data.super_safe_lock(|cm_data| {
            if let Some(downstream) = cm_data.downstream.remove(&downstream_id) {
                downstream.downstream_cancellation_token.cancel();
            }
            cm_data
                .downstream_channel_id_and_job_id_to_template_id
                .retain(|key, _| key.downstream_id != downstream_id);
            cm_data
                .vardiff
                .retain(|key, _| key.downstream_id != downstream_id);
        });
        self.channel_manager_io
            .downstream_sender
            .super_safe_lock(|map| map.remove(&downstream_id));
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
        let header = sv2_frame.get_header();
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
            Mining::OpenExtendedMiningChannel(downstream_channel_request) => {
                let downstream_msg = downstream_channel_request.clone().into_static();

                match self.upstream_state.get() {
                    UpstreamState::NoChannel => {
                        self.channel_manager_data.super_safe_lock(|data| {
                            data.pending_downstream_requests
                                .push_front((downstream_id, downstream_msg).into());
                        });

                        if self
                            .upstream_state
                            .compare_and_set(UpstreamState::NoChannel, UpstreamState::Pending)
                            .is_ok()
                        {
                            let mut upstream_message = downstream_channel_request;
                            let identity = self.user_identity().to_string();
                            upstream_message.user_identity =
                                identity.try_into().map_err(JDCError::shutdown)?;
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
                                Mining::OpenExtendedMiningChannel(upstream_message).into_static();
                            let sv2_frame: Sv2Frame = AnyMessage::Mining(upstream_message)
                                .try_into()
                                .map_err(JDCError::shutdown)?;
                            self.channel_manager_io
                                .upstream_sender
                                .send(sv2_frame)
                                .await
                                .map_err(|_| {
                                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                })?;
                        }
                    }
                    UpstreamState::Pending => {
                        self.channel_manager_data.super_safe_lock(|data| {
                            data.pending_downstream_requests
                                .push_back((downstream_id, downstream_msg).into());
                        });
                    }
                    UpstreamState::Connected => {
                        self.send_open_channel_request_to_mining_handler(
                            downstream_id,
                            Mining::OpenExtendedMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                    UpstreamState::SoloMining => {
                        self.send_open_channel_request_to_mining_handler(
                            downstream_id,
                            Mining::OpenExtendedMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                }
            }
            Mining::OpenStandardMiningChannel(downstream_channel_request) => {
                let downstream_msg = downstream_channel_request.clone().into_static();

                match self.upstream_state.get() {
                    UpstreamState::NoChannel => {
                        self.channel_manager_data.super_safe_lock(|data| {
                            data.pending_downstream_requests
                                .push_front((downstream_id, downstream_msg).into())
                        });

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
                            let upstream_open = OpenExtendedMiningChannel {
                                user_identity: identity.try_into().unwrap(),
                                request_id: 1,
                                nominal_hash_rate: downstream_channel_request.nominal_hash_rate,
                                max_target: downstream_channel_request.max_target,
                                min_extranonce_size: upstream_min_extranonce_size,
                            };

                            let message =
                                Mining::OpenExtendedMiningChannel(upstream_open).into_static();
                            let sv2_frame: Sv2Frame = AnyMessage::Mining(message)
                                .try_into()
                                .map_err(JDCError::shutdown)?;
                            self.channel_manager_io
                                .upstream_sender
                                .send(sv2_frame)
                                .await
                                .map_err(|_| {
                                    JDCError::fallback(JDCErrorKind::ChannelErrorSender)
                                })?;
                        }
                    }
                    UpstreamState::Pending => {
                        self.channel_manager_data.super_safe_lock(|data| {
                            data.pending_downstream_requests
                                .push_back((downstream_id, downstream_msg).into())
                        });
                    }
                    UpstreamState::Connected => {
                        self.send_open_channel_request_to_mining_handler(
                            downstream_id,
                            Mining::OpenStandardMiningChannel(downstream_msg),
                            tlvs.as_deref(),
                        )
                        .await?;
                    }
                    UpstreamState::SoloMining => {
                        self.send_open_channel_request_to_mining_handler(
                            downstream_id,
                            Mining::OpenStandardMiningChannel(downstream_msg),
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

    // Utility method to send open channel request from downstream to message handler.
    #[inline]
    async fn send_open_channel_request_to_mining_handler(
        &mut self,
        downstream_id: DownstreamId,
        message: Mining<'_>,
        tlvs: Option<&[Tlv]>,
    ) -> JDCResult<(), error::ChannelManager> {
        self.handle_mining_message_from_client(Some(downstream_id), message, tlvs)
            .await?;
        Ok(())
    }

    /// Utility method to request for more token to JDS.
    pub async fn allocate_tokens(
        &self,
        token_to_allocate: u32,
    ) -> JDCResult<(), error::ChannelManager> {
        debug!("Allocating {} job tokens", token_to_allocate);

        for i in 0..token_to_allocate {
            let request_id = self
                .channel_manager_data
                .super_safe_lock(|data| data.request_id_factory.fetch_add(1, Ordering::Relaxed));

            debug!(
                request_id,
                "Allocating token {}/{}",
                i + 1,
                token_to_allocate
            );

            let identifier = self.user_identity().to_string();
            let message = JobDeclaration::AllocateMiningJobToken(AllocateMiningJobToken {
                user_identifier: identifier
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
        channel_state: &mut stratum_apps::stratum_core::channels_sv2::server::extended::ExtendedChannel<'static>,
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
                        Mining::SetTarget(SetTarget {
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
        channel: &mut StandardChannel<'static>,
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
                        Mining::SetTarget(SetTarget {
                            channel_id,
                            maximum_target: updated_target.to_le_bytes().into(),
                        }),
                    )
                        .into(),
                );
                debug!("Updated target for standard channel channel_id={channel_id} to {updated_target:?}");
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
        self.channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                for (vardiff_key, vardiff_state) in channel_manager_data.vardiff.iter_mut() {
                    let channel_id = &vardiff_key.channel_id;
                    let downstream_id = &vardiff_key.downstream_id;

                    let Some(downstream) = channel_manager_data.downstream.get_mut(downstream_id)
                    else {
                        continue;
                    };
                    downstream.downstream_data.super_safe_lock(|data| {
                        if let Some(standard_channel) = data.standard_channels.get_mut(channel_id) {
                            Self::run_vardiff_on_standard_channel(
                                *downstream_id,
                                *channel_id,
                                standard_channel,
                                vardiff_state,
                                &mut messages,
                            );
                        }
                        if let Some(extended_channel) = data.extended_channels.get_mut(channel_id) {
                            Self::run_vardiff_on_extended_channel(
                                *downstream_id,
                                *channel_id,
                                extended_channel,
                                vardiff_state,
                                &mut messages,
                            );
                        }
                    });
                }

                if !messages.is_empty() {
                    let mut downstream_hashrate = 0.0;
                    let mut min_target = [0xff; 32];

                    for (_, downstream) in channel_manager_data.downstream.iter() {
                        downstream.downstream_data.super_safe_lock(|data| {
                            let mut update_from_channel = |hashrate: f32, target: &Target| {
                                downstream_hashrate += hashrate;
                                min_target = std::cmp::min(target.to_le_bytes(), min_target);
                            };

                            for (_, channel) in data.standard_channels.iter() {
                                update_from_channel(
                                    channel.get_nominal_hashrate(),
                                    channel.get_target(),
                                );
                            }

                            for (_, channel) in data.extended_channels.iter() {
                                update_from_channel(
                                    channel.get_nominal_hashrate(),
                                    channel.get_target(),
                                );
                            }
                        });
                    }

                    if let Some(ref mut upstream_channel) = channel_manager_data.upstream_channel {
                        debug!(
                            "Checking upstream channel {} with hashrate {} and target {:?}",
                            upstream_channel.get_channel_id(),
                            upstream_channel.get_nominal_hashrate(),
                            upstream_channel.get_target()
                        );

                        // Update the upstream channel's nominal hashrate to reflect
                        // the aggregated downstream hashrate
                        upstream_channel.set_nominal_hashrate(downstream_hashrate);

                        info!("Sending update channel message upstream");
                        messages.push(
                            Mining::UpdateChannel(UpdateChannel {
                                channel_id: upstream_channel.get_channel_id(),
                                nominal_hash_rate: downstream_hashrate,
                                maximum_target: min_target.into(),
                            })
                            .into(),
                        )
                    }
                }
            });

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
            .send(TemplateDistribution::CoinbaseOutputConstraints(msg))
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
                JDCError::shutdown(JDCErrorKind::ChannelErrorSender)
            })?;

        Ok(())
    }
}
