
# SRI Pool

SRI Pool is designed to communicate with Downstream role (most typically a Translator Proxy or a Mining Proxy) running SV2 protocol to exploit features introduced by its sub-protocols.

The most typical high level configuration is:

```
<--- Most Downstream ----------------------------------------- Most Upstream --->

+---------------------------------------------------+  +------------------------+
|                     Mining Farm                   |  |      Remote Pool       |
|                                                   |  |                        |
|  +-------------------+     +------------------+   |  |   +-----------------+  |
|  | SV1 Mining Device | <-> | Translator Proxy | <------> | SV2 Pool Server |  |
|  +-------------------+     +------------------+   |  |   +-----------------+  |
|                                                   |  |                        |
+---------------------------------------------------+  +------------------------+

```

It can receive templates from two potential sources:
- Sv2 Template Provider: a separate Sv2 application running either locally or on a different machine, for which a (optionally encrypted) TCP connection will be established
- Bitcoin Core v30.2+: an officially released Bitcoin Core node running locally, on the same machine, for which a UNIX socket connection will be established

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

## Setup

### Configuration File

There are three example configuration files demonstrating different template provider setups:

1. `pool-config-hosted-sv2-tp-example.toml` - Connects to a community hosted Sv2 Template Provider server
2. `pool-config-local-sv2-tp-example.toml` - Connects to a locally running Sv2 Template Provider
3. `pool-config-bitcoin-core-ipc-example.toml` - Connects directly to a Bitcoin Core node via IPC

The configuration file contains the following information:

1. The SRI Pool information which includes the SRI Pool authority public key
   (`authority_public_key`), the SRI Pool authority secret key (`authority_secret_key`).
2. The address which it will use to listen to new connection from downstream roles (`listen_address`)
3. The coinbase reward script specified as a descriptor (`coinbase_reward_script`)
4. A string that serves as signature on the coinbase tx (`pool_signature`).
5. The `template_provider_type` section, which determines how the pool obtains block templates. There are two options:
   - `[template_provider_type.Sv2Tp]` - Connects to an SV2 Template Provider, with the following parameters:
     - `address` - The Template Provider's network address
     - `public_key` - (Optional) The TP's authority public key for connection verification
   - `[template_provider_type.BitcoinCoreIpc]` - Connects directly to Bitcoin Core via IPC, with the following parameters:
     - `network` - Bitcoin network (mainnet, testnet4, signet, regtest) for determining socket path
     - `data_dir` - (Optional) Custom Bitcoin data directory. Uses OS default if not set
     - `fee_threshold` - Minimum fee threshold to trigger new templates

For connections with a Sv2 Template Provider, you may want to verify that your TP connection is authentic. You can get the `public_key` from the logs of your TP, for example:

```
# 2024-02-13T14:59:24Z Template Provider authority key: EguTM8URcZDQVeEBsM4B5vg9weqEUnufA8pm85fG4bZd
```

Make sure the machine running the Pool application has its clock synced with an NTP server. Certificate validation is time-sensitive, and even a small drift of a few seconds can trigger an `InvalidCertificate` error.

### Run

There are three example configuration files found in `pool-apps/pool/config-examples`:

1. `pool-config-hosted-sv2-tp-example.toml` - Connects to our community hosted Sv2 Template Provider server
2. `pool-config-local-sv2-tp-example.toml` - Runs with your local Sv2 Template Provider
3. `pool-config-bitcoin-core-ipc-example.toml` - Runs with direct Bitcoin Core IPC connection

Run the Pool (example using hosted Sv2 TP):

```bash
cd pool-apps/pool
cargo run -- -c config-examples/pool-config-hosted-sv2-tp-example.toml
```

## Solo Mining Mode

The solo mining mode is computed during runtime and expects that the `user_identity` value of `OpenStandardMiningChannel`/`OpenExtendMiningChannel` to be crafted in specific patterns.
If the `user_identity` does not match any of the patterns, the pool continues with the payout to the pool. If the `user_identity` matches the magic bytes: `sri` but the pattern is malformed we send a `OpenMiningChannelError`.
### User Identity Patterns

Miners must specify their payout mode via `user_identity`:

| Pattern | Mode | Description |
|---------|------|-------------|
| `sri/donate/optional_worker_name` | FullDonation | Full reward goes to pool |
| `sri/solo/payout_address/optional_worker_name` | Solo | Full reward goes to miner's address |
| `bc1qtzqxqaxyy6lda2fhdtp5dp0v56vlf6g0tljy2x.optional_worker_name`| Solo | Full reward goes to miner's address |
| `sri/donate/percentage/payout_address/optional_worker_name` | Donate | Pool gets %, miner gets remainder |


Any payout address valid for the descriptor `addr` (see [BIP-385](https://github.com/bitcoin/bips/blob/master/bip-0385.mediawiki)) is a valid payout address.
In all the patterns on the list, the worker name is optional.

### Error Scenarios

| Error Code | Cause |
|------------|-------|
| `invalid-user-identity` | Pattern doesn't match expected format |

