# Stratum Apps

Complete Stratum V2 application development kit - all utilities in one crate.

## Overview

`stratum-apps` is a unified crate that provides all the utilities needed for building Stratum V2 applications.

## Architecture

This crate is organized into several main modules:

- **`network_helpers`** - High-level networking utilities (from `network_helpers_sv2`)
- **`config_helpers`** - Configuration management helpers (from `config_helpers_sv2`)  
- **`payout`** - Shared payout-mode parsing and coinbase-output distribution helpers
- **`fallback_coordinator`** - Runtime fallback cancellation and acknowledgement helpers

The crate also re-exports `stratum-core`, the central hub for the Stratum V2 ecosystem that provides a cohesive API for all low-level protocol functionality.
With the `bitcoin-core-sv2` feature enabled, it also re-exports `bitcoin_core_sv2` runtime APIs.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
stratum-apps = { version = "0.4.0", features = ["pool"] }
```

Basic usage:

```rust
use stratum_apps::{network_helpers, config_helpers};
```

## Features

### Core Features
- `network` - Networking utilities (enabled by default)
- `fallback-coordinator` - Runtime fallback coordination helpers (enabled by default)
- `config` - Configuration helpers (enabled by default)
- `payout` - Shared payout-mode parsing and coinbase-output distribution helpers (optional)
- `monitoring` - HTTP and Prometheus monitoring helpers (optional)
  - Uses vendored Swagger UI assets to support offline/sandboxed documentation builds
- `std` - Standard-library support for key and random utilities (enabled by default)
- `core` - Re-export and enable `stratum-core`
- `bitcoin-core-sv2` - Re-export and enable `bitcoin_core_sv2`

### Protocol Features
- `sv1` - Enable SV1 protocol support (includes translation utilities)
- `with_buffer_pool` - Enable buffer pooling for better performance

### Role-Specific Bundles
- `pool` - Pool application helpers, including networking, config, buffer pooling, core protocol types, payout helpers, and Bitcoin Core IPC runtime APIs
- `jd_client` - Job Declaration Client helpers, including networking, fallback coordination, config, buffer pooling, core protocol types, and Bitcoin Core IPC runtime APIs
- `jd_server` - Job Declaration Server config helpers and Bitcoin Core IPC runtime APIs
- `translator` - Translator Proxy helpers, including networking, fallback coordination, config, SV1 translation, buffer pooling, and payout helpers
- `mining_device` - Mining device config helpers

Third-party applications that only need payout parsing or verification can use the smaller feature
set:

```toml
[dependencies]
stratum-apps = { version = "0.4.0", default-features = false, features = ["payout"] }
```

## Usage Examples

### Pool Application

```toml
[dependencies]
stratum-apps = { version = "0.4.0", features = ["pool"] }
```

```rust
use stratum_apps::{network_helpers, config_helpers, tokio_util::sync::CancellationToken};

// Use networking
let cancellation_token = CancellationToken::new();
let connection =
    network_helpers::Connection::new(stream, HandshakeRole::Responder, cancellation_token).await?;

// Use configuration
let config: PoolConfig = config_helpers::parse_config("pool.toml")?;
```
