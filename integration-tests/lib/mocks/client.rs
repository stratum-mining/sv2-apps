use super::Connection;
use crate::{
    message_aggregator::DEFAULT_TIMEOUT,
    types::MsgType,
    utils::{CONNECT_RETRY_INTERVAL, handshake_as_client},
};
use std::{net::SocketAddr, time::Duration};
use stratum_apps::stratum_core::{
    codec_sv2::SerializedFrame,
    common_messages_sv2::{Protocol, SetupConnectionOwned},
    parsers_sv2::{AnyMessageOwned, CommonMessagesOwned},
};
use tokio::net::TcpStream;
use tokio_util::sync::{CancellationToken, DropGuard};

/// What a [`MockClient`] asks for in the `SetupConnection` that opens its connection.
pub struct ClientSetupConnection {
    /// The sub-protocol the client asks for.
    pub protocol: Protocol,
    /// The lowest protocol version the client supports.
    pub min_version: u16,
    /// The highest protocol version the client supports.
    pub max_version: u16,
    /// The flags of the client's `SetupConnection`.
    pub flags: u32,
}

impl ClientSetupConnection {
    /// A client asking for `protocol` at version 2, with no flags.
    pub fn new(protocol: Protocol) -> Self {
        Self {
            protocol,
            min_version: 2,
            max_version: 2,
            flags: 0,
        }
    }

    // The client's `SetupConnection`.
    fn message(self) -> AnyMessageOwned {
        AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(SetupConnectionOwned {
            protocol: self.protocol,
            min_version: self.min_version,
            max_version: self.max_version,
            flags: self.flags,
            endpoint_host: "0.0.0.0".try_into().unwrap(),
            endpoint_port: 0,
            vendor: "integration-test".try_into().unwrap(),
            hardware_version: "".try_into().unwrap(),
            firmware: "".try_into().unwrap(),
            device_id: "".try_into().unwrap(),
        }))
    }
}

/// A mock Sv2 client, holding one connection to a server.
pub struct MockClient {
    connection: Connection,
    // Closes the connection and ends the client's task when the client is dropped.
    _tasks: DropGuard,
}

impl MockClient {
    /// Connects to the server at `server_address`, retrying until it accepts the connection.
    ///
    /// With a `setup_connection`, the client sends the `SetupConnection` that opens the connection.
    /// It doesn't wait for the server's answer, which is the test's to assert. Without one, the
    /// client sends nothing on its own.
    pub async fn new(
        server_address: SocketAddr,
        setup_connection: Option<ClientSetupConnection>,
    ) -> Self {
        let label = format!("MockClient of {server_address}");
        let connect = async {
            loop {
                match TcpStream::connect(server_address).await {
                    Ok(stream) => break stream,
                    Err(_) => tokio::time::sleep(CONNECT_RETRY_INTERVAL).await,
                }
            }
        };
        let stream = tokio::time::timeout(DEFAULT_TIMEOUT, connect)
            .await
            .unwrap_or_else(|_| panic!("{label}: timeout while connecting"));
        let tasks = CancellationToken::new();
        let guard = tasks.clone().drop_guard();
        let (frames, sender) = handshake_as_client(stream, tasks)
            .await
            .unwrap_or_else(|| panic!("{label}: the Noise handshake failed"));
        let connection = Connection::new(label, sender);
        if let Some(setup_connection) = setup_connection {
            connection.send(setup_connection.message()).await;
        }
        tokio::spawn({
            let connection = connection.clone();
            async move { connection.capture(frames).await }
        });
        Self {
            connection,
            _tasks: guard,
        }
    }

    /// Sends `message` to the server.
    pub async fn send(&self, message: AnyMessageOwned) {
        self.connection.send(message).await;
    }

    /// Sends `frame` to the server as it is, so a test can send bytes that aren't a valid message.
    pub async fn send_frame(&self, frame: SerializedFrame) {
        self.connection.send_frame(frame).await;
    }

    /// Returns the oldest message received from the server and removes it from the queue. Returns
    /// `None` if the queue is empty.
    pub fn next_message(&self) -> Option<(MsgType, AnyMessageOwned)> {
        self.connection.next_message()
    }

    /// Waits until a message of `message_type` has been received from the server.
    pub async fn wait_for_message_type(&self, message_type: u8) {
        self.connection.wait_for_message_type(message_type).await;
    }

    /// Like [`MockClient::wait_for_message_type`], but also removes the messages from the queue up
    /// to and including the first one of `message_type`.
    pub async fn wait_for_message_type_and_clean_queue(&self, message_type: u8) {
        self.connection
            .wait_for_message_type_and_clean_queue(message_type)
            .await;
    }

    /// Whether the queue holds a message of `message_type` received from the server.
    pub fn has_message_type(&self, message_type: u8) -> bool {
        self.connection.has_message_type(message_type)
    }

    /// Removes every message from the queue.
    pub fn clean_queue(&self) {
        self.connection.clean_queue();
    }

    /// Watches the queue for `deadline`, returning `false` as soon as it holds a message of
    /// `message_type`, and `true` if the whole duration passes without one.
    pub async fn assert_message_not_present(&self, message_type: u8, deadline: Duration) -> bool {
        self.connection
            .assert_message_not_present(message_type, deadline)
            .await
    }

    /// Closes the connection, once the messages already sent on it have been written out.
    pub fn close(&self) {
        self.connection.close();
    }

    /// Waits until the connection is closed, by the server or by [`MockClient::close`].
    pub async fn wait_for_close(&self) {
        self.connection.wait_for_close().await;
    }
}
