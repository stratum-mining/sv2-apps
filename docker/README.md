# SV2 Docker Compose Setup

This repository provides a ready-to-run Docker Compose setup for the full SV2 stack, including:

* **Pool Service**
* **Job Declarator Server (JDS)**
* **Job Declarator Client (JDC)**
* **Translator Proxy**

The services are wired together on a dedicated Docker network and can be enabled via Compose profiles depending on whether you're running pool-side components, miner-side components, or everything at once.

---

## Requirements

* Docker
* Docker Compose (v2+)
* A fully synced **Bitcoin Core v30+** node running
* Access to the `node.sock` file in your Bitcoin data directory

---

### Configuring Bitcoin Core

After downloading Bitcoin Core, you **must** configure it for the network you want to use and for the RPC settings required by the JD Server.

A minimal `bitcoin.conf` for **testnet4** looks like this:

```ini
[testnet4]
server=1
rpcuser=username
rpcpassword=password
rpcbind=0.0.0.0
rpcallowip=0.0.0.0/0
```

If you choose a different network (signet, mainnet, etc.), make sure the matching section exists and that your RPC credentials line up with your `docker_env`.

---

### IPC Requirements (pool + jd_client)

Some components, like the **pool** and **jd_client**, communicate with Bitcoin Core over **IPC** (via `node.sock`).
For this to work, Bitcoin Core must be started with IPC enabled. Whatever network you run, you must start Bitcoin Core with `-ipcbind=unix`

Example: starting a **testnet4** node with IPC bindings:

```bash
./bitcoin-30.0/bin/bitcoin -m node -testnet4 -ipcbind=unix
```

You'll also need to wait for the node to complete Initial Block Download (IBD).

---

## Setting the Bitcoin Socket Path

These are the typical paths for the `node.sock` file.
| Network  | Default Path                               |
| -------- | ------------------------------------------ |
| mainnet  | `~/.bitcoin/node.sock`                     |
| testnet4 | `~/.bitcoin/testnet4/node.sock`            |
| signet   | `~/.bitcoin/signet/node.sock`              |
| macOS    | Inside `~/Library/Application Support/...` |

Two of the services (`pool_sv2` and `jd_client_sv2`) need access to your local Bitcoin Core `node.sock`.
Because this path differs across operating systems, it is **not hardcoded**.
Instead, you must provide it via an environment variable:

### 1. Create a `docker_env` file (recommended)

In the same directory as `docker-compose.yml`, create a `docker_env` file:

```
BITCOIN_SOCKET_PATH=/absolute/path/to/your/node.sock
POOL__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION=31
JDC__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__VERSION=31
```
Make sure the path is correct, if there are spaces (like `Application Support`), keep the value unquoted.

---

## Running the Stack

This compose file uses *profiles* so you can run only what you need.

### Run everything

```bash
docker compose --profile pool_and_miner_apps --env-file docker_env up --build
```

### Run only pool-side services

```bash
docker compose --profile pool_apps --env-file docker_env up --build
```

### Run only miner-side services

```bash
docker compose --profile miner_apps --env-file docker_env up --build
```

### Run only Translator Proxy service 

```bash
docker compose --profile tproxy --env-file docker_env up --build
```

### Miner telemetry

Miner telemetry lets the monitoring API/UI show data read from the ASIC's web/API endpoint,
such as its management IP, firmware, hashrate, power, temperature, and uptime.

Set the subnet variable for the app that your miners connect to:

* Use `JDC_MINER_TELEMETRY_CIDR` when SV2 ASIC miners connect directly to JDC.
* Use `TPROXY_MINER_TELEMETRY_CIDR` when SV1 ASIC miners connect to Translator Proxy.

Use the LAN subnet that contains the miners' web/API addresses. For example, if your Bitaxe web
UI is at `192.168.1.63`, use `192.168.1.0/24`. If SV1 miners connect through Translator Proxy and
Translator Proxy connects to JDC, configure telemetry on Translator Proxy; JDC only fetches
per-miner telemetry for SV2 miners connected directly to it.

Each connected miner should use a unique pool username/worker name. If two connected miners use
the same name, telemetry is left unassigned for those miners and the API reports
`duplicate_worker_name`.

Telemetry is only assigned when the miner's web/API data reports both the expected username and the
stratum port of the app being monitored. This prevents Translator Proxy from showing telemetry for
an ASIC that has moved to JDC while another miner still uses the same username.

---

## Services Overview

Each service is configured **entirely from `docker_env`** — there are no config files to mount.
Variables use the `<PREFIX>__<FIELD>` naming, with `__` separating nested keys (the prefixes are
`POOL`, `JDC` and `TPROXY`). Two field shapes need special syntax:

* **Tagged enums** (e.g. `template_provider_type`) take the variant as a path segment and match
  case-insensitively, e.g. `POOL__TEMPLATE_PROVIDER_TYPE__BITCOINCOREIPC__NETWORK=mainnet`.
* **Upstreams** are arrays, so they use `<PREFIX>__UPSTREAM_<NAME>__<FIELD>` (e.g.
  `TPROXY__UPSTREAM_PRIMARY__ADDRESS`). `<NAME>` just groups one upstream's fields together; entries
  are ordered alphabetically by `<NAME>`. Sorting is lexicographic (`10` sorts before `2`, `BACKUP`
  before `PRIMARY`), so zero-pad numbered entries (`01`, `02`, ..., `10`) to control fallback
  priority.

See `docker_env.example` for a complete, ready-to-edit set of variables for every service.

If something behaves weirdly, 99% of the time your `docker_env` is the culprit.

### **pool_sv2**

* Port **3333**
* Uses `BITCOIN_SOCKET_PATH` for Bitcoin Core access
* use port **3334** to spawn the job declarator 

### **jd_client_sv2**

* Port **34265**
* Also mounts the same `node.sock` path from `docker_env`
* `JDC_MINER_TELEMETRY_CIDR` is the LAN subnet containing directly connected SV2 miners' web/API addresses

### **tproxy_sv2**

* Port **34255**
* Upstream target (JDC or pool) is set with `TPROXY__UPSTREAM_<NAME>__*` and defaults to
  `jd_client_sv2`. For the `pool_and_miner_apps_no_jd` profile, point it at `pool_sv2:3333` instead.
* `TPROXY__VERIFY_PAYOUT=false` is the standard pool-mining default; set it to `true` only for
  solo/donation identities where the upstream `user_identity` encodes the expected on-chain payout

---

## Configuration

All configuration lives in your `docker_env`.
Check `docker_env.example` for every supported variable, then create your real one:

```bash
cp docker_env.example docker_env
```

Keep the `docker_env` in the same directory as `docker-compose.yml`.

---

## Notes

* Double-check file permissions if the Bitcoin socket fails to mount.
* Make sure the machine running the applications has its clock synced with an NTP server. Certificate validation is time-sensitive, and even a small drift of a few seconds can trigger an `InvalidCertificate` error.


## Docker Image Tags

Each service image is available on Docker Hub with versioned tags.
Tags start at **`v0.1.0`** and will continue incrementing with future releases.

You can choose:

* A **specific version tag** (e.g. `v0.1.0`) for predictable, repeatable deployments.
* The **`main`** tag if you want the most recent updates of the main branch of the repository.

Example:

```yaml
image: pool_sv2:v0.1.0   # pinned version
# or
image: pool_sv2:main   #latest changes in the main branch
```

This applies to all images: `pool_sv2`, `jd_client_sv2`, and `translator_sv2`.
