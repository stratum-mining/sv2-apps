//! A simple example of how to use `BitcoinCoreSv2TDP`.
//!
//! This example demonstrates the pattern used in applications where `BitcoinCoreSv2TDP` is
//! spawned in a dedicated thread with its own Tokio runtime and `LocalSet`. This allows the
//! main application to run in a separate async context while `BitcoinCoreSv2TDP` runs in its
//! own isolated thread.
//!
//! We connect to the Bitcoin Core UNIX socket, and log the received Sv2 Template Distribution
//! Protocol messages.
//!
//! We send a `CoinbaseOutputConstraints` message to the `BitcoinCoreSv2TDP` instance once at
//! startup.
//!
//! `BitcoinCoreSv2TDP` will not start distributing new templates until it receives the first
//! `CoinbaseOutputConstraints` message.

use bitcoin_core_sv2::template_distribution_protocol::BitcoinCoreSv2TDP;
use std::path::Path;

use async_channel::unbounded;
use stratum_core::{
    parsers_sv2::TemplateDistribution,
    template_distribution_sv2::{CoinbaseOutputConstraints, RequestTransactionData},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // the user must provide the path to the Bitcoin Core UNIX socket
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <bitcoin_core_unix_socket_path>", args[0]);
        eprintln!("Example: {} /path/to/bitcoin/regtest/node.sock", args[0]);
        std::process::exit(1);
    }

    let bitcoin_core_unix_socket_path = Path::new(&args[1]);

    // `BitcoinCoreSv2TDP` uses this to cancel internally spawned tasks
    let cancellation_token = CancellationToken::new();

    // get new templates whenever the mempool has changed by more than 100 sats
    let fee_threshold = 100;

    // the minimum interval between template updates in seconds
    let min_interval = 5;

    // these messages are sent into the `BitcoinCoreSv2TDP` instance
    let (msg_sender_into_bitcoin_core_sv2, msg_receiver_into_bitcoin_core_sv2) = unbounded();
    // these messages are received from the `BitcoinCoreSv2TDP` instance
    let (msg_sender_from_bitcoin_core_sv2, msg_receiver_from_bitcoin_core_sv2) = unbounded();

    // clone so we can move it into the thread
    let cancellation_token_clone = cancellation_token.clone();
    let bitcoin_core_unix_socket_path_clone = bitcoin_core_unix_socket_path.to_path_buf();

    // spawn a dedicated thread to run the BitcoinCoreSv2TDP instance
    // because we're limited to tokio::task::LocalSet
    //
    // please note that it's important to keep a reference to the join handle so we can wait for it
    // to finish shutdown this will no longer be a pre-requisite once https://github.com/bitcoin/bitcoin/pull/33676 lands in a release
    // see https://github.com/stratum-mining/sv2-apps/issues/81 for more details
    let join_handle = std::thread::spawn(move || {
        // we need a dedicated runtime so we can spawn an async task inside the LocalSet
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!("Failed to create Tokio runtime: {:?}", e);
                cancellation_token_clone.cancel();
                return;
            }
        };
        let tokio_local_set = tokio::task::LocalSet::new();

        tokio_local_set.block_on(&rt, async move {
            // create a new `BitcoinCoreSv2TDP` instance
            let mut sv2_bitcoin_core = match BitcoinCoreSv2TDP::new(
                &bitcoin_core_unix_socket_path_clone,
                fee_threshold,
                min_interval,
                msg_receiver_into_bitcoin_core_sv2,
                msg_sender_from_bitcoin_core_sv2,
                cancellation_token_clone.clone(),
            )
            .await
            {
                Ok(sv2_bitcoin_core) => sv2_bitcoin_core,
                Err(e) => {
                    tracing::error!("Failed to create BitcoinCoreToSv2: {:?}", e);
                    cancellation_token_clone.cancel();
                    return;
                }
            };

            // run the `BitcoinCoreSv2TDP` instance, which will block until the cancellation token
            // is activated
            sv2_bitcoin_core.run().await;
        });
    });

    // clone so we can move it
    let cancellation_token_clone = cancellation_token.clone();

    // clone so we can move it
    let msg_sender_into_bitcoin_core_sv2_clone = msg_sender_into_bitcoin_core_sv2.clone();

    // a task to consume and log the received Sv2 Template Distribution Protocol messages
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // monitor for Ctrl+C, activating the cancellation token and exiting the loop
                _ = tokio::signal::ctrl_c() => {
                    info!("Ctrl+C received");
                    cancellation_token_clone.cancel();
                    return;
                }
                // monitor potential internal activations of the cancellation token for exiting the loop
                _ = cancellation_token_clone.cancelled() => {
                    info!("Cancellation token activated");
                    return;
                }
                // monitor for Sv2 Template Distribution Protocol messages
                // coming from `BitcoinCoreSv2TDP`
                Ok(template_distribution_message) = msg_receiver_from_bitcoin_core_sv2.recv() => {
                    // log the message
                    info!("Message received: {}", template_distribution_message);

                    // send a RequestTransactionData every time a NewTemplate message is received
                    if let TemplateDistribution::NewTemplate(new_template) = template_distribution_message {
                        let template_id = new_template.template_id;
                        let request_transaction_data = TemplateDistribution::RequestTransactionData(RequestTransactionData {
                            template_id,
                        });

                        match msg_sender_into_bitcoin_core_sv2_clone.send(request_transaction_data).await {
                            Ok(_) => (),
                            Err(e) => {
                                tracing::error!("Failed to send request transaction data: {}", e);
                                cancellation_token_clone.cancel();
                                return;
                            }
                        }
                    }
                }
            }
        }
    });

    // send CoinbaseOutputConstraints once at startup
    //
    // `BitcoinCoreSv2TDP` will not start distributing new templates until it receives the first
    // `CoinbaseOutputConstraints` message.
    let new_coinbase_output_constraints =
        TemplateDistribution::CoinbaseOutputConstraints(CoinbaseOutputConstraints {
            coinbase_output_max_additional_size: 2,
            coinbase_output_max_additional_sigops: 2,
        });

    if let Err(e) = msg_sender_into_bitcoin_core_sv2
        .send(new_coinbase_output_constraints)
        .await
    {
        tracing::error!("Failed to send coinbase output constraints: {}", e);
        cancellation_token.cancel();
    }
    info!("Sent CoinbaseOutputConstraints");

    // wait for the cancellation token to be activated
    cancellation_token.cancelled().await;
    info!("Shutting down...");

    // wait for the dedicated thread to finish shutdown
    join_handle.join().unwrap();
    info!("BitcoinCoreSv2TDP dedicated thread shutdown complete.");
}
