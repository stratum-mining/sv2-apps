mod extensions_message_handler;
mod max_target;
mod mining_message_handler;

use crate::{
    TproxyMode,
    error::{self, Action, LoopControl, TproxyError, TproxyErrorKind, TproxyResult},
    utils::{
        AGGREGATED_CHANNEL_ID, AggregatedState, AtomicAggregatedState,
        aggregated_upstream_user_identity, tlv_user_identity_from_sv1_worker_name,
    },
};
use async_channel::{Receiver, Sender};
use max_target::{MAX_TARGET_GRACE_PERIOD, UpstreamMaxTarget};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use stratum_apps::{
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    payout::PayoutMode,
    stratum_core::{
        bitcoin::Target,
        channels_sv2::{
            client::{
                extended::ExtendedChannel, group::GroupChannel,
                share_accounting::ShareValidationError,
            },
            extranonce_manager::{ExtranonceAllocator, bytes_needed},
        },
        extensions_sv2::EXTENSION_TYPE_WORKER_HASHRATE_TRACKING,
        handlers_sv2::{
            HandleExtensionsFromServerOwnedAsync, HandleMiningMessagesFromServerOwnedAsync,
        },
        mining_sv2::{
            ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
            ERROR_CODE_OPEN_MINING_CHANNEL_UNSUPPORTED_MIN_EXTRANONCE_SIZE,
            NewExtendedMiningJobOwned, OpenExtendedMiningChannelSuccessOwned,
            OpenMiningChannelErrorOwned, SetExtranoncePrefixOwned,
        },
        parsers_sv2::{AnyMessageOwned, MiningOwned, TlvField, TlvList},
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{ChannelId, DownstreamId, Hashrate, InboundFrame, OutboundFrame, RequestId},
    },
};

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Maximum number of concurrent downstream channels managed by the
/// aggregated-mode shared [`ExtranonceAllocator`]. Determines
/// [`AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES`] via [`bytes_needed`]. The
/// internal allocation bitmap uses `AGGREGATED_TPROXY_MAX_CHANNELS / 8`
/// bytes of RAM.
pub(crate) const AGGREGATED_TPROXY_MAX_CHANNELS: u32 = 65_536;

/// Bytes the translator reserves for its `local_index` in aggregated mode.
/// In that mode a single upstream channel is subdivided across many
/// downstreams, so each downstream needs a unique index; this value is
/// added on top of the downstream-requested rollable size when forwarding
/// `OpenExtendedMiningChannel` upstream, and every submitted share is
/// rewritten to prepend the allocator-assigned local bytes before being
/// forwarded upstream.
pub(crate) const AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES: u8 =
    bytes_needed(AGGREGATED_TPROXY_MAX_CHANNELS);

/// Maximum number of channels managed by the per-downstream
/// [`ExtranonceAllocator`] built in non-aggregated mode. Each downstream
/// already has its own dedicated upstream channel, so no `local_index` is
/// needed to multiplex. The allocator is only used when the upstream
/// grants more rollable space than requested: `max_channels = 1` makes
/// the allocator mint exactly one prefix (with the extra bytes absorbed
/// as zero-padding in `local_prefix_bytes`) so the miner still rolls
/// exactly `config.downstream_extranonce2_size` bytes of `extranonce2`.
/// If upstream grants exactly what was requested, no allocator is built
/// and share rewriting is a no-op.
pub(crate) const NON_AGGREGATED_TPROXY_MAX_CHANNELS: u32 = 1;

#[derive(Clone, Debug)]
struct ChannelManagerIo {
    upstream_sender: Sender<OutboundFrame>,
    upstream_receiver: Receiver<InboundFrame>,
    sv1_server_sender: Sender<MiningOwned>,
    // Option<String> carries non-empty sv1_worker_name metadata for SubmitSharesExtended.
    sv1_server_receiver: Receiver<(MiningOwned, Option<String>)>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManagerIo {
    fn new(
        upstream_sender: Sender<OutboundFrame>,
        upstream_receiver: Receiver<InboundFrame>,
        sv1_server_sender: Sender<MiningOwned>,
        sv1_server_receiver: Receiver<(MiningOwned, Option<String>)>,
    ) -> Self {
        Self {
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
        }
    }

    fn close(&self) {
        debug!("Dropping channel manager channels");
        self.upstream_sender.close();
        self.sv1_server_sender.close();
        self.upstream_receiver.close_and_drain();
        self.sv1_server_receiver.close_and_drain();
    }
}

/// A downstream channel request waiting for its upstream `OpenExtendedMiningChannelSuccess`.
#[derive(Debug, Clone)]
pub struct PendingChannelRequest {
    pub user_identity: String,
    pub nominal_hashrate: Hashrate,
    pub min_extranonce_size: usize,
    /// The easiest target the upstream may assign when it opens the channel.
    pub max_target: Target,
}

// In both modes, whenever an allocator is used, it mints prefixes with
// layout `[upstream_prefix][local_prefix (padding)][local_index]` whose
// rollable region is exactly `config.downstream_extranonce2_size`,
// guaranteeing every SV1 miner rolls the configured number of bytes
// regardless of upstream policy.

/// Manages SV2 channels and message routing between upstream and downstream.
///
/// The ChannelManager serves as the central component that bridges SV2 upstream
/// connections with SV1 downstream connections. It handles:
/// - SV2 channel lifecycle management (open, close, error handling)
/// - Message translation and routing between protocols
/// - Extranonce management for aggregated vs non-aggregated modes
/// - Share submission processing and validation
/// - Job distribution to downstream connections
///
/// The manager supports two operational modes:
/// - Aggregated: All downstream connections share a single extended channel
/// - Non-aggregated: Each downstream connection gets its own extended channel
///
/// This design allows the translator to efficiently manage multiple mining
/// connections while maintaining proper isolation and state management.
#[derive(Debug, Clone)]
pub struct ChannelManager {
    channel_manager_io: ChannelManagerIo,
    /// Extensions that the translator supports (will request if required by server)
    pub supported_extensions: Vec<u16>,
    /// Extensions that the translator requires (must be supported by server)
    pub required_extensions: Vec<u16>,
    /// Past jobs retained per channel; `None` uses the `channels_sv2` default.
    pub max_past_jobs: Option<usize>,
    /// Store pending channel info by downstream_id: (user_identity, hashrate,
    /// downstream_extranonce_len)
    ///
    /// Semantics differ depending on the operating mode:
    ///
    /// 1. Aggregated mode:
    ///    - Stores the initial downstream request that triggers the single upstream channel open.
    ///    - Buffers additional downstream open-channel requests received while awaiting the
    ///      upstream `OpenExtendedMiningChannelSuccess`.
    ///
    /// 2. Non-aggregated mode:
    ///    - Stores all downstreams that are currently waiting for their corresponding upstream
    ///      `OpenExtendedMiningChannelSuccess`.
    ///
    /// Entries are removed when their upstream channel opens. In aggregated mode, buffering ends
    /// when the shared upstream channel becomes connected; later requests are opened immediately.
    pub pending_downstream_channels: SharedMap<DownstreamId, PendingChannelRequest>,
    /// Map of active extended channels by channel ID.
    /// In aggregated mode, the shared upstream channel is stored under AGGREGATED_CHANNEL_ID.
    /// In non-aggregated mode, each downstream has its own channel with its assigned ID.
    pub extended_channels: SharedMap<ChannelId, ExtendedChannel>,
    /// Last extranonce prefix conveyed to each SV1 downstream, either in its channel-open response
    /// or in a queued `mining.set_extranonce` notification.
    ///
    /// An SV2 job captures the prefix that was current when the job arrived. This map lets tProxy
    /// announce a different captured prefix immediately before translating that exact job, even
    /// when an older future job becomes active after a newer `SetExtranoncePrefix`.
    sv1_advertised_extranonce_prefixes: SharedMap<ChannelId, Vec<u8>>,
    /// Map of active group channels by group channel ID
    pub group_channels: SharedMap<ChannelId, GroupChannel>,
    /// Share sequence number counter for tracking valid shares forwarded upstream.
    /// In aggregated mode: single counter for all shares going to the upstream channel.
    /// In non-aggregated mode: one counter per downstream channel.
    pub share_sequence_counters: SharedMap<u32, u32>,
    /// `max_target` bound each upstream channel must respect when sending `SetTarget`, keyed like
    /// `extended_channels`: by channel ID in non-aggregated mode, and by `AGGREGATED_CHANNEL_ID`
    /// for the shared upstream channel in aggregated mode.
    upstream_max_targets: SharedMap<ChannelId, UpstreamMaxTarget>,
    /// Grace period the upstream is given to process an `UpdateChannel`.
    max_target_grace_period: Duration,
    /// Extensions that have been successfully negotiated with the upstream server
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Single extranonce allocator used in aggregated mode to sub-divide the
    /// upstream-assigned prefix across all downstream channels.
    ///
    /// `None` until the upstream `OpenExtendedMiningChannelSuccess` for the
    /// aggregated channel is received; `Some` afterwards.
    pub aggregated_extranonce_allocator: SharedLock<Option<ExtranonceAllocator>>,
    /// Tracks whether the single upstream channel in aggregated mode is absent,
    /// being established, or connected.
    pub aggregated_channel_state: AtomicAggregatedState,
    /// Expected coinbase payout distribution derived from `user_identity`.
    expected_payout_distribution: Arc<OnceLock<Option<PayoutMode>>>,
    /// Current mode Tproxy is operating in.
    pub(crate) mode: TproxyMode,
    /// Required to show or not show hashrate on monitoring.
    #[cfg(feature = "monitoring")]
    pub(crate) report_hashrate: bool,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManager {
    /// Returns the per-downstream prefixes captured with the active job being forwarded.
    fn active_job_extranonce_prefixes(
        &self,
        message: &NewExtendedMiningJobOwned,
    ) -> Option<Vec<(ChannelId, Vec<u8>)>> {
        if message.channel_id == AGGREGATED_CHANNEL_ID {
            let mut prefixes = Vec::new();
            let mut invalid_channel = None;
            self.extended_channels.for_each(|channel_id, channel| {
                if channel_id == AGGREGATED_CHANNEL_ID {
                    return;
                }
                match channel.get_active_job() {
                    Some(job) if job.job_message.job_id == message.job_id => {
                        prefixes.push((channel_id, job.extranonce_prefix.clone()));
                    }
                    _ => invalid_channel = Some(channel_id),
                }
            });
            if let Some(channel_id) = invalid_channel {
                error!(
                    channel_id,
                    job_id = message.job_id,
                    "Aggregated downstream does not have the job being forwarded as active"
                );
                return None;
            }
            return Some(prefixes);
        }

        self.extended_channels
            .with(&message.channel_id, |channel| {
                let Some(job) = channel.get_active_job() else {
                    error!(
                        channel_id = message.channel_id,
                        job_id = message.job_id,
                        "Channel has no active job while forwarding work to SV1"
                    );
                    return None;
                };
                if job.job_message.job_id != message.job_id {
                    error!(
                        channel_id = message.channel_id,
                        active_job_id = job.job_message.job_id,
                        forwarded_job_id = message.job_id,
                        "Channel active job differs from the job being forwarded to SV1"
                    );
                    return None;
                }
                Some(vec![(message.channel_id, job.extranonce_prefix.clone())])
            })
            .flatten()
    }

    /// Queues any prefix transition required by `message` immediately before the job itself.
    ///
    /// `channels_sv2` retains the prefix captured by each job. Keeping both messages on the same
    /// FIFO channel guarantees that every SV1 miner applies `mining.set_extranonce` to the first
    /// `mining.notify` that actually uses that prefix.
    async fn forward_job_to_sv1_server(
        &self,
        message: NewExtendedMiningJobOwned,
    ) -> TproxyResult<(), error::ChannelManager> {
        let job_prefixes = self
            .active_job_extranonce_prefixes(&message)
            .ok_or_else(|| {
                TproxyError::fallback(TproxyErrorKind::FailedToProcessNewExtendedMiningJob)
            })?;
        for (channel_id, job_prefix) in job_prefixes {
            let prefix_changed = self
                .sv1_advertised_extranonce_prefixes
                .with(&channel_id, |advertised_prefix| {
                    advertised_prefix != &job_prefix
                });
            let Some(prefix_changed) = prefix_changed else {
                // A downstream channel and its advertised-prefix state are created and removed
                // within the same serialized ChannelManager handler. Continuing without that
                // state could advertise a job with the wrong extranonce prefix.
                error!(
                    channel_id,
                    "Active downstream channel is missing its SV1 advertised-prefix state"
                );
                return Err(TproxyError::shutdown(TproxyErrorKind::ChannelNotFound));
            };
            if prefix_changed {
                self.channel_manager_io
                    .sv1_server_sender
                    .send(MiningOwned::SetExtranoncePrefix(SetExtranoncePrefixOwned {
                        channel_id,
                        extranonce_prefix: job_prefix
                            .clone()
                            .try_into()
                            .map_err(TproxyError::shutdown)?,
                    }))
                    .await
                    .map_err(|error| {
                        error!(
                            channel_id,
                            "Failed to queue job extranonce prefix for SV1 server: {error:?}"
                        );
                        TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                    })?;
                self.sv1_advertised_extranonce_prefixes
                    .insert(channel_id, job_prefix);
            }
        }

        self.channel_manager_io
            .sv1_server_sender
            .send(MiningOwned::NewExtendedMiningJob(message))
            .await
            .map_err(|error| {
                error!("Failed to queue NewExtendedMiningJob for SV1 server: {error:?}");
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })
    }

    async fn reject_downstream_channel_request(
        &self,
        request_id: RequestId,
        error_code: &'static str,
    ) -> TproxyResult<(), error::ChannelManager> {
        warn!(
            request_id,
            error_code, "Rejecting downstream channel request"
        );
        self.channel_manager_io
            .sv1_server_sender
            .send(MiningOwned::OpenMiningChannelError(
                OpenMiningChannelErrorOwned {
                    request_id,
                    error_code: error_code
                        .try_into()
                        .expect("static channel error code must fit in Str0255"),
                },
            ))
            .await
            .map_err(|e| {
                error!("Failed to send open channel error to SV1Server: {e:?}");
                TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
            })
    }

    fn expected_payout_distribution(&self) -> &Option<PayoutMode> {
        self.expected_payout_distribution
            .get()
            .expect("payout mode should be set")
    }

    pub(crate) fn set_expected_payout_distribution(&self, payout_mode: Option<PayoutMode>) {
        self.expected_payout_distribution
            .set(payout_mode)
            .expect("paymode should be set only once");
    }

    fn handle_error_action(
        &self,
        context: &str,
        e: &TproxyError<error::ChannelManager>,
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
            // Only the upstream can request a reconnect; anywhere else it means a fallback.
            Action::Fallback | Action::Reconnect => {
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
                LoopControl::Continue
            }
        }
    }

    /// Creates a new ChannelManager instance.
    ///
    /// # Arguments
    /// * `upstream_sender` - Channel to send messages to upstream
    /// * `upstream_receiver` - Channel to receive messages from upstream
    /// * `sv1_server_sender` - Channel to send messages to SV1 server
    /// * `sv1_server_receiver` - Channel to receive messages from SV1 server
    /// * `mode` - Operating mode (Aggregated or NonAggregated)
    /// * `supported_extensions` - Extensions that the translator supports (will request if required
    ///   by server)
    /// * `required_extensions` - Extensions that the translator requires (must be supported by
    ///   server)
    ///
    /// # Returns
    /// A new ChannelManager instance ready to handle message routing
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        upstream_sender: Sender<OutboundFrame>,
        upstream_receiver: Receiver<InboundFrame>,
        sv1_server_sender: Sender<MiningOwned>,
        sv1_server_receiver: Receiver<(MiningOwned, Option<String>)>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        tproxy_mode: TproxyMode,
        max_past_jobs: Option<usize>,
        #[cfg(feature = "monitoring")] report_hashrate: bool,
    ) -> Self {
        // Record the cap actually in force. When unset it comes from the `channels_sv2`
        // default, so it appears nowhere in this application's own config and is
        // otherwise not recoverable from a running instance.
        match max_past_jobs {
            Some(n) if n > 0 => info!(max_past_jobs = n, "past-jobs retention cap configured"),
            _ => info!("past-jobs retention cap: using channels_sv2 default"),
        }

        let channel_manager_io = ChannelManagerIo::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
        );

        Self {
            channel_manager_io,
            supported_extensions,
            required_extensions,
            max_past_jobs,
            pending_downstream_channels: SharedMap::new(),
            extended_channels: SharedMap::new(),
            sv1_advertised_extranonce_prefixes: SharedMap::new(),
            group_channels: SharedMap::new(),
            share_sequence_counters: SharedMap::new(),
            upstream_max_targets: SharedMap::new(),
            max_target_grace_period: MAX_TARGET_GRACE_PERIOD,
            negotiated_extensions: SharedLock::new(Vec::new()),
            aggregated_extranonce_allocator: SharedLock::new(None),
            aggregated_channel_state: AtomicAggregatedState::new(AggregatedState::NoChannel),
            expected_payout_distribution: Arc::new(OnceLock::new()),
            mode: tproxy_mode,
            #[cfg(feature = "monitoring")]
            report_hashrate,
        }
    }

    /// Spawns and runs the main channel manager task loop.
    ///
    /// This method creates an async task that handles all message routing for the
    /// channel manager. The task runs a select loop that processes:
    /// - Shutdown signals for graceful termination
    /// - Messages from upstream SV2 server
    /// - Messages from downstream SV1 server
    ///
    /// The task continues running until a shutdown signal is received or an
    /// unrecoverable error occurs. It ensures proper cleanup of resources
    /// and error reporting.
    ///
    /// # Arguments
    /// * `cancellation_token` - Global application cancellation token
    /// * `fallback_coordinator` - Fallback coordinator
    /// * `task_manager` - Manager for tracking spawned tasks
    pub async fn run_channel_manager_tasks(
        self: Arc<Self>,
        cancellation_token: CancellationToken,
        fallback_coordinator: FallbackCoordinator,
        task_manager: Arc<TaskManager>,
    ) {
        task_manager.spawn(async move {
            // we just spawned a new task that's relevant to fallback coordination
            // so register it with the fallback coordinator
            let fallback_handler = fallback_coordinator.register();

            // get the cancellation token that signals fallback
            let fallback_token = fallback_coordinator.token();

            loop {
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        info!("ChannelManager: received shutdown signal");
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        info!("ChannelManager: fallback triggered");
                        break;
                    }
                    res = self.clone().handle_upstream_frame() => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "ChannelManager::handle_upstream_frame",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    },
                    res = self.clone().handle_downstream_message() => {
                        if let Err(e) = res {
                            if let LoopControl::Break = self.handle_error_action(
                                "ChannelManager::handle_downstream_message",
                                &e,
                                &cancellation_token,
                                &fallback_token,
                            ) {
                                break;
                            }
                        }
                    },
                    else => {
                        warn!("All channel manager message streams closed. Exiting...");
                        break;
                    }
                }
            }

            self.channel_manager_io.close();
            warn!("ChannelManager: unified message loop exited.");

            // signal fallback coordinator that this task has completed its cleanup
            fallback_handler.done();
        });
    }

    /// Handles messages received from the upstream SV2 server.
    ///
    /// This method processes SV2 messages from upstream and routes them appropriately:
    /// - Mining messages: Processed through the roles logic and forwarded to SV1 server
    /// - Channel responses: Handled to manage channel lifecycle
    /// - Job notifications: Converted and distributed to downstream connections
    /// - Error messages: Logged and handled appropriately
    ///
    /// The method implements the core SV2 protocol logic for channel management,
    /// including handling both aggregated and non-aggregated channel modes.
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    async fn handle_upstream_frame(self: Arc<Self>) -> TproxyResult<(), error::ChannelManager> {
        let mut sv2_frame = self
            .channel_manager_io
            .upstream_receiver
            .recv()
            .await
            .map_err(TproxyError::fallback)?;

        let mut channel_manager: ChannelManager = (*self).clone();
        let header = sv2_frame.header();
        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::Mining => {
                channel_manager
                    .handle_mining_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            MessageType::Extensions => {
                channel_manager
                    .handle_extensions_message_frame_from_server(None, header, sv2_frame.payload())
                    .await?;
            }
            _ => {
                error!(
                    extension_type = header.ext_type(),
                    message_type = header.msg_type(),
                    "Received unexpected message type from upstream"
                );
                return Err(TproxyError::fallback(TproxyErrorKind::UnexpectedMessage(
                    header.ext_type(),
                    header.msg_type(),
                )));
            }
        }

        Ok(())
    }

    /// Handles messages received from the downstream SV1 server.
    ///
    /// This method processes requests from the SV1 server, primarily:
    /// - OpenExtendedMiningChannel: Sets up new SV2 channels for downstream connections
    /// - SubmitSharesExtended: Processes share submissions from miners
    ///
    /// For channel opening, the method handles both aggregated and non-aggregated modes:
    /// - Aggregated: Creates extended channels using extranonce prefixes
    /// - Non-aggregated: Opens individual extended channels with the upstream for each downstream
    ///
    /// Share submissions are validated, processed through the channel logic,
    /// and forwarded to the upstream server with appropriate extranonce handling.
    ///
    /// # Returns
    /// * `Ok(())` - Message processed successfully
    /// * `Err(TproxyError)` - Error processing the message
    async fn handle_downstream_message(self: Arc<Self>) -> TproxyResult<(), error::ChannelManager> {
        let (message, sv1_worker_name) = self
            .channel_manager_io
            .sv1_server_receiver
            .recv()
            .await
            .map_err(TproxyError::shutdown)?;
        match message {
            MiningOwned::OpenExtendedMiningChannel(m) => {
                let mut open_channel_msg = m.clone();
                let mut user_identity = m.user_identity.as_utf8_or_hex();
                let hashrate = m.nominal_hash_rate;
                let min_extranonce_size = m.min_extranonce_size as usize;
                let max_target = Target::from_le_bytes(m.max_target.to_array());

                if self.mode.is_aggregated() {
                    match self.aggregated_channel_state.get() {
                        AggregatedState::Connected => {
                            return self
                                .handle_downstream_channel_request_in_aggregated_mode(
                                    open_channel_msg.request_id,
                                    user_identity,
                                    hashrate,
                                    open_channel_msg.min_extranonce_size.into(),
                                )
                                .await;
                        }
                        AggregatedState::Pending => {
                            self.pending_downstream_channels.insert(
                                m.request_id as DownstreamId,
                                PendingChannelRequest {
                                    user_identity,
                                    nominal_hashrate: hashrate,
                                    min_extranonce_size,
                                    max_target,
                                },
                            );
                            return Ok(());
                        }
                        AggregatedState::NoChannel => {
                            self.aggregated_channel_state.set(AggregatedState::Pending);
                            self.pending_downstream_channels.insert(
                                m.request_id as DownstreamId,
                                PendingChannelRequest {
                                    user_identity: user_identity.clone(),
                                    nominal_hashrate: hashrate,
                                    min_extranonce_size,
                                    max_target,
                                },
                            );
                            // Modify user_identity for the upstream `OpenExtendedMiningChannel`.
                            // SRI patterns are passed unchanged to preserve pool-side parsing.
                            // See: https://github.com/stratum-mining/sv2-apps/issues/369
                            user_identity = aggregated_upstream_user_identity(&user_identity);
                            open_channel_msg.user_identity =
                                user_identity.as_str().try_into().unwrap();
                        }
                    }
                }
                // In aggregated mode, widen the upstream request by
                // [`AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES`] so the single
                // upstream channel has room for the [`ExtranonceAllocator`]'s
                // `local_index` that uniquely addresses each multiplexed
                // downstream.
                //
                // In non-aggregated mode there is nothing to multiplex (1
                // upstream ↔ 1 downstream), so we request `min_extranonce_size` verbatim. Any slack
                // upstream may grant on top is absorbed later as allocator padding.
                let upstream_min_extranonce_size = if self.mode.is_aggregated() {
                    min_extranonce_size + AGGREGATED_TPROXY_LOCAL_PREFIX_BYTES as usize
                } else {
                    min_extranonce_size
                };

                // Update the message with the adjusted extranonce size for upstream
                open_channel_msg.min_extranonce_size = upstream_min_extranonce_size as u16;

                // In non-aggregated mode, store the request in the pending_channel to be later
                // used in the `OpenExtendedMiningChannel.Success` handler.
                // In aggregated mode it was already inserted in the `AggregatedState::NoChannel`
                // match arm above.
                if !self.mode.is_aggregated() {
                    self.pending_downstream_channels.insert(
                        open_channel_msg.request_id as DownstreamId,
                        PendingChannelRequest {
                            user_identity,
                            nominal_hashrate: hashrate,
                            min_extranonce_size,
                            max_target,
                        },
                    );
                }

                info!(
                    "Sending OpenExtendedMiningChannel message to upstream: {:?}",
                    open_channel_msg
                );

                let message = MiningOwned::OpenExtendedMiningChannel(open_channel_msg);
                let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
                    .map_err(TproxyError::shutdown)?;
                self.channel_manager_io
                    .upstream_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!("Failed to send open channel message to upstream: {:?}", e);
                        TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                    })?;
            }
            MiningOwned::SubmitSharesExtended(mut m) => {
                if self.mode.is_aggregated()
                    && self.extended_channels.contains_key(&AGGREGATED_CHANNEL_ID)
                {
                    let downstream_channel_id = m.channel_id;
                    let downstream_extranonce_prefix = self
                        .extended_channels
                        .with(&downstream_channel_id, |channel| {
                            channel.get_extranonce_prefix().to_vec()
                        });
                    let upstream_prefix_len = self
                        .aggregated_extranonce_allocator
                        .with(|allocator| {
                            allocator.as_ref().map(|a| a.upstream_prefix_len() as usize)
                        })
                        .map_err(TproxyError::shutdown)?;
                    if let (Some(downstream_extranonce_prefix), Some(upstream_prefix_len)) =
                        (downstream_extranonce_prefix, upstream_prefix_len)
                    {
                        // Strip `upstream_prefix`; the upstream share contains
                        // `local_prefix | local_index | miner extranonce`.
                        let local_prefix_and_index =
                            &downstream_extranonce_prefix[upstream_prefix_len..];
                        let mut new_extranonce = local_prefix_and_index.to_vec();
                        new_extranonce.extend_from_slice(m.extranonce.as_ref());
                        m.extranonce = new_extranonce.try_into().map_err(TproxyError::shutdown)?;
                    }

                    let upstream_extended_channel_id = self
                        .extended_channels
                        .with(&AGGREGATED_CHANNEL_ID, |ch| ch.get_channel_id())
                        .unwrap();
                    m.channel_id = upstream_extended_channel_id;

                    let value = self
                        .extended_channels
                        .with_mut(&AGGREGATED_CHANNEL_ID, |aggregated_channel| {
                            aggregated_channel.validate_share(m.clone())
                        });
                    match value {
                        Some(Ok(_result)) => {
                            info!(
                                "SubmitSharesExtended: valid share, forwarding it to upstream | channel_id: {}, sequence_number: {} ☑️",
                                upstream_extended_channel_id, m.sequence_number
                            );

                            // In aggregated mode, use a single sequence counter for all valid
                            // shares.
                            m.sequence_number =
                                self.next_share_sequence_number(upstream_extended_channel_id);
                        }
                        Some(Err(ShareValidationError::DoesNotMeetTarget(error_code))) => {
                            // Per-miner vardiff can intentionally be easier than the aggregated
                            // upstream target, so these locally accepted shares are expected to be
                            // filtered here.
                            debug!(
                                channel_id = upstream_extended_channel_id,
                                error_code,
                                "SubmitSharesExtended does not meet the upstream channel target"
                            );
                            return Ok(());
                        }
                        Some(Err(validation_error)) => {
                            warn!(
                                channel_id = upstream_extended_channel_id,
                                ?validation_error,
                                "SubmitSharesExtended rejected by upstream channel validation"
                            );
                            return Ok(());
                        }
                        None => {
                            warn!(
                                channel_id = upstream_extended_channel_id,
                                "SubmitSharesExtended references an unknown upstream channel"
                            );
                            return Ok(());
                        }
                    }
                } else {
                    let value = self
                        .extended_channels
                        .with_mut(&m.channel_id, |extended_channel| {
                            extended_channel.validate_share(m.clone())
                        });
                    match value {
                        Some(Ok(_result)) => {
                            info!(
                                "SubmitSharesExtended: valid share, forwarding it to upstream | channel_id: {}, sequence_number: {} ☑️",
                                m.channel_id, m.sequence_number
                            );
                            // In non-aggregated mode, each downstream channel has its own sequence
                            // counter.
                            m.sequence_number = self.next_share_sequence_number(m.channel_id);

                            // Rebuild the upstream share extranonce as
                            // `local_prefix | local_index | miner extranonce`.
                            // A wire-sourced prefix is entirely `upstream_prefix`,
                            // so this naturally leaves no local bytes to prepend.
                            let local_prefix_and_index =
                                self.extended_channels.with(&m.channel_id, |c| {
                                    c.get_extranonce_prefix()[c.upstream_prefix_len() as usize..]
                                        .to_vec()
                                });
                            if let Some(local_prefix_and_index) =
                                local_prefix_and_index.filter(|prefix| !prefix.is_empty())
                            {
                                let mut new_extranonce = local_prefix_and_index;
                                new_extranonce.extend_from_slice(m.extranonce.as_ref());
                                m.extranonce =
                                    new_extranonce.try_into().map_err(TproxyError::shutdown)?;
                            }
                        }
                        Some(Err(ShareValidationError::DoesNotMeetTarget(error_code))) => {
                            // A downstream vardiff target can intentionally be easier than its
                            // upstream channel target, so these locally accepted shares are
                            // expected to be filtered here.
                            debug!(
                                channel_id = m.channel_id,
                                error_code,
                                "SubmitSharesExtended does not meet the upstream channel target"
                            );
                            return Ok(());
                        }
                        Some(Err(validation_error)) => {
                            warn!(
                                channel_id = m.channel_id,
                                ?validation_error,
                                "SubmitSharesExtended rejected by upstream channel validation"
                            );
                            return Ok(());
                        }
                        None => {
                            warn!(
                                channel_id = m.channel_id,
                                "SubmitSharesExtended references an unknown upstream channel"
                            );
                            return Ok(());
                        }
                    }
                }

                // Send the share upstream (common for both aggregated and non-aggregated modes)
                let contains_type_in_negotiated_extension = self
                    .negotiated_extensions
                    .with(|data| data.contains(&EXTENSION_TYPE_WORKER_HASHRATE_TRACKING))
                    .map_err(TproxyError::shutdown)?;

                let mut sent = false;
                if contains_type_in_negotiated_extension {
                    if let Some(sv1_worker_name) = sv1_worker_name
                        .as_deref()
                        .filter(|sv1_worker_name| !sv1_worker_name.is_empty())
                    {
                        let tlv_user_identity =
                            tlv_user_identity_from_sv1_worker_name(sv1_worker_name)
                                .map_err(TproxyError::shutdown)?;
                        let tlv = tlv_user_identity.to_tlv().map_err(TproxyError::shutdown)?;
                        let tlv_list = TlvList::from_slice(&[tlv]).map_err(|e| {
                            error!("Failed to create TLV list: {:?}", e);
                            TproxyError::shutdown(e)
                        })?;
                        let frame_bytes = tlv_list
                            .build_frame_bytes_with_tlvs(MiningOwned::SubmitSharesExtended(
                                m.clone(),
                            ))
                            .map_err(|e| {
                                error!("Failed to build frame bytes with TLVs: {:?}", e);
                                TproxyError::shutdown(e)
                            })?;
                        // The TLV bytes are already framed, so they go out as a raw frame.
                        let sv2_frame =
                            InboundFrame::from_bytes(frame_bytes.into()).map_err(|missing| {
                                error!("Failed to frame the TLV bytes: {missing}");
                                TproxyError::shutdown(TproxyErrorKind::UnexpectedMessage(0, 0))
                            })?;
                        self.channel_manager_io
                            .upstream_sender
                            .send(sv2_frame.into())
                            .await
                            .map_err(|e| {
                                error!(
                                    "Failed to send submit shares extended message to upstream: {:?}",
                                    e
                                );
                                TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                            })?;
                        sent = true;
                    }
                }

                if !sent {
                    let message = MiningOwned::SubmitSharesExtended(m);
                    let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
                        .map_err(TproxyError::shutdown)?;
                    self.channel_manager_io
                        .upstream_sender
                        .send(sv2_frame)
                        .await
                        .map_err(|e| {
                            error!(
                                "Failed to send submit shares extended message to upstream: {:?}",
                                e
                            );
                            TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                        })?;
                }
            }
            MiningOwned::UpdateChannel(mut m) => {
                debug!("Received UpdateChannel from SV1Server: {}", m);

                if self.mode.is_aggregated() {
                    // Update the aggregated channel's nominal hashrate so
                    // that monitoring reports a value consistent with the
                    // downstream vardiff estimate.
                    if let Some(channel_id) = self.extended_channels.with_mut(
                        &AGGREGATED_CHANNEL_ID,
                        |aggregated_extended_channel| {
                            aggregated_extended_channel.set_nominal_hashrate(m.nominal_hash_rate);
                            aggregated_extended_channel.get_channel_id()
                        },
                    ) {
                        m.channel_id = channel_id;
                    } else {
                        warn!(
                            "Ignoring aggregated UpdateChannel before upstream channel is open: {:?}",
                            m
                        );
                        return Ok(());
                    }
                } else {
                    // Non-aggregated: update the specific channel's nominal hashrate
                    self.extended_channels.with_mut(&m.channel_id, |channel| {
                        channel.set_nominal_hashrate(m.nominal_hash_rate);
                    });
                }

                // Every later `SetTarget` is checked against the `max_target` sent here.
                let key = self.upstream_max_target_key(m.channel_id);
                let max_target = Target::from_le_bytes(m.max_target.to_array());
                self.upstream_max_targets.with_mut(&key, |bound| {
                    bound.on_update_channel(
                        max_target,
                        Instant::now(),
                        self.max_target_grace_period,
                    )
                });

                info!(
                    "Sending UpdateChannel message to upstream for channel_id: {}",
                    m.channel_id
                );
                // Forward UpdateChannel message to upstream
                let message = MiningOwned::UpdateChannel(m);
                let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
                    .map_err(TproxyError::shutdown)?;

                self.channel_manager_io
                    .upstream_sender
                    .send(sv2_frame)
                    .await
                    .map_err(|e| {
                        error!("Failed to send UpdateChannel message to upstream: {:?}", e);
                        TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                    })?;
            }
            MiningOwned::CloseChannel(m) => {
                debug!("Received CloseChannel from Sv1Server: {m}");

                // Guard: never remove the aggregated upstream sentinel entry
                // here. `AGGREGATED_CHANNEL_ID` represents the single shared
                // upstream channel in aggregated mode and must only be torn
                // down via fallback/shutdown.
                if self.mode.is_aggregated() && m.channel_id == AGGREGATED_CHANNEL_ID {
                    warn!("Ignoring CloseChannel from Sv1Server targeting AGGREGATED_CHANNEL_ID");
                    return Ok(());
                }

                // Remove the per-downstream `ExtendedChannel`. Dropping it
                // releases the allocator-minted `ExtranoncePrefix` it owns
                // via RAII:
                //
                // - Aggregated mode: clears the downstream's bit in the shared
                //   `aggregated_extranonce_allocator`'s bitmap, making the slot reusable for future
                //   downstreams. This is the primary reclaim path for aggregated slots.
                // - Non-aggregated mode: a silent no-op — the per-channel allocator has already
                //   been dropped right after minting the single prefix (the bitmap was keyed only
                //   on that one slot), so the prefix's `Weak` reference fails to upgrade and there
                //   is nothing to clear.
                if self.extended_channels.remove(&m.channel_id).is_some() {
                    debug!("Removed channel {} from extended_channels", m.channel_id);
                } else {
                    warn!(
                        "Attempted to remove channel {} from extended_channels but it was not found",
                        m.channel_id
                    );
                }
                self.sv1_advertised_extranonce_prefixes
                    .remove(&m.channel_id);

                // In non-aggregated mode the local channel ID is the upstream channel ID, so the
                // closed channel's share sequence counter, `max_target` bound and group membership
                // go with it. In aggregated mode they all belong to the shared upstream channel,
                // whose upstream channel ID can equal a local channel ID, so closing a local
                // channel must not touch them.
                if !self.mode.is_aggregated() {
                    self.share_sequence_counters.remove(&m.channel_id);
                    self.upstream_max_targets.remove(&m.channel_id);
                    self.remove_channel_from_groups(m.channel_id);
                }

                // Only forward `CloseChannel` upstream in non-aggregated
                // mode. In aggregated mode the upstream channel is shared
                // across all SV1 miners and must stay open when any one of
                // them disconnects.
                if !self.mode.is_aggregated() {
                    let message = MiningOwned::CloseChannel(m);
                    let sv2_frame = OutboundFrame::from_message(AnyMessageOwned::Mining(message))
                        .map_err(TproxyError::shutdown)?;

                    self.channel_manager_io
                        .upstream_sender
                        .send(sv2_frame)
                        .await
                        .map_err(|e| {
                            error!("Failed to send CloseChannel message to upstream: {:?}", e);
                            TproxyError::fallback(TproxyErrorKind::ChannelErrorSender)
                        })?;
                }
            }
            _ => {
                warn!("Unhandled downstream message: {}", message);
            }
        }

        Ok(())
    }

    /// Handles a downstream extended channel request in aggregated mode.
    ///
    /// Allocates a new extranonce prefix, creates a new downstream
    /// `ExtendedChannel`, and sends an
    /// `OpenExtendedMiningChannelSuccess` to the SV1Server.
    ///
    /// The new channel is initialized with the aggregated channel’s
    /// current state (chain tip, active job, and future jobs) so the
    /// downstream can start mining immediately.
    ///
    /// The subscribe response uses the active job's upstream prefix, which may differ from the
    /// allocator's current prefix. Each inherited job captures its original upstream prefix and
    /// target on the child channel; subsequent jobs use the current upstream state. All prefix
    /// variants preserve the child's local suffix and share one allocator reservation.
    async fn handle_downstream_channel_request_in_aggregated_mode(
        &self,
        request_id: RequestId,
        user_identity: String,
        hashrate: Hashrate,
        min_extranonce_size: usize,
    ) -> TproxyResult<(), error::ChannelManager> {
        let aggregate_state = self
            .extended_channels
            .with(&AGGREGATED_CHANNEL_ID, |channel| {
                (
                    *channel.get_target(),
                    channel.get_extranonce_prefix().to_vec(),
                    channel.get_active_job().cloned(),
                    channel
                        .get_future_jobs()
                        .map(|(_, job)| job.clone())
                        .collect::<Vec<_>>(),
                    channel.get_chain_tip().cloned(),
                )
            });
        let Some((target, current_upstream_prefix, active_job, future_jobs, chain_tip)) =
            aggregate_state
        else {
            // This function is entered only when the aggregate state is Connected. That state is
            // set after the shared channel and allocator are installed, and downstream cleanup
            // cannot remove the reserved aggregate channel.
            error!("Aggregated channel state is connected without the shared aggregate channel");
            return Err(TproxyError::shutdown(TproxyErrorKind::ChannelNotFound));
        };

        // We already have the unique upstream channel open. Allocate a new
        // extranonce prefix for this downstream and send the
        // OpenExtendedMiningChannelSuccess message directly to the sv1
        // server.
        // The aggregated allocator was built with the upstream prefix padded
        // so that `rollable_extranonce_size == config.downstream_extranonce2_size`.
        // `allocate_extended` therefore returns a prefix whose bytes are
        // already `[true_upstream][zero_padding][local_index]` — exactly what
        // the downstream sees as its SV1 extranonce1.
        let allocation = self
            .aggregated_extranonce_allocator
            .with(|allocator| {
                allocator.as_mut().map(|a| {
                    let rollable = a.rollable_extranonce_size() as usize;
                    (a.allocate_extended(min_extranonce_size), rollable)
                })
            })
            .map_err(TproxyError::shutdown)?;
        let Some((allocation, rollable_extranonce_size)) = allocation else {
            error!("Aggregated channel is connected without an extranonce allocator");
            return Err(TproxyError::shutdown(
                TproxyErrorKind::OpenMiningChannelError,
            ));
        };
        if let Ok(new_extranonce_prefix) = allocation {
            if rollable_extranonce_size == min_extranonce_size {
                // Prefer monotonically increasing downstream IDs while space remains. Once the
                // highest usable ID is reached, scan from 1 for a gap left by a disconnected
                // downstream. AGGREGATED_CHANNEL_ID is reserved for aggregate upstream messages
                // and must never be assigned to an individual downstream.
                let mut channel_id = 0;
                self.extended_channels.for_each(|extended_channel_id, _| {
                    if extended_channel_id != AGGREGATED_CHANNEL_ID {
                        channel_id = channel_id.max(extended_channel_id);
                    }
                });
                let next_channel_id = channel_id
                    .checked_add(1)
                    .filter(|channel_id| *channel_id != AGGREGATED_CHANNEL_ID)
                    .or_else(|| {
                        (1..AGGREGATED_CHANNEL_ID)
                            .find(|channel_id| !self.extended_channels.contains_key(channel_id))
                    });
                let Some(next_channel_id) = next_channel_id else {
                    // The prefix holds its allocator slot until dropped. Release it before the
                    // asynchronous rejection path so a failed request does not consume capacity.
                    drop(new_extranonce_prefix);
                    return self
                        .reject_downstream_channel_request(
                            request_id,
                            ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
                        )
                        .await;
                };
                let mut new_downstream_extended_channel = ExtendedChannel::new(
                    next_channel_id,
                    user_identity.clone(),
                    new_extranonce_prefix.into(),
                    target,
                    hashrate,
                    true,
                    min_extranonce_size as u16,
                    self.max_past_jobs,
                )
                .map_err(|e| {
                    // the target is the aggregated channel's own, validated when it was
                    // installed, so this only fails on an internal inconsistency
                    error!(
                        "Aggregated channel holds a target no share can meet: {:?}",
                        e
                    );
                    TproxyError::shutdown(TproxyErrorKind::OpenMiningChannelError)
                })?;
                let prefix_error = |error| {
                    TproxyError::shutdown(TproxyErrorKind::UpstreamExtranoncePrefixUpdateFailed {
                        channel_id: next_channel_id,
                        error,
                    })
                };
                let replay_error = |error| {
                    error!(
                        channel_id = next_channel_id,
                        ?error,
                        "Failed to initialize inherited downstream job"
                    );
                    TproxyError::shutdown(TproxyErrorKind::FailedToProcessNewExtendedMiningJob)
                };
                let initial_upstream_prefix = active_job
                    .as_ref()
                    .map(|job| job.extranonce_prefix.as_slice())
                    .unwrap_or(&current_upstream_prefix);
                new_downstream_extended_channel
                    .set_upstream_extranonce_prefix(initial_upstream_prefix)
                    .map_err(prefix_error)?;
                let success_extranonce_prefix = new_downstream_extended_channel
                    .get_extranonce_prefix()
                    .to_vec();
                if let Some(chain_tip) = chain_tip {
                    new_downstream_extended_channel.set_chain_tip(chain_tip);
                }
                // Replay before publishing the channel, so any initialization error releases its
                // allocation without exposing partially initialized work to the SV1 server.
                for job in active_job.iter().chain(future_jobs.iter()) {
                    new_downstream_extended_channel
                        .set_upstream_extranonce_prefix(&job.extranonce_prefix)
                        .map_err(prefix_error)?;
                    new_downstream_extended_channel
                        .set_target(job.target)
                        .map_err(replay_error)?;
                    let mut message = job.job_message.clone();
                    message.channel_id = next_channel_id;
                    new_downstream_extended_channel
                        .on_new_extended_mining_job(message)
                        .map_err(replay_error)?;
                }
                // Subsequent jobs use the current prefix. Active work keeps its captured target;
                // queued future jobs follow SetTarget semantics, just like the aggregate channel.
                // All prefix variants retain ownership of the same allocated local index.
                new_downstream_extended_channel
                    .set_upstream_extranonce_prefix(&current_upstream_prefix)
                    .map_err(prefix_error)?;
                new_downstream_extended_channel
                    .set_target(target)
                    .map_err(replay_error)?;
                self.extended_channels
                    .insert(next_channel_id, new_downstream_extended_channel);
                let success_message = MiningOwned::OpenExtendedMiningChannelSuccess(
                    OpenExtendedMiningChannelSuccessOwned {
                        request_id,
                        channel_id: next_channel_id,
                        target: target.to_le_bytes().into(),
                        extranonce_size: min_extranonce_size as u16,
                        extranonce_prefix: success_extranonce_prefix
                            .clone()
                            .try_into()
                            .map_err(TproxyError::shutdown)?,
                        group_channel_id: 0, /* use a dummy value, this
                                              * shouldn't
                                              * matter for the Sv1 server */
                    },
                );

                self.channel_manager_io
                    .sv1_server_sender
                    .send(success_message)
                    .await
                    .map_err(|e| {
                        error!("Failed to send open channel message to SV1Server: {:?}", e);
                        TproxyError::shutdown(TproxyErrorKind::ChannelErrorSender)
                    })?;
                self.sv1_advertised_extranonce_prefixes
                    .insert(next_channel_id, success_extranonce_prefix);
                if let Some(job) = active_job {
                    let mut message = job.job_message;
                    message.channel_id = next_channel_id;
                    self.forward_job_to_sv1_server(message).await?;
                }
                return Ok(());
            }
        }
        if rollable_extranonce_size != min_extranonce_size {
            self.reject_downstream_channel_request(
                request_id,
                ERROR_CODE_OPEN_MINING_CHANNEL_UNSUPPORTED_MIN_EXTRANONCE_SIZE,
            )
            .await
        } else {
            self.reject_downstream_channel_request(
                request_id,
                ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED,
            )
            .await
        }
    }

    /// Opens buffered downstream requests once the aggregate upstream channel is connected.
    async fn open_pending_aggregated_downstream_channels(
        &self,
    ) -> TproxyResult<(), error::ChannelManager> {
        if self.aggregated_channel_state.get() != AggregatedState::Connected {
            return Ok(());
        }
        let mut pending_requests = Vec::new();
        self.pending_downstream_channels
            .for_each(|request_id, request| {
                pending_requests.push((
                    request_id as RequestId,
                    request.user_identity.clone(),
                    request.nominal_hashrate,
                    request.min_extranonce_size,
                ));
            });
        self.pending_downstream_channels.clear();
        if !pending_requests.is_empty() {
            info!(
                count = pending_requests.len(),
                "Opening buffered aggregated downstream channel requests"
            );
        }

        for (request_id, user_identity, hashrate, min_extranonce_size) in pending_requests {
            self.handle_downstream_channel_request_in_aggregated_mode(
                request_id,
                user_identity,
                hashrate,
                min_extranonce_size,
            )
            .await?;
        }

        Ok(())
    }

    /// Gets the next sequence number for a valid share and increments the counter.
    ///
    /// The counter_key determines which counter to use:
    /// - In aggregated mode: use upstream channel ID (single counter for all shares)
    /// - In non-aggregated mode: use downstream channel ID (one counter per channel, removed when
    ///   the channel closes)
    fn next_share_sequence_number(&self, counter_key: u32) -> u32 {
        self.share_sequence_counters
            .with_mut_or_default(counter_key, |counter| {
                *counter += 1;
                *counter
            })
    }

    /// Key of the `max_target` bound of the upstream channel that `channel_id` refers to.
    fn upstream_max_target_key(&self, channel_id: ChannelId) -> ChannelId {
        if self.mode.is_aggregated() {
            AGGREGATED_CHANNEL_ID
        } else {
            channel_id
        }
    }

    /// Checks a `SetTarget` against the `max_target` bound of every upstream channel it applies to,
    /// before any channel state changes.
    #[allow(clippy::result_large_err)]
    fn check_set_target_bounds(
        &self,
        keys: &[ChannelId],
        target: Target,
    ) -> TproxyResult<(), error::ChannelManager> {
        let now = Instant::now();
        for &key in keys {
            let Some(Err(violation)) = self.upstream_max_targets.with_mut(&key, |bound| {
                bound.on_set_target(target, now, self.max_target_grace_period)
            }) else {
                continue;
            };
            error!(
                channel = key,
                target = %violation.target,
                bound = %violation.bound,
                "Upstream SetTarget exceeds the max_target it is bound by"
            );
            return Err(TproxyError::fallback(
                TproxyErrorKind::SetTargetAboveMaxTarget,
            ));
        }
        Ok(())
    }

    /// Removes `channel_id` from every group channel that contains it, and drops the groups it
    /// leaves empty.
    ///
    /// An empty group would otherwise keep its ID reserved and its `full_extranonce_size`, so a
    /// later channel joining a group with the same ID could be rejected.
    fn remove_channel_from_groups(&self, channel_id: ChannelId) {
        let mut emptied_groups = Vec::new();
        self.group_channels
            .for_each_mut(|group_channel_id, group_channel| {
                if group_channel.has_channel_id(channel_id) {
                    group_channel.remove_channel_id(channel_id);
                    debug!("Removed channel {channel_id} from group channel {group_channel_id}");
                    if group_channel.is_empty() {
                        emptied_groups.push(group_channel_id);
                    }
                }
            });
        for group_channel_id in emptied_groups {
            // Only remove the group if no channel joined it in the meantime.
            if self
                .group_channels
                .remove_if(&group_channel_id, |_, group_channel| {
                    group_channel.is_empty()
                })
                .is_some()
            {
                debug!("Removed empty group channel {group_channel_id}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_request(
        user_identity: &str,
        nominal_hashrate: Hashrate,
        min_extranonce_size: usize,
        max_target: Target,
    ) -> PendingChannelRequest {
        PendingChannelRequest {
            user_identity: user_identity.to_string(),
            nominal_hashrate,
            min_extranonce_size,
            max_target,
        }
    }
    use async_channel::unbounded;
    use stratum_apps::stratum_core::{
        binary_sv2::{Seq0255Owned, Str0255Owned, Sv2OptionOwned},
        bitcoin::Target,
        channels_sv2::extranonce_manager::ExtranoncePrefix,
        mining_sv2::{
            CloseChannelOwned, NewExtendedMiningJobOwned, OpenExtendedMiningChannelOwned,
            SetExtranoncePrefixOwned, SetNewPrevHashOwned, SetTargetOwned,
            SubmitSharesExtendedOwned, UpdateChannelErrorOwned, UpdateChannelOwned,
        },
    };

    fn create_test_channel_manager() -> ChannelManager {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender2, upstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (_sv1_server_sender2, sv1_server_receiver) = unbounded();

        ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::from(true),
            None,
            #[cfg(feature = "monitoring")]
            true,
        )
    }

    fn create_connected_aggregated_channel_manager() -> (ChannelManager, Receiver<MiningOwned>) {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::Aggregated);
        connect_aggregated_channel(&manager);
        (manager, io.sv1_server_receiver)
    }

    /// Installs an established aggregated upstream channel with upstream channel ID 42.
    fn connect_aggregated_channel(manager: &ChannelManager) {
        manager.extended_channels.insert(
            AGGREGATED_CHANNEL_ID,
            ExtendedChannel::new(
                42,
                "aggregated".to_string(),
                ExtranoncePrefix::from_wire(vec![0; 4]).unwrap(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                8,
                None,
            )
            .unwrap(),
        );
        manager
            .aggregated_extranonce_allocator
            .set(Some(
                ExtranonceAllocator::from_upstream_prefix(
                    vec![0; 4],
                    vec![],
                    12,
                    AGGREGATED_TPROXY_MAX_CHANNELS,
                )
                .unwrap(),
            ))
            .unwrap();
        manager
            .aggregated_channel_state
            .set(AggregatedState::Connected);
    }

    fn test_extended_job(channel_id: ChannelId, job_id: u32) -> NewExtendedMiningJobOwned {
        NewExtendedMiningJobOwned {
            channel_id,
            job_id,
            ntime_start: Sv2OptionOwned::new(None),
            version: 0x20000000,
            version_rolling_allowed: true,
            merkle_path: Seq0255Owned::new(vec![]).unwrap(),
            coinbase_tx_prefix: hex::decode("02000000010000000000000000000000000000000000000000000000000000000000000000ffffffff265200162f5374726174756d2056322053524920506f6f6c2f2f08")
                .unwrap()
                .try_into()
                .unwrap(),
            coinbase_tx_suffix: hex::decode("feffffff0200f2052a01000000160014ebe1b7dcc293ccaa0ee743a86f89df8258c208fc0000000000000000266a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf901000000")
                .unwrap()
                .try_into()
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn test_handle_downstream_open_channel_message() {
        let manager = create_test_channel_manager();

        // Create an OpenExtendedMiningChannel message
        let open_channel = OpenExtendedMiningChannelOwned {
            request_id: 1,
            user_identity: "test_user".try_into().unwrap(),
            nominal_hash_rate: 1000.0,
            max_target: [0xFFu8; 32].into(),
            min_extranonce_size: 4,
        };

        // Store the pending channel information
        manager.pending_downstream_channels.insert(
            1,
            pending_request("test_user", 1000.0, 4, Target::from_le_bytes([0xff; 32])),
        );

        // Test that the message can be handled without panicking
        // In a real test environment, we would need to mock the upstream sender
        // For now, we just verify the channel manager can process the message type
        let mining_message = MiningOwned::OpenExtendedMiningChannel(open_channel);

        // Verify the message can be processed (would normally be sent to upstream)
        match mining_message {
            MiningOwned::OpenExtendedMiningChannel(msg) => {
                assert_eq!(msg.request_id, 1);
                assert_eq!(msg.nominal_hash_rate, 1000.0);
                assert_eq!(msg.min_extranonce_size, 4);
            }
            _ => panic!("Expected OpenExtendedMiningChannel"),
        }
    }

    #[tokio::test]
    async fn test_handle_downstream_submit_shares_message() {
        let _manager = create_test_channel_manager();

        // Create a SubmitSharesExtended message
        let submit_shares = SubmitSharesExtendedOwned {
            channel_id: 1,
            sequence_number: 100,
            job_id: 42,
            nonce: 0x12345678,
            ntime: 1234567890,
            version: 0x20000000,
            extranonce: vec![0x01, 0x02, 0x03, 0x04].try_into().unwrap(),
        };

        // Test that the message can be handled
        let mining_message = MiningOwned::SubmitSharesExtended(submit_shares);

        // Verify the message structure
        match mining_message {
            MiningOwned::SubmitSharesExtended(msg) => {
                assert_eq!(msg.channel_id, 1);
                assert_eq!(msg.sequence_number, 100);
                assert_eq!(msg.job_id, 42);
                assert_eq!(msg.nonce, 0x12345678);
            }
            _ => panic!("Expected SubmitSharesExtended"),
        }
    }

    #[tokio::test]
    async fn test_handle_downstream_update_channel_message() {
        let _manager = create_test_channel_manager();

        // Create an UpdateChannel message
        let update_channel = UpdateChannelOwned {
            channel_id: 1,
            nominal_hash_rate: 2000.0,
            max_target: [0xFFu8; 32].into(),
        };

        // Test that the message can be handled
        let mining_message = MiningOwned::UpdateChannel(update_channel);

        // Verify the message structure
        match mining_message {
            MiningOwned::UpdateChannel(msg) => {
                assert_eq!(msg.channel_id, 1);
                assert_eq!(msg.nominal_hash_rate, 2000.0);
            }
            _ => panic!("Expected UpdateChannel"),
        }
    }

    #[tokio::test]
    async fn test_aggregated_update_channel_without_open_channel_is_not_forwarded() {
        let (upstream_sender, upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver_for_manager) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver) = unbounded();
        let (sv1_server_sender_for_test, sv1_server_receiver) = unbounded();

        let manager = std::sync::Arc::new(ChannelManager::new(
            upstream_sender,
            upstream_receiver_for_manager,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::Aggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        ));

        let update_channel = UpdateChannelOwned {
            channel_id: 0,
            nominal_hash_rate: 0.0,
            max_target: [0xFFu8; 32].into(),
        };

        sv1_server_sender_for_test
            .send((MiningOwned::UpdateChannel(update_channel), None))
            .await
            .unwrap();

        manager.clone().handle_downstream_message().await.unwrap();

        // The pre-open UpdateChannel must be dropped before reaching upstream.
        assert!(upstream_receiver.try_recv().is_err());
    }

    #[test]
    fn test_channel_manager_debug() {
        let manager = create_test_channel_manager();

        // Test that Debug trait is implemented
        let debug_str = format!("{manager:?}");
        assert!(debug_str.contains("ChannelManager"));
    }

    #[test]
    fn test_channel_manager_data_access() {
        let manager = create_test_channel_manager();
        // Test that we can access and modify channel manager data
        manager.pending_downstream_channels.insert(
            1,
            pending_request("test", 100.0, 4, Target::from_le_bytes([0xff; 32])),
        );
        let has_pending = manager.pending_downstream_channels.contains_key(&1);

        assert!(has_pending);
    }

    #[tokio::test]
    async fn job_without_required_version_rolling_triggers_fallback() {
        let mut manager = create_test_channel_manager();
        let job = NewExtendedMiningJobOwned {
            channel_id: 1,
            job_id: 1,
            ntime_start: Sv2OptionOwned::new(None),
            version: 0x20000000,
            version_rolling_allowed: false,
            merkle_path: Seq0255Owned::new(vec![]).unwrap(),
            coinbase_tx_prefix: vec![].try_into().unwrap(),
            coinbase_tx_suffix: vec![].try_into().unwrap(),
        };

        let error = manager
            .handle_new_extended_mining_job(None, job, None)
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::VersionRollingNotAllowed
        ));
    }

    #[tokio::test]
    async fn non_aggregated_upstream_close_is_forwarded_to_sv1_server() {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let mut manager = ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::NonAggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        manager.extended_channels.insert(
            42,
            ExtendedChannel::new(
                42,
                "miner".to_string(),
                ExtranoncePrefix::from_wire(vec![0; 4]).unwrap(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                8,
                None,
            )
            .unwrap(),
        );
        let close = CloseChannelOwned {
            channel_id: 42,
            reason_code: Str0255Owned::try_from("upstream closed channel".to_string()).unwrap(),
        };

        manager
            .handle_close_channel(None, close, None)
            .await
            .unwrap();

        assert!(!manager.extended_channels.contains_key(&42));
        assert!(matches!(
            sv1_server_receiver_for_test.try_recv(),
            Ok(MiningOwned::CloseChannel(close)) if close.channel_id == 42
        ));
    }

    /// Channel endpoints a test uses to feed a channel manager and observe what it sends.
    struct TestIo {
        downstream_sender: Sender<(MiningOwned, Option<String>)>,
        upstream_receiver: Receiver<OutboundFrame>,
        sv1_server_receiver: Receiver<MiningOwned>,
    }

    fn channel_manager_with_test_io(mode: TproxyMode) -> (ChannelManager, TestIo) {
        let (upstream_sender, upstream_receiver) = unbounded();
        let (_upstream_inbound_sender, upstream_inbound_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver) = unbounded();
        let (downstream_sender, downstream_receiver) = unbounded();
        let manager = ChannelManager::new(
            upstream_sender,
            upstream_inbound_receiver,
            sv1_server_sender,
            downstream_receiver,
            vec![],
            vec![],
            mode,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        (
            manager,
            TestIo {
                downstream_sender,
                upstream_receiver,
                sv1_server_receiver,
            },
        )
    }

    fn test_extended_channel(channel_id: ChannelId) -> ExtendedChannel {
        ExtendedChannel::new(
            channel_id,
            "miner".to_string(),
            ExtranoncePrefix::from_wire(vec![0; 4]).unwrap(),
            Target::from_le_bytes([0xff; 32]),
            1.0,
            true,
            8,
            None,
        )
        .unwrap()
    }

    fn close_channel(channel_id: ChannelId) -> CloseChannelOwned {
        CloseChannelOwned {
            channel_id,
            reason_code: Str0255Owned::try_from("closed".to_string()).unwrap(),
        }
    }

    #[tokio::test]
    async fn upstream_close_removes_share_sequence_counters_of_closed_channels() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        let mut group = GroupChannel::new(100);
        for channel_id in [7, 8, 9] {
            manager
                .extended_channels
                .insert(channel_id, test_extended_channel(channel_id));
            manager.next_share_sequence_number(channel_id);
        }
        for channel_id in [8, 9] {
            group.add_channel_id(channel_id, 12).unwrap();
        }
        manager.group_channels.insert(100, group);

        // One close addressed to a channel, one addressed to the group of the other two.
        for close in [close_channel(7), close_channel(100)] {
            manager
                .handle_close_channel(None, close, None)
                .await
                .unwrap();
        }

        let mut forwarded = Vec::new();
        while let Ok(MiningOwned::CloseChannel(close)) = io.sv1_server_receiver.try_recv() {
            forwarded.push(close.channel_id);
        }
        forwarded.sort();
        assert_eq!(forwarded, vec![7, 8, 9]);
        for channel_id in [7, 8, 9] {
            assert!(!manager.share_sequence_counters.contains_key(&channel_id));
        }

        // Closing an already closed channel is only logged.
        let repeated = manager
            .handle_close_channel(None, close_channel(7), None)
            .await
            .unwrap_err();
        assert!(matches!(repeated.action, Action::Log));

        // A reused channel ID starts a fresh sequence.
        assert_eq!(manager.next_share_sequence_number(7), 1);
    }

    #[tokio::test]
    async fn non_aggregated_downstream_close_removes_the_share_sequence_counter() {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        manager
            .extended_channels
            .insert(7, test_extended_channel(7));
        manager.next_share_sequence_number(7);
        let manager = Arc::new(manager);

        io.downstream_sender
            .send((MiningOwned::CloseChannel(close_channel(7)), None))
            .await
            .unwrap();
        manager.clone().handle_downstream_message().await.unwrap();

        assert!(io.upstream_receiver.try_recv().is_ok());
        assert!(!manager.share_sequence_counters.contains_key(&7));
        assert_eq!(manager.next_share_sequence_number(7), 1);
    }

    #[tokio::test]
    async fn aggregated_downstream_close_preserves_the_shared_share_sequence_counter() {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::Aggregated);
        // The shared upstream channel's ID equals the local ID of the channel being closed.
        manager
            .extended_channels
            .insert(AGGREGATED_CHANNEL_ID, test_extended_channel(42));
        manager
            .extended_channels
            .insert(42, test_extended_channel(42));
        manager.next_share_sequence_number(42);
        let manager = Arc::new(manager);

        io.downstream_sender
            .send((MiningOwned::CloseChannel(close_channel(42)), None))
            .await
            .unwrap();
        manager.clone().handle_downstream_message().await.unwrap();

        assert!(!manager.extended_channels.contains_key(&42));
        assert_eq!(manager.next_share_sequence_number(42), 2);
    }

    #[tokio::test]
    async fn upstream_close_of_the_last_group_member_removes_the_group() {
        let (mut manager, _io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        let mut group = GroupChannel::new(100);
        for channel_id in [8, 9] {
            manager
                .extended_channels
                .insert(channel_id, test_extended_channel(channel_id));
            group.add_channel_id(channel_id, 12).unwrap();
        }
        manager.group_channels.insert(100, group);

        manager
            .handle_close_channel(None, close_channel(8), None)
            .await
            .unwrap();
        // The group still has a live member.
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(9))
                .unwrap()
        );

        manager
            .handle_close_channel(None, close_channel(9), None)
            .await
            .unwrap();
        assert!(!manager.group_channels.contains_key(&100));
    }

    #[tokio::test]
    async fn non_aggregated_downstream_close_of_the_last_group_member_removes_the_group() {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        let mut group = GroupChannel::new(100);
        for channel_id in [8, 9] {
            manager
                .extended_channels
                .insert(channel_id, test_extended_channel(channel_id));
            group.add_channel_id(channel_id, 12).unwrap();
        }
        manager.group_channels.insert(100, group);
        let manager = Arc::new(manager);

        io.downstream_sender
            .send((MiningOwned::CloseChannel(close_channel(8)), None))
            .await
            .unwrap();
        manager.clone().handle_downstream_message().await.unwrap();
        // The group still has a live member.
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(9))
                .unwrap()
        );

        io.downstream_sender
            .send((MiningOwned::CloseChannel(close_channel(9)), None))
            .await
            .unwrap();
        manager.clone().handle_downstream_message().await.unwrap();
        assert!(!manager.group_channels.contains_key(&100));
    }

    #[tokio::test]
    async fn aggregated_downstream_close_keeps_the_upstream_channel_in_its_group() {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::Aggregated);
        // The shared upstream channel's ID equals the local ID of the channel being closed.
        manager
            .extended_channels
            .insert(AGGREGATED_CHANNEL_ID, test_extended_channel(42));
        manager
            .extended_channels
            .insert(42, test_extended_channel(42));
        let mut group = GroupChannel::new(100);
        group.add_channel_id(42, 12).unwrap();
        manager.group_channels.insert(100, group);
        let manager = Arc::new(manager);

        io.downstream_sender
            .send((MiningOwned::CloseChannel(close_channel(42)), None))
            .await
            .unwrap();
        manager.clone().handle_downstream_message().await.unwrap();

        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(42))
                .unwrap()
        );
    }

    fn drain<T>(receiver: &Receiver<T>) {
        while receiver.try_recv().is_ok() {}
    }

    fn open_channel_request(request_id: u32, min_extranonce_size: u16) -> MiningOwned {
        MiningOwned::OpenExtendedMiningChannel(OpenExtendedMiningChannelOwned {
            request_id,
            user_identity: "miner".try_into().unwrap(),
            nominal_hash_rate: 1.0,
            max_target: [0xff; 32].into(),
            min_extranonce_size,
        })
    }

    #[tokio::test]
    async fn non_aggregated_channel_churn_leaves_no_channel_state_behind() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        // Clones share the same maps; the downstream handler needs an `Arc`.
        let downstream_handler = Arc::new(manager.clone());

        for cycle in 0..32 {
            let request_id = cycle + 1;
            // Like SRI Pool, this upstream never reuses channel IDs. It also hands out a new
            // group ID per channel, the worst case for group retention.
            let channel_id = 1_000 + cycle;
            let group_channel_id = 2_000 + cycle;

            io.downstream_sender
                .send((open_channel_request(request_id, 4), None))
                .await
                .unwrap();
            downstream_handler
                .clone()
                .handle_downstream_message()
                .await
                .unwrap();
            manager
                .handle_open_extended_mining_channel_success(
                    None,
                    OpenExtendedMiningChannelSuccessOwned {
                        request_id,
                        channel_id,
                        target: [0xff; 32].into(),
                        extranonce_size: 4,
                        extranonce_prefix: vec![0xaa; 4].try_into().unwrap(),
                        group_channel_id,
                    },
                    None,
                )
                .await
                .unwrap();
            manager.next_share_sequence_number(channel_id);

            // Alternate the downstream and upstream close directions.
            if cycle % 2 == 0 {
                io.downstream_sender
                    .send((MiningOwned::CloseChannel(close_channel(channel_id)), None))
                    .await
                    .unwrap();
                downstream_handler
                    .clone()
                    .handle_downstream_message()
                    .await
                    .unwrap();
            } else {
                manager
                    .handle_close_channel(None, close_channel(channel_id), None)
                    .await
                    .unwrap();
            }
            drain(&io.upstream_receiver);
            drain(&io.sv1_server_receiver);
        }

        assert!(manager.pending_downstream_channels.is_empty());
        assert!(manager.extended_channels.is_empty());
        assert!(manager.sv1_advertised_extranonce_prefixes.is_empty());
        assert!(manager.group_channels.is_empty());
        assert!(manager.share_sequence_counters.is_empty());
        assert!(manager.upstream_max_targets.is_empty());
    }

    #[tokio::test]
    async fn aggregated_channel_churn_only_keeps_the_shared_upstream_state() {
        let (manager, io) = channel_manager_with_test_io(TproxyMode::Aggregated);
        connect_aggregated_channel(&manager);
        let mut group = GroupChannel::new(100);
        group.add_channel_id(42, 12).unwrap();
        manager.group_channels.insert(100, group);
        manager.upstream_max_targets.insert(
            AGGREGATED_CHANNEL_ID,
            UpstreamMaxTarget::new(target_from_byte(100)),
        );
        let manager = Arc::new(manager);

        for cycle in 0..32 {
            io.downstream_sender
                .send((open_channel_request(cycle + 1, 6), None))
                .await
                .unwrap();
            manager.clone().handle_downstream_message().await.unwrap();
            let Ok(MiningOwned::OpenExtendedMiningChannelSuccess(success)) =
                io.sv1_server_receiver.try_recv()
            else {
                panic!("expected the downstream channel to open");
            };
            manager.next_share_sequence_number(42);

            io.downstream_sender
                .send((
                    MiningOwned::CloseChannel(close_channel(success.channel_id)),
                    None,
                ))
                .await
                .unwrap();
            manager.clone().handle_downstream_message().await.unwrap();
            drain(&io.upstream_receiver);
            drain(&io.sv1_server_receiver);
        }

        assert!(manager.pending_downstream_channels.is_empty());
        assert_eq!(manager.extended_channels.len(), 1);
        assert!(
            manager
                .extended_channels
                .contains_key(&AGGREGATED_CHANNEL_ID)
        );
        assert!(manager.sv1_advertised_extranonce_prefixes.is_empty());
        assert_eq!(
            manager
                .aggregated_extranonce_allocator
                .with(|allocator| allocator.as_ref().unwrap().allocated_count())
                .unwrap(),
            0
        );
        assert!(
            manager
                .group_channels
                .with(&100, |group| group.has_channel_id(42))
                .unwrap()
        );
        assert_eq!(manager.group_channels.len(), 1);
        assert_eq!(manager.share_sequence_counters.len(), 1);
        assert_eq!(manager.next_share_sequence_number(42), 33);
        assert_eq!(manager.upstream_max_targets.len(), 1);
        assert!(
            manager
                .upstream_max_targets
                .contains_key(&AGGREGATED_CHANNEL_ID)
        );
    }

    /// A target built from its most significant byte; a larger value is easier.
    fn target_from_byte(value: u8) -> Target {
        let mut bytes = [0; 32];
        bytes[31] = value;
        Target::from_le_bytes(bytes)
    }

    fn set_target(channel_id: ChannelId, target: Target) -> SetTargetOwned {
        SetTargetOwned {
            channel_id,
            target: target.to_le_bytes().into(),
        }
    }

    fn update_channel(channel_id: ChannelId, max_target: Target) -> MiningOwned {
        MiningOwned::UpdateChannel(UpdateChannelOwned {
            channel_id,
            nominal_hash_rate: 1.0,
            max_target: max_target.to_le_bytes().into(),
        })
    }

    /// Opens a non-aggregated channel whose open request carried `max_target`.
    async fn open_channel_with_max_target(
        manager: &mut ChannelManager,
        request_id: u32,
        channel_id: ChannelId,
        group_channel_id: ChannelId,
        max_target: Target,
    ) {
        manager.pending_downstream_channels.insert(
            request_id as DownstreamId,
            pending_request("miner", 1.0, 4, max_target),
        );
        manager
            .handle_open_extended_mining_channel_success(
                None,
                OpenExtendedMiningChannelSuccessOwned {
                    request_id,
                    channel_id,
                    target: max_target.to_le_bytes().into(),
                    extranonce_size: 4,
                    extranonce_prefix: vec![0xaa; 4].try_into().unwrap(),
                    group_channel_id,
                },
                None,
            )
            .await
            .unwrap();
    }

    /// Decodes the `UpdateChannel` messages sent upstream so far as `(channel_id, max_target)`.
    fn sent_update_channels(upstream: &Receiver<OutboundFrame>) -> Vec<(ChannelId, Target)> {
        use stratum_apps::stratum_core::{
            codec_sv2::EncodableFrame as _,
            parsers_sv2::{AnyMessage, Mining},
        };

        let mut updates = Vec::new();
        while let Ok(frame) = upstream.try_recv() {
            let mut encoded = vec![0; frame.encoded_length()];
            frame.encode_into(&mut encoded).unwrap();
            let mut frame = InboundFrame::from_bytes(encoded.into()).unwrap();
            if let AnyMessage::Mining(Mining::UpdateChannel(update)) =
                AnyMessage::try_from((frame.header(), frame.payload())).unwrap()
            {
                updates.push((
                    update.channel_id,
                    Target::from_le_bytes(*update.max_target.as_array()),
                ));
            }
        }
        updates
    }

    fn assert_max_target_violation(error: TproxyError<error::ChannelManager>) {
        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::SetTargetAboveMaxTarget
        ));
    }

    #[tokio::test]
    async fn set_target_above_the_requested_max_target_triggers_fallback() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        open_channel_with_max_target(&mut manager, 1, 7, 100, target_from_byte(100)).await;
        drain(&io.sv1_server_receiver);

        let error = manager
            .handle_set_target(None, set_target(7, target_from_byte(101)), None)
            .await
            .unwrap_err();
        assert_max_target_violation(error);
        // Nothing was applied or forwarded.
        assert_eq!(
            manager
                .extended_channels
                .with(&7, |channel| *channel.get_target()),
            Some(target_from_byte(100))
        );
        assert!(io.sv1_server_receiver.try_recv().is_err());

        manager
            .handle_set_target(None, set_target(7, target_from_byte(90)), None)
            .await
            .unwrap();
        assert!(io.sv1_server_receiver.try_recv().is_ok());
    }

    #[tokio::test]
    async fn group_set_target_is_bound_by_every_member() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        open_channel_with_max_target(&mut manager, 1, 7, 100, target_from_byte(100)).await;
        open_channel_with_max_target(&mut manager, 2, 8, 100, target_from_byte(50)).await;
        drain(&io.sv1_server_receiver);

        let error = manager
            .handle_set_target(None, set_target(100, target_from_byte(80)), None)
            .await
            .unwrap_err();
        assert_max_target_violation(error);

        manager
            .handle_set_target(None, set_target(100, target_from_byte(50)), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn aggregated_set_target_is_bound_by_the_aggregated_channel() {
        let (mut manager, _sv1_server_receiver) = create_connected_aggregated_channel_manager();
        let mut group = GroupChannel::new(100);
        group.add_channel_id(42, 12).unwrap();
        manager.group_channels.insert(100, group);
        manager.upstream_max_targets.insert(
            AGGREGATED_CHANNEL_ID,
            UpstreamMaxTarget::new(target_from_byte(100)),
        );

        for addressed_to in [42, 100] {
            let error = manager
                .handle_set_target(None, set_target(addressed_to, target_from_byte(101)), None)
                .await
                .unwrap_err();
            assert_max_target_violation(error);
        }
    }

    /// Sends `UpdateChannel`s from the SV1 server side, as vardiff would.
    async fn request_max_targets(
        manager: &ChannelManager,
        io: &TestIo,
        channel_id: ChannelId,
        max_targets: &[u8],
    ) {
        let downstream_handler = Arc::new(manager.clone());
        for &max_target in max_targets {
            io.downstream_sender
                .send((
                    update_channel(channel_id, target_from_byte(max_target)),
                    None,
                ))
                .await
                .unwrap();
            downstream_handler
                .clone()
                .handle_downstream_message()
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn max_target_changes_are_sent_right_away() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        open_channel_with_max_target(&mut manager, 1, 7, 100, target_from_byte(100)).await;

        request_max_targets(&manager, &io, 7, &[50, 30]).await;
        assert_eq!(
            sent_update_channels(&io.upstream_receiver),
            vec![(7, target_from_byte(50)), (7, target_from_byte(30))]
        );

        // The upstream may not have processed either change yet, so any target within the
        // channel's previous value is still allowed.
        for target in [40, 80, 100] {
            manager
                .handle_set_target(None, set_target(7, target_from_byte(target)), None)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_processed_max_target_change_bounds_set_target() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        open_channel_with_max_target(&mut manager, 1, 7, 100, target_from_byte(100)).await;
        drain(&io.sv1_server_receiver);
        // Every change is processed as soon as it is sent.
        manager.max_target_grace_period = Duration::ZERO;

        request_max_targets(&manager, &io, 7, &[50]).await;

        let error = manager
            .handle_set_target(None, set_target(7, target_from_byte(80)), None)
            .await
            .unwrap_err();
        assert_max_target_violation(error);
        assert!(io.sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_rejected_max_target_change_keeps_the_previous_bound() {
        let (mut manager, io) = channel_manager_with_test_io(TproxyMode::NonAggregated);
        open_channel_with_max_target(&mut manager, 1, 7, 100, target_from_byte(50)).await;

        request_max_targets(&manager, &io, 7, &[80]).await;
        // The upstream may already have accepted 80.
        manager
            .handle_set_target(None, set_target(7, target_from_byte(70)), None)
            .await
            .unwrap();

        manager
            .handle_update_channel_error(
                None,
                UpdateChannelErrorOwned {
                    channel_id: 7,
                    error_code: "max-target-out-of-range".to_string().try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap();
        let error = manager
            .handle_set_target(None, set_target(7, target_from_byte(70)), None)
            .await
            .unwrap_err();
        assert_max_target_violation(error);
    }

    #[tokio::test]
    async fn active_channel_without_advertised_prefix_requests_shutdown() {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let manager = ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::NonAggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        let mut job = test_extended_job(42, 1);
        job.ntime_start = Sv2OptionOwned::new(Some(0));
        let mut channel = ExtendedChannel::new(
            42,
            "miner".to_string(),
            ExtranoncePrefix::from_wire(vec![0; 4]).unwrap(),
            Target::from_le_bytes([0xff; 32]),
            1.0,
            true,
            8,
            None,
        )
        .unwrap();
        channel.on_new_extended_mining_job(job.clone()).unwrap();
        manager.extended_channels.insert(42, channel);

        let error = manager.forward_job_to_sv1_server(job).await.unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelNotFound));
    }

    #[tokio::test]
    async fn aggregated_extranonce_prefix_change_preserves_channel_allocations() {
        let (mut manager, sv1_server_receiver) = create_connected_aggregated_channel_manager();
        let (first_prefix, second_prefix) = manager
            .aggregated_extranonce_allocator
            .with(|allocator| {
                let allocator = allocator.as_mut().unwrap();
                (
                    allocator.allocate_extended(6).unwrap(),
                    allocator.allocate_extended(6).unwrap(),
                )
            })
            .unwrap();
        let first_local_prefix_and_index = first_prefix.as_bytes()[4..].to_vec();
        let second_local_prefix_and_index = second_prefix.as_bytes()[4..].to_vec();
        for (channel_id, prefix) in [(7, first_prefix), (8, second_prefix)] {
            manager.extended_channels.insert(
                channel_id,
                ExtendedChannel::new(
                    channel_id,
                    "miner".to_string(),
                    prefix.into(),
                    Target::from_le_bytes([0xff; 32]),
                    1.0,
                    true,
                    6,
                    None,
                )
                .unwrap(),
            );
        }
        assert_eq!(
            manager
                .aggregated_extranonce_allocator
                .with(|allocator| allocator.as_ref().unwrap().allocated_count())
                .unwrap(),
            2
        );
        let set_prefix = SetExtranoncePrefixOwned {
            channel_id: 42,
            extranonce_prefix: vec![1, 2, 3].try_into().unwrap(),
        };

        manager
            .handle_set_extranonce_prefix(None, set_prefix, None)
            .await
            .unwrap();

        assert_eq!(
            manager
                .extended_channels
                .with(&AGGREGATED_CHANNEL_ID, |channel| channel
                    .get_extranonce_prefix()
                    .to_vec()),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            manager
                .aggregated_extranonce_allocator
                .with(|allocator| allocator.as_ref().unwrap().upstream_prefix().to_vec())
                .unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            manager
                .extended_channels
                .with(&7, |channel| channel.get_extranonce_prefix().to_vec()),
            Some([vec![1, 2, 3], first_local_prefix_and_index].concat())
        );
        assert_eq!(
            manager
                .extended_channels
                .with(&8, |channel| channel.get_extranonce_prefix().to_vec()),
            Some([vec![1, 2, 3], second_local_prefix_and_index].concat())
        );
        assert_eq!(
            manager
                .aggregated_extranonce_allocator
                .with(|allocator| allocator.as_ref().unwrap().allocated_count())
                .unwrap(),
            2
        );
        assert!(sv1_server_receiver.try_recv().is_err());
        assert_eq!(
            manager.aggregated_channel_state.get(),
            AggregatedState::Connected
        );
    }

    #[tokio::test]
    async fn aggregated_extranonce_prefix_change_rejects_oversized_full_extranonce() {
        let (mut manager, _sv1_server_receiver) = create_connected_aggregated_channel_manager();
        let set_prefix = SetExtranoncePrefixOwned {
            channel_id: 42,
            extranonce_prefix: vec![1; 32].try_into().unwrap(),
        };

        let error = manager
            .handle_set_extranonce_prefix(None, set_prefix, None)
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::InvalidExtranonceSize {
                prefix_len: 32,
                rollable_size: 8,
            }
        ));
    }

    #[tokio::test]
    async fn non_aggregated_extranonce_prefix_change_updates_affected_channel() {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let mut manager = ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::NonAggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        manager.extended_channels.insert(
            42,
            ExtendedChannel::new(
                42,
                "miner".to_string(),
                ExtranoncePrefix::from_wire(vec![0; 4]).unwrap(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                8,
                None,
            )
            .unwrap(),
        );
        let set_prefix = SetExtranoncePrefixOwned {
            channel_id: 42,
            extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
        };

        manager
            .handle_set_extranonce_prefix(None, set_prefix, None)
            .await
            .unwrap();

        assert_eq!(
            manager
                .extended_channels
                .with(&42, |channel| channel.get_extranonce_prefix().to_vec()),
            Some(vec![1, 2, 3, 4])
        );
        assert!(sv1_server_receiver_for_test.try_recv().is_err());
    }

    #[tokio::test]
    async fn aggregated_prefix_change_is_advertised_with_the_first_matching_job() {
        let (mut manager, sv1_server_receiver) = create_connected_aggregated_channel_manager();
        manager.set_expected_payout_distribution(None);
        let mut group = GroupChannel::new(100);
        group.add_channel_id(42, 12).unwrap();
        manager.group_channels.insert(100, group);

        let (first_prefix, second_prefix) = manager
            .aggregated_extranonce_allocator
            .with(|allocator| {
                let allocator = allocator.as_mut().unwrap();
                (
                    allocator.allocate_extended(6).unwrap(),
                    allocator.allocate_extended(6).unwrap(),
                )
            })
            .unwrap();
        for (channel_id, prefix) in [(7, first_prefix), (8, second_prefix)] {
            let prefix_bytes = prefix.as_bytes().to_vec();
            manager.extended_channels.insert(
                channel_id,
                ExtendedChannel::new(
                    channel_id,
                    "miner".to_string(),
                    prefix.into(),
                    Target::from_le_bytes([0xff; 32]),
                    1.0,
                    true,
                    6,
                    None,
                )
                .unwrap(),
            );
            manager
                .sv1_advertised_extranonce_prefixes
                .insert(channel_id, prefix_bytes);
        }

        // The future job captures the old prefixes before the upstream changes them.
        manager
            .handle_new_extended_mining_job(None, test_extended_job(42, 1), None)
            .await
            .unwrap();
        manager
            .handle_set_extranonce_prefix(
                None,
                SetExtranoncePrefixOwned {
                    channel_id: 42,
                    extranonce_prefix: vec![1, 2, 3].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(sv1_server_receiver.try_recv().is_err());

        // A new immediate job can arrive while the old future job remains queued. Its new
        // per-downstream prefixes must be advertised before the broadcast job.
        let mut new_prefix_job = test_extended_job(42, 2);
        new_prefix_job.ntime_start = Sv2OptionOwned::new(Some(0));
        manager
            .handle_new_extended_mining_job(None, new_prefix_job, None)
            .await
            .unwrap();

        let mut updates = vec![
            sv1_server_receiver.try_recv().unwrap(),
            sv1_server_receiver.try_recv().unwrap(),
        ];
        updates.sort_by_key(|message| match message {
            MiningOwned::SetExtranoncePrefix(message) => message.channel_id,
            other => panic!("expected SetExtranoncePrefix, got {other:?}"),
        });
        for (message, expected_channel_id) in updates.into_iter().zip([7, 8]) {
            assert!(matches!(
                message,
                MiningOwned::SetExtranoncePrefix(message)
                    if message.channel_id == expected_channel_id
                        && message.extranonce_prefix.to_owned_bytes()
                            == manager.extended_channels.with(&expected_channel_id, |channel|
                                channel.get_active_job().unwrap().extranonce_prefix.clone()).unwrap()
            ));
        }
        assert!(matches!(
            sv1_server_receiver.try_recv().unwrap(),
            MiningOwned::NewExtendedMiningJob(job) if job.job_id == 2
        ));
        assert!(sv1_server_receiver.try_recv().is_err());

        // If the old future job is subsequently activated, switch each miner back to the old
        // prefix captured by that job before broadcasting it.
        manager
            .handle_set_new_prev_hash(
                None,
                SetNewPrevHashOwned {
                    channel_id: 42,
                    job_id: 1,
                    prev_hash: vec![0; 32].try_into().unwrap(),
                    ntime_start: 0,
                    nbits: 0x207fffff,
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            sv1_server_receiver.try_recv().unwrap(),
            MiningOwned::SetNewPrevHash(_)
        ));
        let mut old_prefix_updates = vec![
            sv1_server_receiver.try_recv().unwrap(),
            sv1_server_receiver.try_recv().unwrap(),
        ];
        old_prefix_updates.sort_by_key(|message| match message {
            MiningOwned::SetExtranoncePrefix(message) => message.channel_id,
            other => panic!("expected SetExtranoncePrefix, got {other:?}"),
        });
        for (message, expected_channel_id) in old_prefix_updates.into_iter().zip([7, 8]) {
            assert!(matches!(
                message,
                MiningOwned::SetExtranoncePrefix(message)
                    if message.channel_id == expected_channel_id
                        && message.extranonce_prefix.to_owned_bytes()
                            == manager.extended_channels.with(&expected_channel_id, |channel|
                                channel.get_active_job().unwrap().extranonce_prefix.clone()).unwrap()
            ));
        }
        assert!(matches!(
            sv1_server_receiver.try_recv().unwrap(),
            MiningOwned::NewExtendedMiningJob(job) if job.job_id == 1
        ));
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_aggregated_prefix_change_is_advertised_with_the_first_matching_job() {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let (sv1_server_sender, sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let mut manager = ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::NonAggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        manager.set_expected_payout_distribution(None);
        let old_prefix = vec![0; 4];
        manager.extended_channels.insert(
            42,
            ExtendedChannel::new(
                42,
                "miner".to_string(),
                ExtranoncePrefix::from_wire(old_prefix.clone()).unwrap(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                8,
                None,
            )
            .unwrap(),
        );
        manager
            .sv1_advertised_extranonce_prefixes
            .insert(42, old_prefix);

        manager
            .handle_new_extended_mining_job(None, test_extended_job(42, 1), None)
            .await
            .unwrap();
        manager
            .handle_set_extranonce_prefix(
                None,
                SetExtranoncePrefixOwned {
                    channel_id: 42,
                    extranonce_prefix: vec![1, 2, 3, 4].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap();
        assert!(sv1_server_receiver_for_test.try_recv().is_err());

        manager
            .handle_set_new_prev_hash(
                None,
                SetNewPrevHashOwned {
                    channel_id: 42,
                    job_id: 1,
                    prev_hash: vec![0; 32].try_into().unwrap(),
                    ntime_start: 0,
                    nbits: 0x207fffff,
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            sv1_server_receiver_for_test.try_recv().unwrap(),
            MiningOwned::SetNewPrevHash(_)
        ));
        assert!(matches!(
            sv1_server_receiver_for_test.try_recv().unwrap(),
            MiningOwned::NewExtendedMiningJob(job) if job.job_id == 1
        ));

        let mut new_prefix_job = test_extended_job(42, 2);
        new_prefix_job.ntime_start = Sv2OptionOwned::new(Some(0));
        manager
            .handle_new_extended_mining_job(None, new_prefix_job, None)
            .await
            .unwrap();
        assert!(matches!(
            sv1_server_receiver_for_test.try_recv().unwrap(),
            MiningOwned::SetExtranoncePrefix(message)
                if message.channel_id == 42
                    && message.extranonce_prefix.to_owned_bytes() == vec![1, 2, 3, 4]
        ));
        assert!(matches!(
            sv1_server_receiver_for_test.try_recv().unwrap(),
            MiningOwned::NewExtendedMiningJob(job) if job.job_id == 2
        ));
        assert!(sv1_server_receiver_for_test.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_aggregated_prefix_change_counts_local_prefix_and_index() {
        let (upstream_sender, _upstream_receiver) = unbounded();
        let (_upstream_sender, upstream_receiver) = unbounded();
        let (sv1_server_sender, _sv1_server_receiver_for_test) = unbounded();
        let (_sv1_server_sender, sv1_server_receiver) = unbounded();
        let mut manager = ChannelManager::new(
            upstream_sender,
            upstream_receiver,
            sv1_server_sender,
            sv1_server_receiver,
            vec![],
            vec![],
            TproxyMode::NonAggregated,
            None,
            #[cfg(feature = "monitoring")]
            true,
        );
        let mut allocator =
            ExtranonceAllocator::from_upstream_prefix(vec![0; 4], vec![0; 23], 32, 1).unwrap();
        let allocated_prefix = allocator.allocate_extended(4).unwrap();
        let original_prefix = allocated_prefix.as_bytes().to_vec();
        assert_eq!(original_prefix.len(), 28);
        manager.extended_channels.insert(
            42,
            ExtendedChannel::new(
                42,
                "miner".to_string(),
                allocated_prefix.into(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                4,
                None,
            )
            .unwrap(),
        );

        let error = manager
            .handle_set_extranonce_prefix(
                None,
                SetExtranoncePrefixOwned {
                    channel_id: 42,
                    extranonce_prefix: vec![1; 5].try_into().unwrap(),
                },
                None,
            )
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Fallback));
        assert!(matches!(
            error.kind,
            TproxyErrorKind::UpstreamExtranoncePrefixUpdateFailed {
                channel_id: 42,
                error: stratum_apps::stratum_core::channels_sv2::client::error::ExtendedChannelError::NewExtranoncePrefixTooLarge,
            }
        ));
        assert_eq!(
            manager
                .extended_channels
                .with(&42, |channel| channel.get_extranonce_prefix().to_vec()),
            Some(original_prefix)
        );
        assert_eq!(
            manager
                .extended_channels
                .with(&42, |channel| channel.upstream_prefix_len()),
            Some(4)
        );
    }

    #[tokio::test]
    async fn aggregated_channel_capacity_exhaustion_rejects_request() {
        let (manager, sv1_server_receiver) = create_connected_aggregated_channel_manager();
        let mut allocator =
            ExtranonceAllocator::from_upstream_prefix(vec![0; 4], vec![0; 1], 12, 1).unwrap();
        let occupied_prefix = allocator.allocate_extended(6).unwrap();
        manager
            .aggregated_extranonce_allocator
            .set(Some(allocator))
            .unwrap();

        manager
            .handle_downstream_channel_request_in_aggregated_mode(
                7,
                "rejected-miner".to_string(),
                1.0,
                6,
            )
            .await
            .unwrap();

        let error = match sv1_server_receiver.try_recv().unwrap() {
            MiningOwned::OpenMiningChannelError(error) => error,
            other => panic!("expected open error, got {other:?}"),
        };
        assert_eq!(error.request_id, 7);
        assert_eq!(
            error.error_code.as_utf8_or_hex(),
            ERROR_CODE_OPEN_MINING_CHANNEL_CHANNEL_CAPACITY_EXHAUSTED
        );
        assert!(sv1_server_receiver.try_recv().is_err());

        drop(occupied_prefix);
    }

    #[tokio::test]
    async fn connected_aggregate_without_shared_channel_requests_shutdown() {
        let (manager, _sv1_server_receiver) = create_connected_aggregated_channel_manager();
        manager.extended_channels.remove(&AGGREGATED_CHANNEL_ID);

        let error = manager
            .handle_downstream_channel_request_in_aggregated_mode(
                7,
                "new-miner".to_string(),
                1.0,
                6,
            )
            .await
            .unwrap_err();

        assert!(matches!(error.action, Action::Shutdown));
        assert!(matches!(error.kind, TproxyErrorKind::ChannelNotFound));
    }

    #[tokio::test]
    async fn aggregated_channel_id_allocation_does_not_use_reserved_id() {
        let (manager, sv1_server_receiver) = create_connected_aggregated_channel_manager();
        manager.extended_channels.insert(
            AGGREGATED_CHANNEL_ID - 1,
            ExtendedChannel::new(
                AGGREGATED_CHANNEL_ID - 1,
                "last-channel".to_string(),
                ExtranoncePrefix::from_wire(vec![0xaa; 6]).unwrap(),
                Target::from_le_bytes([0xff; 32]),
                1.0,
                true,
                6,
                None,
            )
            .unwrap(),
        );

        manager
            .handle_downstream_channel_request_in_aggregated_mode(
                7,
                "new-miner".to_string(),
                1.0,
                6,
            )
            .await
            .unwrap();

        let success = match sv1_server_receiver.try_recv().unwrap() {
            MiningOwned::OpenExtendedMiningChannelSuccess(success) => success,
            other => panic!("expected open success, got {other:?}"),
        };
        assert_ne!(success.channel_id, AGGREGATED_CHANNEL_ID);
        assert_eq!(success.channel_id, 1);
        assert_eq!(
            manager
                .extended_channels
                .with(&AGGREGATED_CHANNEL_ID, |channel| channel.get_channel_id()),
            Some(42)
        );
    }

    #[tokio::test]
    async fn late_aggregated_join_targets_initial_job_to_new_channel_only() {
        let (manager, sv1_server_receiver) = create_connected_aggregated_channel_manager();

        manager
            .extended_channels
            .with_mut(&AGGREGATED_CHANNEL_ID, |aggregated_channel| {
                aggregated_channel
                    .on_new_extended_mining_job(test_extended_job(42, 1))
                    .unwrap();
                aggregated_channel
                    .on_set_new_prev_hash(SetNewPrevHashOwned {
                        channel_id: 42,
                        job_id: 1,
                        prev_hash: vec![0; 32].try_into().unwrap(),
                        ntime_start: 0,
                        nbits: 0x207fffff,
                    })
                    .unwrap();
            })
            .unwrap();

        manager
            .handle_downstream_channel_request_in_aggregated_mode(
                7,
                "new-miner".to_string(),
                1.0,
                6,
            )
            .await
            .unwrap();

        let success = match sv1_server_receiver.try_recv().unwrap() {
            MiningOwned::OpenExtendedMiningChannelSuccess(success) => success,
            other => panic!("expected open success, got {other:?}"),
        };
        let job = match sv1_server_receiver.try_recv().unwrap() {
            MiningOwned::NewExtendedMiningJob(job) => job,
            other => panic!("expected initial job, got {other:?}"),
        };

        assert_eq!(job.channel_id, success.channel_id);
        assert_ne!(job.channel_id, AGGREGATED_CHANNEL_ID);
        assert!(sv1_server_receiver.try_recv().is_err());
    }

    /// Checks the miner's advertised extranonce against child validation and the actual
    /// share frame produced by ChannelManager's upstream submission path.
    async fn assert_late_join_share_reaches_upstream(
        manager: &ChannelManager,
        channel_id: ChannelId,
        advertised_prefix: &[u8],
    ) {
        use stratum_apps::stratum_core::{
            channels_sv2::client::share_accounting::ShareValidationResult,
            parsers_sv2::{AnyMessage, Mining},
        };
        let mut share = manager
            .extended_channels
            .with(&channel_id, |channel| {
                let job = channel.get_active_job().unwrap();
                assert_eq!(job.extranonce_prefix, advertised_prefix);
                SubmitSharesExtendedOwned {
                    channel_id,
                    sequence_number: 0,
                    job_id: job.job_message.job_id,
                    nonce: 0,
                    ntime: *job.job_message.ntime_start.as_ref().unwrap(),
                    version: job.job_message.version,
                    extranonce: vec![0x11; channel.get_rollable_extranonce_size() as usize]
                        .try_into()
                        .unwrap(),
                }
            })
            .unwrap();
        let valid_share = (0..10_000)
            .find_map(|nonce| {
                share.nonce = nonce;
                match manager
                    .extended_channels
                    .with_mut(&channel_id, |channel| channel.validate_share(share.clone()))
                    .unwrap()
                {
                    Ok(ShareValidationResult::Valid(_) | ShareValidationResult::BlockFound(_)) => {
                        Some(share.clone())
                    }
                    Err(ShareValidationError::DoesNotMeetTarget(_)) => None,
                    other => panic!("unexpected validation for job {}: {other:?}", share.job_id),
                }
            })
            .expect("fixture target should be easy to satisfy");
        let (to_manager, from_sv1) = unbounded();
        let (to_upstream, from_manager) = unbounded();
        let mut submission_manager = manager.clone();
        submission_manager.channel_manager_io.sv1_server_receiver = from_sv1;
        submission_manager.channel_manager_io.upstream_sender = to_upstream;
        to_manager
            .send((MiningOwned::SubmitSharesExtended(valid_share.clone()), None))
            .await
            .unwrap();
        Arc::new(submission_manager)
            .handle_downstream_message()
            .await
            .unwrap();
        let frame = from_manager
            .try_recv()
            .expect("validated share must reach upstream");
        use stratum_apps::stratum_core::codec_sv2::EncodableFrame as _;
        let mut encoded = vec![0; frame.encoded_length()];
        frame.encode_into(&mut encoded).unwrap();
        let mut frame = InboundFrame::from_bytes(encoded.into()).unwrap();
        let AnyMessage::Mining(Mining::SubmitSharesExtended(forwarded)) =
            AnyMessage::try_from((frame.header(), frame.payload())).unwrap()
        else {
            panic!("expected upstream share");
        };
        assert_eq!(forwarded.channel_id, 42);
        assert_eq!(forwarded.job_id, valid_share.job_id);
        assert_eq!(forwarded.nonce, valid_share.nonce);
        let upstream_prefix = manager
            .extended_channels
            .with(&AGGREGATED_CHANNEL_ID, |channel| {
                channel.get_active_job().unwrap().extranonce_prefix.clone()
            })
            .unwrap();
        assert_eq!(
            [upstream_prefix, forwarded.extranonce.as_ref().to_vec()].concat(),
            [
                advertised_prefix.to_vec(),
                valid_share.extranonce.to_owned_bytes()
            ]
            .concat()
        );
    }

    #[tokio::test]
    async fn late_aggregated_join_replays_each_jobs_prefix_and_target() {
        let job_with_extranonce_size = |job_id, total_size: usize| {
            let mut job = test_extended_job(42, job_id);
            let mut prefix = job.coinbase_tx_prefix.to_owned_bytes();
            // This fixture's first input has a one-byte scriptSig length at offset 41.
            // Replacement work must encode its own complete extranonce size in that length.
            prefix[41] = (prefix[41] as usize - 12 + total_size) as u8;
            job.coinbase_tx_prefix = prefix.try_into().unwrap();
            job
        };
        for new_prefix_len in [3, 24] {
            let (mut manager, receiver) = create_connected_aggregated_channel_manager();
            manager.set_expected_payout_distribution(None);
            let mut group = GroupChannel::new(100);
            group.add_channel_id(42, 12).unwrap();
            manager.group_channels.insert(100, group);
            let old_target = Target::from_le_bytes([0xff; 32]);
            let future_target = Target::from_le_bytes([0x80; 32]);
            let current_target = Target::from_le_bytes([0x40; 32]);
            manager
                .extended_channels
                .with_mut(&AGGREGATED_CHANNEL_ID, |channel| {
                    channel
                        .on_new_extended_mining_job(test_extended_job(42, 1))
                        .unwrap();
                    channel
                        .on_set_new_prev_hash(SetNewPrevHashOwned {
                            channel_id: 42,
                            job_id: 1,
                            prev_hash: vec![0; 32].try_into().unwrap(),
                            ntime_start: 0,
                            nbits: 0x207fffff,
                        })
                        .unwrap();
                    channel
                        .on_new_extended_mining_job(test_extended_job(42, 2))
                        .unwrap();
                })
                .unwrap();
            manager
                .handle_set_extranonce_prefix(
                    None,
                    SetExtranoncePrefixOwned {
                        channel_id: 42,
                        extranonce_prefix: vec![0xcc; 5].try_into().unwrap(),
                    },
                    None,
                )
                .await
                .unwrap();
            manager
                .extended_channels
                .with_mut(&AGGREGATED_CHANNEL_ID, |channel| {
                    channel.set_target(future_target).unwrap();
                    channel
                        .on_new_extended_mining_job(job_with_extranonce_size(3, 13))
                        .unwrap();
                    channel.set_target(current_target).unwrap();
                })
                .unwrap();
            let current_prefix = vec![0xbb; new_prefix_len];
            manager
                .handle_set_extranonce_prefix(
                    None,
                    SetExtranoncePrefixOwned {
                        channel_id: 42,
                        extranonce_prefix: current_prefix.clone().try_into().unwrap(),
                    },
                    None,
                )
                .await
                .unwrap();

            manager
                .handle_downstream_channel_request_in_aggregated_mode(
                    7,
                    "new-miner".to_string(),
                    1.0,
                    6,
                )
                .await
                .unwrap();
            assert!(!manager.pending_downstream_channels.contains_key(&7));
            let MiningOwned::OpenExtendedMiningChannelSuccess(success) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected immediate open success");
            };
            let advertised = success.extranonce_prefix.to_owned_bytes();
            assert_eq!(&advertised[..4], &[0; 4]);
            let local_suffix = advertised[4..].to_vec();
            assert_eq!(local_suffix.len(), 2);
            let MiningOwned::NewExtendedMiningJob(job) = receiver.try_recv().unwrap() else {
                panic!("expected bootstrap job without prefix notification");
            };
            assert_eq!((job.channel_id, job.job_id), (success.channel_id, 1));
            assert!(receiver.try_recv().is_err());
            manager
                .extended_channels
                .with(&success.channel_id, |channel| {
                    assert_eq!(channel.get_target(), &current_target);
                    assert_eq!(
                        channel.get_extranonce_prefix(),
                        [current_prefix.clone(), local_suffix.clone()].concat()
                    );
                    let active = channel.get_active_job().unwrap();
                    assert_eq!(active.extranonce_prefix, advertised);
                    assert_eq!(active.target, old_target);
                    let futures = channel.get_future_jobs().collect::<Vec<_>>();
                    assert_eq!(futures.len(), 2);
                    for (id, job) in futures {
                        let upstream = if *id == 2 {
                            vec![0; 4]
                        } else {
                            assert_eq!(*id, 3);
                            vec![0xcc; 5]
                        };
                        assert_eq!(
                            job.extranonce_prefix,
                            [upstream, local_suffix.clone()].concat()
                        );
                        // Main's SetTarget semantics refresh queued future jobs, not active work.
                        assert_eq!(job.target, current_target);
                    }
                })
                .unwrap();
            assert_eq!(
                manager
                    .aggregated_extranonce_allocator
                    .with(|a| a.as_ref().unwrap().allocated_count())
                    .unwrap(),
                1
            );

            assert_late_join_share_reaches_upstream(&manager, success.channel_id, &advertised)
                .await;
            // Activating an inherited future job selects its captured prefix, not the current one.
            manager
                .handle_set_new_prev_hash(
                    None,
                    SetNewPrevHashOwned {
                        channel_id: 42,
                        job_id: 3,
                        prev_hash: vec![1; 32].try_into().unwrap(),
                        ntime_start: 0,
                        nbits: 0x207fffff,
                    },
                    None,
                )
                .await
                .unwrap();
            assert!(matches!(
                receiver.try_recv().unwrap(),
                MiningOwned::SetNewPrevHash(_)
            ));
            let MiningOwned::SetExtranoncePrefix(update) = receiver.try_recv().unwrap() else {
                panic!("expected inherited future prefix");
            };
            assert_eq!(
                update.extranonce_prefix.to_owned_bytes(),
                [vec![0xcc; 5], local_suffix.clone()].concat()
            );
            assert!(matches!(receiver.try_recv().unwrap(),
                MiningOwned::NewExtendedMiningJob(job) if job.job_id == 3));

            assert_late_join_share_reaches_upstream(
                &manager,
                success.channel_id,
                &update.extranonce_prefix.to_owned_bytes(),
            )
            .await;
            let mut current_job = job_with_extranonce_size(4, new_prefix_len + 8);
            current_job.ntime_start = Sv2OptionOwned::new(Some(0));
            manager
                .handle_new_extended_mining_job(None, current_job, None)
                .await
                .unwrap();
            let MiningOwned::SetExtranoncePrefix(update) = receiver.try_recv().unwrap() else {
                panic!("expected current prefix before new job");
            };
            assert_eq!(
                update.extranonce_prefix.to_owned_bytes(),
                [current_prefix, local_suffix].concat()
            );
            assert!(matches!(receiver.try_recv().unwrap(),
                MiningOwned::NewExtendedMiningJob(job) if job.job_id == 4));
            assert!(receiver.try_recv().is_err());

            assert_late_join_share_reaches_upstream(
                &manager,
                success.channel_id,
                &update.extranonce_prefix.to_owned_bytes(),
            )
            .await;
            // A subsequent whole-prefix rotation must not release the allocation held by jobs
            // inherited or created under previous upstream-prefix variants.
            manager
                .extended_channels
                .with_mut(&success.channel_id, |channel| {
                    channel
                        .set_extranonce_prefix(ExtranoncePrefix::from_wire(vec![0xee; 4]).unwrap())
                        .unwrap();
                })
                .unwrap();
            assert_eq!(
                manager
                    .aggregated_extranonce_allocator
                    .with(|a| a.as_ref().unwrap().allocated_count())
                    .unwrap(),
                1
            );
            drop(manager.extended_channels.remove(&success.channel_id));
            assert_eq!(
                manager
                    .aggregated_extranonce_allocator
                    .with(|a| a.as_ref().unwrap().allocated_count())
                    .unwrap(),
                0
            );
        }
    }
}
