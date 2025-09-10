# Job Declarator Server (JDS) Architecture Analysis

## Application Overview

The Job Declarator Server (JDS) is a critical component in the Stratum V2 ecosystem that serves as an intermediary between mining pools and Job Declarator Clients (JDCs). It enables miners to declare custom transaction selections while maintaining pool coordination.

**Location**: `pool-apps/jd-server/`  
**Binary**: Job Declarator Server  
**Primary Role**: Job declaration protocol coordination and transaction management

## Architecture Patterns

### Actor Model with Task Separation
- Each major component runs in its own async task
- Communication through bounded/unbounded channels (`async_channel`)
- Clear separation of concerns with independent lifecycles

### Centralized Error Handling
- All errors funnel through a status channel to the main runtime loop
- Structured error propagation with context-aware handling
- Graceful shutdown coordination across all components

### Shared State with Arc<Mutex<T>>
- Thread-safe access to shared resources (mempool, downstream connections)
- Lock contention minimized through careful design

### Message-Driven Protocol Implementation
- SV2 protocol messages handled through structured parsing and response generation
- State machines for managing connection lifecycles

## Key Components

### Core Runtime (`lib/mod.rs`)
- **Central orchestrator** that coordinates all system components
- Spawns and monitors background tasks:
  - Mempool synchronization with Bitcoin node
  - Block submission handling
  - Downstream connection management
  - Transaction integration
- Implements shutdown logic via `select!` loop listening for SIGINT or critical errors

### Job Declarator (`job_declarator/mod.rs`)
- **TCP connection handling** for downstream JDC clients
- **SV2 protocol implementation** including:
  - `AllocateMiningJobToken` - Issues tokens for mining jobs
  - `DeclareMiningJob` - Processes job declarations with transaction lists
  - `ProvideMissingTransactions` - Handles missing transaction data
  - `PushSolution` - Receives mining solutions for block assembly
- **Per-client state management** with dedicated `JobDeclaratorDownstream` instances
- **Noise handshake and SetupConnection** protocol handling

### Mempool (`mempool/mod.rs`)
- **Local transaction cache** using `HashMap<Txid, Option<(Transaction, u32)>>`
  - `None` = transaction known by ID only
  - `Some(tx, count)` = full transaction data with reference counter
- **Bitcoin node RPC integration** for:
  - Fetching raw transactions (`getrawtransaction`)
  - Submitting completed blocks (`submitblock`)
  - Synchronizing with node mempool (`getrawmempool`)
- **Transaction lifecycle management** with reference counting for memory efficiency

## Data Flow

### Transaction Flow Pipeline

1. **Mempool Synchronization**
   - Background task polls Bitcoin node mempool every configured interval
   - Inserts "thin" entries (txid only) into local mempool cache

2. **Job Declaration Processing**
   ```
   JDC sends DeclareMiningJob → JDS validates token → 
   JDS checks transaction availability → 
   JDS responds with success/ProvideMissingTransactions
   ```

3. **Transaction Resolution**
   - **Known transactions**: JDS fetches full data from Bitcoin node via RPC
   - **Unknown transactions**: JDC provides full transaction data in `ProvideMissingTransactionsSuccess`

4. **Mining Solution Processing**
   ```
   JDC sends PushSolution → JDS reconstructs complete block → 
   JDS submits block to Bitcoin node → Success/failure logged
   ```

### Concurrency Model
- **Multi-task architecture** with independent async tasks for each major function
- **Channel-based communication** with bounded queues to prevent memory exhaustion
- **Shared state protection** via Arc<Mutex<T>> with minimal lock duration
- **Error propagation** through dedicated status channels to central coordinator

## Configuration

### Configuration Structure
- **TOML configuration files** with examples for local and hosted deployments
- **Key configuration parameters**:
  - `listen_jd_address`: Server listening address for JDC connections
  - `core_rpc_url/port/user/pass`: Bitcoin node RPC connection details
  - `authority_public_key/secret_key`: Cryptographic keys for authentication
  - `coinbase_reward_script`: Bitcoin script descriptor for coinbase outputs
  - `mempool_update_interval`: Frequency of mempool synchronization

### Deployment Modes
- **Local deployment** (`127.0.0.1:34264`) for development/testing
- **Hosted deployment** (`0.0.0.0:34264`) for production environments
- **Flexible Bitcoin node integration** - can connect to any compatible RPC endpoint

## Dependencies

### Core SV2 Dependencies
- `stratum-common`: Core SV2 protocol implementation and message types
- `rpc_sv2`: Mini RPC client for Bitcoin node communication
- `buffer_sv2`: Binary message serialization/deserialization
- `key-utils`: Cryptographic operations (secp256k1, Noise)

### System Dependencies
- `tokio`: Async runtime with full feature set for networking and concurrency
- `async-channel`: Multi-producer, multi-consumer channels for task communication
- `serde`/`ext-config`: Configuration parsing and serialization
- `hashbrown`: High-performance HashMap implementation with custom hashers

### External Integrations
- **Bitcoin Core RPC**: Full dependency on Bitcoin node for mempool data and block submission
- **Network protocols**: TCP sockets with Noise encryption for secure client connections

## Key Strengths

1. **Transaction Management**: Efficient mempool synchronization with reference counting
2. **Protocol Compliance**: Full SV2 Job Declaration Protocol implementation
3. **Scalability**: Multi-client support with per-connection state management
4. **Security**: Noise protocol encryption and cryptographic job token validation
5. **Reliability**: Robust error handling and graceful shutdown coordination
6. **Bitcoin Integration**: Direct RPC integration for transaction fetching and block submission

## Memory Management
- **Reference counting** for transactions shared across multiple mining jobs
- **Automatic cleanup** when jobs are superseded or clients disconnect
- **Bounded channels** to prevent unbounded memory growth under load