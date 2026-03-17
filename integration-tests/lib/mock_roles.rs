use crate::utils::{create_downstream, create_upstream, message_from_frame, wait_for_client};
use async_channel::Sender;
use std::{convert::TryInto, net::SocketAddr};
use stratum_apps::{
    stratum_core::{
        codec_sv2::StandardEitherFrame,
        common_messages_sv2::{
            Protocol, SetupConnection, SetupConnectionError, SetupConnectionSuccess,
            MESSAGE_TYPE_SETUP_CONNECTION,
        },
        parsers_sv2::{AnyMessage, CommonMessages, IsSv2Message},
    },
    utils::types::Sv2Frame,
};
use tokio::net::TcpStream;
use tracing::info;

pub enum WithSetup {
    Yes(SetupConnection<'static>),
    No,
}

impl WithSetup {
    pub fn yes_with_defaults(protocol: Protocol, flags: u32) -> Self {
        WithSetup::Yes(SetupConnection {
            protocol,
            min_version: 2,
            max_version: 2,
            flags,
            endpoint_host: b"0.0.0.0".to_vec().try_into().unwrap(),
            endpoint_port: 0,
            vendor: b"integration-test".to_vec().try_into().unwrap(),
            hardware_version: b"".to_vec().try_into().unwrap(),
            firmware: b"".to_vec().try_into().unwrap(),
            device_id: b"".to_vec().try_into().unwrap(),
        })
    }

    pub fn yes(setup_connection: SetupConnection<'static>) -> Self {
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

    pub async fn start(self) -> Sender<AnyMessage<'static>> {
        let upstream_address = self.upstream_address;

        let (proxy_sender, proxy_receiver) = async_channel::unbounded::<AnyMessage<'static>>();

        let (upstream_receiver, upstream_sender) = create_upstream(loop {
            match TcpStream::connect(upstream_address).await {
                Ok(stream) => break stream,
                Err(_) => {
                    tracing::warn!(
                        "MockDownstream: unable to connect to upstream, retrying after 1 second"
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
            }
        })
        .await
        .expect("Failed to create upstream");

        if let WithSetup::Yes(setup_connection) = self.setup {
            let protocol = setup_connection.protocol;
            let flags = setup_connection.flags;
            let msg = AnyMessage::Common(CommonMessages::SetupConnection(setup_connection));
            let message_type = msg.message_type();
            let frame = StandardEitherFrame::<AnyMessage<'_>>::Sv2(
                Sv2Frame::from_message(msg, message_type, 0, false)
                    .expect("Failed to create SetupConnection frame"),
            );
            upstream_sender
                .send(frame)
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
                let frame = StandardEitherFrame::<AnyMessage<'_>>::Sv2(
                    Sv2Frame::from_message(message, message_type, 0, false)
                        .expect("Failed to create frame from message"),
                );
                if upstream_sender.send(frame).await.is_err() {
                    break;
                }
            }
        });

        proxy_sender
    }
}

pub struct MockUpstream {
    listening_address: SocketAddr,
    setup: WithSetup,
}

impl MockUpstream {
    pub fn new(listening_address: SocketAddr, setup: WithSetup) -> Self {
        Self {
            listening_address,
            setup,
        }
    }

    pub async fn start(self) -> Sender<AnyMessage<'static>> {
        let listening_address = self.listening_address;

        let (proxy_sender, proxy_receiver) = async_channel::unbounded::<AnyMessage<'static>>();

        tokio::spawn(async move {
            let (downstream_receiver, downstream_sender) =
                create_downstream(wait_for_client(listening_address).await)
                    .await
                    .expect("Failed to connect to downstream");

            if let WithSetup::Yes(expected_setup) = self.setup {
                let expected_protocol = expected_setup.protocol;
                let flags = expected_setup.flags;

                let mut frame = downstream_receiver
                    .recv()
                    .await
                    .expect("Failed to receive first message from downstream");
                let (msg_type, msg) = message_from_frame(&mut frame);
                info!(
                    "MockUpstream: received message from downstream: {} {}",
                    msg_type, msg
                );

                if msg_type == MESSAGE_TYPE_SETUP_CONNECTION {
                    if let AnyMessage::Common(CommonMessages::SetupConnection(setup_msg)) = &msg {
                        if setup_msg.protocol == expected_protocol {
                            let success = AnyMessage::Common(
                                CommonMessages::SetupConnectionSuccess(SetupConnectionSuccess {
                                    used_version: 2,
                                    flags,
                                }),
                            );
                            let success_type = success.message_type();
                            let response_frame = StandardEitherFrame::<AnyMessage<'_>>::Sv2(
                                Sv2Frame::from_message(success, success_type, 0, false)
                                    .expect("Failed to create SetupConnectionSuccess frame"),
                            );
                            downstream_sender
                                .send(response_frame)
                                .await
                                .expect("Failed to send SetupConnectionSuccess");
                            info!(
                                "MockUpstream: sent SetupConnectionSuccess with flags {}",
                                flags
                            );
                        } else {
                            let error = AnyMessage::Common(CommonMessages::SetupConnectionError(
                                SetupConnectionError {
                                    flags: 0,
                                    error_code: "unsupported-protocol"
                                        .to_string()
                                        .into_bytes()
                                        .try_into()
                                        .unwrap(),
                                },
                            ));
                            let error_type = error.message_type();
                            let response_frame = StandardEitherFrame::<AnyMessage<'_>>::Sv2(
                                Sv2Frame::from_message(error, error_type, 0, false)
                                    .expect("Failed to create SetupConnectionError frame"),
                            );
                            downstream_sender
                                .send(response_frame)
                                .await
                                .expect("Failed to send SetupConnectionError");
                            info!(
                                "MockUpstream: sent SetupConnectionError for wrong protocol {:?}, expected {:?}",
                                setup_msg.protocol, expected_protocol
                            );
                        }
                    }
                } else {
                    panic!(
                        "MockUpstream: first message must be SetupConnection, got {}",
                        msg_type
                    );
                }
            }

            tokio::spawn(async move {
                while let Ok(mut frame) = downstream_receiver.recv().await {
                    let (msg_type, msg) = message_from_frame(&mut frame);
                    info!(
                        "MockUpstream: received message from downstream: {} {}",
                        msg_type, msg
                    );
                }
            });

            while let Ok(message) = proxy_receiver.recv().await {
                let message_type = message.message_type();
                let frame = StandardEitherFrame::<AnyMessage<'_>>::Sv2(
                    Sv2Frame::from_message(message, message_type, 0, false)
                        .expect("Failed to create frame from message"),
                );
                if downstream_sender.send(frame).await.is_err() {
                    break;
                }
            }
        });

        proxy_sender
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{interceptor::MessageDirection, start_sniffer};
    use std::net::TcpListener;
    use stratum_apps::stratum_core::{
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS,
        },
        mining_sv2::MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
    };

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
