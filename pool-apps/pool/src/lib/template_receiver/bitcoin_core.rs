use crate::{
    error::{self, PoolError, PoolErrorKind},
    status::{handle_error, State, Status, StatusSender},
};
use async_channel::{Receiver, Sender};
use bitcoin_core_sv2::template_distribution_protocol::{BitcoinCoreSv2TDP, CancellationToken};
use std::{path::PathBuf, sync::Arc, thread::JoinHandle};
use stratum_apps::{stratum_core::parsers_sv2::TemplateDistribution, task_manager::TaskManager};

#[derive(Clone)]
pub struct BitcoinCoreSv2TDPConfig {
    pub unix_socket_path: PathBuf,
    pub fee_threshold: u64,
    pub min_interval: u8,
    pub incoming_tdp_receiver: Receiver<TemplateDistribution<'static>>,
    pub outgoing_tdp_sender: Sender<TemplateDistribution<'static>>,
    pub cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure)]
pub async fn connect_to_bitcoin_core(
    bitcoin_core_config: BitcoinCoreSv2TDPConfig,
    cancellation_token: CancellationToken,
    task_manager: Arc<TaskManager>,
    status_sender: Sender<Status>,
) -> JoinHandle<()> {
    let bitcoin_core_canc_token = bitcoin_core_config.cancellation_token.clone();
    let status_sender_clone = status_sender.clone();

    // spawn a task to handle shutdown signals and cancellation token activations
    task_manager.spawn(async move {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                bitcoin_core_canc_token.cancel();
            }
            _ = bitcoin_core_canc_token.cancelled() => {
                // turn status_sender into a StatusSender::TemplateReceiver
                let status_sender = StatusSender::TemplateReceiver(status_sender_clone);

                handle_error::<error::TemplateProvider>(
                    &status_sender,
                    PoolError::shutdown(PoolErrorKind::BitcoinCoreSv2TDPCancellationTokenActivated),
                )
                .await;
            }
        }
    });

    let status_sender_clone = status_sender.clone();

    // spawn a dedicated thread to run the BitcoinCoreSv2TDP instance
    // because we're limited to tokio::task::LocalSet due to the use of `capnp` clients on
    // `bitcoin-core-sv2`, which are not `Send`
    std::thread::spawn(move || {
        // we need a dedicated runtime so we can spawn an async task inside the LocalSet
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("Failed to create Tokio runtime: {:?}", e);

                // we can't use handle_error here because we're not in a async context yet
                let _ = status_sender_clone.send_blocking(Status {
                    state: State::TemplateReceiverShutdown(
                        PoolErrorKind::FailedToCreateBitcoinCoreTokioRuntime,
                    ),
                });
                return;
            }
        };
        let tokio_local_set = tokio::task::LocalSet::new();

        tokio_local_set.block_on(&rt, async move {
            // create a new BitcoinCoreSv2TDP instance
            let mut sv2_bitcoin_core = match BitcoinCoreSv2TDP::new(
                &bitcoin_core_config.unix_socket_path,
                bitcoin_core_config.fee_threshold,
                bitcoin_core_config.min_interval,
                bitcoin_core_config.incoming_tdp_receiver,
                bitcoin_core_config.outgoing_tdp_sender,
                bitcoin_core_config.cancellation_token.clone(),
            )
            .await
            {
                Ok(sv2_bitcoin_core) => sv2_bitcoin_core,
                Err(e) => {
                    tracing::error!("Failed to create BitcoinCoreToSv2: {:?}", e);
                    bitcoin_core_config.cancellation_token.cancel();
                    return;
                }
            };

            // run the BitcoinCoreSv2TDP instance, which will block until the cancellation token is
            // activated
            sv2_bitcoin_core.run().await;
        });
    })
}
