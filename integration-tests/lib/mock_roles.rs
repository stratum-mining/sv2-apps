use crate::{
    interceptor::MessageDirection,
    sniffer::Sniffer,
    start_sniffer,
    types::InboundFrame,
    utils::{CONNECT_RETRY_INTERVAL, create_downstream, create_upstream, message_from_frame},
};
use async_channel::{Receiver, Sender};
use std::{
    convert::TryInto,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use stratum_apps::{
    stratum_core::{
        binary_sv2::Seq064KOwned,
        codec_sv2::{MessageFrame, SerializedFrame},
        common_messages_sv2::{
            ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol, SetupConnectionErrorOwned,
            SetupConnectionOwned, SetupConnectionSuccessOwned,
        },
        extensions_sv2::RequestExtensionsOwned,
        mining_sv2::{
            MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS, OpenExtendedMiningChannelOwned,
            OpenExtendedMiningChannelSuccessOwned, OpenStandardMiningChannelOwned,
            UpdateChannelOwned,
        },
        parsers_sv2::{
            AnyMessageOwned, CommonMessagesOwned, IsSv2Message, MiningOwned,
            TemplateDistributionOwned,
        },
        template_distribution_sv2::{NewTemplateOwned, SetNewPrevHashOwned},
    },
    utils::types::OutboundFrame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::info;

pub enum WithSetup {
    Yes(SetupConnectionOwned),
    No,
}

impl WithSetup {
    pub fn yes_with_defaults(protocol: Protocol, flags: u32) -> Self {
        WithSetup::Yes(SetupConnectionOwned {
            protocol,
            min_version: 2,
            max_version: 2,
            flags,
            endpoint_host: "0.0.0.0".try_into().unwrap(),
            endpoint_port: 0,
            vendor: "integration-test".try_into().unwrap(),
            hardware_version: "".try_into().unwrap(),
            firmware: "".try_into().unwrap(),
            device_id: "".try_into().unwrap(),
        })
    }

    pub fn yes(setup_connection: SetupConnectionOwned) -> Self {
        WithSetup::Yes(setup_connection)
    }

    pub fn no() -> Self {
        WithSetup::No
    }
}

pub struct MockDownstream {
    upstream_address: SocketAddr,
    setup: WithSetup,
}

impl MockDownstream {
    pub fn new(upstream_address: SocketAddr, setup: WithSetup) -> Self {
        Self {
            upstream_address,
            setup,
        }
    }

    pub async fn start(self) -> Sender<AnyMessageOwned> {
        let upstream_address = self.upstream_address;

        let (proxy_sender, proxy_receiver) = async_channel::unbounded::<AnyMessageOwned>();

        let (upstream_receiver, upstream_sender) = create_upstream(loop {
            match TcpStream::connect(upstream_address).await {
                Ok(stream) => break stream,
                Err(_) => {
                    tracing::warn!("MockDownstream: unable to connect to upstream, retrying");
                    tokio::time::sleep(crate::utils::CONNECT_RETRY_INTERVAL).await;
                }
            }
        })
        .await
        .expect("Failed to create upstream");

        if let WithSetup::Yes(setup_connection) = self.setup {
            let protocol = setup_connection.protocol;
            let flags = setup_connection.flags;
            let msg =
                AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup_connection));
            let message_type = msg.message_type();
            let frame = MessageFrame::from_message(msg, message_type, 0, false)
                .expect("Failed to create SetupConnection frame");
            upstream_sender
                .send(frame.into())
                .await
                .expect("Failed to send SetupConnection");
            info!(
                "MockDownstream: sent SetupConnection with protocol {:?} and flags {}",
                protocol, flags
            );
        }

        tokio::spawn(async move {
            while let Ok(mut frame) = upstream_receiver.recv().await {
                let (msg_type, msg) = message_from_frame(&mut frame);
                info!(
                    "MockDownstream: received message from upstream: {} {}",
                    msg_type, msg
                );
            }
        });

        tokio::spawn(async move {
            while let Ok(message) = proxy_receiver.recv().await {
                let message_type = message.message_type();
                let frame = MessageFrame::from_message(message, message_type, 0, false)
                    .expect("Failed to create frame from message");
                if upstream_sender.send(frame.into()).await.is_err() {
                    break;
                }
            }
        });

        proxy_sender
    }
}

/// Decides how a [`MockUpstream`] answers one message from a downstream. Returning an empty
/// vector leaves the message unanswered.
type MessageHandler = dyn Fn(AnyMessageOwned) -> Vec<OutboundFrame> + Send + Sync;

type Connections = Arc<Mutex<Vec<(u64, Sender<OutboundFrame>)>>>;

/// A running [`MockUpstream`].
pub struct MockUpstreamHandle {
    to_downstream: Sender<AnyMessageOwned>,
    next_connection: Receiver<MockConnection>,
    stop_accepting: CancellationToken,
}

impl MockUpstreamHandle {
    /// Sends `message` to every connection the mock has open. Messages sent before the first
    /// connection are delivered once it is set up.
    pub async fn send(
        &self,
        message: AnyMessageOwned,
    ) -> Result<(), async_channel::SendError<AnyMessageOwned>> {
        self.to_downstream.send(message).await
    }

    /// Waits for the next connection the mock accepts to finish setting up.
    pub async fn next_connection(&self) -> MockConnection {
        self.next_connection
            .recv()
            .await
            .expect("the mock upstream accepts a connection")
    }

    /// Stops accepting new connections. Connections already open stay open.
    pub fn stop_accepting(&self) {
        self.stop_accepting.cancel();
    }
}

/// One connection accepted by a [`MockUpstream`].
pub struct MockConnection {
    id: u64,
    sender: Sender<OutboundFrame>,
    receiver: Receiver<InboundFrame>,
    closed: watch::Receiver<bool>,
}

impl MockConnection {
    /// The connection's ID, starting at 0 for the first connection the mock accepts.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sends `message` on this connection alone.
    pub async fn send(
        &self,
        message: AnyMessageOwned,
    ) -> Result<(), async_channel::SendError<OutboundFrame>> {
        self.sender
            .send(OutboundFrame::from_message(message).expect("the message can be framed"))
            .await
    }

    /// Closes the connection.
    pub fn close(&self) {
        self.sender.close();
        self.receiver.close();
    }

    /// Waits until the connection is closed, by either side.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        while !*closed.borrow() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }
}

/// A mock upstream server.
pub struct MockUpstream {
    listening_address: SocketAddr,
    setup: WithSetup,
    handler: Option<Arc<MessageHandler>>,
}

impl MockUpstream {
    pub fn new(listening_address: SocketAddr, setup: WithSetup) -> Self {
        Self {
            listening_address,
            setup,
            handler: None,
        }
    }

    /// Answers every message from every connection through `handler`, so a test decides what the
    /// client gets back, malformed frames included. The handler is shared by all the connections
    /// the mock accepts, and returning an empty vector leaves a message unanswered. When the mock
    /// answers `SetupConnection` itself, the handler only sees the messages that follow it.
    pub fn with_handler<H>(mut self, handler: H) -> Self
    where
        H: Fn(AnyMessageOwned) -> Vec<OutboundFrame> + Send + Sync + 'static,
    {
        self.handler = Some(Arc::new(handler));
        self
    }

    /// Starts accepting connections to the mock's address.
    pub async fn start(self) -> MockUpstreamHandle {
        let Self {
            listening_address,
            setup,
            handler,
        } = self;

        let listener = TcpListener::bind(listening_address)
            .await
            .expect("the mock upstream's address is available");
        let (to_downstream, downstream_messages) = async_channel::unbounded::<AnyMessageOwned>();
        let (connection_sender, next_connection) = async_channel::unbounded::<MockConnection>();
        let stop_accepting = CancellationToken::new();
        let connections: Connections = Arc::new(Mutex::new(Vec::new()));
        let connection_ready = Arc::new(Notify::new());
        let setup = match setup {
            WithSetup::Yes(setup) => Some((setup.protocol, setup.flags)),
            WithSetup::No => None,
        };

        tokio::spawn({
            let connections = Arc::clone(&connections);
            let connection_ready = Arc::clone(&connection_ready);
            let stop_accepting = stop_accepting.clone();
            async move {
                let mut connection_id = 0;
                loop {
                    let stream = tokio::select! {
                        _ = stop_accepting.cancelled() => break,
                        accepted = listener.accept() => accepted.expect("the client connects").0,
                    };
                    tokio::spawn(run_connection(
                        stream,
                        connection_id,
                        ConnectionContext {
                            setup,
                            connections: Arc::clone(&connections),
                            connection_ready: Arc::clone(&connection_ready),
                            handler: handler.clone(),
                            connection_sender: connection_sender.clone(),
                        },
                    ));
                    connection_id += 1;
                }
            }
        });

        tokio::spawn(async move {
            while let Ok(message) = downstream_messages.recv().await {
                loop {
                    let senders = connections
                        .lock()
                        .expect("the mock upstream's connections are not poisoned")
                        .clone();
                    if senders.is_empty() {
                        connection_ready.notified().await;
                        continue;
                    }
                    for (_, sender) in senders {
                        let frame = OutboundFrame::from_message(message.clone())
                            .expect("the message can be framed");
                        _ = sender.send(frame).await;
                    }
                    break;
                }
            }
        });

        MockUpstreamHandle {
            to_downstream,
            next_connection,
            stop_accepting,
        }
    }
}

struct ConnectionContext {
    setup: Option<(Protocol, u32)>,
    connections: Connections,
    connection_ready: Arc<Notify>,
    handler: Option<Arc<MessageHandler>>,
    connection_sender: Sender<MockConnection>,
}

async fn run_connection(stream: TcpStream, connection_id: u64, context: ConnectionContext) {
    let ConnectionContext {
        setup,
        connections,
        connection_ready,
        handler,
        connection_sender,
    } = context;
    let (receiver, sender) = create_downstream(stream)
        .await
        .expect("the noise handshake completes");
    let (closed_sender, closed) = watch::channel(false);

    if let Some((expected_protocol, flags)) = setup {
        let mut frame = receiver
            .recv()
            .await
            .expect("the client sends a SetupConnection");
        let (msg_type, msg) = message_from_frame(&mut frame);
        info!("MockUpstream: received message from downstream: {msg_type} {msg}");
        let AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup_msg)) = &msg else {
            panic!("MockUpstream: first message must be SetupConnection, got {msg_type}");
        };

        if setup_msg.protocol == expected_protocol {
            let success = AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(
                SetupConnectionSuccessOwned {
                    used_version: 2,
                    flags,
                },
            ));
            let success_type = success.message_type();
            let response_frame = MessageFrame::from_message(success, success_type, 0, false)
                .expect("Failed to create SetupConnectionSuccess frame");

            sender
                .send(response_frame.into())
                .await
                .expect("Failed to send SetupConnectionSuccess");
            info!("MockUpstream: sent SetupConnectionSuccess with flags {flags}");
        } else {
            let error = AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionError(
                SetupConnectionErrorOwned {
                    flags: 0,
                    error_code: ERROR_CODE_SETUP_CONNECTION_UNSUPPORTED_PROTOCOL
                        .to_string()
                        .into_bytes()
                        .try_into()
                        .unwrap(),
                },
            ));

            let error_type = error.message_type();
            let response_frame = MessageFrame::from_message(error, error_type, 0, false)
                .expect("Failed to create SetupConnectionError frame");

            sender
                .send(response_frame.into())
                .await
                .expect("Failed to send SetupConnectionError");

            info!(
                "MockUpstream: sent SetupConnectionError for a wrong protocol {:?}, expected {:?}",
                setup_msg.protocol, expected_protocol
            );
        }
    }

    connections
        .lock()
        .expect("the mock upstream's connections are not poisoned")
        .push((connection_id, sender.clone()));
    connection_ready.notify_one();

    _ = connection_sender
        .send(MockConnection {
            id: connection_id,
            sender: sender.clone(),
            receiver: receiver.clone(),
            closed,
        })
        .await;

    'connection: while let Ok(mut frame) = receiver.recv().await {
        let (msg_type, msg) = message_from_frame(&mut frame);
        info!("MockUpstream: received message from downstream: {msg_type} {msg}");
        if let Some(handler) = &handler {
            for reply in handler(msg) {
                if sender.send(reply).await.is_err() {
                    break 'connection;
                }
            }
        }
    }

    connections
        .lock()
        .expect("the mock upstream's connections are not poisoned")
        .retain(|(id, _)| *id != connection_id);
    _ = closed_sender.send(true);
}

/// A `SetupConnection.Success` accepting version 2 with no flags.
pub fn setup_connection_success() -> OutboundFrame {
    OutboundFrame::from_message(AnyMessageOwned::Common(
        CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
            used_version: 2,
            flags: 0,
        }),
    ))
    .expect("SetupConnection.Success can be framed")
}

/// A `SetupConnection.Success` header with an empty payload: the six bytes of `used_version` and
/// `flags` are missing, so the message can't be parsed.
pub fn truncated_setup_connection_success() -> OutboundFrame {
    let frame = SerializedFrame::from_bytes(
        vec![0, 0, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, 0, 0, 0].into(),
    )
    .expect("a header with an empty payload is a whole frame");
    OutboundFrame::Raw(frame)
}

/// A known-good template under `template_id`, also used by the channel job-factory tests.
pub fn template(template_id: u64, future_template: bool) -> NewTemplateOwned {
    NewTemplateOwned {
        template_id,
        future_template,
        version: 536_870_912,
        coinbase_tx_version: 2,
        coinbase_prefix: vec![82, 0]
            .try_into()
            .expect("coinbase prefix must fit into B0255"),
        coinbase_tx_input_sequence: u32::MAX,
        coinbase_tx_value_remaining: 5_000_000_000,
        coinbase_tx_outputs_count: 1,
        coinbase_tx_outputs: vec![
            0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 226, 246, 28, 63, 113, 209,
            222, 253, 63, 169, 153, 223, 163, 105, 83, 117, 92, 105, 6, 137, 121, 153, 98, 180,
            139, 235, 216, 54, 151, 78, 140, 249,
        ]
        .try_into()
        .expect("coinbase outputs must fit into B064K"),
        coinbase_tx_locktime: 0,
        merkle_path: vec![].try_into().expect("empty merkle path must be valid"),
    }
}

/// Sends `template` from the mock Template Provider behind `tp`.
pub async fn send_template(tp: &MockUpstreamHandle, template: NewTemplateOwned) {
    tp.send(AnyMessageOwned::TemplateDistribution(
        TemplateDistributionOwned::NewTemplate(template),
    ))
    .await
    .expect("mock TP should send NewTemplate");
}

/// Sends, from the mock Template Provider behind `tp`, future template `template_id` and the
/// prevhash activating it.
pub async fn send_chain_tip(tp: &MockUpstreamHandle, template_id: u64) {
    send_template(tp, template(template_id, true)).await;
    tp.send(AnyMessageOwned::TemplateDistribution(
        TemplateDistributionOwned::SetNewPrevHash(SetNewPrevHashOwned {
            template_id,
            prev_hash: [template_id as u8; 32].into(),
            ntime_start: 1_700_000_000,
            n_bits: 0x1d00ffff,
            target: [0xff; 32].into(),
        }),
    ))
    .await
    .expect("mock TP should send SetNewPrevHash");
}

/// A Pool's success for `request`, opening the channel with what the request asked for.
pub fn channel_open_success(
    request: &OpenExtendedMiningChannelOwned,
) -> OpenExtendedMiningChannelSuccessOwned {
    OpenExtendedMiningChannelSuccessOwned {
        request_id: request.request_id,
        channel_id: 9,
        target: request.max_target.clone(),
        extranonce_size: request.min_extranonce_size,
        extranonce_prefix: vec![0; 4]
            .try_into()
            .expect("extranonce prefix must fit into B032"),
        group_channel_id: 1,
    }
}

/// A downstream's request for an extended channel, with `max_target` in little-endian, as on the
/// wire.
pub fn extended_channel_request(max_target: [u8; 32]) -> AnyMessageOwned {
    AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(
        OpenExtendedMiningChannelOwned {
            request_id: 1,
            user_identity: "mock-downstream".try_into().unwrap(),
            nominal_hash_rate: 1_000.0,
            max_target: max_target.into(),
            min_extranonce_size: 0,
        },
    ))
}

/// A downstream's request for a standard channel, with `max_target` in little-endian, as on the
/// wire.
pub fn standard_channel_request(max_target: [u8; 32]) -> AnyMessageOwned {
    AnyMessageOwned::Mining(MiningOwned::OpenStandardMiningChannel(
        OpenStandardMiningChannelOwned {
            request_id: 1,
            user_identity: "mock-downstream".try_into().unwrap(),
            nominal_hash_rate: 1_000.0,
            max_target: max_target.into(),
        },
    ))
}

/// A downstream's `RequestExtensions` asking for `extensions`.
pub fn request_extensions(extensions: Vec<u16>) -> AnyMessageOwned {
    AnyMessageOwned::Extensions(
        RequestExtensionsOwned {
            request_id: 1,
            requested_extensions: Seq064KOwned::new(extensions)
                .expect("the extensions fit into Seq064K"),
        }
        .into(),
    )
}

/// A mock downstream connected to a server through a sniffer, so a test sends what the downstream
/// sends and observes what the server sends back.
pub struct SniffedDownstream {
    pub sniffer: Sniffer<'static>,
    sender: Sender<AnyMessageOwned>,
}

impl SniffedDownstream {
    /// Connects to `server` through a sniffer named `identifier`, and returns once the server has
    /// accepted the connection.
    pub async fn connect(server: SocketAddr, identifier: &'static str) -> Self {
        let (sniffer, sniffer_address) = start_sniffer(identifier, server, false, vec![], None);
        let sender = MockDownstream::new(
            sniffer_address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;
        let downstream = Self { sniffer, sender };
        downstream
            .wait_for(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
        downstream
    }

    /// Asks for an extended channel with the easiest `max_target`.
    pub async fn request_extended_channel(&self) {
        self.request_extended_channel_with_max_target([0xff; 32])
            .await;
    }

    /// Asks for an extended channel with `max_target`, in little-endian, as on the wire.
    pub async fn request_extended_channel_with_max_target(&self, max_target: [u8; 32]) {
        self.sender
            .send(extended_channel_request(max_target))
            .await
            .expect("mock downstream should request an extended channel");
    }

    /// Sends an `UpdateChannel` for `channel_id`, with `max_target` in little-endian, as on the
    /// wire.
    pub async fn update_channel(&self, channel_id: u32, max_target: [u8; 32]) {
        self.sender
            .send(AnyMessageOwned::Mining(MiningOwned::UpdateChannel(
                UpdateChannelOwned {
                    channel_id,
                    nominal_hash_rate: 1_000.0,
                    max_target: max_target.into(),
                },
            )))
            .await
            .expect("mock downstream should send UpdateChannel");
    }

    /// Waits for the server to open this downstream's extended channel, and returns the channel's
    /// ID.
    pub async fn opened_channel_id(&self) -> u32 {
        self.wait_for(MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS)
            .await;
        loop {
            if let Some((
                _,
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannelSuccess(success)),
            )) = self.sniffer.next_message_from_upstream()
            {
                return success.channel_id;
            }
        }
    }

    /// Waits for the server to send this downstream a message of `message_type`.
    pub async fn wait_for(&self, message_type: u8) {
        self.sniffer
            .wait_for_message_type(MessageDirection::ToDownstream, message_type)
            .await;
    }
}

/// Connects a downstream straight to `server`, without a mock role or a sniffer, so the test reads
/// every frame the server sends and closes the connection by closing the sender. Returns once the
/// server has accepted the connection's `SetupConnection`.
pub async fn connect_closable_downstream(
    server: SocketAddr,
) -> (Receiver<InboundFrame>, Sender<OutboundFrame>) {
    let stream = loop {
        match TcpStream::connect(server).await {
            Ok(stream) => break stream,
            Err(_) => tokio::time::sleep(CONNECT_RETRY_INTERVAL).await,
        }
    };
    let (receiver, sender) = create_upstream(stream)
        .await
        .expect("the noise handshake completes");
    let WithSetup::Yes(setup_connection) =
        WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0)
    else {
        unreachable!("a SetupConnection was asked for");
    };
    send_message(
        &sender,
        AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup_connection)),
    )
    .await;
    assert_eq!(
        next_message_type(&receiver).await,
        MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS
    );
    (receiver, sender)
}

/// Frames `message` and sends it on a connection from [`connect_closable_downstream`].
pub async fn send_message(sender: &Sender<OutboundFrame>, message: AnyMessageOwned) {
    sender
        .send(OutboundFrame::from_message(message).expect("the message can be framed"))
        .await
        .expect("the downstream connection is open");
}

/// Waits for the next frame on a connection from [`connect_closable_downstream`], and returns its
/// message type.
pub async fn next_message_type(receiver: &Receiver<InboundFrame>) -> u8 {
    receiver
        .recv()
        .await
        .expect("the server keeps the connection open")
        .header()
        .msg_type()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{interceptor::MessageDirection, start_sniffer, utils::get_available_address};
    use std::{
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use stratum_apps::stratum_core::{
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        },
        mining_sv2::MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
    };

    #[tokio::test]
    async fn mock_upstream_answers_every_connection_through_one_handler() {
        let address = get_available_address();
        let handled = Arc::new(AtomicUsize::new(0));
        let handler_count = Arc::clone(&handled);

        let mock_upstream = MockUpstream::new(
            address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .with_handler(move |message| {
            handler_count.fetch_add(1, Ordering::SeqCst);
            match message {
                AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(request)) => {
                    vec![
                        OutboundFrame::from_message(AnyMessageOwned::Mining(
                            MiningOwned::OpenExtendedMiningChannelSuccess(channel_open_success(
                                &request,
                            )),
                        ))
                        .expect("OpenExtendedMiningChannel.Success can be framed"),
                    ]
                }
                _ => vec![],
            }
        })
        .start()
        .await;

        for expected_id in 0..2 {
            let (receiver, sender) = connect_closable_downstream(address).await;
            let connection = mock_upstream.next_connection().await;
            assert_eq!(connection.id(), expected_id);

            send_message(&sender, extended_channel_request([0xff_u8; 32])).await;
            assert_eq!(
                next_message_type(&receiver).await,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL_SUCCESS
            );
        }

        assert_eq!(
            handled.load(Ordering::SeqCst),
            2,
            "one handler serves every connection"
        );
    }

    #[tokio::test]
    async fn mock_upstream_answers_setup_connection_through_its_handler() {
        let address = get_available_address();
        let _to_downstream = MockUpstream::new(address, WithSetup::no())
            .with_handler(|message| match message {
                AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(_)) => {
                    vec![setup_connection_success()]
                }
                _ => vec![],
            })
            .start()
            .await;

        let (_receiver, _sender) = connect_closable_downstream(address).await;
    }

    #[tokio::test]
    async fn mock_upstream_reports_closed_connections() {
        let address = get_available_address();
        let mock_upstream = MockUpstream::new(
            address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        let (_receiver, sender) = connect_closable_downstream(address).await;
        let connection = mock_upstream.next_connection().await;
        assert_eq!(connection.id(), 0);
        sender.close();

        tokio::time::timeout(Duration::from_secs(5), connection.closed())
            .await
            .expect("the mock reports the closed connection");
    }

    #[tokio::test]
    async fn mock_upstream_sends_on_one_connection() {
        let address = get_available_address();
        let mock_upstream = MockUpstream::new(
            address,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        let (first_receiver, _first_sender) = connect_closable_downstream(address).await;
        let first_connection = mock_upstream.next_connection().await;
        let (second_receiver, _second_sender) = connect_closable_downstream(address).await;
        let _second_connection = mock_upstream.next_connection().await;

        first_connection
            .send(AnyMessageOwned::Common(
                CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
                    used_version: 2,
                    flags: 0,
                }),
            ))
            .await
            .expect("the connection is open");

        assert_eq!(
            next_message_type(&first_receiver).await,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                next_message_type(&second_receiver)
            )
            .await
            .is_err(),
            "a message sent on one connection must not reach another"
        );
    }

    #[tokio::test]
    async fn test_implicit_setup_connection() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let upstream_socket_addr = SocketAddr::from(([127, 0, 0, 1], port));

        let _mock_upstream = MockUpstream::new(
            upstream_socket_addr,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        let (sniffer, sniffer_addr) = start_sniffer(
            "implicit_setup_test",
            upstream_socket_addr,
            false,
            vec![],
            None,
        );

        let _send_to_upstream = MockDownstream::new(
            sniffer_addr,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        sniffer
            .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
            )
            .await;
    }

    #[tokio::test]
    async fn test_assert_message_not_present() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let upstream_socket_addr = SocketAddr::from(([127, 0, 0, 1], port));

        let _mock_upstream = MockUpstream::new(
            upstream_socket_addr,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        let (sniffer, sniffer_addr) = start_sniffer(
            "assert_not_present_test",
            upstream_socket_addr,
            false,
            vec![],
            None,
        );

        let _send_to_upstream = MockDownstream::new(
            sniffer_addr,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        sniffer
            .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;

        // SetupConnection was sent, so has_message_type should find it
        assert!(
            sniffer.has_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
        );

        // OpenExtendedMiningChannel was never sent, so assert_message_not_present should return
        // true
        assert!(
            sniffer
                .assert_message_not_present(
                    MessageDirection::ToUpstream,
                    MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                    std::time::Duration::from_secs(1),
                )
                .await
        );

        // SetupConnection IS present, so assert_message_not_present should return false
        assert!(
            !sniffer
                .assert_message_not_present(
                    MessageDirection::ToUpstream,
                    MESSAGE_TYPE_SETUP_CONNECTION,
                    std::time::Duration::from_millis(200),
                )
                .await
        );
    }

    #[tokio::test]
    async fn test_setup_connection_wrong_protocol() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let upstream_socket_addr = SocketAddr::from(([127, 0, 0, 1], port));

        let _mock_upstream = MockUpstream::new(
            upstream_socket_addr,
            WithSetup::yes_with_defaults(Protocol::MiningProtocol, 0),
        )
        .start()
        .await;

        let (sniffer, sniffer_addr) = start_sniffer(
            "wrong_protocol_test",
            upstream_socket_addr,
            false,
            vec![],
            None,
        );

        let _send_to_upstream = MockDownstream::new(
            sniffer_addr,
            WithSetup::yes_with_defaults(Protocol::TemplateDistributionProtocol, 0),
        )
        .start()
        .await;

        sniffer
            .wait_for_message_type(MessageDirection::ToUpstream, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;

        sniffer
            .wait_for_message_type(
                MessageDirection::ToDownstream,
                MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
            )
            .await;
    }
}
