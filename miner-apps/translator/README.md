# SV1 to SV2 Translator Proxy

A proxy that translates between Stratum V1 (SV1) and Stratum V2 (SV2) mining protocols. This translator enables SV1 mining devices to connect to SV2 pools and infrastructure, bridging the gap between legacy mining hardware and modern mining protocols.

## Architecture Overview

The translator sits between SV1 downstream roles (mining devices) and SV2 upstream roles (pool servers or proxies), providing seamless protocol translation and advanced features like channel aggregation and failover.

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

## Configuration

### Configuration File Structure

The translator uses TOML configuration files with the following structure:

```toml
# Downstream SV1 Connection (where miners connect)
downstream_address = "0.0.0.0"
downstream_port = 34255

# Protocol Version Support
max_supported_version = 2
min_supported_version = 2

# Extranonce Configuration
downstream_extranonce2_size = 4  # Min: 2, Max: 16 (CGminer max: 8)

# User Identity (appended with counter for each miner unless it starts with `sri/`)
user_identity = "your_username_here"

# Payout verification is opt-in. Keep false for standard pool mining,
# including pools that use a Bitcoin address as the username.
verify_payout = false

# Channel Configuration
aggregate_channels = true  # true: shared channel, false: individual channels

# Share audit. Unset share_audit_path keeps ordinary shares in the log only.
# Candidates are written to ./block-candidates.jsonl unless this is set to "".
# share_audit_path = "./share-audit.jsonl"
# block_candidate_audit_path = "./block-candidates.jsonl"

# Optional monitoring API and ASIC telemetry
monitoring_address = "0.0.0.0:9092"
monitoring_cache_refresh_secs = 15

# LAN subnet containing ASIC miner web/API addresses (for example, 192.168.1.0/24).
[miner_telemetry]
cidrs = ["192.168.1.0/24"]

# Downstream Difficulty Configuration
[downstream_difficulty_config]
min_individual_miner_hashrate = 10_000_000_000_000.0  # 10 TH/s
shares_per_minute = 6.0
enable_vardiff = true  # Set to false when using with Job Declarator Client (JDC)

# Upstream SV2 Connections (supports multiple with failover)
[[upstreams]]
address = "127.0.0.1"
port = 34254
authority_pubkey = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"

[[upstreams]]
address = "backup.pool.com"
port = 34254
authority_pubkey = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"
```

### Configuration Parameters

Make sure the machine running the Translator Proxy has its clock synced with an NTP server. Certificate validation is time-sensitive, and even a small drift of a few seconds can trigger an `InvalidCertificate` error.

#### **Downstream Configuration**
- `downstream_address`: IP address for SV1 miners to connect to. SV1 is plaintext, so keep it on
  a trusted LAN (see [Trust Model](#trust-model))
- `downstream_port`: Port for SV1 miners to connect to

tProxy uses `sv1_api` to parse the initial `mining.configure`, `mining.subscribe`, or
`mining.authorize` request before requesting a mining channel. Configure is optional,
and subscribe and authorize may arrive in either order. Ancillary setup requests may
follow; shares are accepted only after both subscribe and authorize responses have
completed.

Prefix updates follow job boundaries. If a change is processed before subscription,
the subscribe response includes it. Otherwise, tProxy sends `mining.set_extranonce`
immediately before the first delivered job using that prefix. During setup, it caches
the transition instead of disconnecting immediately. The miner must announce support
with `mining.extranonce.subscribe` before that job is delivered; without support,
tProxy disconnects rather than advertising work with an unknown prefix. There is no
additional capability-negotiation timeout.

In aggregated mode, new miners can join while old-prefix jobs remain active or pending.
Their subscribe response uses the active job's prefix, and inherited jobs retain their
own upstream prefixes and validation targets. Later jobs use the current upstream state.
As in `channels_sv2`, target changes update queued future jobs but leave active jobs' targets intact.
All prefix variants preserve the miner's allocated `local_prefix | local_index`; joining
does not reset the shared allocator, job history, or keepalive timer.

#### **Protocol Configuration**
- `max_supported_version`/`min_supported_version`: SV2 protocol version support
- `min_extranonce2_size`: Minimum extranonce2 size (affects mining efficiency)

#### **Channel Configuration**
- `aggregate_channels`: 
  - `true`: All miners share one upstream extended channel (more efficient)
  - `false`: Each miner gets its own upstream extended channel (more isolated)
- `user_identity`: Username for pool authentication (auto-suffixed per miner)
- `verify_payout`: When `true`, verify upstream coinbase payouts against a payout address encoded
  by `user_identity`. Keep `false` for standard pool mining, including pools that use a Bitcoin
  address as the username.

#### **Solo/Donation Payout Verification**
Payout verification defaults to `false` for compatibility with standard pool mining. When payout
verification is disabled, tProxy trusts the upstream server's coinbase payout policy and does not
verify that jobs pay the miner.
Set `verify_payout = true` for solo mining, or donation configurations where `user_identity`
intentionally encodes an on-chain payout address:

- `sri/solo/<payout_address>/<worker>`: tProxy verifies every upstream extended job pays 100% of spendable coinbase outputs to `<payout_address>`
- `<payout_address>[.worker]`: legacy solo mode, verified by checking that at least 90% of spendable coinbase outputs go to `<payout_address>`
- `sri/donate/<pool_percentage>/<payout_address>/<worker>`: tProxy verifies the miner address receives the remaining percentage
- `sri/donate/<worker>`: full donation mode; keep `verify_payout = false` because no miner payout address is present

If verification fails, tProxy triggers upstream fallback instead of forwarding the job to SV1 miners.

#### **Difficulty Configuration**
- `min_individual_miner_hashrate`: Expected hashrate of weakest miner (in H/s)
- `shares_per_minute`: Target share submission rate
- `enable_vardiff`: Enable/disable variable difficulty adjustment (set to false when using with JDC)
  - When `true`: Translator manages difficulty adjustments based on share submission rates
  - When `false`: Upstream manages difficulty, translator forwards SetTarget messages to miners

- `job_keepalive_interval_secs`: Idle interval before a miner receives a keepalive
  `mining.notify`; `0` disables keepalives. Only miners whose own interval has elapsed
  receive one. In aggregated mode, tProxy generates at most one new shared keepalive
  per interval. A miner becoming due between advances receives the existing
  shared job. A late joiner also receives the current shared job without resetting
  shared history or the keepalive schedule. Normal upstream jobs are forwarded
  without waiting for keepalive deadlines.

#### **Difficulty and Targets**
Each miner is advertised the easier of two targets: the one vardiff wants for it, and the target
of its upstream SV2 channel. With `enable_vardiff = false`, only the upstream target applies. The
difficulty is worked out again whenever either target changes, and a new `mining.set_difficulty`
is sent only when the result changes. A larger target is easier, so a miner never works at a
difficulty harder than the upstream's and never discards a share the upstream would credit, and it
works at the difficulty vardiff wants as far as the upstream allows. Shares that meet a miner's
difficulty but not the upstream target are accepted from the miner and not forwarded.

- **Initial difficulty**: with `enable_vardiff = true`, vardiff starts from the difficulty derived
  from `min_individual_miner_hashrate` and `shares_per_minute`, which tProxy also requests as the
  channel's `max_target`, the easiest target the upstream may assign. With
  `enable_vardiff = false`, tProxy accepts any target and a miner starts at the upstream's target
  for the channel; `shares_per_minute` is then only used to derive the hashrate reported for it.
- **Vardiff updates**: each update is sent upstream as an `UpdateChannel` whose `max_target` is
  the new target (in aggregated mode, the hardest across miners). An easier target applies right
  away; a harder one applies as far as the upstream target allows, and fully once the upstream
  lowers its target to it with `SetTarget`.
- **Upstream targets**: a miner follows a `SetTarget` easier than its difficulty. The upstream can
  send one without breaking the rules below, for instance while a lower `max_target` is within its
  30 seconds or after rejecting it, and in aggregated mode a miner can join a shared channel whose
  target is easier than its initial difficulty. A `SetTarget` harder than the target vardiff wants
  leaves the miner at the vardiff difficulty.
- **Rounding**: difficulties of 1 and above are advertised rounded down to a power of two. Shares
  are validated against the advertised difficulty, so a miner is never held to a harder one than
  it was told.

tProxy enforces the Stratum V2 rules on targets:
- The target assigned when a channel opens must not exceed the requested `max_target`.
- A `SetTarget` must not exceed the `max_target` of the latest `UpdateChannel` the upstream
  accepted, or of the open request. tProxy gives the upstream 30 seconds to process each
  `UpdateChannel`: until then, a `SetTarget` within the previous value is still accepted, since
  the upstream may have sent it before processing the change. A change the upstream rejects with
  `UpdateChannel.Error` leaves the previous value in force.

An upstream that breaks either rule triggers fallback to the next configured upstream.

The specification recommends not changing `max_target` again within those 30 seconds, so that a
client can tell which value a `SetTarget` is bound by. tProxy sends every change right away
instead, so the upstream learns each new value without waiting, and accepts a `SetTarget` within
any value the upstream may still be bound by. If several changes are within their 30 seconds when
an `UpdateChannel.Error` arrives, tProxy cannot tell which one was rejected and keeps the easiest
of them in force until the next change.

An upstream that takes longer than 30 seconds to answer an `UpdateChannel` can still be misjudged.
When its `UpdateChannel.Error` arrives late, tProxy goes back to the easier of the rejected value
and the one it replaced. But a `SetTarget` above a lower value that arrives after the 30 seconds
and before that late rejection triggers fallback, since nothing tells tProxy that a rejection is
coming.

#### **Miner Telemetry**
Translator Proxy can enrich the monitoring API with telemetry from the ASICs connected to its SV1
port. This is useful when you want the UI to show each miner's management IP, firmware, hashrate,
power, temperature, uptime, and mining status.

Set `[miner_telemetry].cidrs` to the LAN subnet that contains the miners' web/API addresses. For
example, if a Bitaxe web UI is available at `192.168.1.63`, use:

```toml
# LAN subnet containing ASIC miner web/API addresses (for example, 192.168.1.0/24).
[miner_telemetry]
cidrs = ["192.168.1.0/24"]
```

Use the miner's LAN subnet, not the Docker subnet and not the Translator Proxy's own IP. Translator
Proxy scans that subnet and assigns telemetry to an SV1 connection when the username configured in
the miner's pool settings matches the worker name accepted by Translator Proxy and the miner's pool
port matches `downstream_port`.

Keep pool usernames/worker names unique for connected miners. If two connected miners use the same
name, telemetry is not assigned to either of them and the monitoring API reports
`duplicate_worker_name`.

#### **Upstream Configuration**
- `address`/`port`: SV2 upstream server connection details
- `authority_pubkey`: Public key for SV2 connection authentication

#### **Share audit**

A share that meets the downstream target is an info line. `share_hash` is the header hash in the same hex order bitcoind and block explorers display. `worker` is the name from the miner's `mining.authorize`. The config `user_identity` is the pool account and is not written on these lines.

```text
INFO share_accepted
     worker=rig07 downstream_channel_id=1 job_id=42
     share_hash=0000000000008483… meets_downstream_target=true
```

A share that also meets the upstream channel target is forwarded. The info line adds `upstream_channel_id`, `sequence_number`, and `block_candidate`. A share that meets the downstream target and misses the upstream target is `share_not_forwarded` at debug, with `reason=does_not_meet_upstream_target`. That gap is expected while vardiff is on: the miner target is often easier than the pool target.

```text
INFO share_forwarded
     worker=rig07 downstream_channel_id=1 upstream_channel_id=7
     job_id=42 sequence_number=9 share_hash=0000000000008483…
     block_candidate=false
```

When the header meets the chain-tip `nbits`, the same forward is also a warning. Grep `BLOCK_CANDIDATE` in the translator log. The line means the hash met `nbits` when it was validated. `chain_status` on that line stays `unknown`. It is not a statement that the block is in the best chain. A later chain-status line, described below, is a separate event.

```text
WARN BLOCK_CANDIDATE meets network target; not a chain-tip confirmation
     share_hash=0000… nbits=0x17020093 meets_network_target=true
     chain_status=unknown
```

`share_audit_path` is an append-only JSONL file, one JSON object per line. Leave it unset and ordinary shares stay in the log only. `share_audit_block_candidates_only = true` keeps that file to candidates and their pool results. `block_candidate_audit_path` defaults to `block-candidates.jsonl` in the working directory, including when `share_audit_path` is unset. Set it to `""` to disable that file.

The pool acks shares in a batch. `SubmitSharesSuccess` carries `last_sequence_number` and `new_submits_accepted_count`. Each sequence in that window becomes one `share_upstream_result` row, with the hash from the matching forward. The match key is `(upstream_channel_id, sequence_number)`.

In aggregated mode every miner shares one upstream channel, so sequence numbers are one series on that channel. Each row still has its own `downstream_channel_id` and `worker`. In non-aggregated mode each miner has its own upstream channel. The same sequence number on two channels is two different shares. A pool ack on one channel does not attach to the other.

`result` is `success` or `error`. An error row includes `error_code`, for example `difficulty-too-low`.

```json
{"event":"share_forwarded","worker":"rig07","downstream_channel_id":1,"upstream_channel_id":7,"job_id":42,"sequence_number":9,"share_hash":"0000000000008483…","block_candidate":false}
{"event":"share_upstream_result","upstream_channel_id":7,"downstream_channel_id":1,"sequence_number":9,"job_id":42,"worker":"rig07","share_hash":"0000000000008483…","result":"success","block_candidate":false}
{"event":"block_candidate","share_hash":"0000…","nbits":"0x17020093","meets_network_target":true,"chain_status":"unknown","worker":"rig07","sequence_number":1}
```

The forward path appends those lines and returns. It does not call bitcoind. Each line is flushed as it is written. Worker names are stored as the miner sent them. File rotation is outside the process.

The default log level is info (`RUST_LOG` unset). That is one `share_accepted` line per share that met the downstream target, and one `share_forwarded` line per share sent upstream. With `shares_per_minute = 6`, expect about six accept lines per miner per minute, plus the forwards that cleared the pool target. `share_not_forwarded` stays at debug, so the larger set of shares that miss the pool target stays out of the log unless `RUST_LOG=debug`.

`RUST_LOG` is a single level name: `error`, `warn`, `info`, `debug`, or `trace`. A module filter is ignored and the level falls back to info. `RUST_LOG=warn` keeps `BLOCK_CANDIDATE`, upstream share errors, and `orphaned_or_stale`, and drops the per-share info lines, including `in_best_chain` and `not_found`. The JSONL files do not follow `RUST_LOG`. Leave `share_audit_path` unset when the log is enough. The candidate file stays small, because a share that meets `nbits` is rare.

Environment overrides use the `TPROXY__` prefix:

```bash
TPROXY__SHARE_AUDIT_PATH=./share-audit.jsonl
TPROXY__SHARE_AUDIT_BLOCK_CANDIDATES_ONLY=false
TPROXY__BLOCK_CANDIDATE_AUDIT_PATH=./block-candidates.jsonl
TPROXY__SHARE_AUDIT_BITCOIN_RPC_URL=http://127.0.0.1:8332
TPROXY__SHARE_AUDIT_BITCOIN_RPC_COOKIE_PATH=/absolute/path/to/.cookie
```

#### **Chain status**

Chain-status checks stay off until `share_audit_bitcoin_rpc_url` is set to a bitcoind RPC endpoint on the same network as the upstream pool. Auth is `share_audit_bitcoin_rpc_cookie_path`, or `share_audit_bitcoin_rpc_user` together with `share_audit_bitcoin_rpc_password`. With a valid URL, startup logs:

```text
INFO Chain-status RPC enabled; only block candidates are checked
     url=http://127.0.0.1:8332 initial_secs=120 max_secs=86400 stop_confirmations=6
```

An unset or invalid URL does not emit that line, and ordinary shares never call bitcoind. A check starts only after a share meets the network `nbits` target (`BLOCK_CANDIDATE`). The first `getblockheader` waits `share_audit_chain_poll_initial_secs` (default 120). Later tries wait 10 minutes, then 30, then 60. Polling stops at `share_audit_chain_stop_confirmations` (default 6) or after `share_audit_chain_poll_max_secs` (default 86400).

Each answer is an `event=chain_status` log. `share_hash` is the candidate. `network` is the node's chain (`main` is logged as `mainnet`). `checked_at` in the JSONL row is the time of that RPC call.

```text
INFO chain_status block is in the best chain
     chain_status=in_best_chain confirmations=6 network=mainnet poll_expired=false

WARN chain_status header is known but not in the best chain
     chain_status=orphaned_or_stale confirmations=-1 network=mainnet poll_expired=false

INFO chain_status block hash is not in the node yet
     chain_status=not_found network=mainnet poll_expired=false
```

`in_best_chain` means the header is in the node's active chain. The tip itself has `confirmations=1`. `orphaned_or_stale` means the node already knows the header and it is not in that chain (`confirmations` is negative). `not_found` means this node has not seen the hash yet; `poll_expired=true` means the time limit passed on that answer. A node that is down or still loading is a warning only and does not write `not_found`. The `BLOCK_CANDIDATE` line keeps `chain_status=unknown`; these lines are separate.

### Environment Variables

Every configuration value can also be supplied through the environment. Variables are prefixed
with `TPROXY` and joined with a **double underscore** (`__`) between nested keys — a single
underscore is just part of a field name (`TPROXY__DOWNSTREAM_PORT` sets `downstream_port`).

Environment variables take precedence over the TOML file, and the file itself is optional: the
translator can be configured entirely from the environment. If a mandatory parameter is supplied
by neither source, the translator exits with an error.

```bash
TPROXY__DOWNSTREAM_ADDRESS=0.0.0.0
TPROXY__DOWNSTREAM_PORT=34255
# Nested fields join each level with `__`:
TPROXY__DOWNSTREAM_DIFFICULTY_CONFIG__SHARES_PER_MINUTE=6.0
# List fields (supported_extensions, required_extensions) are comma-separated.
# A lone value is read as a 1-element list:
TPROXY__SUPPORTED_EXTENSIONS=2,3
# Nested list fields work the same way (see Miner Telemetry above):
TPROXY__MINER_TELEMETRY__CIDRS=192.168.1.0/24
```

#### Upstreams via the environment

The `[[upstreams]]` array cannot be addressed with `__` paths alone, so it uses a dedicated
form: `TPROXY__UPSTREAM_<NAME>__<FIELD>`. `<NAME>` groups one upstream's fields together. If any
such variable is set, the resulting list **fully replaces** the `upstreams` array from the file.

```bash
TPROXY__UPSTREAM_01__ADDRESS=primary.pool.com
TPROXY__UPSTREAM_01__PORT=34254
TPROXY__UPSTREAM_01__AUTHORITY_PUBKEY=primary_pool_pubkey
TPROXY__UPSTREAM_02__ADDRESS=backup.pool.com
TPROXY__UPSTREAM_02__PORT=34254
TPROXY__UPSTREAM_02__AUTHORITY_PUBKEY=backup_pool_pubkey
```

Upstreams are prioritized in **alphabetical order** of `<NAME>`. Watch out for these footguns:

* Ordering is lexicographic, not numeric: `10` sorts before `2`. Zero-pad numbered upstreams
  (`01`, `02`, …, `10`) to keep the intended order.
* Names sort alphabetically, not by meaning: `BACKUP` sorts before `PRIMARY`.
* Only a **double** underscore separates the name from the field; single underscores belong to
  the name or field itself (`TPROXY__UPSTREAM_POOL_A__PORT` is upstream `POOL_A`'s `port`).

See `docker/docker_env.example` for a complete working set of variables.

## Usage

### Installation & Build

```bash
# Clone the repository
git clone https://github.com/stratum-mining/stratum.git
cd stratum

# Build the translator
cargo build --release -p translator_sv2
```

### Running the Translator

#### **With Local Pool**
```bash
cd miner-apps/translator
cargo run -- -c config-examples/mainnet/tproxy-config-local-pool-example.toml
```

#### **With Job Declaration Client**
```bash
cd miner-apps/translator
cargo run -- -c config-examples/mainnet/tproxy-config-local-jdc-example.toml
```

#### **With Hosted Pool**
```bash
cd miner-apps/translator
cargo run -- -c config-examples/mainnet/tproxy-config-hosted-pool-example.toml
```

### Command Line Options

```bash
# Use specific config file
translator_sv2 -c /path/to/config.toml
translator_sv2 --config /path/to/config.toml

# Show help
translator_sv2 -h
translator_sv2 --help
```

## Configuration Examples

### Example 1: Local Pool Setup
For connecting to a local SV2 pool server:

```toml
downstream_address = "0.0.0.0"
downstream_port = 34255
user_identity = "miner_farm_1"
verify_payout = false
aggregate_channels = true

[downstream_difficulty_config]
min_individual_miner_hashrate = 10_000_000_000_000.0
shares_per_minute = 6.0
enable_vardiff = true

[[upstreams]]
address = "127.0.0.1"
port = 34254
authority_pubkey = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"
```

### Example 2: High-Availability Setup
For production environments with failover:

```toml
downstream_address = "0.0.0.0"
downstream_port = 34255
user_identity = "production_farm"
verify_payout = false
aggregate_channels = true

[downstream_difficulty_config]
min_individual_miner_hashrate = 50_000_000_000_000.0  # 50 TH/s
shares_per_minute = 10.0
enable_vardiff = true

# Primary upstream
[[upstreams]]
address = "primary.pool.com"
port = 34254
authority_pubkey = "primary_pool_pubkey"

# Backup upstream
[[upstreams]]
address = "backup.pool.com"
port = 34254
authority_pubkey = "backup_pool_pubkey"
```

## Architecture Details

### **Component Overview**

1. **SV1 Server**: Handles incoming SV1 connections from mining devices
2. **SV2 Upstream**: Manages connections to SV2 pool servers with failover
3. **Channel Manager**: Orchestrates message routing and protocol translation
4. **Task Manager**: Manages async task lifecycle and coordination
5. **Status System**: Provides real-time monitoring and health reporting

### **Channel Modes**

- **Aggregated Mode**: All miners share one  extended channel
  - More efficient for large farms
  - Reduced upstream connection overhead
  - Shared work distribution

- **Non-Aggregated Mode**: Each miner gets individual upstream channel
  - Better isolation between miners
  - Individual difficulty adjustment by the upstream Pool

### **Upstream Fallback**

When the current upstream cannot be used, tProxy falls back to the next upstream in its
configuration. Fallback disconnects every SV1 miner; miners reconnect to tProxy once the next
upstream is ready. Each upstream is used at most once: when none is left, tProxy stops.

A failed channel open triggers fallback in both channel modes, whether the upstream rejects it
with `OpenMiningChannel.Error` or assigns a target easier than the requested `max_target`. In
non-aggregated mode the channel belongs to a single miner, yet every miner is disconnected, since
the next upstream may accept the same request. When the miner or the configuration causes the
failure instead, the miner fails again after reconnecting and causes another fallback, as tProxy
trusts the miners connected to it (see [Trust Model](#trust-model)).

An upstream can also ask tProxy to move with `Reconnect`, for example before maintenance. tProxy
disconnects its miners as for a fallback, then connects to the requested host and port with the
same authority key and user identity, so the request can only point to another server of the same
pool. If the requested endpoint cannot be used or reached, tProxy moves on to the configured
upstreams. A request arriving less than 5 minutes after the last one tProxy followed is ignored,
and tProxy stays on the current upstream. A `ChannelEndpointChanged` reconnects to the current
upstream the same way, so that extensions are negotiated again. Within 5 minutes of the last
followed request it is ignored like a `Reconnect`, and the extensions negotiated on the current
connection stay in use.

Fallback is also triggered by an upstream payout that fails verification (see
[Solo/Donation Payout Verification](#solodonation-payout-verification)) and by a `SetTarget` above
the allowed `max_target` (see [Difficulty and Targets](#difficulty-and-targets)).

### **Trust Model**

tProxy is meant to run in the same LAN as the SV1 miners it serves, and that LAN is trusted.

- **The SV1 side is plaintext.** Miners connect over unencrypted, unauthenticated SV1. Anyone on
  the network path between miners and tProxy can read their traffic, including the
  `mining.authorize` credentials, and an on-path peer can modify it. tProxy does not offer TLS for
  SV1: protecting this hop would depend on every miner's firmware supporting TLS and verifying
  certificates.
- **The upstream side is protected.** The connection that crosses untrusted networks, to the
  upstream, uses Stratum V2 with Noise encryption, authenticated with the configured
  `authority_pubkey`.
- **Listeners must stay on the LAN.** The example configurations bind `downstream_address` and
  `monitoring_address` to `0.0.0.0`, which listens on every interface. The monitoring API has no
  authentication either. On a host reachable from untrusted networks, bind both to the LAN
  interface or restrict them with a firewall, and never expose them to the internet.
- **Connected miners are trusted.** tProxy assumes every connected miner acts in good faith: some
  failures caused by a single miner affect all the miners sharing the translator (see
  [Upstream Fallback](#upstream-fallback)).

These are operational assumptions: nothing in tProxy verifies that the network or the miners can
be trusted.
