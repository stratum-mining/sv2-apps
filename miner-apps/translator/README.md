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
- `downstream_address`: IP address for SV1 miners to connect to
- `downstream_port`: Port for SV1 miners to connect to

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
Payout verification is disabled by default. Set `verify_payout = true` for solo mining or
donation configurations where `user_identity` intentionally encodes an on-chain payout address:

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
