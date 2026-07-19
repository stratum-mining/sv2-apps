# bitcoin_core_sv2

A Rust library that integrates [Bitcoin Core](https://bitcoin.org/en/bitcoin-core/) with the [Stratum V2 Template Distribution Protocol](https://github.com/stratum-mining/sv2-spec/blob/main/07-Template-Distribution-Protocol.md) via IPC over a UNIX socket.

## Overview

`bitcoin_core_sv2` allows for the official Bitcoin Core distribution to be leveraged for the following use-cases:
- building Sv2 applications that act as a Client under the Template Distribution Protocol (e.g.: Pool or JDC) while connecting directly to the Bitcoin Core node.
- building a Sv2 Template Provider application that acts as a Template Distribution Protocol Server while creating templates from a Bitcoin Core node.

`bitcoin_core_sv2::runtime_api` is the main interface of the crate. Downstream implementations should use the factories:
- `bitcoin_core_sv2::runtime_api::template_distribution_protocol::new(version: BitcoinCoreVersion, ...) -> Result<BitcoinCoreSv2TDP, BitcoinCoreSv2TDPError>`
- `bitcoin_core_sv2::runtime_api::job_declaration_protocol::new(version: BitcoinCoreVersion, ...) -> Result<BitcoinCoreSv2JDP, BitcoinCoreSv2JDPError>`

while selecting the desired version.

### Flavor naming rationale

`unix_capnp` is intentionally explicit: it identifies the current backend flavor as
UNIX-socket Cap'n Proto IPC.

This leaves room for future backend families without overloading the current namespace, for
example:

- `bitcoin_core_sv2::tcp_capnp` (theoretical/future)
- `bitcoin_core_sv2::http_json_rpc` (theoretical/future)

Downstream applications should integrate through `bitcoin_core_sv2::runtime_api`, choose the Bitcoin Core major version at runtime, and build runtimes via `template_distribution_protocol::new` / `job_declaration_protocol::new`.

### JDP transaction lookup behavior by Bitcoin Core version

`DeclareMiningJob` handling differs by selected runtime:

- v30.x / v31.x: transaction availability is resolved from the local mempool mirror and missing wtxids trigger `MissingTransactions`.
- v32.x: transaction lookup uses Bitcoin Core IPC `getTransactionsByWitnessID` directly. No local mempool mirror is maintained for transaction data.

## Requirements

In order to build this, crate you need `capnproto` on your system.

For example, on Ubuntu/Debian:
```
apt-get install capnproto libcapnp-dev
```

Or macOS:
```
brew install capnproto
```

## Important Notes

### `LocalSet` Requirement

Due to limitations in the `capnp-rpc` dependency (where some abstractions do not implement the `Send` trait), `BitcoinCoreSv2TDP` and `BitcoinCoreSv2JDP` must be run within a [`tokio::task::LocalSet`](https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html). The crate examples demonstrate the proper setup pattern.

### Fee Threshold

The `fee_threshold` parameter (in satoshis) determines when a new template is distributed due to mempool changes. When the mempool fee delta exceeds this threshold, a new `NewTemplate` message is sent.

## Minimum Interval

The `min_interval` parameter (in seconds) determines the minimum amount of time between two consecutive `NewTemplate` messages (with exception to Chain Tip updates, which are always sent immediately, followed by `SetNewPrevHash`).

## Examples

- `tdp_logger` - Template Distribution Protocol logger built through `bitcoin_core_sv2::runtime_api`, pinned to Bitcoin Core v31.x (change one line to target another supported version).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
