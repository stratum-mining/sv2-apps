
# Job Declarator Client

The **Job Declarator Client (JDC)** is responsible for:

* Connecting to the **Pool** and **JD Server**.
* Connecting to the **Template Provider**.
* Receiving custom block templates from the Template Provider and declaring them to the pool via the **Job Declaration Protocol**.
* Sending jobs to downstream clients.
* Forwarding shares to the pool.

## Architecture Overview

The JDC sits between **SV2 downstream clients** (e.g., SV2 mining devices or Translator Proxies) and **SV2 upstream servers** (the Pool and JD Server).

* It obtains templates from the Bitcoin node.
* It creates and broadcasts jobs to downstream clients.
* It declares and sets custom jobs to the pool side.
* It also supports solo mining mode in case no upstream is available or the upstream is fraudulent

Note: while JDC can cater for multiple downstream clients, with either one or multiple channels per client, it only opens one single extended channel with the upstream Pool server.

```
<--- Most Downstream ------------------------------------------------------------------------------------------------ Most Upstream --->

+----------------------------------------------------------------------------------------------------+   +------------------------------+
|                     Mining Farm                                                                     |  |      Remote Pool             |
|                                                                                                     |  |                              |
|  +-------------------+     +------------------+                                                     |  |    +-----------------+       |
|  | SV1 Mining Device | <-> | Translator Proxy |-------|                  |------------------------------->  | SV2 Pool Server |       |
|  +-------------------+     +------------------+       |                  |                          |  |    +-----------------+       |
|                                                       |                  |                          |  |                              |
|                                                       |                  |                          |  |                              |
|                                                 +-----------------------+|                          |  |                              |
|                                                 | Job Declarator Client |                           |  |                              |
|                                                 +-----------------------+|                          |  |    +-----------------------+ |
|                                                     |                    |--------------------------------> | Job Declarator Server | |
|   +-------------------+                             |                                               |  |    +-----------------------+ |
|   | SV2 Mining Device |-----------------------------|                                               |  |                              |
|   +-------------------+                                                                             |  |                              |
|                                                                                                     |  |                              |
|                                                                                                     |  |                              |
|                                                                                                     |  |                              |
+----------------------------------------------------------------------------------------------------+   +------------------------------+

It can receive templates from two potential sources:
- Sv2 Template Provider: a separate Sv2 application running either locally or on a different machine, for which a (optionally encrypted) TCP connection will be established
- Bitcoin Core v30.2+: an officially released Bitcoin Core node running locally, on the same machine, for which a UNIX socket connection will be established

```

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

The configuration file contains the following information:

1. The downstream socket information, which includes the listening IP address (`downstream_address`) and port (`downstream_port`).
2. The maximum and minimum protocol versions (`max_supported_version` and `min_supported_version`) with size as (`min_extranonce2_size`)
3. The authentication keys used for the downstream connections (`authority_public_key`, `authority_secret_key`)
4. The `template_provider_type` section, which determines how the pool obtains block templates. There are two options:
   - `[template_provider_type.Sv2Tp]` - Connects to an SV2 Template Provider, with the following parameters:
     - `address` - The Template Provider's network address
     - `public_key` - (Optional) The TP's authority public key for connection verification
   - `[template_provider_type.BitcoinCoreIpc]` - Connects directly to Bitcoin Core via IPC, with the following parameters:
     - `version` - Required Bitcoin Core IPC schema major version (`30` or `31`, any other value fails startup)
     - `network` - Bitcoin network (mainnet, testnet4, signet, regtest) for determining socket path
     - `data_dir` - (Optional) Custom Bitcoin data directory. Uses OS default if not set
     - `fee_threshold` - Minimum fee threshold to trigger new templates
     - `min_interval` - Minimum interval between template updates in seconds

For connections with a Sv2 Template Provider, you may want to verify that your TP connection is authentic. You can get the `public_key` from the logs of your TP, for example:

```
# 2024-02-13T14:59:24Z Template Provider authority key: EguTM8URcZDQVeEBsM4B5vg9weqEUnufA8pm85fG4bZd
```

Make sure the machine running the JDC has its clock synced with an NTP server. Certificate validation is time-sensitive, and even a small drift of a few seconds can trigger an `InvalidCertificate` error.

### Miner Telemetry

JDC can enrich the monitoring API with telemetry from SV2 ASIC miners that connect directly to JDC.
This lets the UI show each miner's management IP, firmware, hashrate, power, temperature, uptime,
and mining status.

Set `[miner_telemetry].cidrs` to the LAN subnet that contains the miners' web/API addresses. For
example, if a Bitaxe web UI is available at `192.168.1.63`, use:

```toml
monitoring_address = "0.0.0.0:9091"
monitoring_cache_refresh_secs = 15

# LAN subnet containing ASIC miner web/API addresses (for example, 192.168.1.0/24).
[miner_telemetry]
cidrs = ["192.168.1.0/24"]
```

Use the miner's LAN subnet, not the Docker subnet and not JDC's own IP. JDC scans that subnet and
assigns telemetry to a connected SV2 miner when the username configured in the miner's pool settings
matches the user identity on the miner's SV2 channel and the miner's pool port matches JDC's
listening port.

If SV1 miners connect to Translator Proxy and Translator Proxy connects to JDC, configure miner
telemetry on Translator Proxy instead. JDC identifies Translator Proxy as a proxy client, not as the
ASICs behind it.

Keep pool usernames/worker names unique for connected miners. If two connected miners use the same
name, telemetry is not assigned to either of them and the monitoring API reports
`duplicate_worker_name`.

### Environment Variables

Every configuration value can also be supplied through the environment. Variables are prefixed
with `JDC` and joined with a **double underscore** (`__`) between nested keys — a single
underscore is just part of a field name (`JDC__LISTENING_ADDRESS` sets `listening_address`).

Environment variables take precedence over the TOML file, and the file itself is optional: JDC
can be configured entirely from the environment. If a mandatory parameter is supplied by neither
source, JDC exits with an error.

```bash
JDC__LISTENING_ADDRESS=0.0.0.0:34265
# Tagged enums (template_provider_type) take the variant as a path segment,
# matched case-insensitively:
JDC__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION=31
JDC__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__NETWORK=mainnet
# List fields (supported_extensions, required_extensions) are comma-separated.
# A lone numeric value is read as a scalar, not a 1-element list, so list at
# least two values:
JDC__SUPPORTED_EXTENSIONS=2,3
```

#### Upstreams via the environment

The `[[upstreams]]` array cannot be addressed with `__` paths alone, so it uses a dedicated
form: `JDC__UPSTREAM_<NAME>__<FIELD>`. `<NAME>` groups one upstream's fields together. If any
such variable is set, the resulting list **fully replaces** the `upstreams` array from the file.

```bash
JDC__UPSTREAM_01__AUTHORITY_PUBKEY=9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72
JDC__UPSTREAM_01__POOL_ADDRESS=127.0.0.1
JDC__UPSTREAM_01__POOL_PORT=34254
JDC__UPSTREAM_01__JDS_ADDRESS=127.0.0.1
JDC__UPSTREAM_01__JDS_PORT=34264
JDC__UPSTREAM_01__USER_IDENTITY=your_username_here
```

Upstreams are prioritized in **alphabetical order** of `<NAME>`. Watch out for these footguns:

* Ordering is lexicographic, not numeric: `10` sorts before `2`. Zero-pad numbered upstreams
  (`01`, `02`, …, `10`) to keep the intended order.
* Names sort alphabetically, not by meaning: `BACKUP` sorts before `PRIMARY`.
* Only a **double** underscore separates the name from the field; single underscores belong to
  the name or field itself (`JDC__UPSTREAM_POOL_A__POOL_PORT` is upstream `POOL_A`'s
  `pool_port`).

See `docker/docker_env.example` for a complete working set of variables.

### Run

Example configuration files are available under `miner-apps/jd-client/config-examples/<network>/`.
Common choices include:

1. `jdc-config-hosted-infra-example.toml` - Connects to a community hosted infra (Pool + JDS + Sv2 TP)
2. `jdc-config-local-infra-example.toml` - Connects to a local infra (Pool + JDS + Sv2 TP)
3. `jdc-config-bitcoin-core-ipc-hosted-infra-example.toml` - Connects to a local Bitcoin Core via IPC, and a community hosted infra (Pool + JDS)
4. `jdc-config-bitcoin-core-ipc-local-infra-example.toml` - Connects to a local Bitcoin Core via IPC, and a local infra (Pool + JDS)
5. `jdc-config-solo-mining-example.toml` - Runs JDC in solo mining mode

Run JDC (example using hosted infra):
```bash
cd miner-apps/jd-client
cargo run -- -c config-examples/mainnet/jdc-config-bitcoin-core-ipc-hosted-infra-example.toml
```

## Architecture Details

### **Component Overview**

1. **Channel Manager**: Orchestrates message routing among sub-systems in JDC
2. **Task Manager**: Manages async task lifecycle and coordination
3. **Status System**: Provides real-time monitoring and health reporting

## Internal Architecture

JDC is built from several modules that divide responsibility for handling different roles and protocols:

### **Modules**

1. **Upstream**

   * Connects to the **pool**.
   * Handles messages coming from the Pool  (the ones defined in the Common Protocol are directly handled, others are forwarded to the Channel Manager).

2. **Downstream**

   * Accepts connections from Sv2 Mining Devices or Translator Proxies.
   * Includes a **ChannelState**, which provisions new channels when `OpenStandard/ExtendedChannel` messages arrive from the downstreams.

3. **Template Receiver**

   * Connects to the **Template Provider**.
   * Handles messages received by the TP (the ones defined in the Common Protocol are directly handled, while the others are forwarded to the Channel Manager).

4. **Job Declarator**

   * Connects to the **Job Declarator Server (JDS)**.
   * Handles messages received by the JDS (the ones defined in the Common Protocol are directly handled, while the others are forwarded to the Channel Manager).

5. **Channel Manager (Orchestrator)**

   * Central coordination point.
   * Responsibilities:

     * Handles **non-common messages** forwarded from all modules.
     * Maintains **upstream channel state**.
     * Maintains most of the **Job Declarator state**.
     * Orchestrates job lifecycle and state synchronization across upstream and downstream roles.
