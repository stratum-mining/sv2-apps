use async_channel::{Receiver, Sender};
use std::{path::PathBuf, sync::Arc, thread::JoinHandle};
use stratum_apps::{
    bitcoin_core_sv2::{
        CancellationToken,
        runtime_api::{BitcoinCoreVersion, template_distribution_protocol},
    },
    stratum_core::parsers_sv2::TemplateDistributionOwned,
    task_manager::TaskManager,
};

#[derive(Clone)]
pub struct BitcoinCoreSv2TDPConfig {
    pub version: BitcoinCoreVersion,
    pub unix_socket_path: PathBuf,
    pub fee_threshold: u64,
    pub min_interval: u8,
    pub incoming_tdp_receiver: Receiver<TemplateDistributionOwned>,
    pub outgoing_tdp_sender: Sender<TemplateDistributionOwned>,
    pub cancellation_token: CancellationToken,
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
pub async fn connect_to_bitcoin_core(
    bitcoin_core_config: BitcoinCoreSv2TDPConfig,
    cancellation_token: CancellationToken,
    task_manager: Arc<TaskManager>,
) -> JoinHandle<()> {
    let bitcoin_core_sv2_token = bitcoin_core_config.cancellation_token.clone();
    let cancellation_token_clone = cancellation_token.clone();

    // spawn a task to handle shutdown signals and cancellation token activations
    task_manager.spawn(async move {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                bitcoin_core_sv2_token.cancel();
            }
            _ = bitcoin_core_sv2_token.cancelled() => {
                cancellation_token.cancel();
            }
        }
    });

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
                cancellation_token_clone.cancel();
                return;
            }
        };
        let tokio_local_set = tokio::task::LocalSet::new();

        tokio_local_set.block_on(&rt, async move {
            // create a new BitcoinCoreSv2TDP instance
            let mut sv2_bitcoin_core = match template_distribution_protocol::new(
                bitcoin_core_config.version,
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
