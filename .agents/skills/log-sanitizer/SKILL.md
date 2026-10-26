---
name: log-sanitizer
description: Production logging standard for Stratum V2 services — enforces signal-to-noise hygiene across operators and developers, structured tracing fields, actionable warning thresholds, and clean protocol formatting.
---

# Stratum V2 Logging Standard

This guide defines the production logging philosophy and review standard for Stratum V2 services and crates across `sv2-apps`.

---

## 1. The Three Audiences

Logs serve three distinct consumers across different severity tiers:

1. **Automated Monitoring & Alerting** (`ERROR`, `WARN`): Indexes structured fields to evaluate node health and trigger paging alerts with minimal false positives.
2. **On-Call Operators** (`INFO`, `WARN`): Follows high-level system state, milestones, and actionable remediation steps in production.
3. **Developers & Protocol Engineers** (`DEBUG`, `TRACE`): Inspects wire receipts, channel state transitions, frame contents, and edge cases during debugging and development.

---

## 2. Scope & Boundaries

1. **Confined to Logging Ergonomics**: This standard governs log level tiering, phrasing, formatting traits (`Display` vs `Debug`), and structured tracing fields. It must never alter error handling structures, return types, error conversions, or control flow.
2. **Relaxed Rules for `DEBUG` & `TRACE`**: The strict phrasing templates, frequency limits, and conciseness rules apply to `INFO`, `WARN`, and `ERROR`. `DEBUG` and `TRACE` logs are for developers—they can be high-volume, granular, and free-form. Do not police developer verbosity or strip diagnostic context at these tiers.

---

## 3. Severity Taxonomy

| Level | Primary Audience | Scope & Definition | Frequency |
| :--- | :--- | :--- | :--- |
| **`ERROR`** | Alerts, Operators | **Unrecoverable failures**. Invariant violations, unrecoverable state corruptions, or fatal protocol violations triggering immediate process or component abort. Never for retriable connection failures. | Rare |
| **`WARN`** | Operators, Alerts | **Actionable anomalies & degraded states**. Situations where the system survived, but is operating sub-optimally or requires attention (see criteria below). | Low |
| **`INFO`** | Operators | **Lifecycle milestones & benign races**. Key connection transitions, channel activations, aggregated throughput, and natural self-healing network races. | Low / Interval |
| **`DEBUG`** | Developers | **Protocol diagnostics**. Wire message receipts, internal task routing, per-share validations, and expected clean shutdowns. | Medium / High |
| **`TRACE`** | Developers | **Low-level packet dumps**. High-frequency frame dumps, cryptographic handshakes, and tight-loop iterations. | Very High |

### The `WARN` Criteria

A log belongs at **`WARN`** if it falls into one of these four categories:

1. **Actionable Operator Intervention**: Something is misconfigured or an external dependency is unreachable, and an engineer can/should fix it (*e.g. DNS resolution failure, miner difficulty set too low causing share flooding, deprecated config option*).
2. **Degraded Operation & Automatic Fallback**: The node survived automatically, but is running with reduced redundancy or capacity (*e.g. primary upstream dropped and system fell back to solo mining, backup pool activated*).
3. **Approaching Critical Thresholds**: The system hasn't failed yet, but will if the trend continues (*e.g. retries nearly exhausted on attempt 3 of 3, token or deduplication cache reaching capacity*).
4. **Peer Protocol Misbehavior**: A remote client or peer sent invalid frames, unsupported extensions, or malformed data that the local server safely rejected or isolated (*e.g. downstream invalid extranonce size, peer requesting incompatible extensions*).

### What is Never a `WARN`
- **Benign protocol races**: A new Bitcoin block mined on the network causing in-flight jobs to be rejected as a stale tip (this is natural Bitcoin behavior → `INFO`).
- **Routine lifecycle events**: Clean SIGINT/SIGTERM shutdown signals, reader/writer thread joins, or normal miner socket disconnects/reboots (→ `INFO` or `DEBUG`).
- **Standard retry progression**: Attempt 1 of 3 retrying after a normal backoff interval (→ `INFO` or `DEBUG`).

---

## 4. Formatting & Tracing Conventions

### Structured Fields Over String Interpolation
Keep the message template static so log aggregation engines (Loki, Datadog, CloudWatch) can group log lines cleanly. Put identifiers, socket addresses, timeouts, and error objects in structured tracing fields:

```rust
// ❌ Anti-pattern: String interpolation prevents template grouping
info!("DeclareMiningJob rejected on channel {} with request id {}", channel_id, request_id);

// ✅ Standard: Static template with structured key-value fields
info!(channel_id, request_id, "DeclareMiningJob rejected by JDS: stale chain tip (ignoring)");
```

### Protocol Message Printing (`{}` vs `{:?}`)
- **Always `{}` for SV2 messages**: Protocol structs implement `fmt::Display`. Using `{}` renders clean protocol names and decodes UTF-8 fields (`user_identity`).
- **Never `{:?}` for SV2 messages**: `{:?}` dumps raw byte arrays (e.g. `[115, 111, 109, ...]`) and internal wrapper types.
- **Error objects**: Use `error = ?e` (Debug for error kind/context) or `error = %e` (Display for human description).

### Tone & Cleanliness
- **No decorative/alarmist emojis**: Eliminate emojis (`🛑`, `❌`, `⚠️`, `🔌`).
- **Block Found Exception**: Finding a Bitcoin block is the ultimate mining milestone; loud, celebratory formatting (`💰 Block Found!!! 💰`) is explicitly welcomed.
- **No internal Rust variable names**: Describe domain concepts rather than code internals (e.g. "previous block hash not available" instead of `last_declare_job is None`).
- **Accepted Acronyms**: `JDS` (Job Declarator Server), `JDC` (Job Declarator Client), `TP` (Template Provider).
- **Forbidden Abbreviations**:
  - `prevhash` → `previous block hash`
  - `ch` → `channel` (or structured field `channel_id`)
  - `seq` → `sequence_number`

---

## 5. Message Phrasing: Formulaic Hybrid Style

All `INFO` and `WARN` statements should follow the **Formulaic Hybrid** pattern, answering *what happened*, *why*, and *what the operator should check*:

```text
"<Context> <outcome>: <plain reason> (<action>)"
```

**Examples:**
- `warn!(downstream_id, channel_id, "Disconnecting downstream: standard channel share rate too high (check miner difficulty target)");`
- `info!(request_id, "DeclareMiningJob rejected by JDS: stale chain tip (ignoring)");`
- `warn!(host = %host, port, error = %e, "Failed to resolve upstream address (triggering fallback)");`
- `warn!("Waiting for initial template from Template Provider (check if Bitcoin node is undergoing IBD)");`

---

## 6. Common Protocol Recipes

| Scenario | Prescribed Level & Pattern |
| :--- | :--- |
| **Wire message receipt** | `debug!("Received: {}", msg);` |
| **Internal channel closed (teardown)** | `debug!(error = ?e, "Failed to forward message (channel closed)");` |
| **Routine connection closure / EOF** | `debug!(peer_addr = %addr, "Connection closed cleanly");` |
| **Benign network tip race** | `info!(request_id, "<Message> rejected by <peer>: stale chain tip (ignoring)");` |
| **Missing previous block hash** | `warn!(request_id, "Cannot apply declared job: previous block hash is not available yet");` |
| **Share validation (routine)** | `debug!(channel_id, sequence_number, share_hash = %hash, "Forwarding valid share to upstream");` |
| **Share batch acknowledgment** | `info!(channel_id, accepted_count, work_sum, "Acknowledged share batch from downstream");` |
| **Block found** | `info!(channel_id, sequence_number, share_hash = %hash, "💰 Block Found!!! 💰 Forwarding solution to upstream");` |
| **Routine shutdown signal** | `info!("Shutdown signal received, aborting connection");` |
| **Degraded fallback** | `info!("Upstream initialization failed; falling back to solo mining mode");` |
| **Malformed peer message** | `warn!(downstream_id, "Received unsupported message type from downstream (ignoring)");` |

---

## 7. Review Checklist for New Code & PRs

When reviewing or writing code:
1. **The WARN Check**: Does every `warn!` meet one of the 4 criteria (actionable, degraded, threshold risk, peer misbehavior)? If it's a routine shutdown or expected race, demote it.
2. **The Flood Check**: Are routine wire messages or individual share validations logged at `info!`? (Demote to `debug!`).
3. **The Shutdown Check**: Are clean cancellation tokens or task exits triggering warnings/errors? (Keep at `info!` for milestone or `debug!` for internal task exit).
4. **The Tracing Check**: Are dynamic variables interpolated into strings instead of key-value tracing fields? Are SV2 messages using `{}` instead of `{:?}`?
