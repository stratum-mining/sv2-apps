# SV2 Translator Proxy Architecture Analysis

## Application Overview

The **Translator Proxy** serves as a critical bridge component in the Stratum V2 ecosystem that enables **legacy SV1 mining devices** to connect to **modern SV2 pools** without requiring firmware updates. It acts as a protocol translator between SV1 miners and SV2 pool infrastructure.

**Location**: `miner-apps/translator/`  
**Binary**: Translator Proxy  
**Primary Role**: Protocol translation between Stratum V1 and Stratum V2

## Architecture Patterns

### Actor-Based Concurrent Architecture
The application uses an actor-like pattern with three main components:
- **Upstream Handler** (SV2 client role)
- **Bridge** (protocol translator)  
- **Downstream Handler** (SV1 server role)

### Channel-Based Message Passing
Extensive use of async channels for inter-component communication:
```rust
tx_sv2_submit_shares_ext      // Bridge -> Upstream
rx_sv1_downstream            // Downstream -> Bridge  
tx_sv1_notify               // Bridge -> Downstream (broadcast)
tx_status                   // All -> Main loop
```

### Event-Driven Status Management
Centralized error handling and status reporting through a unified status system with automatic reconnection logic.

### Factory Pattern for Channel Management
Uses `ProxyExtendedChannelFactory` for managing SV2 mining channels with the upstream pool.

## Key Components

### 1. `main.rs` - Entry Point
- CLI argument processing
- Configuration loading (TOML)
- Logging initialization
- Instantiates and starts `TranslatorSv2`

### 2. `lib/mod.rs` - Core Orchestration (`TranslatorSv2`)
**Primary Responsibilities:**
- Main event loop management
- Component lifecycle coordination
- Status monitoring and error handling
- Graceful shutdown and reconnection logic
- Task management and abortion

**Key Features:**
- Randomized reconnection delays (0-3000ms) to prevent thundering herd
- Centralized task collection for cleanup
- Signal handling (Ctrl+C)
- Automatic upstream reconnection on channel errors

### 3. `lib/upstream_sv2/` - SV2 Upstream Connection
- **upstream.rs**: Core upstream connection logic
- **upstream_connection.rs**: Low-level network handling
- **diff_management.rs**: Difficulty adjustment logic

**Responsibilities:**
- SV2 protocol handshake and connection establishment
- Noise protocol encryption setup
- Channel opening with upstream pool
- Receiving jobs (`SetNewPrevHash`, `NewExtendedMiningJob`)
- Submitting translated shares (`SubmitSharesExtended`)
- Managing upstream difficulty and hashrate reporting

### 4. `lib/downstream_sv1/` - SV1 Downstream Server
- **downstream.rs**: Core downstream server logic
- **diff_management.rs**: SV1 difficulty management

**Responsibilities:**
- TCP listener for SV1 miner connections
- SV1 protocol handshake (`mining.subscribe`, `mining.authorize`)
- Receiving SV1 share submissions (`mining.submit`)
- Broadcasting SV1 job notifications (`mining.notify`)
- Individual miner difficulty management and vardiff implementation
- Connection timeout handling (10s subscribe timeout)

### 5. `lib/proxy/bridge.rs` - Protocol Translation Core
This is the **heart of the application** responsible for:

**Message Translation:**
- **SV1 → SV2**: `mining.submit` → `SubmitSharesExtended`
- **SV2 → SV1**: `SetNewPrevHash` + `NewExtendedMiningJob` → `mining.notify`

**State Management:**
- Job ID mapping and tracking (`last_job_id`)
- Future job buffering for jobs received before prev hash
- Target difficulty coordination
- Channel factory management for SV2 extended channels

**Data Flow Coordination:**
- Manages communication between upstream and downstream
- Handles job sequencing and mining notify generation
- Coordinates share submissions with proper channel IDs

## Data Flow

### 1. Initialization Flow
```
CLI Args → Config Loading → TranslatorSv2::new() → TranslatorSv2::start()
  ↓
Status Channel Setup → Target State Initialization → Task Collector
  ↓  
internal_start() → Component Initialization
```

### 2. Upstream Connection Flow
```
Upstream::new() → Connection Establishment → SV2 Handshake → 
Channel Opening → Extranonce Reception → Bridge Initialization
```

### 3. Share Submission Flow (SV1 → SV2)
```
SV1 Miner → mining.submit → Downstream → SubmitShareWithChannelId → 
Bridge → SubmitSharesExtended → Upstream → SV2 Pool
```

### 4. Job Distribution Flow (SV2 → SV1)
```
SV2 Pool → SetNewPrevHash + NewExtendedMiningJob → Upstream → Bridge → 
mining.notify → Broadcast → SV1 Miners
```

### 5. Error Handling & Reconnection Flow
```
Component Error → Status Channel → Main Event Loop → 
Decision (Shutdown vs Reconnect) → Task Abortion → Component Restart
```

## Configuration

### Configuration Structure
The application uses TOML-based configuration with two deployment models:

1. **Local Pool Mode** (`tproxy-config-local-pool-example.toml`):
   - Direct connection to local SV2 pool (port 34254)
   - No Job Declaration Client involvement

2. **Local JDC Mode** (`tproxy-config-local-jdc-example.toml`):
   - Connection through Job Declaration Client (port 34265)
   - Supports job negotiation features

### Key Configuration Parameters
```toml
# Upstream SV2 connection
upstream_address = "127.0.0.1"
upstream_port = 34254
upstream_authority_pubkey = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72"

# Downstream SV1 interface  
downstream_address = "0.0.0.0"
downstream_port = 34255

# Protocol parameters
min_extranonce2_size = 4
max_supported_version = 2

# Difficulty management
[downstream_difficulty_config]
min_individual_miner_hashrate = 10_000_000_000_000.0  # 10 TH/s
shares_per_minute = 6.0

[upstream_difficulty_config]
channel_diff_update_interval = 60  # seconds
channel_nominal_hashrate = 10_000_000_000_000.0  # 10 TH/s
```

## Dependencies

### Core SV2 Protocol Dependencies
```toml
stratum-common = { features = ["with_network_helpers"] }
buffer_sv2, v1, error_handling, key-utils  # SV2 protocol stack
```

### Async Runtime & Networking
```toml
tokio = { features = ["full"] }  # Async runtime
async-channel = "1.5.1"         # Inter-task communication
futures = "0.3.25"              # Stream processing
```

### Cryptography & Security
- **Noise Protocol**: Encrypted communication with SV2 upstream
- **Secp256k1**: Public key authentication for upstream authority
- **Network Security**: TLS-like security through noise protocol handshake

## Key Strengths

1. **Resilient Design**: Automatic reconnection with randomized delays prevents cascading failures
2. **Scalable Communication**: Broadcast channels efficiently distribute job notifications to multiple miners
3. **Modular Architecture**: Clear separation of concerns between protocol layers
4. **Comprehensive Error Handling**: Centralized status system with granular error classification
5. **Configuration Flexibility**: Supports multiple deployment scenarios (direct pool vs JDC)
6. **Performance Optimization**: Async/await throughout, efficient channel-based communication

## Current Limitations

1. **Share Response Limitation**: Always responds `"result": true` to SV1 submits regardless of upstream rejection
2. **Hardcoded Subscription ID**: Uses fixed subscription ID instead of generating unique ones
3. **Limited Job Declaration**: Basic proxy functionality without full job negotiation capabilities

## Role in SV2 Ecosystem

The Translator Proxy enables smooth SV2 adoption while maintaining compatibility with existing mining infrastructure, serving as a **bridge solution** that allows mining farms with legacy SV1 equipment to benefit from SV2's advanced features without requiring hardware firmware updates.