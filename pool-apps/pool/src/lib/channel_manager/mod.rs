use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU32, AtomicUsize},
        Arc,
    },
};

use async_channel::{Receiver, Sender};
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
        mining_sv2::{ExtendedExtranonce, SetTarget},
        parsers_sv2::{Mining, TemplateDistribution, Tlv},
        template_distribution_sv2::{NewTemplate, SetNewPrevHash},
    },
    task_manager::TaskManager,
    utils::types::{ChannelId, DownstreamId, SharesPerMinute, VardiffKey},
};
use tokio::{net::TcpListener, select, sync::broadcast};
use tracing::{debug, error, info, warn};

use jd_server_sv2::job_declarator::JobDeclarator;

use crate::{
    config::PoolConfig,
    downstream::Downstream,
    error::{self, PoolError, PoolErrorKind, PoolResult},
    status::{handle_error, Status, StatusSender},
};

mod mining_message_handler;
mod template_distribution_message_handler;

const POOL_ALLOCATION_BYTES: usize = 4;
const CLIENT_SEARCH_SPACE_BYTES: usize = 16;
pub const FULL_EXTRANONCE_SIZE: usize = POOL_ALLOCATION_BYTES + CLIENT_SEARCH_SPACE_BYTES;

pub struct ChannelManagerData {
    // Mapping of `downstream_id` → `Downstream` object,
    // used by the channel manager to locate and interact with downstream clients.
    pub(crate) downstream: HashMap<DownstreamId, Downstream>,
    // Extranonce prefix factory for **extended downstream channels**.
    // Each new extended downstream receives a unique extranonce prefix.
    extranonce_prefix_factory_extended: ExtendedExtranonce,
    // Extranonce prefix factory for **standard downstream channels**.
    // Each new standard downstream receives a unique extranonce prefix.
    extranonce_prefix_factory_standard: ExtendedExtranonce,
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
pub struct ChannelManagerChannel {
    tp_sender: Sender<TemplateDistribution<'static>>,
    tp_receiver: Receiver<TemplateDistribution<'static>>,
    downstream_sender: broadcast::Sender<(usize, Mining<'static>, Option<Vec<Tlv>>)>,
    downstream_receiver: Receiver<(usize, Mining<'static>, Option<Vec<Tlv>>)>,
}

/// Contains all the state of mutable and immutable data required
/// by channel manager to process its task along with channels
/// to perform message traversal.
#[derive(Clone)]
pub struct ChannelManager {
    pub(crate) channel_manager_data: Arc<Mutex<ChannelManagerData>>,
    channel_manager_channel: ChannelManagerChannel,
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
    /// Constructor method used to instantiate the ChannelManager
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: PoolConfig,
        tp_sender: Sender<TemplateDistribution<'static>>,
        tp_receiver: Receiver<TemplateDistribution<'static>>,
        downstream_sender: broadcast::Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        downstream_receiver: Receiver<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        coinbase_outputs: Vec<u8>,
        job_declarator: Option<JobDeclarator>,
    ) -> PoolResult<Self, error::ChannelManager> {
        let range_0 = 0..0;
        let range_1 = 0..POOL_ALLOCATION_BYTES;
        let range_2 = POOL_ALLOCATION_BYTES..POOL_ALLOCATION_BYTES + CLIENT_SEARCH_SPACE_BYTES;

        let make_extranonce_factory = || {
            // simulating a scenario where there are multiple mining servers
            // this static prefix allows unique extranonce_prefix allocation
            // for this mining server
            let static_prefix = config.server_id().to_be_bytes().to_vec();

            ExtendedExtranonce::new(
                range_0.clone(),
                range_1.clone(),
                range_2.clone(),
                Some(static_prefix),
            )
            .expect("Failed to create ExtendedExtranonce with valid ranges")
        };

        let extranonce_prefix_factory_extended = make_extranonce_factory();
        let extranonce_prefix_factory_standard = make_extranonce_factory();

        let channel_manager_data = Arc::new(Mutex::new(ChannelManagerData {
            downstream: HashMap::new(),
            extranonce_prefix_factory_extended,
            extranonce_prefix_factory_standard,
            downstream_id_factory: AtomicUsize::new(1),
            vardiff: HashMap::new(),
            coinbase_outputs,
            last_future_template: None,
            last_new_prev_hash: None,
        }));

        let channel_manager_channel = ChannelManagerChannel {
            tp_sender,
            tp_receiver,
            downstream_sender,
            downstream_receiver,
        };

        let channel_manager = ChannelManager {
            channel_manager_data,
            channel_manager_channel,
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
            FULL_EXTRANONCE_SIZE,
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
        status_sender: Sender<Status>,
        channel_manager_sender: Sender<(DownstreamId, Mining<'static>, Option<Vec<Tlv>>)>,
        channel_manager_receiver: broadcast::Sender<(
            DownstreamId,
            Mining<'static>,
            Option<Vec<Tlv>>,
        )>,
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
            select! {
                _ = cancellation_token.cancelled() => {
                    info!("Channel Manager: received shutdown while waiting for templates");
                    return Ok(());
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
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
                                let status_sender_inner = status_sender.clone();
                                let channel_manager_sender_inner = channel_manager_sender.clone();
                                let channel_manager_receiver_inner = channel_manager_receiver.clone();
                                let task_manager_inner = task_manager_clone.clone();

                                task_manager_clone.spawn(async move {
                                    let noise_stream = tokio::select! {
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
                                            let error = PoolError::<error::ChannelManager>::shutdown(PoolErrorKind::CouldNotInitiateSystem);
                                            handle_error(&StatusSender::ChannelManager(status_sender_inner), error).await;
                                            return;
                                        }
                                    };

                                    let downstream = Downstream::new(
                                        downstream_id,
                                        channel_id_factory,
                                        group_channel,
                                        channel_manager_sender_inner,
                                        channel_manager_receiver_inner,
                                        noise_stream,
                                        cancellation_token_inner.clone(),
                                        task_manager_inner.clone(),
                                        this.supported_extensions.clone(),
                                        this.required_extensions.clone(),
                                    );

                                    this.channel_manager_data.super_safe_lock(|data| {
                                        data.downstream.insert(downstream_id, downstream.clone());
                                    });

                                    downstream
                                        .start(
                                            cancellation_token_inner,
                                            status_sender_inner,
                                            task_manager_inner,
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
        status_sender: Sender<Status>,
        task_manager: Arc<TaskManager>,
        coinbase_outputs: Vec<TxOut>,
    ) -> PoolResult<(), error::ChannelManager> {
        let status_sender = StatusSender::ChannelManager(status_sender);

        self.coinbase_output_constraints(coinbase_outputs).await?;

        task_manager.spawn(async move {
            let cm = self.clone();
            let vardiff_future = self.run_vardiff_loop();
            tokio::pin!(vardiff_future);
            loop {
                let mut cm_template = cm.clone();
                let mut cm_downstreams = cm.clone();
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Channel Manager: received shutdown signal");
                        break;
                    }
                    res = &mut vardiff_future => {
                        info!("Vardiff loop completed with: {res:?}");
                    }
                    res = cm_template.handle_template_provider_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Template Receiver message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
                    }
                    res = cm_downstreams.handle_downstream_mining_message() => {
                        if let Err(e) = res {
                            error!(error = ?e, "Error handling Downstreams message");
                            if handle_error(&status_sender, e).await {
                                break;
                            }
                        }
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
    #[allow(clippy::result_large_err)]
    pub fn remove_downstream(
        &self,
        downstream_id: DownstreamId,
    ) -> PoolResult<(), error::ChannelManager> {
        self.channel_manager_data.super_safe_lock(|cm_data| {
            cm_data.downstream.remove(&downstream_id);
            cm_data
                .vardiff
                .retain(|key, _| key.downstream_id != downstream_id);
        });
        Ok(())
    }

    // Handles messages received from the TP subsystem.
    //
    // This method listens for incoming frames on the `tp_receiver` channel.
    // - If the frame contains a TemplateDistribution message, it forwards it to the template
    //   distribution message handler.
    // - If the frame contains any unsupported message type, an error is returned.
    async fn handle_template_provider_message(&mut self) -> PoolResult<(), error::ChannelManager> {
        if let Ok(message) = self.channel_manager_channel.tp_receiver.recv().await {
            self.handle_template_distribution_message_from_server(None, message, None)
                .await?;
        }
        Ok(())
    }

    async fn handle_downstream_mining_message(&mut self) -> PoolResult<(), error::ChannelManager> {
        if let Ok((downstream_id, message, tlv_fields)) = self
            .channel_manager_channel
            .downstream_receiver
            .recv()
            .await
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
            message.forward(&self.channel_manager_channel).await;
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

        self.channel_manager_channel
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

#[derive(Clone)]
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
    pub async fn forward(self, channel_manager_channel: &ChannelManagerChannel) {
        match self {
            RouteMessageTo::Downstream((downstream_id, message)) => {
                _ = channel_manager_channel.downstream_sender.send((
                    downstream_id,
                    message.into_static(),
                    None,
                ));
            }
            RouteMessageTo::TemplateProvider(message) => {
                _ = channel_manager_channel
                    .tp_sender
                    .send(message.into_static())
                    .await;
            }
        }
    }
}
