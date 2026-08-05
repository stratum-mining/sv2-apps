#![allow(clippy::new_ret_no_self)]
use crate::network_helpers::{
    Error, NOISE_HANDSHAKE_TIMEOUT,
    noise_stream::{NoiseTcpReadHalf, NoiseTcpStream, NoiseTcpWriteHalf},
};
use async_channel::{Receiver, Sender, unbounded};
use std::sync::Arc;
use stratum_core::{
    binary_sv2::{Deserialize, GetSize, Serialize},
    codec_sv2::{HandshakeRole, StandardEitherFrame},
};
use tokio::{net::TcpStream, task};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

pub struct Connection;

struct ConnectionState<Message> {
    sender_incoming: Sender<StandardEitherFrame<Message>>,
    receiver_incoming: Receiver<StandardEitherFrame<Message>>,
    sender_outgoing: Sender<StandardEitherFrame<Message>>,
    receiver_outgoing: Receiver<StandardEitherFrame<Message>>,
}

impl<Message> ConnectionState<Message> {
    fn close_all(&self) {
        self.sender_incoming.close();
        self.receiver_incoming.close();
        self.sender_outgoing.close();
        self.receiver_outgoing.close();
    }
}

impl Connection {
    /// Performs the Noise handshake and spawns the reader and writer tasks.
    ///
    /// Cancelling `cancellation_token` makes both tasks exit and closes all channels.
    pub async fn new<Message>(
        stream: TcpStream,
        role: HandshakeRole,
        cancellation_token: CancellationToken,
    ) -> Result<
        (
            Receiver<StandardEitherFrame<Message>>,
            Sender<StandardEitherFrame<Message>>,
        ),
        Error,
    >
    where
        Message: Serialize + for<'decoder> Deserialize<'decoder> + GetSize + Send + 'static,
    {
        let (sender_incoming, receiver_incoming) = unbounded();
        let (sender_outgoing, receiver_outgoing) = unbounded();

        let conn_state = Arc::new(ConnectionState {
            sender_incoming,
            receiver_incoming: receiver_incoming.clone(),
            sender_outgoing: sender_outgoing.clone(),
            receiver_outgoing,
        });

        let (read_half, write_half) =
            NoiseTcpStream::<Message>::new(stream, role, NOISE_HANDSHAKE_TIMEOUT)
                .await?
                .into_split();

        Self::spawn_reader(
            read_half,
            Arc::clone(&conn_state),
            cancellation_token.clone(),
        );
        Self::spawn_writer(write_half, conn_state, cancellation_token);

        Ok((receiver_incoming, sender_outgoing))
    }
    fn spawn_reader<Message>(
        mut read_half: NoiseTcpReadHalf<Message>,
        conn_state: Arc<ConnectionState<Message>>,
        cancellation_token: CancellationToken,
    ) -> task::JoinHandle<()>
    where
        Message: Serialize + for<'decoder> Deserialize<'decoder> + GetSize + Send + 'static,
    {
        let sender_incoming = conn_state.sender_incoming.clone();

        task::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        debug!("Reader received shutdown signal.");
                        break;
                    }
                    res = read_half.read_frame() => match res {
                        Ok(frame) => {
                            if sender_incoming.send(frame).await.is_err() {
                                error!("Reader: channel closed, shutting down.");
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Reader: error while reading frame: {e:?}");
                            break;
                        }
                    }
                }
            }

            conn_state.close_all();
        })
    }

    fn spawn_writer<Message>(
        mut write_half: NoiseTcpWriteHalf<Message>,
        conn_state: Arc<ConnectionState<Message>>,
        cancellation_token: CancellationToken,
    ) -> task::JoinHandle<()>
    where
        Message: Serialize + for<'decoder> Deserialize<'decoder> + GetSize + Send + 'static,
    {
        let receiver_outgoing = conn_state.receiver_outgoing.clone();

        task::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        debug!("Writer received shutdown signal.");
                        break;
                    }
                    res = receiver_outgoing.recv() => match res {
                        Ok(frame) => {
                            if let Err(e) = write_half.write_frame(frame).await {
                                error!("Writer: error while writing frame: {e:?}");
                                break;
                            }
                        }
                        Err(_) => {
                            debug!("Writer: channel closed, shutting down.");
                            break;
                        }
                    }
                }
            }

            if let Err(e) = write_half.shutdown().await {
                error!("Writer: error during shutdown: {e:?}");
            }

            conn_state.close_all();
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key_utils::{Secp256k1PublicKey, Secp256k1SecretKey};
    use std::time::Duration;
    use stratum_core::{
        noise_sv2::{Initiator, Responder},
        parsers_sv2::AnyMessageOwned,
    };
    use tokio::net::TcpListener;

    // same test authority keypair used by the integration tests
    const PUB_KEY: &str = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72";
    const PRV_KEY: &str = "mkDLTBBRxdBv998612qipDYoTK3YUrqLe8uWw7gu3iXbSrn2n";

    #[tokio::test]
    async fn cancellation_shuts_down_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let responder_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let pub_key = PUB_KEY.parse::<Secp256k1PublicKey>().unwrap().into_bytes();
            let prv_key = PRV_KEY.parse::<Secp256k1SecretKey>().unwrap().into_bytes();
            let responder =
                Responder::from_authority_kp(&pub_key, &prv_key, Duration::from_secs(10_000))
                    .unwrap();
            Connection::new::<AnyMessageOwned>(
                stream,
                HandshakeRole::Responder(responder),
                CancellationToken::new(),
            )
            .await
            .unwrap()
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let initiator = Initiator::without_pk().unwrap();
        let cancellation_token = CancellationToken::new();
        let (receiver, sender) = Connection::new::<AnyMessageOwned>(
            stream,
            HandshakeRole::Initiator(initiator),
            cancellation_token.clone(),
        )
        .await
        .unwrap();
        let _responder_side = responder_task.await.unwrap();

        cancellation_token.cancel();

        // both tasks must exit and close every channel
        assert!(receiver.recv().await.is_err());
        assert!(sender.is_closed());
    }
}
