use crate::interceptor::MessageDirection;
use async_channel::{Receiver, Sender};
use std::{collections::VecDeque, net::SocketAddr, sync::Arc};
use stratum_apps::{
    network_helpers::sv1_connection::ConnectionSV1,
    stratum_core::sv1_api::{self, server_to_client},
};
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq)]
enum SnifferError {
    DownstreamClosed,
    UpstreamClosed,
}

/// Represents an SV1 sniffer.
///
/// This struct acts as a middleman between two SV1 roles. It forwards messages from one role to
/// the other and vice versa. It also provides methods to wait for specific messages to be received
/// from the downstream or upstream role.
#[derive(Debug, Clone)]
pub struct SnifferSV1 {
    listening_address: SocketAddr,
    upstream_address: SocketAddr,
    messages_from_downstream: MessagesAggregatorSV1,
    messages_from_upstream: MessagesAggregatorSV1,
}

impl SnifferSV1 {
    /// Create a new [`SnifferSV1`] instance.
    ///
    /// The listening address is the address the sniffer will listen on for incoming connections
    /// from the downstream role. The upstream address is the address the sniffer will connect to
    /// in order to forward messages to the upstream role.
    pub fn new(listening_address: SocketAddr, upstream_address: SocketAddr) -> Self {
        Self {
            listening_address,
            upstream_address,
            messages_from_downstream: MessagesAggregatorSV1::new(),
            messages_from_upstream: MessagesAggregatorSV1::new(),
        }
    }

    /// Start the sniffer.
    pub fn start(&self) {
        let upstream_address = self.upstream_address;
        let listening_address = self.listening_address;
        let messages_from_downstream = self.messages_from_downstream.clone();
        let messages_from_upstream = self.messages_from_upstream.clone();
        tokio::spawn(async move {
            let listener = TcpListener::bind(listening_address)
                .await
                .expect("Failed to listen on given address");
            let sniffer_to_upstream_stream = loop {
                match TcpStream::connect(upstream_address).await {
                    Ok(s) => break s,
                    Err(_) => {
                        tracing::warn!(
                            "SnifferSV1: unable to connect to upstream, retrying after 1 second"
                        );
                        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            };
            let (downstream_stream, _) = listener
                .accept()
                .await
                .expect("Failed to accept downstream connection");
            let sniffer_to_upstream_connection =
                ConnectionSV1::new(sniffer_to_upstream_stream, CancellationToken::new()).await;
            let downstream_to_sniffer_connection =
                ConnectionSV1::new(downstream_stream, CancellationToken::new()).await;
            select! {
                _ = Self::recv_from_down_send_to_up_sv1(
                    downstream_to_sniffer_connection.receiver(),
                    sniffer_to_upstream_connection.sender(),
                    messages_from_downstream
                ) => { },
                _ = Self::recv_from_up_send_to_down_sv1(
                    sniffer_to_upstream_connection.receiver(),
                    downstream_to_sniffer_connection.sender(),
                    messages_from_upstream
                ) => { },
            };
        });
    }

    /// Wait for a specific message to be received from the downstream role.
    pub async fn wait_for_message(&self, message: &[&str], direction: MessageDirection) {
        if message.is_empty() {
            panic!("Message cannot be empty");
        }
        let now = std::time::Instant::now();
        loop {
            match direction {
                MessageDirection::ToUpstream => {
                    if self.messages_from_downstream.has_message(message).await {
                        break;
                    }
                }
                MessageDirection::ToDownstream => {
                    if self.messages_from_upstream.has_message(message).await {
                        break;
                    }
                }
            }
            if now.elapsed().as_secs() > 60 {
                panic!(
                    "Timeout: SV1 message {} not found",
                    message.first().unwrap()
                );
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        }
    }

    /// Wait for a mining.notify message with a job_id that is a keepalive job.
    /// Keepalive job IDs contain the '#' delimiter (format: `{original_job_id}#{counter}`).
    pub async fn wait_for_keepalive_notify(&self, direction: MessageDirection) {
        let now = std::time::Instant::now();
        loop {
            let has_notify = match direction {
                MessageDirection::ToUpstream => {
                    self.messages_from_downstream.has_keepalive_notify().await
                }
                MessageDirection::ToDownstream => {
                    self.messages_from_upstream.has_keepalive_notify().await
                }
            };
            if has_notify {
                break;
            }
            if now.elapsed().as_secs() > 60 {
                panic!("Timeout: keepalive mining.notify (job_id containing '#') not found");
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        }
    }

    /// Waits for a message and executes an assertion closure on it.
    pub async fn wait_and_assert<F>(
        &self,
        filter: SV1MessageFilter,
        direction: MessageDirection,
        mut assertion: F,
    ) where
        F: FnMut(sv1_api::Message),
    {
        let f = filter.inner();
        self.wait_for_message(&[&f], direction).await;

        let aggregator = match direction {
            MessageDirection::ToUpstream => &self.messages_from_downstream,
            MessageDirection::ToDownstream => &self.messages_from_upstream,
        };

        let message = match filter {
            SV1MessageFilter::WithMessageName(method_name) => aggregator
                .get_last_matching(|msg| match msg {
                    sv1_api::Message::StandardRequest(req) => req.method == method_name,
                    sv1_api::Message::Notification(notif) => notif.method == method_name,
                    _ => false,
                })
                .await
                .expect("Message disappeared after wait_for_message"),
            SV1MessageFilter::WithMessageId(method_id) => aggregator
                .get_last_matching(|msg| match msg {
                    sv1_api::Message::StandardRequest(req) => req.id == method_id,
                    sv1_api::Message::OkResponse(req) => req.id == method_id,
                    sv1_api::Message::ErrorResponse(req) => req.id == method_id,
                    _ => false,
                })
                .await
                .expect("Message disappeared after wait_for_message"),
        };
        assertion(message);
    }

    async fn recv_from_up_send_to_down_sv1(
        recv: Receiver<sv1_api::Message>,
        send: Sender<sv1_api::Message>,
        upstream_messages: MessagesAggregatorSV1,
    ) -> Result<(), SnifferError> {
        while let Ok(msg) = recv.recv().await {
            send.send(msg.clone())
                .await
                .map_err(|_| SnifferError::DownstreamClosed)?;
            upstream_messages.add_message(msg.clone()).await;
            tracing::info!("🔍 Sv1 Sniffer | Direction: ⬇ | Forwarded: {}", msg);
        }
        Err(SnifferError::UpstreamClosed)
    }

    async fn recv_from_down_send_to_up_sv1(
        recv: Receiver<sv1_api::Message>,
        send: Sender<sv1_api::Message>,
        downstream_messages: MessagesAggregatorSV1,
    ) -> Result<(), SnifferError> {
        while let Ok(msg) = recv.recv().await {
            send.send(msg.clone())
                .await
                .map_err(|_| SnifferError::UpstreamClosed)?;
            downstream_messages.add_message(msg.clone()).await;
            tracing::info!("🔍 Sv1 Sniffer | Direction: ⬆ | Forwarded: {}", msg);
        }
        Err(SnifferError::DownstreamClosed)
    }
}

/// Represents a filter by which it is possbile to get a message using
/// [`SnifferSV1::wait_and_assert`]
///
/// For `WithMessageName` you can pass method name like `mining.subscribe`, And for `WithMessageId`
/// you can pass the id of the message you are interested in filtering.
pub enum SV1MessageFilter {
    WithMessageName(&'static str),
    WithMessageId(u64),
}

impl SV1MessageFilter {
    fn inner(&self) -> String {
        match self {
            SV1MessageFilter::WithMessageName(mn) => mn.to_string(),
            SV1MessageFilter::WithMessageId(mi) => mi.to_string(),
        }
    }
}

/// Represents a SV1 message manager.
///
/// This struct can be used in order to aggregate and manage SV1 messages.
#[derive(Debug, Clone)]
pub(crate) struct MessagesAggregatorSV1 {
    messages: Arc<Mutex<VecDeque<sv1_api::Message>>>,
}

impl MessagesAggregatorSV1 {
    fn new() -> Self {
        Self {
            messages: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn add_message(&self, message: sv1_api::Message) {
        let mut messages = self.messages.lock().await;
        messages.push_back(message);
    }

    async fn has_message(&self, expected_msg: &[&str]) -> bool {
        let messages = self.messages.lock().await;
        let target = match expected_msg.first() {
            Some(t) => t,
            None => return false,
        };

        messages.iter().any(|msg| match msg {
            sv1_api::Message::StandardRequest(req) => req.method == *target,
            sv1_api::Message::Notification(notif) => notif.method == *target,
            sv1_api::Message::OkResponse(res) => {
                let id_match = res.id.to_string() == *target;

                if id_match {
                    true
                } else if let Ok(serialized) = corepc_node::serde_json::to_string(&res) {
                    expected_msg.iter().all(|m| serialized.contains(m))
                } else {
                    false
                }
            }
            sv1_api::Message::ErrorResponse(res) => {
                let id_match = res.id.to_string() == *target;
                let error_msg_match = res
                    .error
                    .as_ref()
                    .map(|e| e.message.contains(*target))
                    .unwrap_or(false);

                id_match || error_msg_match
            }
        })
    }

    /// Checks if there's a keepalive mining.notify message.
    /// Keepalive job IDs contain the '#' delimiter (format: `{original_job_id}#{counter}`).
    async fn has_keepalive_notify(&self) -> bool {
        let messages = self.messages.lock().await;
        messages.iter().any(|msg| {
            if let sv1_api::Message::Notification(notif) = msg {
                if notif.method == "mining.notify" {
                    if let Ok(notify) = server_to_client::Notify::try_from(notif.clone()) {
                        return notify.job_id.contains('#');
                    }
                }
            }
            false
        })
    }
    // Finds the last message that matches a predicate and returns it.
    async fn get_last_matching<F>(&self, predicate: F) -> Option<sv1_api::Message>
    where
        F: Fn(&sv1_api::Message) -> bool,
    {
        let messages = self.messages.lock().await;
        messages.iter().rev().find(|m| predicate(m)).cloned()
    }
}
