//! Mock Sv2 roles: a [`MockServer`] that roles under test connect to, and a [`MockClient`] that
//! connects to a role under test.
//!
//! A test drives a mock through its methods. It sends messages and raw frames, asserts on the
//! messages the mock has received, closes a connection and waits for the peer to close it.
//!
//! `SetupConnection` is the only exchange a mock performs on its own, and only when its
//! constructor is given one. Every other message is the test's to send: it waits for a message
//! to arrive and then sends the answer.
//!
//! A mock's tasks never panic. When one of them can't go on with a connection, for example because
//! the peer sent a frame that can't be parsed, every method the test then calls on that connection,
//! other than closing it, panics with the reason, in the test's own task.
//!
//! Dropping a mock closes its connections and ends its tasks.

mod client;
mod server;

pub use client::{ClientSetupConnection, MockClient};
pub use server::{ConnectionId, MockServer, ServerSetupConnection};

use crate::{
    message_aggregator::{DEFAULT_TIMEOUT, MessagesAggregator},
    types::{InboundFrame, MsgType},
    utils::{POLL_INTERVAL, try_message_from_frame},
};
use async_channel::{Receiver, Sender};
use std::time::{Duration, Instant};
use stratum_apps::{
    stratum_core::{
        codec_sv2::SerializedFrame,
        parsers_sv2::{AnyMessageOwned, message_type_to_name},
    },
    sync::SharedLock,
    utils::types::OutboundFrame,
};

// One Noise connection of a mock, and the messages received on it. Clones share the connection.
#[derive(Clone)]
struct Connection {
    // Names the mock and the connection in panic messages.
    label: String,
    sender: Sender<OutboundFrame>,
    received: MessagesAggregator,
    // Set once no further message will be queued. The first end is kept.
    end: SharedLock<Option<End>>,
}

// Why a connection queues no further message.
#[derive(Clone)]
enum End {
    // The connection closed, and everything received on it is in `received`.
    Closed,
    // What went wrong on the connection inside a task of the mock. Those tasks don't panic, so
    // every method of the connection other than `close` panics with this instead.
    Failed(String),
}

impl Connection {
    fn new(label: String, sender: Sender<OutboundFrame>) -> Self {
        Self {
            label,
            sender,
            received: MessagesAggregator::new(),
            end: SharedLock::new(None),
        }
    }

    // Parses `frame` and queues its message. A frame that can't be parsed fails the connection.
    fn record(&self, frame: &mut InboundFrame) -> Option<AnyMessageOwned> {
        let Some((message_type, message)) = try_message_from_frame(frame) else {
            self.fail(format!("received a frame that can't be parsed: {frame:?}"));
            return None;
        };
        tracing::info!("{}: received {}", self.label, message);
        self.received.add_message(message_type, message.clone());
        Some(message)
    }

    // Queues every message arriving on `frames`, until the connection closes or fails.
    async fn capture(&self, frames: Receiver<InboundFrame>) {
        while let Ok(mut frame) = frames.recv().await {
            if self.record(&mut frame).is_none() {
                return;
            }
        }
        self.end(End::Closed);
    }

    fn end(&self, end: End) {
        _ = self.end.with(|first| {
            first.get_or_insert(end);
        });
    }

    // Ends the connection with what went wrong on it, unless it has already ended.
    fn fail(&self, reason: String) {
        tracing::error!("{}: {}", self.label, reason);
        self.end(End::Failed(reason));
    }

    fn failure(&self) -> Option<String> {
        match self.end.get().ok().flatten() {
            Some(End::Failed(reason)) => Some(reason),
            _ => None,
        }
    }

    fn closed(&self) -> bool {
        matches!(self.end.get().ok().flatten(), Some(End::Closed))
    }

    // Panics in the test's task if the connection failed inside a task of the mock.
    fn panic_on_failure(&self) {
        if let Some(failure) = self.failure() {
            panic!("{}: {failure}", self.label);
        }
    }

    // Sends `message`, or returns why it couldn't be sent.
    async fn try_send(&self, message: AnyMessageOwned) -> Result<(), String> {
        let frame = OutboundFrame::from_message(message)
            .map_err(|error| format!("the message can't be framed: {error:?}"))?;
        self.sender
            .send(frame)
            .await
            .map_err(|_| "the connection is closed".to_string())
    }

    async fn send(&self, message: AnyMessageOwned) {
        self.panic_on_failure();
        if let Err(reason) = self.try_send(message).await {
            panic!("{}: {reason}", self.label);
        }
    }

    async fn send_frame(&self, frame: SerializedFrame) {
        self.panic_on_failure();
        if self.sender.send(frame.into()).await.is_err() {
            panic!("{}: the connection is closed", self.label);
        }
    }

    fn next_message(&self) -> Option<(MsgType, AnyMessageOwned)> {
        self.panic_on_failure();
        self.received.next_message()
    }

    // Waits for a message of `message_type`, giving up as soon as it can no longer arrive: the
    // connection failed, or closed with everything received on it already queued.
    async fn wait_for_message_type(&self, message_type: u8) {
        let arrived = tokio::select! {
            arrived = self.received.wait_for_message_type(message_type, DEFAULT_TIMEOUT) => arrived,
            _ = self.stopped_receiving() => self.received.has_message_type(message_type),
        };
        self.panic_on_failure();
        if arrived {
            return;
        }
        let awaited = message_type_to_name(message_type);
        if self.closed() {
            panic!(
                "{}: the connection closed while waiting for message {awaited}",
                self.label
            );
        }
        panic!(
            "{}: timeout while waiting for message {awaited}",
            self.label
        );
    }

    // Waits for a message of `message_type`, then removes the messages from the queue up to and
    // including the first one of that type.
    async fn wait_for_message_type_and_clean_queue(&self, message_type: u8) {
        self.wait_for_message_type(message_type).await;
        self.received.has_message_type_with_remove(message_type);
    }

    // Completes once no further message will be queued.
    async fn stopped_receiving(&self) {
        while self.end.get().ok().flatten().is_none() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    // A failed connection stopped queuing messages, so their absence from the queue proves
    // nothing: this panics instead of answering.
    async fn assert_message_not_present(&self, message_type: u8, deadline: Duration) -> bool {
        let absent = self
            .received
            .assert_message_not_present(message_type, deadline)
            .await;
        self.panic_on_failure();
        absent
    }

    // Closes the connection once the frames already sent have been written out.
    fn close(&self) {
        self.sender.close();
    }

    async fn wait_for_close(&self) {
        let start = Instant::now();
        while !self.sender.is_closed() {
            self.panic_on_failure();
            if start.elapsed() > DEFAULT_TIMEOUT {
                panic!(
                    "{}: timeout while waiting for the connection to close",
                    self.label
                );
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        self.panic_on_failure();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::get_available_address;
    use stratum_apps::stratum_core::{
        common_messages_sv2::{
            MESSAGE_TYPE_SETUP_CONNECTION, MESSAGE_TYPE_SETUP_CONNECTION_ERROR,
            MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, Protocol, SetupConnectionSuccessOwned,
        },
        mining_sv2::MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
        parsers_sv2::CommonMessagesOwned,
    };

    fn mining() -> Option<ServerSetupConnection> {
        Some(ServerSetupConnection::new([(Protocol::MiningProtocol, 0)]))
    }

    #[tokio::test]
    async fn server_accepts_a_supported_setup_connection() {
        let address = get_available_address();
        let server = MockServer::new(address, mining()).await;
        let client = MockClient::new(
            address,
            Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
        )
        .await;

        client
            .wait_for_message_type(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
        server
            .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
        assert!(
            server
                .assert_message_not_present(
                    0,
                    MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                    Duration::from_millis(200),
                )
                .await
        );
    }

    #[tokio::test]
    async fn server_refuses_an_unsupported_protocol_or_version() {
        let address = get_available_address();
        let _server = MockServer::new(address, mining()).await;

        let wrong_protocol = MockClient::new(
            address,
            Some(ClientSetupConnection::new(
                Protocol::TemplateDistributionProtocol,
            )),
        )
        .await;
        let wrong_version = MockClient::new(
            address,
            Some(ClientSetupConnection {
                min_version: 3,
                max_version: 3,
                ..ClientSetupConnection::new(Protocol::MiningProtocol)
            }),
        )
        .await;

        for client in [wrong_protocol, wrong_version] {
            client
                .wait_for_message_type(MESSAGE_TYPE_SETUP_CONNECTION_ERROR)
                .await;
            client.wait_for_close().await;
        }
    }

    // A peer that closes its connection without sending `SetupConnection` still takes its place
    // among the server's connections, where the test finds out what happened.
    #[tokio::test]
    #[should_panic(
        expected = "connection 0: the peer closed the connection before sending SetupConnection"
    )]
    async fn server_reports_a_connection_closed_before_setup_connection() {
        let address = get_available_address();
        let server = MockServer::new(address, mining()).await;
        MockClient::new(address, None).await.close();
        server.wait_for_connection(0).await;

        let _second = MockClient::new(
            address,
            Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
        )
        .await;
        server
            .wait_for_message_type(1, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;

        server
            .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
    }

    // A test that only waits for a connection to close still finds out that it failed.
    #[tokio::test]
    #[should_panic(expected = "connection 0: the first message was")]
    async fn waiting_for_close_reports_a_failed_connection() {
        let address = get_available_address();
        let server = MockServer::new(address, mining()).await;
        let client = MockClient::new(address, None).await;
        client
            .send(AnyMessageOwned::Common(
                CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
                    used_version: 2,
                    flags: 0,
                }),
            ))
            .await;
        server.wait_for_connection(0).await;
        client.close();

        server.wait_for_close(0).await;
    }

    #[tokio::test]
    #[should_panic(expected = "connection 0: received a frame that can't be parsed")]
    async fn absence_is_not_asserted_on_a_connection_that_failed() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        let client = MockClient::new(address, None).await;
        // A `SetupConnection.Success` header with no payload.
        let truncated = SerializedFrame::from_bytes(
            vec![0, 0, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS, 0, 0, 0].into(),
        )
        .expect("a header with an empty payload is a whole frame");
        client.send_frame(truncated).await;

        server
            .assert_message_not_present(
                0,
                MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL,
                Duration::from_millis(500),
            )
            .await;
    }

    #[tokio::test]
    #[should_panic(expected = "the connection closed while waiting for message")]
    async fn a_wait_ends_when_the_connection_closes() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        MockClient::new(address, None).await.close();

        // The wait must end because the connection closed, well before its own timeout.
        tokio::time::timeout(
            Duration::from_secs(10),
            server.wait_for_message_type(0, MESSAGE_TYPE_OPEN_EXTENDED_MINING_CHANNEL),
        )
        .await
        .expect("the wait ends once the connection closes");
    }

    // A server with a `ServerSetupConnection` fails a connection whose first message is anything
    // else, and a wait reports it even when the message it waits for did arrive.
    #[tokio::test]
    #[should_panic(expected = "connection 0: the first message was")]
    async fn server_reports_a_first_message_that_is_not_setup_connection() {
        let address = get_available_address();
        let server = MockServer::new(address, mining()).await;
        let client = MockClient::new(address, None).await;
        client
            .send(AnyMessageOwned::Common(
                CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
                    used_version: 2,
                    flags: 0,
                }),
            ))
            .await;

        server
            .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
    }

    #[tokio::test]
    async fn server_answers_each_protocol_with_its_own_flags() {
        let address = get_available_address();
        let _server = MockServer::new(
            address,
            Some(ServerSetupConnection::new([
                (Protocol::MiningProtocol, 0b0001),
                (Protocol::JobDeclarationProtocol, 0b0010),
            ])),
        )
        .await;

        for (protocol, flags) in [
            (Protocol::MiningProtocol, 0b0001),
            (Protocol::JobDeclarationProtocol, 0b0010),
        ] {
            let client = MockClient::new(address, Some(ClientSetupConnection::new(protocol))).await;
            client
                .wait_for_message_type(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
                .await;
            let Some((
                _,
                AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(success)),
            )) = client.next_message()
            else {
                panic!("the server answers with SetupConnection.Success");
            };
            assert_eq!(success.flags, flags);
        }
    }

    // Without a `SetupConnection` configured, neither mock sends anything on its own, and the
    // server tells its connections apart: a message sent to one doesn't reach the other.
    #[tokio::test]
    async fn server_addresses_each_connection() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        let first = MockClient::new(address, None).await;
        server.wait_for_connection(0).await;
        let second = MockClient::new(address, None).await;

        server
            .send(
                1,
                AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(
                    SetupConnectionSuccessOwned {
                        used_version: 2,
                        flags: 0,
                    },
                )),
            )
            .await;

        second
            .wait_for_message_type(MESSAGE_TYPE_SETUP_CONNECTION_SUCCESS)
            .await;
        assert!(first.next_message().is_none());
        assert!(server.next_message(0).is_none());
    }

    // A peer that closes right after its `SetupConnection` may be gone before the server's answer
    // is sent. That is a closed connection, not a failed one.
    #[tokio::test]
    async fn a_peer_closing_after_setup_connection_is_not_a_failure() {
        let address = get_available_address();
        let server = MockServer::new(address, mining()).await;
        let client = MockClient::new(
            address,
            Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
        )
        .await;
        client.close();

        server.wait_for_close(0).await;
        server
            .wait_for_message_type(0, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
    }

    #[tokio::test]
    async fn waiting_and_cleaning_removes_the_awaited_message() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        let _client = MockClient::new(
            address,
            Some(ClientSetupConnection::new(Protocol::MiningProtocol)),
        )
        .await;

        server
            .wait_for_message_type_and_clean_queue(0, MESSAGE_TYPE_SETUP_CONNECTION)
            .await;
        assert!(server.next_message(0).is_none());
    }

    #[tokio::test]
    async fn closing_a_connection_is_seen_by_the_peer() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        let first = MockClient::new(address, None).await;
        server.wait_for_connection(0).await;
        let second = MockClient::new(address, None).await;

        server.close(0).await;
        first.wait_for_close().await;

        second.close();
        server.wait_for_close(1).await;
    }

    #[tokio::test]
    async fn dropping_a_mock_closes_its_connections() {
        let address = get_available_address();
        let server = MockServer::new(address, None).await;
        let client = MockClient::new(address, None).await;
        server.wait_for_connection(0).await;

        drop(server);
        client.wait_for_close().await;

        let server = MockServer::new(address, None).await;
        let client = MockClient::new(address, None).await;
        drop(client);
        server.wait_for_close(0).await;
    }
}
