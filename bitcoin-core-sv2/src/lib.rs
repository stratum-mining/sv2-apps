//! # Bitcoin Core Sv2 Library
//!
//! A Rust library that leverages [Bitcoin Core](https://bitcoin.org/en/bitcoin-core/) IPC to interact with the Stratum V2:
//! - [Stratum V2 Template Distribution Protocol](https://github.com/stratum-mining/sv2-spec/blob/main/07-Template-Distribution-Protocol.md)
//! - [Stratum V2 Job Declaration Protocol](https://github.com/stratum-mining/sv2-spec/blob/main/08-Job-Declaration-Protocol.md)
//!
//! ## Overview
//!
//! `bitcoin_core_sv2` allows for the official Bitcoin Core v31+ distribution (or any compatible
//! fork) to be leveraged for the following use-cases:
//!
//! - Building Sv2 applications that act as a Client under the Template Distribution Protocol (e.g.:
//!   Pool or JDC) while connecting directly to the Bitcoin Core node.
//! - Building a Sv2 Template Provider application that acts as a Template Distribution Protocol
//!   Server while leveraging a Bitcoin Core node as source of truth.
//! - Building Sv2 applications that act as a Server under the Job Declaration Protocol (e.g.: Pool
//!   or JDS) while connecting directly to the Bitcoin Core node.
//!
//! ## `LocalSet` Requirement
//!
//! Due to limitations in the `capnp-rpc` dependency,
//! [`template_distribution_protocol::BitcoinCoreSv2TDP`] and
//! [`job_declaration_protocol::BitcoinCoreSv2JDP`] must be run within a [`tokio::task::LocalSet`].
//! The crate examples demonstrate the proper setup pattern.
//!
//! ## `BitcoinCoreSv2TDP`
//!
//! The [`template_distribution_protocol::BitcoinCoreSv2TDP`] struct is designed to be interface for
//! interacting with Bitcoin Core via Sv2 Template Distribution Protocol. It is instantiated with a
//! UNIX socket path, a fee threshold, and two channels for incoming and outgoing messages.
//!
//! The struct operates with three main IO paths:
//!
//! 1. **Incoming Sv2 Messages** (`incoming_messages` channel):
//!    - Receives [`TemplateDistribution`] messages from the Sv2 protocol side
//!    - Handles [`CoinbaseOutputConstraints`] to configure coinbase output limits
//!    - Processes [`RequestTransactionData`] requests and responds with transaction data
//!    - Accepts [`SubmitSolution`] messages and forwards them to Bitcoin Core via IPC
//!    - Processed asynchronously by the `monitor_incoming_messages()` task
//!
//! 2. **Outgoing Sv2 Messages** (`outgoing_messages` channel):
//!    - Sends [`TemplateDistribution`] messages to the Sv2 protocol side
//!    - Distributes `NewTemplate` messages when templates change (chain tip or mempool updates)
//!    - Sends `SetNewPrevHash` messages when the chain tip changes
//!    - Responds to transaction data requests with `RequestTransactionDataSuccess` or
//!      `RequestTransactionDataError`
//!    - Triggered by Bitcoin Core events or in response to incoming messages
//!
//! 3. **Bitcoin Core IPC Communication** (UNIX socket):
//!    - Establishes a Cap'n Proto IPC connection over a UNIX socket during initialization
//!    - Uses multiple IPC clients: `thread_ipc_client` for execution context, `mining_ipc_client`
//!      for mining operations, and `template_ipc_client` for block templates
//!    - The `monitor_ipc_templates()` task continuously polls Bitcoin Core via `waitNext` requests
//!      to detect:
//!      - **Chain tip changes**: When a new block is mined, detected by comparing prev_hash values
//!      - **Mempool fee changes**: When mempool fees exceed the configured `fee_threshold`
//!    - Fetches block templates via `get_block_request()` and deserializes them into Bitcoin blocks
//!    - Submits mining solutions back to Bitcoin Core through the template IPC client
//!
//! The architecture enables bidirectional communication: Bitcoin Core events flow through IPC to
//! the struct, which then distributes them via the outgoing channel, while incoming Sv2 protocol
//! messages are processed and forwarded to Bitcoin Core as needed.
//!
//! ### Fee Threshold
//!
//! When instantiating a [`template_distribution_protocol::BitcoinCoreSv2TDP`] instance,
//! the `fee_threshold` parameter (in satoshis) determines when a new template is distributed due
//! to mempool changes. When the mempool fee delta exceeds this threshold, a new `NewTemplate`
//! message is sent.
//!
//! ### Minimum Interval
//!
//! When instantiating a [`template_distribution_protocol::BitcoinCoreSv2TDP`] instance,
//! the `min_interval` parameter (in seconds) determines the minimum interval between template
//! updates. When the interval between two template updates is less than the minimum interval, the
//! [`template_distribution_protocol::BitcoinCoreSv2TDP`] instance will sleep for the remaining time
//! to reach the minimum interval.
//!
//! The exception is when the chain tip changes, in which case a new `NewTemplate` message is sent
//! immediately, followed by a corresponding `SetNewPrevHash` message.
//!
//! ## `BitcoinCoreSv2JDP`
//!
//! The [`job_declaration_protocol::BitcoinCoreSv2JDP`] struct is designed to be interface for
//! interacting with Bitcoin Core via Sv2 Job Declaration Protocol. It is instantiated with a
//! UNIX socket path.
//!
//! Differently from [`template_distribution_protocol::BitcoinCoreSv2TDP`], it does not operate with
//! two main IO paths. Instead, handlers are provided for the following Sv2 messages:
//! - `DeclareMiningJob`
//! - `PushSolution`
//!
//! Please note that Sv2 JDP token management is not covered here.
//!
//! More specifically, it is the caller's responsability to manage the tokens to be
//! added to:
//! - `AllocateMiningJobToken.Success.mining_job_token`
//! - `DeclareMiningJob.Success.new_mining_job_token`
//!
//! Similarly, validation of:
//! - `DeclareMiningJob.mining_job_token` against `AllocateMiningJobToken.Success.mining_job_token`
//! - `SetCustomMiningJob.mining_job_token` against `DeclareMiningJob.Success.new_mining_job_token`
//! - `SetCustomMiningJob` Coinbase Tx against `DeclareMiningJob`
//! - `SetCustomMiningJob.merkle_path` against `DeclareMiningJob`
//!
//! are also the caller's responsability.
//!
//! ## IPC Communication
//!
//! This library leverages [`bitcoin_capnp_types`](https://github.com/2140-dev/bitcoin-capnp-types) to interact with Bitcoin Core via IPC over a
//! UNIX socket. The connection is established during [`BitcoinCoreSv2TDP::new`] and maintained
//! throughout the lifetime of the instance.

pub mod job_declaration_protocol;
pub mod template_distribution_protocol;

/// The minimum block reserved weight established by Bitcoin Core.
pub const MIN_BLOCK_RESERVED_WEIGHT: u64 = 2000;
