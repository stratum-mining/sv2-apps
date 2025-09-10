# SV2 Pool Server Architecture Analysis

## Application Overview

The SV2 Pool Server is a core component in the Stratum V2 mining ecosystem that acts as an intermediary between Bitcoin miners and Template Providers. It manages multiple downstream miners, distributes work, validates shares, and coordinates with upstream Template Providers.

**Location**: `pool-apps/pool/`  
**Binary**: Pool Server  
**Primary Role**: Mining pool coordination and work distribution

## Architecture Patterns

### Actor-Based Concurrency Model
- Each downstream connection runs in its own Tokio task
- Separate tasks handle template reception, share processing, and connection management
- Message-passing through async channels for inter-task communication

### Factory Pattern
- `IdFactory` for generating unique channel IDs
- `ExtendedExtranonce` factories for managing extranonce allocation
- Channel factories for creating standard and extended mining channels

### Observer/Event-Driven Pattern
- Status monitoring system with centralized error handling
- Event propagation through status channels
- Reactive processing of template updates and share submissions

### Repository Pattern
- Channel management through HashMap-based storage
- Separate storage for standard channels, extended channels, and vardiff state
- Job storage abstraction with `DefaultJobStore`

## Key Components

### Core Library Structure (`src/lib/`)

1. **`mod.rs`** - Main orchestrator
   - Manages the `PoolSv2` struct lifecycle
   - Coordinates startup of all subsystems
   - Handles graceful shutdown and status monitoring

2. **`mining_pool/`** - Downstream miner management
   - `Pool`: Central state manager for all downstream connections
   - `Downstream`: Represents individual miner connections
   - `message_handler.rs`: SV2 protocol message processing
   - `setup_connection.rs`: Connection establishment and handshake

3. **`template_receiver/`** - Upstream Template Provider interface
   - `TemplateRx`: Manages TP connection and message relay
   - Receives `NewTemplate` and `SetNewPrevHash` messages
   - Forwards `SubmitSolution` messages upstream

4. **`config.rs`** - Configuration management
   - `PoolConfig`: Centralized configuration structure
   - Authority keys, network addresses, coinbase settings
   - Template Provider configuration

5. **`status.rs`** - Health monitoring and error handling
   - Centralized status reporting system
   - Component lifecycle management
   - Error classification and routing

## Data Flow

### Upstream Flow (Template Provider → Pool)
1. `TemplateRx` maintains persistent connection to Template Provider
2. Receives `NewTemplate` messages with job parameters
3. Receives `SetNewPrevHash` messages for chain updates
4. Forwards template data to `Pool` via async channels
5. Pool distributes appropriate jobs to all connected miners

### Downstream Flow (Pool → Miners)
1. Pool accepts TCP connections and performs Noise handshake
2. `Downstream` instances handle SV2 setup connection negotiation
3. Creates standard or extended mining channels based on miner capabilities
4. Distributes mining jobs with appropriate targets and extranonce ranges
5. Variable difficulty adjustment every 60 seconds per channel

### Share Processing Flow
1. Miners submit shares through `SubmitSharesStandard`/`SubmitSharesExtended`
2. Pool validates shares against current job parameters
3. Valid shares update share accounting and vardiff statistics
4. Network-difficulty solutions create `SubmitSolution` messages
5. Solutions forwarded to Template Provider via `TemplateRx`

## Configuration

- **Authority Keys**: Public/private key pairs for SV2 authentication
- **Network Settings**: Listen addresses and connection parameters
- **Coinbase Configuration**: Script descriptors for pool payouts
- **Template Provider Settings**: Upstream connection details with optional authentication
- **Mining Parameters**: Shares per minute, batch sizes, difficulty targets

## Dependencies

### Core SV2 Stack
- `stratum-common`: SV2 protocol implementation and network helpers
- `buffer_sv2`: Message serialization and framing
- `roles_logic_sv2`: Mining protocol logic and channel management
- `codec_sv2`: SV2 message encoding/decoding
- `key-utils`: Cryptographic operations and key management

### Infrastructure
- `tokio`: Async runtime for concurrent task management
- `async-channel`: Inter-task communication primitives
- `config` (ext-config): TOML configuration parsing
- `tracing`: Structured logging and observability

## Key Strengths

1. **Scalability**: Concurrent handling of multiple miners with per-connection tasks
2. **Resilience**: Robust error handling with component isolation
3. **Protocol Compliance**: Full SV2 specification implementation
4. **Flexibility**: Support for both standard and extended channel types
5. **Security**: Noise protocol encryption and authority-based authentication
6. **Observability**: Comprehensive logging and status reporting