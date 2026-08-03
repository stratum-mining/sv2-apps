use std::sync::atomic::Ordering;
use stratum_apps::stratum_core::{
    parsers_sv2::MiningOwned,
    template_distribution_sv2::{
        NewTemplateOwned, RequestTransactionDataErrorOwned, RequestTransactionDataSuccessOwned,
    },
};

use stratum_apps::stratum_core::{
    bitcoin::Amount, channels_sv2::outputs::deserialize_outputs,
    handlers_sv2::HandleTemplateDistributionMessagesFromServerOwnedAsync,
    mining_sv2::SetNewPrevHashOwned as SetNewPrevHashMp, parsers_sv2::Tlv,
    template_distribution_sv2::SetNewPrevHashOwned as SetNewPrevHashTdp,
};
use tracing::{info, warn};

use crate::{
    channel_manager::{ChannelManager, RouteMessageTo},
    error::{self, PoolError, PoolErrorKind},
};

#[cfg_attr(not(test), hotpath::measure_all)]
impl HandleTemplateDistributionMessagesFromServerOwnedAsync for ChannelManager {
    type Error = PoolError<error::ChannelManager>;

    fn get_negotiated_extensions_with_server(
        &self,
        _server_id: Option<usize>,
    ) -> Result<Vec<u16>, Self::Error> {
        Ok(vec![])
    }

    async fn handle_new_template(
        &mut self,
        _server_id: Option<usize>,
        msg: NewTemplateOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);

        if msg.future_template {
            self.last_future_template
                .set(Some(msg.clone()))
                .map_err(PoolError::shutdown)?;
        }

        let mut messages: Vec<RouteMessageTo> = Vec::new();
        let mut coinbase_output =
            deserialize_outputs(self.coinbase_outputs.clone()).expect("deserialization failed");
        coinbase_output[0].value = Amount::from_sat(msg.coinbase_tx_value_remaining);

        self.downstreams.try_for_each(|downstream_id, downstream| {
            // If REQUIRES_CUSTOM_WORK is set, skip template handling entirely
            // (see https://github.com/stratum-mining/sv2-apps/issues/55).
            if downstream.requires_custom_work.load(Ordering::SeqCst) {
                return Ok(());
            }

            let downstream_coinbase_outputs = downstream
                .payout_mode
                .with(|payout_mode| match payout_mode.as_ref() {
                    Some(mode) => mode.coinbase_outputs(
                        msg.coinbase_tx_value_remaining,
                        &self.coinbase_reward_script,
                    ),
                    None => coinbase_output.clone(),
                })
                .map_err(PoolError::shutdown)?;

            let requires_standard_jobs = downstream.requires_standard_jobs.load(Ordering::SeqCst);
            let mut downstream_messages = Vec::new();

            let group_channel_job = downstream
                .group_channel
                .with(|group_channel| {
                    group_channel
                        .on_new_template(
                            msg.clone(),
                            downstream_coinbase_outputs.clone(),
                        )
                        .map_err(PoolError::shutdown)?;
                    let group_job = if msg.future_template {
                        let Some(future_job_id) =
                            group_channel.get_future_job_id_from_template_id(msg.template_id)
                        else {
                            return Err(PoolError::shutdown(PoolErrorKind::JobNotFound));
                        };
                        let Some(future_job) = group_channel.get_future_job(future_job_id) else {
                            return Err(PoolError::shutdown(PoolErrorKind::JobNotFound));
                        };
                        future_job
                    } else {
                        let Some(active_job) = group_channel.get_active_job() else {
                            return Err(PoolError::shutdown(PoolErrorKind::JobNotFound));
                        };
                        active_job
                    };
                    // If REQUIRES_STANDARD_JOBS is not set and the group channel is not
                    // empty we need to send the NewExtendedMiningJob message to the group
                    // channel.
                    if !requires_standard_jobs && !group_channel.is_empty() {
                        downstream_messages.push(
                            (
                                downstream_id,
                                MiningOwned::NewExtendedMiningJob(group_job.get_job_message().clone()),
                            )
                                .into(),
                        );
                    }
                    Ok::<_, Self::Error>(group_job.clone())
                })
                .map_err(PoolError::shutdown)??;

            // Loop over every standard channel.
            // If REQUIRES_STANDARD_JOBS is not set, we need to call
            // on_group_channel_job on each standard channel.
            // If REQUIRES_STANDARD_JOBS is set, we need to call on_new_template and send
            // individual NewMiningJob messages for each standard channel.
            downstream.standard_channels.try_for_each_mut(|channel_id, standard_channel| {
                if !requires_standard_jobs {
                    standard_channel
                        .on_group_channel_job(group_channel_job.clone())
                        .map_err(|e| {
                            tracing::error!("Error while adding group channel job to standard channel with id: {channel_id:?}");
                            PoolError::shutdown(e)
                        })?;
                } else {
                    standard_channel
                        .on_new_template(
                            msg.clone(),
                            downstream_coinbase_outputs.clone(),
                        )
                        .map_err(|e| {
                            tracing::error!("Error while adding template to standard channel");
                            PoolError::shutdown(e)
                        })?;
                    let standard_job = if msg.future_template {
                        let job_id = standard_channel
                            .get_future_job_id_from_template_id(msg.template_id)
                            .expect("future job id must exist");
                        standard_channel
                            .get_future_job(job_id)
                            .expect("future job must exist")
                    } else {
                        standard_channel
                            .get_active_job()
                            .expect("active job must exist")
                    };
                    downstream_messages.push(
                        (
                            downstream_id,
                            MiningOwned::NewMiningJob(standard_job.get_job_message().clone()),
                        )
                            .into(),
                    );
                }
                Ok::<(), Self::Error>(())
            })?;

            // Loop over every extended channel and call on_group_channel_job on each one.
            downstream.extended_channels.try_for_each_mut(|channel_id, channel| {
                channel
                    .on_group_channel_job(group_channel_job.clone())
                    .map_err(|e| {
                        tracing::error!("Error while adding group channel job to extended channel with id: {channel_id:?}");
                        PoolError::shutdown(e)
                    })
            })?;

            messages.extend(downstream_messages);
            Ok(())
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }

    async fn handle_request_tx_data_error(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestTransactionDataErrorOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        warn!("Received: {:?}", msg);
        Ok(())
    }

    async fn handle_request_tx_data_success(
        &mut self,
        _server_id: Option<usize>,
        msg: RequestTransactionDataSuccessOwned,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);
        Ok(())
    }

    async fn handle_set_new_prev_hash(
        &mut self,
        _server_id: Option<usize>,
        msg: SetNewPrevHashTdp,
        _tlv_fields: Option<&[Tlv]>,
    ) -> Result<(), Self::Error> {
        info!("Received: {:?}", msg);

        self.last_new_prev_hash
            .set(Some(msg.clone()))
            .map_err(PoolError::shutdown)?;

        let mut messages: Vec<RouteMessageTo> = vec![];
        self.downstreams.try_for_each(|downstream_id, downstream| {
            // If downstream requires custom work, skip template handling entirely
            // (see https://github.com/stratum-mining/sv2-apps/issues/55).
            if downstream.requires_custom_work.load(Ordering::SeqCst) {
                return Ok(());
            }

            let requires_standard_jobs = downstream.requires_standard_jobs.load(Ordering::SeqCst);
            let mut downstream_messages = Vec::new();

            downstream
                .group_channel
                .with(|group_channel| {
                    // Call on_set_new_prev_hash on the group channel to update the channel
                    // state.
                    group_channel
                        .on_set_new_prev_hash(msg.clone())
                        .map_err(|e| {
                            tracing::error!("Error while adding new prev hash to group channel");
                            PoolError::shutdown(e)
                        })?;
                    // Did SetupConnection have the REQUIRES_STANDARD_JOBS flag set?
                    // If not, and the group channel is not empty, we need to send the
                    // SetNewPrevHash message to the group channel.
                    if !requires_standard_jobs && !group_channel.is_empty() {
                        let active_job_id = group_channel
                            .get_active_job()
                            .expect("active job must exist")
                            .get_job_id();
                        downstream_messages.push(
                            (
                                downstream_id,
                                MiningOwned::SetNewPrevHash(SetNewPrevHashMp {
                                    channel_id: group_channel.get_group_channel_id(),
                                    job_id: active_job_id,
                                    prev_hash: msg.prev_hash.clone(),
                                    min_ntime: msg.header_timestamp,
                                    nbits: msg.n_bits,
                                }),
                            )
                                .into(),
                        );
                    }
                    Ok::<(), Self::Error>(())
                })
                .map_err(PoolError::shutdown)??;

            // Loop over every extended channel and call on_set_new_prev_hash on each
            // extended channel to update the channel state.
            downstream.extended_channels.try_for_each_mut(|channel_id, channel| {
                channel
                    .on_set_new_prev_hash(msg.clone())
                    .map_err(|e| {
                        tracing::error!("Error while adding new prev hash to extended channel: {channel_id:?} {e:?}");
                        PoolError::shutdown(e)
                    })
            })?;

            // Loop over every standard channel and call on_set_new_prev_hash on each
            // standard channel to update the channel state.
            downstream.standard_channels.try_for_each_mut(|channel_id, channel| {
                // Call on_set_new_prev_hash on the standard channel to update the channel
                // state.
                channel
                    .on_set_new_prev_hash(msg.clone())
                    .map_err(|e| {
                        tracing::error!("Error while adding new prev hash to standard channel: {channel_id:?} {e:?}");
                        PoolError::shutdown(e)
                    })?;
                // Did SetupConnection have the REQUIRES_STANDARD_JOBS flag set?
                // If yes, we need to send the SetNewPrevHash message to each standard
                // channel.
                if requires_standard_jobs {
                    let Some(active_job) = channel.get_active_job() else {
                        return Err(PoolError::shutdown(PoolErrorKind::JobNotFound));
                    };
                    let active_job_id = active_job.get_job_id();
                    downstream_messages.push(
                        (
                            downstream_id,
                            MiningOwned::SetNewPrevHash(SetNewPrevHashMp {
                                channel_id,
                                job_id: active_job_id,
                                prev_hash: msg.prev_hash.clone(),
                                min_ntime: msg.header_timestamp,
                                nbits: msg.n_bits,
                            }),
                        )
                            .into(),
                    );
                }
                Ok::<(), Self::Error>(())
            })?;

            messages.extend(downstream_messages);
            Ok(())
        })?;

        for message in messages {
            // A send can only fail if the receiver side of the channel is closed.
            // Since this is an unbounded channel, it cannot fail due to capacity
            // limits (which would only apply to bounded channels).
            if let Err(e) = message.forward(&self.channel_manager_io).await {
                tracing::error!("Failed to forward message {e:?}");
            }
        }

        Ok(())
    }
}
