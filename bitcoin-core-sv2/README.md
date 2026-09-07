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

### Socket Trust

The IPC transport has no authentication. The crate connects to whatever process listens at the socket path (`<data_dir>/node.sock`, or the network subdirectory for non-mainnet networks) and trusts its templates, mempool data and block validation results as if they came from Bitcoin Core. Anyone who can create or replace that socket file can therefore answer as Bitcoin Core.

Bitcoin Core protects the socket through filesystem permissions alone, and so does this crate. Bitcoin Core's defaults already do the right thing: the data directory and `node.sock` are created with the node's umask, so only the user running Bitcoin Core can replace the socket or connect to it. Keep it that way:

- The directory holding `node.sock` must be owned by the user running Bitcoin Core and must not be writable by other users. Do not point `-ipcbind` or `data_dir` at a shared directory such as `/tmp`.
- Run the application as the user running Bitcoin Core, or as a user you explicitly trust with group access. The application writes a `solutions/` directory next to the socket, so it needs write access to that directory anyway.
- A local user who can write to Bitcoin Core's data directory can already tamper with `settings.json`, wallets and the chainstate, so this requirement is no stricter than what running the node already demands.

On connect, the crate logs the uid of the process serving the socket (`Bitcoin Core IPC socket is served by uid N`). Check it against the user running your node after deploying. In containerised setups the application usually runs as root while the node runs as another uid, so the two are expected to differ there.

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
