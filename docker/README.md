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
* A fully synced **Bitcoin Core v30.2++** node running on **testnet4** (or any network you prefer)
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

---

## Services Overview

Each service loads its settings from a template in `config/`, but **you never touch those templates directly**.
All configuration comes from a single `docker_env` file, and Docker Compose automatically substitutes the values into the right places.

If something behaves weirdly, 99% of the time your `docker_env` is the culprit.

### **pool_sv2**

* Port **3333**
* Uses `BITCOIN_SOCKET_PATH` for Bitcoin Core access
* use port **3334** to spawn the job declarator 

### **jd_client_sv2**

* Port **34265**
* Also mounts the same `node.sock` path from `docker_env`

### **tproxy_sv2**

* Port **34255**
* Upstream target (JDC or pool) is fully controlled via `docker_env` variables

---

## Configuration (centralized)

Everything lives in your `docker_env`.
Check `docker_env.example` for all supported options, then create your real one:

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
