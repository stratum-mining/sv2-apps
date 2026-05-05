use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, AtomicUsize},
        Arc,
    },
};

use async_channel::{unbounded, Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::CancellationToken;
use core::sync::atomic::Ordering;
use stratum_apps::{
    coinbase_output_constraints::coinbase_output_constraints_message_with_offset,
    config_helpers::CoinbaseRewardScript,
    custom_mutex::Mutex,
    key_utils::{Secp256k1PublicKey, Secp256k1SecretKey},
    network_helpers::accept_noise_connection,
    stratum_core::{
        bitcoin::{Amount, TxOut},
        channels_sv2::{
            extranonce_manager::{bytes_needed, ExtranonceAllocator},
            server::{
                extended::ExtendedChannel,
                group::GroupChannel,
                jobs::{extended::ExtendedJob, job_store::DefaultJobStore, standard::StandardJob},
                standard::StandardChannel,
            },
            Vardiff, VardiffState,
        },
        handlers_sv2::{
            HandleMiningMessagesFromClientAsync, HandleTemplateDistributionMessagesFromServerAsync,
        },
        mining_sv2::SetTarget,
        parsers_sv2::{Mining, TemplateDistribution, Tlv},
        template_distribution_sv2::{NewTemplate, SetNewPrevHash},
    },
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, SharesPerMinute, VardiffKey},
};
use tokio::{net::TcpListener, select};
use tracing::{debug, error, info, warn};

use jd_server_sv2::job_declarator::JobDeclarator;

use crate::{
    config::PoolConfig,
    downstream::Downstream,
    error::{self, Action, LoopControl, PoolError, PoolErrorKind, PoolResult},
    utils::DownstreamMessage,
};

mod mining_message_handler;
mod template_distribution_message_handler;

// Size of the static identifier for this pool server, placed at the start of the pool's
// extranonce allocation. One byte covers up to 256 distinct pool servers.
const POOL_SERVER_BYTES: u8 = 1;
// Maximum number of concurrent channels the pool can allocate. Determines
// [`POOL_LOCAL_PREFIX_BYTES`] via [`bytes_needed`]. The internal allocation
// bitmap uses `POOL_MAX_CHANNELS / 8` bytes of RAM.
const POOL_MAX_CHANNELS: u32 = 16_777_216;
// Bytes consumed by the per-channel `local_index`. Derived from
// [`POOL_MAX_CHANNELS`] so the two stay in sync.
const POOL_LOCAL_PREFIX_BYTES: u8 = bytes_needed(POOL_MAX_CHANNELS);
const POOL_ALLOCATION_BYTES: u8 = POOL_SERVER_BYTES + POOL_LOCAL_PREFIX_BYTES;
const CLIENT_SEARCH_SPACE_BYTES: u8 = 16;
pub const FULL_EXTRANONCE_SIZE: u8 = POOL_ALLOCATION_BYTES + CLIENT_SEARCH_SPACE_BYTES;

pub struct ChannelManagerData {
    // Mapping of `downstream_id` → `Downstream` object,
    // used by the channel manager to locate and interact with downstream clients.
    pub(crate) downstream: HashMap<DownstreamId, Downstream>,
    // Unified extranonce prefix allocator, shared by standard and extended
    // downstream channels. The allocated [`ExtranoncePrefix`] is stored on the
    // channel itself, so dropping the channel automatically releases the slot.
    extranonce_allocator: ExtranonceAllocator,
    // Factory that assigns a unique ID to each new **downstream connection**.
    downstream_id_factory: AtomicUsize,
    // Mapping of `(downstream_id, channel_id)` → vardiff controller.
    // Each entry manages variable difficulty for a specific downstream channel.
    vardiff: HashMap<VardiffKey, VardiffState>,
    // Coinbase outputs
    coinbase_outputs: Vec<u8>,
    // Last new prevhash
    last_new_prev_hash: Option<SetNewPrevHash<'static>>,
    // Last future template
    last_future_template: Option<NewTemplate<'static>>,
}

#[derive(Clone)]
pub struct ChannelManagerIo {
    tp_sender: Sender<TemplateDistribution<'static>>,
    tp_receiver: Receiver<TemplateDistribution<'static>>,
    downstream_sender: Arc<Mutex<HashMap<DownstreamId, Sender<DownstreamMessage>>>>,
    downstream_receiver: Receiver<(usize, Mining<'static>, Option<Vec<Tlv>>)>,
}

/// Contains all the state of mutable and immutable data required
/// by channel manager to process its task along with channels
/// to perform message traversal.
#[derive(Clone)]
pub struct ChannelManager {
    pub(crate) channel_manager_data: Arc<Mutex<ChannelManagerData>>,
    channel_manager_io: ChannelManagerIo,
    pool_tag_string: String,
    share_batch_size: usize,
    shares_per_minute: SharesPerMinute,
    coinbase_reward_script: CoinbaseRewardScript,
    /// Protocol extensions that the pool supports (will accept if requested by clients).
    supported_extensions: Vec<u16>,
    /// Protocol extensions that the pool requires (clients must support these).
    required_extensions: Vec<u16>,
    /// Embedded Job Declaration engine (present when `[jds]` config is set).
    job_declarator: Option<JobDeclarator>,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl ChannelManager {
    fn handle_error_action(
        &self,
        context: &str,
        e: &PoolError<error::ChannelManager>,
        cancellation_token: &CancellationToken,
    ) -> LoopControl {
        if cancellation_token.is_cancelled() {
            debug!(
                error_kind = ?e.kind,
                "{context} returned an error after shutdown was requested"
            );
            return LoopControl::Continue;
        }
        match e.action {
            Action::Log => {
                warn!(error_kind = ?e.kind, "{context} returned a log-only error");
                LoopControl::Continue
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
            Action::Shutdown => {
                warn!(error_kind = ?e.kind, "{context} requested shutdown");
                cancellation_token.cancel();
                LoopControl::Break
            }
        }
    }

    /// Constructor method used to instantiate the ChannelManager
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: PoolConfig,
        tp_sender: Sender<TemplateDistribution<'static>>,
        tp_receiver: Receiver<TemplateDistribution<'static>>,
        downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        coinbase_outputs: Vec<u8>,
        job_declarator: Option<JobDeclarator>,
    ) -> PoolResult<Self, error::ChannelManager> {
        // Simulating a scenario where there are multiple mining servers,
        // `server_id` is used as `local_prefix_bytes` so each pool instance
        // allocates extranonce prefixes in its own distinct namespace.
        let local_prefix_bytes = config.server_id().to_be_bytes().to_vec();

        let extranonce_allocator =
            ExtranonceAllocator::new(local_prefix_bytes, FULL_EXTRANONCE_SIZE, POOL_MAX_CHANNELS)
                .map_err(PoolError::shutdown)?;

        let channel_manager_data = Arc::new(Mutex::new(ChannelManagerData {
            downstream: HashMap::new(),
            extranonce_allocator,
            downstream_id_factory: AtomicUsize::new(1),
            vardiff: HashMap::new(),
            coinbase_outputs,
            last_future_template: None,
            last_new_prev_hash: None,
        }));

        let channel_manager_io = ChannelManagerIo {
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
            pool_tag_string: config.pool_signature().to_string(),
            coinbase_reward_script: config.coinbase_reward_script().clone(),
            supported_extensions: config.supported_extensions().to_vec(),
            required_extensions: config.required_extensions().to_vec(),
            job_declarator,
        };

        Ok(channel_manager)
    }

    // Bootstraps a group channel with the given parameters.
    // Returns a `GroupChannel` if successful, otherwise returns `None`.
    //
    // To be called before calling Downstream::new.
    fn bootstrap_group_channel(
        &self,
        channel_id: ChannelId,
    ) -> Option<GroupChannel<'static, DefaultJobStore<ExtendedJob<'static>>>> {
        let (last_future_template, last_set_new_prev_hash) =
            self.channel_manager_data.super_safe_lock(|data| {
                (
                    data.last_future_template
                        .clone()
                        .expect("No future template found after readiness check"),
                    data.last_new_prev_hash
                        .clone()
                        .expect("No new prevhash found after readiness check"),
                )
            });
        let mut group_channel = match GroupChannel::new_for_pool(
            channel_id,
            DefaultJobStore::new(),
            FULL_EXTRANONCE_SIZE as usize,
            self.pool_tag_string.clone(),
        ) {
            Ok(channel) => channel,
            Err(e) => {
                error!(error = ?e, "Failed to bootstrap group channel");
                return None;
            }
        };

        let coinbase_output = TxOut {
            value: Amount::from_sat(last_future_template.coinbase_tx_value_remaining),
            script_pubkey: self.coinbase_reward_script.script_pubkey(),
        };

        if let Err(e) = group_channel.on_new_template(last_future_template, vec![coinbase_output]) {
            error!(error = ?e, "Failed to add template to group channel");
            return None;
        }

        if let Err(e) = group_channel.on_set_new_prev_hash(last_set_new_prev_hash) {
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
        channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
    ) -> PoolResult<(), error::ChannelManager> {
        // todo: let start_downstream_server accept Arc, instead of clone.
        let this = Arc::new(self);

        // Wait for initial template and prevhash before accepting connections
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
                biased;
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                _ = cancellation_token.cancelled() => {
                    info!("Channel Manager: received shutdown while waiting for templates");
                    return Ok(());
                }
            }
        }

        info!("Starting downstream server at {listening_address}");
        let server = TcpListener::bind(listening_address)
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to bind downstream server at {listening_address}");
                e
            })
            .map_err(PoolError::shutdown)?;

        let task_manager_clone = task_manager.clone();
        let cancellation_token_clone = cancellation_token.clone();
        task_manager.spawn(async move {
            loop {
                select! {
                    biased;
                    _ = cancellation_token_clone.cancelled() => {
                        info!("Channel Manager: received shutdown signal");
                        break;
                    }
                    res = server.accept() => {
                        match res {
                            Ok((stream, socket_address)) => {
                                info!(%socket_address, "New downstream connection");

                                let this = Arc::clone(&this);
                                let cancellation_token_inner = cancellation_token_clone.clone();
                                let channel_manager_sender_inner = channel_manager_sender.clone();
                                let task_manager_inner = task_manager_clone.clone();

                                task_manager_clone.spawn(async move {
                                    let cancellation_token_clone = cancellation_token_inner.clone();
                                    let noise_stream = tokio::select! {
                                        biased;
                                        result = accept_noise_connection(stream, authority_public_key, authority_secret_key, cert_validity_sec) => {
                                            match result {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    error!(error = ?e, "Noise handshake failed");
                                                    return;
                                                }
                                            }
                                        }
                                        _ = cancellation_token_inner.cancelled() => {
                                            info!("Shutdown received during handshake, dropping connection");
                                            return;
                                        }
                                    };

                                    let downstream_id = this.channel_manager_data
                                        .super_safe_lock(|data| data.downstream_id_factory.fetch_add(1, Ordering::SeqCst));

                                    let channel_id_factory = AtomicU32::new(1);
                                    let group_channel_id = channel_id_factory.fetch_add(1, Ordering::SeqCst);

                                    let group_channel = match this.bootstrap_group_channel(group_channel_id) {
                                        Some(group_channel) => group_channel,
                                        None => {
                                            error!("Failed to bootstrap group channel - disconnecting downstream {downstream_id}");
                                            cancellation_token_clone.cancel();
                                            return;
                                        }
                                    };

                                    let (channel_manager_sender, channel_manager_receiver) = unbounded();

                                    let downstream = Downstream::new(
                                        downstream_id,
                                        channel_id_factory,
                                        group_channel,
                                        channel_manager_sender_inner,
                                        channel_manager_receiver,
                                        noise_stream,
                                        cancellation_token_inner.clone(),
                                        task_manager_inner.clone(),
                                        this.supported_extensions.clone(),
                                        this.required_extensions.clone(),
                                    );

                                    this.channel_manager_io.downstream_sender.super_safe_lock(|map| map.insert(downstream_id, channel_manager_sender));

                                    this.channel_manager_data.super_safe_lock(|data| {
                                        data.downstream.insert(downstream_id, downstream.clone());
                                    });

                                    downstream
                                        .start(
                                            cancellation_token_inner,
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
        task_manager: Arc<TaskManager>,
        coinbase_outputs: Vec<TxOut>,
    ) -> PoolResult<(), error::ChannelManager> {
        self.coinbase_output_constraints(coinbase_outputs).await?;

        task_manager.spawn(async move {
            let cm = self.clone();
            let vardiff_future = self.run_vardiff_loop();
            tokio::pin!(vardiff_future);
            loop {
                let mut cm_template = cm.clone();
                let mut cm_downstreams = cm.clone();
                tokio::select! {
                    biased;
                    res = &mut vardiff_future => {
                        info!("Vardiff loop completed with: {res:?}");
                    }
                    res = cm_template.handle_template_provider_message(),
                        if !cm.channel_manager_io.tp_receiver.is_closed() =>
                    {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Template Receiver message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_template_provider_message",
                                &e,
                                &cancellation_token,
                            ) {
                                break;
                            }
                        }
                    }
                    res = cm_downstreams.handle_downstream_mining_message(),
                        if !cm.channel_manager_io.downstream_receiver.is_closed() =>
                    {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Downstreams message");
                            if let LoopControl::Break = cm.handle_error_action(
                                "ChannelManager::handle_downstream_mining_message",
                                &e,
                                &cancellation_token,
                            ) {
                                break;
                            }
                        }
                    }
                    _ = cancellation_token.cancelled() => {
                        info!("Channel Manager: received shutdown signal");
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    // Removes a Downstream entry from the ChannelManager’s state.
    //
    // Given a `downstream_id`, this method:
    // 1. Removes the corresponding Downstream from the `downstream` map.
    // 2. Removes the channels of the corresponding Downstream from `vardiff` map.
    pub fn remove_downstream(&self, downstream_id: DownstreamId) {
        self.channel_manager_data.super_safe_lock(|cm_data| {
            cm_data.downstream.remove(&downstream_id);
            cm_data
                .vardiff
                .retain(|key, _| key.downstream_id != downstream_id);
        });
        self.channel_manager_io
            .downstream_sender
            .super_safe_lock(|map| map.remove(&downstream_id));
    }

    // Handles messages received from the TP subsystem.
    //
    // This method listens for incoming frames on the `tp_receiver` channel.
    // - If the frame contains a TemplateDistribution message, it forwards it to the template
    //   distribution message handler.
    // - If the frame contains any unsupported message type, an error is returned.
    async fn handle_template_provider_message(&mut self) -> PoolResult<(), error::ChannelManager> {
        if let Ok(message) = self.channel_manager_io.tp_receiver.recv().await {
            self.handle_template_distribution_message_from_server(None, message, None)
                .await?;
        }
        Ok(())
    }

    async fn handle_downstream_mining_message(&mut self) -> PoolResult<(), error::ChannelManager> {
        if let Ok((downstream_id, message, tlv_fields)) =
            self.channel_manager_io.downstream_receiver.recv().await
        {
            let tlv_slice = tlv_fields.as_deref();
            self.handle_mining_message_from_client(Some(downstream_id), message, tlv_slice)
                .await?;
        }

        Ok(())
    }

    // Runs the vardiff on extended channel.
    fn run_vardiff_on_extended_channel(
        downstream_id: DownstreamId,
        channel_id: ChannelId,
        channel_state: &mut ExtendedChannel<'static, DefaultJobStore<ExtendedJob<'static>>>,
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
            return;
        };

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
        channel: &mut StandardChannel<'static, DefaultJobStore<StandardJob<'static>>>,
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

        if let Some(new_hashrate) = new_hashrate_opt {
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
                    debug!(
                        "Updated target for standard channel channel_id={channel_id} to {updated_target:?}"
                    );
                }
                Err(e) => warn!(
                    "Failed to update standard channel channel_id={channel_id} during vardiff {e:?}"
                ),
            }
        }
    }

    // Periodic vardiff task loop.
    //
    // # Purpose
    // - Executes the vardiff cycle every 60 seconds for all downstreams.
    // - Delegates to [`Self::run_vardiff`] on each tick.
    async fn run_vardiff_loop(&self) -> PoolResult<(), error::ChannelManager> {
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
    async fn run_vardiff(&self) -> PoolResult<(), error::ChannelManager> {
        let mut messages: Vec<RouteMessageTo> = vec![];
        self.channel_manager_data
            .super_safe_lock(|channel_manager_data| {
                for (vardiff_key, vardiff_state) in channel_manager_data.vardiff.iter_mut() {
                    let downstream_id = &vardiff_key.downstream_id;
                    let channel_id = &vardiff_key.channel_id;

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
            });

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                error!("Failed to forward message {e:?}");
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
    ) -> PoolResult<(), error::ChannelManager> {
        let msg = coinbase_output_constraints_message_with_offset(coinbase_outputs);

        self.channel_manager_io
            .tp_sender
            .send(TemplateDistribution::CoinbaseOutputConstraints(msg))
            .await
            .map_err(|e| {
                error!(error = ?e, "Failed to send CoinbaseOutputConstraints message to TP");
                PoolError::shutdown(PoolErrorKind::ChannelErrorSender)
            })?;

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum RouteMessageTo<'a> {
    /// Route to the template provider subsystem.
    TemplateProvider(TemplateDistribution<'a>),
    /// Route to a specific downstream client by ID, along with its mining message.
    Downstream((DownstreamId, Mining<'a>)),
}

impl<'a> From<TemplateDistribution<'a>> for RouteMessageTo<'a> {
    fn from(value: TemplateDistribution<'a>) -> Self {
        Self::TemplateProvider(value)
    }
}

impl<'a> From<(DownstreamId, Mining<'a>)> for RouteMessageTo<'a> {
    fn from(value: (DownstreamId, Mining<'a>)) -> Self {
        Self::Downstream(value)
    }
}

impl RouteMessageTo<'_> {
    pub async fn forward(self, channel_manager_io: &ChannelManagerIo) -> Result<(), PoolErrorKind> {
        match self {
            RouteMessageTo::Downstream((downstream_id, message)) => {
                let sender = channel_manager_io
                    .downstream_sender
                    .super_safe_lock(|map| map.get(&downstream_id).cloned());

                if let Some(sender) = sender {
                    sender.send((message.into_static(), None)).await?;
                } else {
                    debug!("Dropping message for downstream {downstream_id}: no longer connected");
                }
            }
            RouteMessageTo::TemplateProvider(message) => {
                channel_manager_io
                    .tp_sender
                    .send(message.into_static())
                    .await?;
            }
        }
        Ok(())
    }
}
