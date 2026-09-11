use std::sync::Arc;

use async_channel::{Receiver, Sender};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf},
    task_manager::TaskManager,
    utils::types::{InboundFrame, OutboundFrame},
};
use tracing::{Instrument as _, error, trace, warn};

/// Spawns async reader and writer tasks for handling framed I/O with shutdown support.
#[track_caller]
#[cfg_attr(not(test), hotpath::measure)]
pub fn spawn_io_tasks(
    task_manager: Arc<TaskManager>,
    mut reader: NoiseTcpReadHalf,
    mut writer: NoiseTcpWriteHalf,
    outbound_rx: Receiver<OutboundFrame>,
    inbound_tx: Sender<InboundFrame>,
    cancellation_token: CancellationToken,
) {
    let caller = std::panic::Location::caller();
    let inbound_tx_clone = inbound_tx.clone();
    let outbound_rx_clone = outbound_rx.clone();

    {
        let cancellation_token = cancellation_token.clone();

        task_manager.spawn(
            async move {
                trace!("Reader task started");
                loop {
                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            trace!("Received shutdown");
                            inbound_tx.close();
                            break;
                        }
                        res = reader.read_frame() => {
                            match res {
                                Ok(sv2_frame) => {
                                    trace!("Received inbound frame");
                                    if let Err(e) = inbound_tx.send(sv2_frame).await {
                                        inbound_tx.close();
                                        error!(error=?e, "Failed to forward inbound frame");
                                        break;
                                    }
                                }
                                Err(e) => {
                                    error!(error=?e, "Reader error");
                                    inbound_tx.close();
                                    break;
                                }
                            }
                        }
                    }
                }
                inbound_tx.close();
                outbound_rx_clone.close_and_drain();
                drop(inbound_tx);
                drop(outbound_rx_clone);
                warn!("Reader task exited.");
            }
            .instrument(tracing::trace_span!(
                "reader_task",
                spawned_at = %format!("{}:{}", caller.file(), caller.line())
            )),
        );
    }

    {
        let cancellation_token = cancellation_token.clone();
        task_manager.spawn(
            async move {
                trace!("Writer task started");
                loop {
                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            trace!("Received shutdown");
                            outbound_rx.close_and_drain();
                            break;
                        }
                        res = outbound_rx.recv() => {
                            match res {
                                Ok(frame) => {
                                    trace!("Sending outbound frame");
                                    if let Err(e) = writer.write_frame(frame).await {
                                        error!(error=?e, "Writer error");
                                        outbound_rx.close_and_drain();
                                        break;
                                    }
                                }
                                Err(_) => {
                                    outbound_rx.close_and_drain();
                                    warn!("Outbound channel closed");
                                    break;
                                }
                            }
                        }
                    }
                }
                outbound_rx.close_and_drain();
                inbound_tx_clone.close();
                drop(outbound_rx);
                drop(inbound_tx_clone);
                warn!("Writer task exited.");
            }
            .instrument(tracing::trace_span!(
                "writer_task",
                spawned_at = %format!("{}:{}", caller.file(), caller.line())
            )),
        );
    }
}
