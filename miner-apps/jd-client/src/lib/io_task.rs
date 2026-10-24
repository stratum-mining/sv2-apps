use std::sync::Arc;

use async_channel::{Receiver, Sender};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    channel_utils::ReceiverCleanup,
    fallback_coordinator::FallbackCoordinator,
    network_helpers::{
        Error as NetworkError,
        noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf},
    },
    task_manager::TaskManager,
    utils::types::{InboundFrame, OutboundFrame},
};
use tracing::{Instrument as _, debug, error, trace};

// Spawns `fut` through `task_manager`, tracked by `fallback_coordinator` when there is one.
#[track_caller]
fn spawn_io_task<F>(
    task_manager: &TaskManager,
    fallback_coordinator: Option<&FallbackCoordinator>,
    fut: F,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match fallback_coordinator {
        Some(fallback_coordinator) => task_manager.spawn(fallback_coordinator.track(fut)),
        None => task_manager.spawn(fut),
    }
}

/// Spawns async reader and writer tasks for handling framed I/O with shutdown support.
///
/// If a fallback coordinator is provided, both tasks are tracked by it and listen for fallback
/// cancellation.
#[track_caller]
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(test), hotpath::measure)]
pub fn spawn_io_tasks(
    task_manager: Arc<TaskManager>,
    mut reader: NoiseTcpReadHalf,
    mut writer: NoiseTcpWriteHalf,
    outbound_rx: Receiver<OutboundFrame>,
    inbound_tx: Sender<InboundFrame>,
    cancellation_token: CancellationToken,
    fallback_coordinator: Option<FallbackCoordinator>,
) {
    let caller = std::panic::Location::caller();
    let inbound_tx_clone = inbound_tx.clone();
    let outbound_rx_clone = outbound_rx.clone();
    // A connection outside fallback coordination gets a token that never fires.
    let fallback_token = fallback_coordinator
        .as_ref()
        .map_or_else(CancellationToken::new, |c| c.token());

    {
        let cancellation_token_clone = cancellation_token.clone();
        let fallback_token = fallback_token.clone();

        spawn_io_task(
            &task_manager,
            fallback_coordinator.as_ref(),
            async move {
                trace!("Reader task started");

                loop {
                    tokio::select! {
                        biased;
                        _ = cancellation_token_clone.cancelled() => {
                            trace!("Received shutdown signal");
                            inbound_tx.close();
                            break;
                        }
                        _ = fallback_token.cancelled() => {
                            trace!("Received fallback signal");
                            inbound_tx.close();
                            break;
                        }
                        res = reader.read_frame() => {
                            match res {
                                Ok(sv2_frame) => {
                                    trace!("Received inbound frame");
                                    if let Err(e) = inbound_tx.send(sv2_frame).await {
                                        inbound_tx.close();
                                        debug!(error=?e, "Inbound channel closed, stopping reader");
                                        break;
                                    }
                                }
                                Err(NetworkError::SocketClosed) => {
                                    debug!("Socket closed by remote peer");
                                    inbound_tx.close();
                                    break;
                                }
                                Err(e) => {
                                    error!(error = %e, "Failed to read or decode frame from peer; terminating connection");
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

                debug!("Reader task exited");
            }
            .instrument(tracing::trace_span!(
                "reader_task",
                spawned_at = %format!("{}:{}", caller.file(), caller.line())
            )),
        );
    }

    spawn_io_task(
        &task_manager,
        fallback_coordinator.as_ref(),
        async move {
            trace!("Writer task started");

            loop {
                tokio::select! {
                    biased;
                    _ = cancellation_token.cancelled() => {
                        trace!("Received shutdown signal");
                        inbound_tx_clone.close();
                        break;
                    }
                    _ = fallback_token.cancelled() => {
                        trace!("Received fallback signal");
                        inbound_tx_clone.close();
                        break;
                    }
                    res = outbound_rx.recv() => {
                        match res {
                            Ok(frame) => {
                                trace!("Sending outbound frame");
                                match writer.write_frame(frame).await {
                                    Ok(()) => {}
                                    Err(NetworkError::SocketClosed) => {
                                        debug!("Socket closed by remote peer while writing");
                                        outbound_rx.close_and_drain();
                                        break;
                                    }
                                    Err(e) => {
                                        error!(error = %e, "Failed to encode or send frame to peer; terminating connection");
                                        outbound_rx.close_and_drain();
                                        break;
                                    }
                                }
                            }
                            Err(_) => {
                                outbound_rx.close_and_drain();
                                debug!("Outbound channel closed");
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

            debug!("Writer task exited");
        }
        .instrument(tracing::trace_span!(
            "writer_task",
            spawned_at = %format!("{}:{}", caller.file(), caller.line())
        )),
    );
}
