# Job Declarator Client (JDC) Architecture Analysis

## Application Overview

The Job Declarator Client (JDC) serves as a critical miner-side component in the Stratum V2 ecosystem that enables **Job Declaration** - allowing miners to create and use custom block templates rather than being forced to use pool-provided templates.

**Location**: `miner-apps/jd-client/`  
**Binary**: Job Declarator Client  
**Primary Role**: Custom job creation and declaration with pool fallback capabilities

## Architecture Patterns

### Actor-Based Concurrent Architecture
- Uses Tokio's async runtime with separate tasks for each major component
- Components communicate via async channels (`async_channel::unbounded`)
- Each component runs independently and can fail/restart without affecting others

### Message-Passing Architecture
- Heavy use of SV2 protocol messages for inter-component communication
- Implements standardized message handlers for different protocol types
- Uses channel-based communication between components

### State Machine Pattern
- `DownstreamMiningNodeStatus` tracks connection lifecycle stages
- Pool fallback mechanism uses state transitions
- Template handling uses state coordination via `IS_NEW_TEMPLATE_HANDLED`

### Observer/Status Reporting Pattern
- Centralized status reporting system in `lib/status.rs`
- Components report health/errors to main control loop
- Enables coordinated shutdown and error handling

## Key Components

### Core Runtime (`lib/mod.rs`)
- **JobDeclaratorClient**: Main orchestrator struct (4,872 total lines of code)
- Manages startup sequence: Pool → JDS → Downstream → Template Receiver
- Handles pool fallback logic and graceful shutdown
- Contains `PoolChangerTrigger` for detecting unresponsive pools

### Upstream SV2 (`lib/upstream_sv2/`)
- **Upstream**: Manages connection to mining pools
- Handles SV2 Mining Protocol communication
- Implements `SetCustomMiningJob` functionality
- Manages job ID correlation and share forwarding

### Job Declarator (`lib/job_declarator/`)
- **JobDeclarator**: Communicates with Job Declarator Server (JDS)
- Manages mining job token allocation (maintains pool of 2 tokens)
- Handles `DeclareMiningJob` and `CommitMiningJob` messages
- Tracks future jobs and template correlation

### Downstream (`lib/downstream.rs`)
- **DownstreamMiningNode**: Handles connections from miners/proxies
- Implements SV2 Job Distribution Protocol
- Currently limited to single downstream connection (noted as needing refactor)
- Manages mining channel setup and share processing

### Template Receiver (`lib/template_receiver/`)
- **TemplateRx**: Connects to Template Provider (e.g., Bitcoin Core)
- Receives `NewTemplate` and `SetNewPrevHash` messages
- Coordinates template distribution to job declarator and downstream
- Handles solution submission back to template provider

## Data Flow

### Initialization Sequence
```
1. Upstream: ->SetupConnection, <-SetupConnectionSuccess
2. Downstream: <-SetupConnection, ->SetupConnectionSuccess, <-OpenExtendedMiningChannel
3. Upstream: ->OpenExtendedMiningChannel, <-OpenExtendedMiningChannelSuccess
4. Downstream: ->OpenExtendedMiningChannelSuccess
5. JobDeclarator: ->SetupConnection, <-SetupConnectionSuccess, ->AllocateMiningJobToken(x2)
6. TemplateRx: ->CoinbaseOutputDataSize
```

### Main Processing Loop
```
1. TemplateRx: <-NewTemplate, SetNewPrevHash
2. JobDeclarator: -> CommitMiningJob, <-CommitMiningJobSuccess  
3. Upstream: ->SetCustomMiningJob, Downstream: ->NewExtendedMiningJob, ->SetNewPrevHash
4. Downstream: <-Share
5. Upstream: ->Share
```

### Critical Timing Optimization
- **Immediate Job Distribution**: `NewExtendedMiningJob` sent to downstream immediately upon `NewTemplate` receipt
- **Delayed Pool Declaration**: `SetCustomMiningJob` only sent to pool when template becomes "active"
- **Token Pre-allocation**: Maintains 2 job tokens to avoid blocking on token requests

### Synchronization Mechanism
- Uses `IS_NEW_TEMPLATE_HANDLED` atomic boolean for template/prev_hash coordination
- Acquire-Release memory ordering ensures proper message sequencing
- Template receiver waits for downstream processing before handling `SetNewPrevHash`

## Configuration

### Configuration Structure
- **TOML-based configuration** with two example modes:
  - `jdc-config-local-example.toml`: For local development/testing
  - `jdc-config-hosted-example.toml`: For connecting to community-hosted services

### Key Configuration Elements
- **Upstream pools**: Array of backup pools with authority keys and addresses
- **Template Provider**: Connection details and optional authority verification
- **Cryptographic keys**: Authority public/secret keys for Noise encryption
- **Network settings**: Listening address, protocol versions, timeouts
- **Solo mining**: Coinbase output script for fallback mode

## Dependencies

### Core SV2 Dependencies
```toml
stratum-common = { git = "https://github.com/stratum-mining/stratum" }
buffer_sv2 = { git = "https://github.com/stratum-mining/stratum" }
key-utils = { git = "https://github.com/stratum-mining/stratum" }
config_helpers_sv2 = { git = "https://github.com/stratum-mining/stratum" }
```

### Key External Components
1. **Template Provider** (e.g., Bitcoin Core): Source of block templates
2. **Job Declarator Server (JDS)**: Pool-side job declaration endpoint
3. **Mining Pool**: SV2-compatible pool for share submission
4. **Downstream Miners**: Mining devices or proxies connecting to JDC

### Networking and Crypto
- **secp256k1**: Cryptographic operations
- **tokio**: Async runtime and networking
- **Noise Protocol**: Encrypted communication channels

## Key Strengths

1. **Fault Tolerance**: Automatic pool fallback and solo mining capability
2. **Performance Optimization**: Minimizes latency between template receipt and mining start
3. **Protocol Compliance**: Full SV2 specification implementation
4. **Modularity**: Clean separation of concerns between components
5. **Observability**: Comprehensive status reporting and error handling

## Current Limitations

1. **Single Downstream Limitation**: Currently supports only one downstream connection (noted in code comments as needing refactor)
2. **Error Recovery**: Some components may need more granular error recovery mechanisms
3. **Configuration Validation**: Could benefit from more extensive configuration validation

## Role in SV2 Ecosystem

The JDC serves as a **bridge** between template providers (like Bitcoin Core) and mining pools, enabling miners to maintain control over transaction selection while still participating in pool mining with robust fallback mechanisms and performance optimizations.