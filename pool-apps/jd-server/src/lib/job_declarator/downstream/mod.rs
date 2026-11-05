//! Per-downstream connection management.
//!
//! Each TCP client that connects to the JDS downstream server is represented by a
//! [`Downstream`] instance. It owns the Noise-encrypted I/O tasks, the pending
//! `DeclareMiningJob` map, and a per-downstream [`CancellationToken`] for clean teardown.

use super::{
    ALLOCATED_TOKEN_TIMEOUT_SECS, DownstreamJobDeclarationMessage, JANITOR_INTERVAL_SECS,
    JobDeclarationMessage,
};
use crate::{
    error,
    error::{JDSResult, LoopControl},
    io_task::spawn_io_tasks,
};
use async_channel::{Receiver, Sender, unbounded};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use stratum_apps::{
    bitcoin_core_sv2::CancellationToken,
    network_helpers::noise_stream::NoiseTcpStream,
    stratum_core::{
        binary_sv2::{B016MOwned, Seq064KOwned},
        bitcoin::{Weight, Wtxid, hashes::Hash},
        common_messages_sv2::MESSAGE_TYPE_SETUP_CONNECTION,
        handlers_sv2::HandleCommonMessagesFromClientOwnedAsync,
        job_declaration_sv2::{DeclareMiningJobOwned, ProvideMissingTransactionsSuccessOwned},
        parsers_sv2::{AnyMessageOwned, parse_message_frame_with_tlvs},
    },
    sync::{SharedLock, SharedMap},
    task_manager::TaskManager,
    utils::{
        protocol_message_type::{MessageType, protocol_message_type},
        types::{DownstreamId, InboundFrame, OutboundFrame, RequestId},
    },
};
use tracing::{debug, error, info, warn};

mod common_message_handler;

/// How many times one declaration may ask the client for transactions.
///
/// The node is asked for every declared transaction on every validation round, so a transaction
/// it held on one round can be gone by the next, replaced, evicted or confirmed meanwhile, and a
/// round can end with transactions missing that the client was never asked for. Each further
/// round costs a full lookup of the declared list, so a declaration gets a few of them and is
/// refused when the node is still losing transactions after that. This bounds the work a client
/// can force per declaration, by making the node lose a transaction it controls between rounds,
/// not only how long a declaration waits on a churning mempool: an honest declaration is refused
/// only when every round allowed lost a transaction, which rounds that take longer on a busy JDS
/// make likelier, and five keeps that negligible.
pub const MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS: usize = 5;

/// The most bytes of supplied transactions one declaration keeps, over all rounds.
///
/// Everything a declaration is asked for goes in one block, and a block's serialized size never
/// exceeds its weight, so a client that supplies what it declared stays within this.
pub const MAX_SUPPLIED_BYTES: usize = Weight::MAX_BLOCK.to_wu() as usize;

/// How many of the transactions a refusal is about its log line names.
///
/// A client decides how many transactions a refusal is about, up to a whole declared list, so the
/// line carries their count and the first of them. A node that lost transactions between rounds
/// loses a handful, which this names in full.
const LOGGED_WTXIDS: usize = 16;

/// A declaration waiting on transactions the client has been asked for.
///
/// The client is asked for each transaction once, and everything it has supplied is kept until
/// the declaration is answered, so a round that ends with transactions missing can ask for the
/// ones the client has not been asked for yet and resume with the rest. A transaction the client
/// was already asked for and is still missing is one the client withheld, which refuses the
/// declaration.
pub struct PendingDeclareMiningJob {
    /// When the declaration was received, from which the janitor expires it.
    pub inserted_at: Instant,
    /// The declaration itself, validated again on every round.
    pub declare_mining_job: DeclareMiningJobOwned,
    // Every transaction the client has been asked for, over all rounds.
    requested: HashSet<Wtxid>,
    // Every transaction the client has supplied, in the order they were asked for.
    supplied: Vec<B016MOwned>,
    rounds: usize,
}

impl PendingDeclareMiningJob {
    /// Starts waiting on `missing_wtxids`, the transactions the first validation round found
    /// missing, and returns their positions in the declared list for `ProvideMissingTransactions`.
    pub fn new(
        declare_mining_job: DeclareMiningJobOwned,
        missing_wtxids: &[Wtxid],
    ) -> (Self, Vec<u16>) {
        let mut pending = Self {
            inserted_at: Instant::now(),
            declare_mining_job,
            requested: HashSet::new(),
            supplied: Vec::new(),
            rounds: 0,
        };
        let positions = pending.ask(missing_wtxids);
        (pending, positions)
    }

    /// Keeps the transactions the client supplied in answer to the last round.
    ///
    /// Returns `false`, keeping none of them, if they would bring what is kept past the number
    /// of transactions the declaration lists or past [`MAX_SUPPLIED_BYTES`]: the client is only
    /// ever asked for declared transactions, each once, and together they fit in a block, so
    /// such an answer is invalid before anything in it is looked at. The bytes are counted as
    /// supplied, whether or not they decode, which is what bounds the memory one declaration
    /// can pin.
    pub fn supply(&mut self, transactions: Vec<B016MOwned>) -> bool {
        if self.supplied.len() + transactions.len() > self.declare_mining_job.wtxid_list.len() {
            return false;
        }
        let bytes: usize = self
            .supplied
            .iter()
            .chain(&transactions)
            .map(|transaction| transaction.as_ref().len())
            .sum();
        if bytes > MAX_SUPPLIED_BYTES {
            return false;
        }
        self.supplied.extend(transactions);
        true
    }

    /// Everything the client has supplied so far, as one answer for the validator.
    pub fn supplied(&self) -> ProvideMissingTransactionsSuccessOwned {
        ProvideMissingTransactionsSuccessOwned {
            request_id: self.declare_mining_job.request_id,
            transaction_list: Seq064KOwned::new(self.supplied.clone())
                .expect("never more transactions than the declared list, itself a Seq064K"),
        }
    }

    /// Asks the client for `missing_wtxids`, the transactions the last round found missing, in
    /// one more round: the positions in the declared list for `ProvideMissingTransactions`.
    ///
    /// `None` refuses the declaration: a missing transaction the client was already asked for is
    /// one it withheld, and a declaration gets
    /// [`MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS`] rounds. Either way the log line carries how
    /// many transactions the refusal is about and the first [`LOGGED_WTXIDS`] of them, for
    /// `client_id`, since request ids are only unique per client.
    pub fn next_round(
        &mut self,
        client_id: DownstreamId,
        missing_wtxids: &[Wtxid],
    ) -> Option<Vec<u16>> {
        let request_id = self.declare_mining_job.request_id;
        let withheld: Vec<&Wtxid> = missing_wtxids
            .iter()
            .filter(|wtxid| self.requested.contains(*wtxid))
            .collect();
        if !withheld.is_empty() {
            warn!(
                client_id,
                request_id,
                withheld = withheld.len(),
                first_withheld = ?&withheld[..withheld.len().min(LOGGED_WTXIDS)],
                "Refusing DeclareMiningJob: the client did not supply transactions it was asked for"
            );
            return None;
        }
        if self.rounds >= MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS {
            warn!(
                client_id,
                request_id,
                rounds = self.rounds,
                missing = missing_wtxids.len(),
                first_missing = ?&missing_wtxids[..missing_wtxids.len().min(LOGGED_WTXIDS)],
                "Refusing DeclareMiningJob: the node lost declared transactions on every round allowed"
            );
            return None;
        }
        Some(self.ask(missing_wtxids))
    }

    // Records one round asking the client for `wtxids` and returns their positions in the
    // declared list, in declaration order. A wtxid the declaration does not list has no
    // position, and the validator only ever reports declared ones missing.
    fn ask(&mut self, wtxids: &[Wtxid]) -> Vec<u16> {
        self.rounds += 1;
        self.requested.extend(wtxids);
        let asked: HashSet<&Wtxid> = wtxids.iter().collect();
        self.declare_mining_job
            .wtxid_list
            .iter()
            .enumerate()
            .filter(|(_, u256)| asked.contains(&Wtxid::from_byte_array(u256.to_array())))
            .map(|(position, _)| position as u16)
            .collect()
    }
}

/// Channel endpoints for a single downstream connection.
#[derive(Clone)]
pub struct DownstreamIo {
    to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
    from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
    to_downstream_sender: Sender<OutboundFrame>,
    from_downstream_receiver: Receiver<InboundFrame>,
}

/// Represents a downstream client connected to this node.
#[derive(Clone)]
pub struct Downstream {
    /// Extensions that have been successfully negotiated with this client.
    pub negotiated_extensions: SharedLock<Vec<u16>>,
    /// Jobs waiting for missing transactions (keyed by `request_id`).
    pub pending_declare_mining_jobs: SharedMap<RequestId, PendingDeclareMiningJob>,
    pub downstream_io: DownstreamIo,
    pub downstream_id: DownstreamId,
    /// Extensions that JDS supports
    #[allow(unused)]
    pub supported_extensions: Vec<u16>,
    /// Extensions that JDS requires
    #[allow(unused)]
    pub required_extensions: Vec<u16>,
    /// Per-downstream cancellation token (child of the global token).
    /// Cancelling this stops IO tasks, the pending jobs janitor, and the downstream loop
    /// without affecting other downstreams or the server.
    downstream_cancellation_token: CancellationToken,
}

#[cfg_attr(not(test), hotpath::measure_all)]
impl Downstream {
    fn handle_error_action(
        &self,
        context: &str,
        e: &error::JDSError<error::Downstream>,
    ) -> LoopControl {
        match e.action {
            error::Action::Log => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} returned a log-only error"
                );
                LoopControl::Continue
            }
            error::Action::Disconnect(_) => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested disconnect"
                );
                LoopControl::Break
            }
            error::Action::Shutdown => {
                warn!(
                    downstream_id = self.downstream_id,
                    error_kind = ?e.kind,
                    "{context} requested shutdown"
                );
                LoopControl::Break
            }
        }
    }

    /// Creates a new [`Downstream`] and spawns its Noise I/O tasks.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        downstream_id: DownstreamId,
        noise_stream: NoiseTcpStream,
        to_job_declarator_sender: Sender<DownstreamJobDeclarationMessage>,
        from_job_declarator_receiver: Receiver<JobDeclarationMessage>,
        supported_extensions: Vec<u16>,
        required_extensions: Vec<u16>,
        task_manager: Arc<TaskManager>,
        global_cancellation_token: CancellationToken,
    ) -> Self {
        let (noise_stream_reader, noise_stream_writer) = noise_stream.into_split();
        let (inbound_tx, inbound_rx) = unbounded::<InboundFrame>();
        let (outbound_tx, outbound_rx) = unbounded::<OutboundFrame>();

        let downstream_cancellation_token = global_cancellation_token.child_token();

        spawn_io_tasks(
            task_manager,
            noise_stream_reader,
            noise_stream_writer,
            outbound_rx,
            inbound_tx,
            downstream_cancellation_token.clone(),
        );

        let negotiated_extensions = SharedLock::new(Vec::new());
        let pending_declare_mining_jobs = SharedMap::new();

        let downstream_io = DownstreamIo {
            to_job_declarator_sender,
            from_job_declarator_receiver,
            to_downstream_sender: outbound_tx,
            from_downstream_receiver: inbound_rx,
        };

        Self {
            negotiated_extensions,
            pending_declare_mining_jobs,
            downstream_io,
            downstream_id,
            supported_extensions,
            required_extensions,
            downstream_cancellation_token,
        }
    }

    /// Starts the downstream message loop.
    ///
    /// Performs the initial `SetupConnection` handshake, spawns the pending-jobs janitor, then
    /// enters a `tokio::select!` loop over inbound and outbound JDP messages.
    /// On exit (error, disconnect, or cancellation) it cancels its own
    /// [`CancellationToken`] and notifies [`JobDeclarator`](super::JobDeclarator)
    /// via `disconnect_sender`.
    pub async fn start(
        mut self,
        task_manager: Arc<TaskManager>,
        cleanup: impl FnOnce(DownstreamId) + Send + 'static,
    ) {
        // Setup initial connection
        if let Err(e) = self.setup_connection_with_downstream().await {
            error!(?e, "Failed to set up downstream connection");

            // sleep to make sure SetupConnectionError is sent
            // before we break the TCP connection
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;

            self.downstream_cancellation_token.cancel();

            warn!(
                downstream_id = self.downstream_id,
                "Downstream exiting before main loop; invoking cleanup callback"
            );

            // cleanup entry on JobDeclarator
            cleanup(self.downstream_id);

            return;
        }

        self.spawn_pending_jobs_janitor(task_manager.clone());

        let cancellation_token = self.downstream_cancellation_token.clone();
        task_manager.spawn(async move {
            let downstream_id = self.downstream_id;
            let mut self_clone_1 = self.clone();
            let self_clone_2 = self.clone();
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        info!("Downstream {downstream_id}: cancellation token triggered");
                        break;
                    }

                    res = self_clone_1.handle_message_from_downstream() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling downstream message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_message_from_downstream",
                                &e,
                            ) {
                                break;
                            }
                        }
                    }
                    res = self_clone_2.handle_job_declarator_message() => {
                        if let Err(e) = res {
                            error!(?e, "Error handling job declarator message for {downstream_id}");
                            if let LoopControl::Break = self.handle_error_action(
                                "Downstream::handle_job_declarator_message",
                                &e,
                            ) {
                                break;
                            }
                        }
                    }
                }
            }
            cancellation_token.cancel();

            // cleanup entry on JobDeclarator
            cleanup(downstream_id);

            info!("Downstream {downstream_id}: disconnected.");
        });
    }

    /// Spawns a periodic task that evicts stale [`PendingDeclareMiningJob`] entries
    /// older than `ALLOCATED_TOKEN_TIMEOUT_SECS`.
    fn spawn_pending_jobs_janitor(&self, task_manager: Arc<TaskManager>) {
        let cancellation_token = self.downstream_cancellation_token.clone();
        let pending_declare_mining_jobs = self.pending_declare_mining_jobs.clone();
        let downstream_id = self.downstream_id;
        let token_timeout = Duration::from_secs(ALLOCATED_TOKEN_TIMEOUT_SECS);
        let janitor_interval = Duration::from_secs(JANITOR_INTERVAL_SECS);
        task_manager.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancellation_token.cancelled() => {
                        break;
                    }
                    _ = tokio::time::sleep(janitor_interval) => {
                        pending_declare_mining_jobs.retain(|request_id, pending| {
                            let expired = Instant::now().duration_since(pending.inserted_at) > token_timeout;
                            if expired {
                                debug!(
                                    downstream_id,
                                    request_id,
                                    "Removing expired pending declare mining job",
                                );
                            }
                            !expired
                        });
                    }
                }
            }
        });
    }

    async fn setup_connection_with_downstream(&mut self) -> JDSResult<(), error::Downstream> {
        let mut frame = self
            .downstream_io
            .from_downstream_receiver
            .recv()
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        let header = frame.header();

        if header.ext_type() == 0 && header.msg_type() == MESSAGE_TYPE_SETUP_CONNECTION {
            self.handle_common_message_frame_from_client(
                Some(self.downstream_id),
                header,
                frame.payload(),
            )
            .await?;
        } else {
            return Err(error::JDSError::disconnect(
                error::JDSErrorKind::UnexpectedMessage(
                    header.ext_type_without_channel_msg(),
                    header.msg_type(),
                ),
                self.downstream_id,
            ));
        }

        Ok(())
    }

    // Handles messages from the job declarator to this downstream client.
    async fn handle_job_declarator_message(&self) -> JDSResult<(), error::Downstream> {
        let receiver = &self.downstream_io.from_job_declarator_receiver;

        // todo: handle tlv fields?
        let (msg, _tlv_fields) = match receiver.recv().await {
            Ok(msg) => msg,
            Err(e) => {
                error!("Error receiving message: {:?}", e);
                return Err(error::JDSError::<error::Downstream>::disconnect(
                    e,
                    self.downstream_id,
                ));
            }
        };

        let message = AnyMessageOwned::JobDeclaration(msg);
        let std_frame = OutboundFrame::from_message(message)
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        self.downstream_io
            .to_downstream_sender
            .send(std_frame)
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

        Ok(())
    }

    // Handles messages from this downstream client.
    async fn handle_message_from_downstream(&mut self) -> JDSResult<(), error::Downstream> {
        let mut sv2_frame = self
            .downstream_io
            .from_downstream_receiver
            .recv()
            .await
            .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;
        let header = sv2_frame.header();

        match protocol_message_type(header.ext_type(), header.msg_type()) {
            MessageType::JobDeclaration => {
                debug!("Received mining SV2 frame from downstream.");
                let negotiated_extensions = self
                    .negotiated_extensions
                    .get()
                    .map_err(error::JDSError::shutdown)?;
                let (any_message, tlv_fields) = parse_message_frame_with_tlvs(
                    header,
                    sv2_frame.payload(),
                    &negotiated_extensions,
                )
                .map_err(|e| error::JDSError::disconnect(e, self.downstream_id))?;

                let jd_message = match any_message.into_owned() {
                    AnyMessageOwned::JobDeclaration(msg) => msg,
                    _ => {
                        error!("Expected JobDeclaration message but got different type");
                        return Err(error::JDSError::disconnect(
                            error::JDSErrorKind::UnexpectedMessage(
                                header.ext_type_without_channel_msg(),
                                header.msg_type(),
                            ),
                            self.downstream_id,
                        ));
                    }
                };

                self.downstream_io
                    .to_job_declarator_sender
                    .send((self.downstream_id, jd_message, tlv_fields))
                    .await
                    .map_err(error::JDSError::shutdown)?;
            }
            MessageType::Extensions => {
                // todo
                // self.handle_extensions_message_frame_from_client
            }
            MessageType::Common | MessageType::Mining | MessageType::TemplateDistribution => {
                warn!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Received unexpected message from downstream."
                );
            }
            MessageType::Unknown => {
                warn!(
                    ext_type = ?header.ext_type(),
                    msg_type = ?header.msg_type(),
                    "Received unknown message from downstream."
                );
            }
        }
        Ok(())
    }

    // activates the downstream cancellation token, causing any pending tasks to be cancelled
    pub fn shutdown(&mut self) {
        self.downstream_cancellation_token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stratum_apps::stratum_core::binary_sv2::U256Owned;

    fn wtxid(byte: u8) -> Wtxid {
        Wtxid::from_byte_array([byte; 32])
    }

    fn declaration(wtxids: &[Wtxid]) -> DeclareMiningJobOwned {
        DeclareMiningJobOwned {
            request_id: 7,
            mining_job_token: vec![0; 8].try_into().unwrap(),
            version: 0,
            coinbase_tx_prefix: Vec::<u8>::new().try_into().unwrap(),
            coinbase_tx_suffix: Vec::<u8>::new().try_into().unwrap(),
            wtxid_list: Seq064KOwned::new(
                wtxids
                    .iter()
                    .map(|wtxid| U256Owned::from(wtxid.to_byte_array()))
                    .collect(),
            )
            .unwrap(),
            excess_data: Vec::<u8>::new().try_into().unwrap(),
        }
    }

    fn raw_tx(byte: u8) -> B016MOwned {
        vec![byte; 4].try_into().unwrap()
    }

    // A transaction the node loses between rounds is asked for once more, in declaration order,
    // and the answers to every round reach the validator together.
    #[test]
    fn asks_once_for_each_transaction_the_node_lost() {
        let (mut pending, positions) =
            PendingDeclareMiningJob::new(declaration(&[wtxid(1), wtxid(2), wtxid(3)]), &[wtxid(3)]);
        assert_eq!(positions, vec![2]);

        assert!(pending.supply(vec![raw_tx(3)]));
        assert_eq!(
            pending.next_round(0, &[wtxid(2), wtxid(1)]),
            Some(vec![0, 1])
        );

        assert!(pending.supply(vec![raw_tx(2), raw_tx(1)]));
        assert_eq!(
            pending.supplied().transaction_list.into_inner(),
            vec![raw_tx(3), raw_tx(2), raw_tx(1)]
        );
    }

    // An answer that would bring what is kept past the declared list is invalid, and nothing of
    // it is kept.
    #[test]
    fn keeps_no_more_transactions_than_the_declaration_lists() {
        let (mut pending, _) =
            PendingDeclareMiningJob::new(declaration(&[wtxid(1), wtxid(2)]), &[wtxid(1)]);
        assert!(pending.supply(vec![raw_tx(1)]));
        assert!(!pending.supply(vec![raw_tx(2), raw_tx(2)]));
        assert_eq!(
            pending.supplied().transaction_list.into_inner(),
            vec![raw_tx(1)]
        );
    }

    // What is kept never exceeds a block's weight in bytes, over all rounds, whether or not the
    // bytes decode.
    #[test]
    fn keeps_no_more_bytes_than_a_block_weighs() {
        let (mut pending, _) =
            PendingDeclareMiningJob::new(declaration(&[wtxid(1), wtxid(2)]), &[wtxid(1)]);
        assert!(pending.supply(vec![vec![0; MAX_SUPPLIED_BYTES].try_into().unwrap()]));
        assert!(!pending.supply(vec![raw_tx(2)]));
        assert_eq!(pending.supplied().transaction_list.len(), 1);
    }

    // A transaction still missing after the client was asked for it was withheld, whatever else
    // the round found missing.
    #[test]
    fn refuses_a_transaction_the_client_was_already_asked_for() {
        let (mut pending, _) =
            PendingDeclareMiningJob::new(declaration(&[wtxid(1), wtxid(2)]), &[wtxid(1)]);
        assert_eq!(pending.next_round(0, &[wtxid(2), wtxid(1)]), None);
    }

    #[test]
    fn refuses_after_the_last_round_allowed() {
        let wtxids: Vec<Wtxid> = (1..=MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS as u8 + 1)
            .map(wtxid)
            .collect();
        let (mut pending, _) = PendingDeclareMiningJob::new(declaration(&wtxids), &wtxids[..1]);
        for lost in &wtxids[1..MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS] {
            assert!(pending.next_round(0, &[*lost]).is_some());
        }
        assert_eq!(
            pending.next_round(0, &wtxids[MAX_PROVIDE_MISSING_TRANSACTIONS_ROUNDS..]),
            None
        );
    }
}
