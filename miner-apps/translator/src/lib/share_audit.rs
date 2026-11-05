//! Append-only JSONL share-hash audit trail (Phase 2).
//!
//! When [`TranslatorConfig::share_audit_path`](crate::config::TranslatorConfig) is set,
//! forwarded shares and upstream ack/nack results are written as one JSON object per line.
//! See `SHARE_HASH_AUDIT_SCHEMA.md`.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use chrono::{SecondsFormat, Utc};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

/// Clonable handle; disabled when constructed with [`ShareAudit::disabled`].
#[derive(Clone, Debug, Default)]
pub struct ShareAudit {
    /// General share audit (`share_audit_path`). May be candidates-only.
    inner: Option<Arc<ShareAuditInner>>,
    /// Dedicated block-candidate file. `None` when disabled or when it is the same
    /// path as `inner` (that file already records candidates).
    candidates: Option<Arc<ShareAuditInner>>,
    /// Queues block-candidate hashes for a bitcoind check. Unset when RPC is off.
    chain_checks: Option<ChainCheckSink>,
}

#[derive(Clone)]
struct ChainCheckSink(UnboundedSender<ChainCheckRequest>);

impl std::fmt::Debug for ChainCheckSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChainCheckSink")
    }
}

/// A network-difficulty share waiting for a bitcoind lookup.
#[derive(Clone, Debug)]
pub struct ChainCheckRequest {
    pub share_hash: String,
    pub worker: Option<String>,
    pub downstream_channel_id: u32,
    pub upstream_channel_id: u32,
    pub job_id: u32,
    pub sequence_number: u32,
}

/// One bitcoind answer appended to the audit trail.
pub struct ChainStatusUpdate {
    pub share_hash: String,
    pub worker: Option<String>,
    pub downstream_channel_id: u32,
    pub upstream_channel_id: u32,
    pub job_id: u32,
    pub sequence_number: u32,
    pub chain_status: &'static str,
    pub confirmations: Option<i64>,
    pub network: Option<String>,
    pub poll_expired: bool,
}

#[derive(Debug)]
struct ShareAuditInner {
    file: Mutex<File>,
    block_candidates_only: bool,
    /// Pending forwards keyed by (upstream_channel_id, sequence_number).
    pending: Mutex<HashMap<(u32, u32), PendingForward>>,
}

#[derive(Debug, Clone)]
struct PendingForward {
    share_hash: String,
    worker: Option<String>,
    job_id: u32,
    downstream_channel_id: u32,
    block_candidate: bool,
}

/// Fields recorded when a share is forwarded upstream.
pub struct ForwardedShare<'a> {
    pub worker: Option<&'a str>,
    pub downstream_channel_id: u32,
    pub upstream_channel_id: u32,
    pub job_id: u32,
    pub sequence_number: u32,
    pub share_hash: String,
    pub block_candidate: bool,
    /// Compact network target (`nbits`) from the chain tip used to classify `BlockFound`.
    pub nbits: Option<u32>,
}

impl ShareAudit {
    pub fn disabled() -> Self {
        Self {
            inner: None,
            candidates: None,
            chain_checks: None,
        }
    }

    pub fn open(path: &Path, block_candidates_only: bool) -> io::Result<Self> {
        info!(
            path = %path.display(),
            block_candidates_only,
            "Share audit JSONL enabled"
        );
        Ok(Self {
            inner: Some(open_inner(path, block_candidates_only)?),
            candidates: None,
            chain_checks: None,
        })
    }

    /// Opens the general share audit (optional) and a dedicated block-candidate file.
    ///
    /// Candidates are always recorded on `candidate_path` unless it is the same file as
    /// `share_path` (that file already stores `block_candidate` rows).
    pub fn open_audits(
        share_path: Option<&Path>,
        share_block_candidates_only: bool,
        candidate_path: Option<&Path>,
    ) -> io::Result<Self> {
        let inner = match share_path {
            Some(path) => {
                info!(
                    path = %path.display(),
                    block_candidates_only = share_block_candidates_only,
                    "Share audit JSONL enabled"
                );
                Some(open_inner(path, share_block_candidates_only)?)
            }
            None => None,
        };

        let candidates = match candidate_path {
            Some(path) if share_path != Some(path) => {
                info!(
                    path = %path.display(),
                    "Block-candidate JSONL enabled"
                );
                Some(open_inner(path, true)?)
            }
            Some(path) => {
                info!(
                    path = %path.display(),
                    "Block-candidate JSONL shares the share-audit file"
                );
                None
            }
            None => None,
        };

        Ok(Self {
            inner,
            candidates,
            chain_checks: None,
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some() || self.candidates.is_some()
    }

    pub fn with_chain_checks(mut self, tx: UnboundedSender<ChainCheckRequest>) -> Self {
        self.chain_checks = Some(ChainCheckSink(tx));
        self
    }

    pub fn record_forwarded(&self, share: ForwardedShare<'_>) {
        if let Some(inner) = &self.inner {
            self.record_on(inner, &share);
        }
        if let Some(candidates) = &self.candidates {
            self.record_on(candidates, &share);
        }
        if share.block_candidate {
            self.enqueue_chain_check(&share);
        }
    }

    fn enqueue_chain_check(&self, share: &ForwardedShare<'_>) {
        let Some(sink) = &self.chain_checks else {
            return;
        };
        let request = ChainCheckRequest {
            share_hash: share.share_hash.clone(),
            worker: share.worker.map(str::to_string),
            downstream_channel_id: share.downstream_channel_id,
            upstream_channel_id: share.upstream_channel_id,
            job_id: share.job_id,
            sequence_number: share.sequence_number,
        };
        if sink.0.send(request).is_err() {
            warn!(
                share_hash = %share.share_hash,
                "chain status: checker is not running"
            );
            return;
        }
        info!(
            share_hash = %share.share_hash,
            "chain status: queued block candidate for bitcoind check"
        );
    }

    /// Appends one bitcoind answer for a block candidate.
    ///
    /// The candidate line itself stays `chain_status=unknown`. This writes a separate
    /// `event=chain_status` log and JSONL row:
    /// - `in_best_chain` (info): header is in the node's active chain, with `confirmations`
    ///   (the tip is 1).
    /// - `orphaned_or_stale` (warn): the node knows the header and it is not in that chain
    ///   (`confirmations` is negative).
    /// - `not_found` (info): this node has not seen the hash. `poll_expired` is true when the
    ///   time limit passed on that answer.
    ///
    /// `network` is the node chain (`main` is recorded as `mainnet`). A failed RPC is not
    /// written here.
    pub fn record_chain_status(&self, update: &ChainStatusUpdate) {
        match update.chain_status {
            "in_best_chain" => info!(
                event = "chain_status",
                share_hash = %update.share_hash,
                worker = update.worker.as_deref().unwrap_or(""),
                chain_status = update.chain_status,
                confirmations = update.confirmations.unwrap_or(0),
                network = update.network.as_deref().unwrap_or(""),
                poll_expired = update.poll_expired,
                "chain_status block is in the best chain"
            ),
            "orphaned_or_stale" => warn!(
                event = "chain_status",
                share_hash = %update.share_hash,
                worker = update.worker.as_deref().unwrap_or(""),
                chain_status = update.chain_status,
                confirmations = update.confirmations.unwrap_or(-1),
                network = update.network.as_deref().unwrap_or(""),
                poll_expired = update.poll_expired,
                "chain_status header is known but not in the best chain"
            ),
            _ => info!(
                event = "chain_status",
                share_hash = %update.share_hash,
                worker = update.worker.as_deref().unwrap_or(""),
                chain_status = update.chain_status,
                network = update.network.as_deref().unwrap_or(""),
                poll_expired = update.poll_expired,
                "chain_status block hash is not in the node yet"
            ),
        }

        let line = json!({
            "event": "chain_status",
            "ts": now_rfc3339(),
            "checked_at": now_rfc3339(),
            "share_hash": update.share_hash,
            "worker": update.worker,
            "downstream_channel_id": update.downstream_channel_id,
            "upstream_channel_id": update.upstream_channel_id,
            "job_id": update.job_id,
            "sequence_number": update.sequence_number,
            "chain_status": update.chain_status,
            "confirmations": update.confirmations,
            "network": update.network,
            "poll_expired": update.poll_expired,
        });
        if let Some(inner) = &self.inner {
            self.write_line(inner, line.clone());
        }
        if let Some(candidates) = &self.candidates {
            self.write_line(candidates, line);
        }
    }

    fn record_on(&self, inner: &ShareAuditInner, share: &ForwardedShare<'_>) {
        let pending = PendingForward {
            share_hash: share.share_hash.clone(),
            worker: share.worker.map(str::to_string),
            job_id: share.job_id,
            downstream_channel_id: share.downstream_channel_id,
            block_candidate: share.block_candidate,
        };

        if share.block_candidate {
            self.write_line(
                inner,
                json!({
                    "event": "block_candidate",
                    "ts": now_rfc3339(),
                    "worker": share.worker,
                    "downstream_channel_id": share.downstream_channel_id,
                    "upstream_channel_id": share.upstream_channel_id,
                    "job_id": share.job_id,
                    "sequence_number": share.sequence_number,
                    "share_hash": share.share_hash,
                    "nbits": share.nbits.map(|n| format!("0x{n:08x}")),
                    "meets_network_target": true,
                    "block_candidate": true,
                    "chain_status": "unknown",
                }),
            );
        }

        let should_write_forward = share.block_candidate || !inner.block_candidates_only;
        if should_write_forward {
            self.write_line(
                inner,
                json!({
                    "event": "share_forwarded",
                    "ts": now_rfc3339(),
                    "worker": share.worker,
                    "downstream_channel_id": share.downstream_channel_id,
                    "upstream_channel_id": share.upstream_channel_id,
                    "job_id": share.job_id,
                    "sequence_number": share.sequence_number,
                    "share_hash": share.share_hash,
                    "block_candidate": share.block_candidate,
                }),
            );
        }

        // Keep pending when we care about upstream correlation for this share.
        if should_write_forward {
            if let Ok(mut map) = inner.pending.lock() {
                map.insert((share.upstream_channel_id, share.sequence_number), pending);
            }
        }
    }

    /// Braiins (and others) batch acks: correlate the last `accepted_count` sequences.
    pub fn on_upstream_success(
        &self,
        upstream_channel_id: u32,
        last_sequence_number: u32,
        accepted_count: u32,
    ) {
        if let Some(inner) = &self.inner {
            self.success_on(
                inner,
                upstream_channel_id,
                last_sequence_number,
                accepted_count,
            );
        }
        if let Some(candidates) = &self.candidates {
            self.success_on(
                candidates,
                upstream_channel_id,
                last_sequence_number,
                accepted_count,
            );
        }
    }

    fn success_on(
        &self,
        inner: &ShareAuditInner,
        upstream_channel_id: u32,
        last_sequence_number: u32,
        accepted_count: u32,
    ) {
        if accepted_count == 0 {
            return;
        }

        let first = last_sequence_number.saturating_sub(accepted_count.saturating_sub(1));
        for sequence_number in first..=last_sequence_number {
            let pending = {
                let Ok(mut map) = inner.pending.lock() else {
                    continue;
                };
                map.remove(&(upstream_channel_id, sequence_number))
            };
            let Some(pending) = pending else {
                continue;
            };
            if inner.block_candidates_only && !pending.block_candidate {
                continue;
            }
            self.write_line(
                inner,
                json!({
                    "event": "share_upstream_result",
                    "ts": now_rfc3339(),
                    "upstream_channel_id": upstream_channel_id,
                    "downstream_channel_id": pending.downstream_channel_id,
                    "sequence_number": sequence_number,
                    "job_id": pending.job_id,
                    "worker": pending.worker,
                    "share_hash": pending.share_hash,
                    "result": "success",
                    "block_candidate": pending.block_candidate,
                }),
            );
        }
    }

    pub fn on_upstream_error(
        &self,
        upstream_channel_id: u32,
        sequence_number: u32,
        error_code: &str,
    ) {
        if let Some(inner) = &self.inner {
            self.error_on(inner, upstream_channel_id, sequence_number, error_code);
        }
        if let Some(candidates) = &self.candidates {
            self.error_on(candidates, upstream_channel_id, sequence_number, error_code);
        }
    }

    fn error_on(
        &self,
        inner: &ShareAuditInner,
        upstream_channel_id: u32,
        sequence_number: u32,
        error_code: &str,
    ) {
        let pending = {
            let Ok(mut map) = inner.pending.lock() else {
                return;
            };
            map.remove(&(upstream_channel_id, sequence_number))
        };
        // A candidates-only file never stores ordinary forwards, so an unmatched nack
        // here is not a block candidate. Skip it instead of writing a null-hash row.
        let Some(p) = pending else {
            if inner.block_candidates_only {
                return;
            }
            self.write_line(
                inner,
                json!({
                    "event": "share_upstream_result",
                    "ts": now_rfc3339(),
                    "upstream_channel_id": upstream_channel_id,
                    "downstream_channel_id": Value::Null,
                    "sequence_number": sequence_number,
                    "job_id": Value::Null,
                    "worker": Value::Null,
                    "share_hash": Value::Null,
                    "result": "error",
                    "error_code": error_code,
                    "block_candidate": false,
                }),
            );
            return;
        };
        if inner.block_candidates_only && !p.block_candidate {
            return;
        }
        let (share_hash, worker, job_id, downstream_channel_id, block_candidate) = (
            Some(p.share_hash),
            p.worker,
            Some(p.job_id),
            Some(p.downstream_channel_id),
            p.block_candidate,
        );

        self.write_line(
            inner,
            json!({
                "event": "share_upstream_result",
                "ts": now_rfc3339(),
                "upstream_channel_id": upstream_channel_id,
                "downstream_channel_id": downstream_channel_id,
                "sequence_number": sequence_number,
                "job_id": job_id,
                "worker": worker,
                "share_hash": share_hash,
                "result": "error",
                "error_code": error_code,
                "block_candidate": block_candidate,
            }),
        );
    }

    fn write_line(&self, inner: &ShareAuditInner, value: Value) {
        let Ok(mut file) = inner.file.lock() else {
            warn!("share audit: failed to lock JSONL file");
            return;
        };
        if let Err(e) = writeln!(file, "{value}") {
            warn!(error = %e, "share audit: failed to write JSONL line");
            return;
        }
        if let Err(e) = file.flush() {
            warn!(error = %e, "share audit: failed to flush JSONL file");
        }
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn open_inner(path: &Path, block_candidates_only: bool) -> io::Result<Arc<ShareAuditInner>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    Ok(Arc::new(ShareAuditInner {
        file: Mutex::new(file),
        block_candidates_only,
        pending: Mutex::new(HashMap::new()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    #[test]
    fn writes_forward_and_success_with_hash_correlation() {
        let dir = std::env::temp_dir().join(format!("tproxy-share-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");

        let audit = ShareAudit::open(&path, false).unwrap();
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 7,
            job_id: 42,
            sequence_number: 10,
            share_hash: "aa".repeat(32),
            block_candidate: false,
            nbits: None,
        });
        audit.on_upstream_success(7, 10, 1);

        let file = File::open(&path).unwrap();
        let lines: Vec<String> = BufReader::new(file).lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 2);

        let forward: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(forward["event"], "share_forwarded");
        assert_eq!(forward["sequence_number"], 10);
        assert_eq!(forward["share_hash"], "aa".repeat(32));

        let result: Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(result["event"], "share_upstream_result");
        assert_eq!(result["result"], "success");
        assert_eq!(result["share_hash"], "aa".repeat(32));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_candidates_only_skips_ordinary_forwards() {
        let dir =
            std::env::temp_dir().join(format!("tproxy-share-audit-bco-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");

        let audit = ShareAudit::open(&path, true).unwrap();
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 7,
            job_id: 1,
            sequence_number: 1,
            share_hash: "bb".repeat(32),
            block_candidate: false,
            nbits: None,
        });
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 7,
            job_id: 1,
            sequence_number: 2,
            share_hash: "cc".repeat(32),
            block_candidate: true,
            nbits: Some(0x1702_0093),
        });

        let file = File::open(&path).unwrap();
        let lines: Vec<String> = BufReader::new(file).lines().map(|l| l.unwrap()).collect();
        // block_candidate event + share_forwarded for the candidate only
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().any(|l| l.contains("block_candidate")));
        assert!(lines.iter().any(|l| l.contains("share_forwarded")));
        assert!(!lines.iter().any(|l| l.contains(&"bb".repeat(32))));
        assert!(lines.iter().any(|l| l.contains("0x17020093")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("\"chain_status\":\"unknown\""))
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedicated_candidate_file_records_candidates_when_share_audit_is_off() {
        let dir =
            std::env::temp_dir().join(format!("tproxy-share-audit-cand-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let candidates = dir.join("block-candidates.jsonl");

        let audit = ShareAudit::open_audits(None, false, Some(&candidates)).unwrap();
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 1,
            job_id: 9,
            sequence_number: 3,
            share_hash: "dd".repeat(32),
            block_candidate: false,
            nbits: None,
        });
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 1,
            job_id: 9,
            sequence_number: 4,
            share_hash: "ee".repeat(32),
            block_candidate: true,
            nbits: Some(0x1702_0093),
        });

        let lines: Vec<String> = BufReader::new(File::open(&candidates).unwrap())
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert!(
            lines
                .iter()
                .all(|l| l.contains("block_candidate") || l.contains("ee"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("\"event\":\"block_candidate\""))
        );
        assert!(!lines.iter().any(|l| l.contains(&"dd".repeat(32))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dedicated_candidate_file_skips_ordinary_upstream_errors() {
        let dir = std::env::temp_dir().join(format!(
            "tproxy-share-audit-cand-err-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shares = dir.join("shares.jsonl");
        let candidates = dir.join("block-candidates.jsonl");

        let audit = ShareAudit::open_audits(Some(&shares), false, Some(&candidates)).unwrap();
        audit.record_forwarded(ForwardedShare {
            worker: Some("rig1"),
            downstream_channel_id: 1,
            upstream_channel_id: 1,
            job_id: 8,
            sequence_number: 22,
            share_hash: "ff".repeat(32),
            block_candidate: false,
            nbits: Some(0x1702_0093),
        });
        audit.on_upstream_error(1, 22, "difficulty-too-low");

        let candidate_lines: Vec<String> = BufReader::new(File::open(&candidates).unwrap())
            .lines()
            .map(|l| l.unwrap())
            .collect();
        assert!(candidate_lines.is_empty());

        let share_text = std::fs::read_to_string(&shares).unwrap();
        assert!(share_text.contains("difficulty-too-low"));
        assert!(share_text.contains(&"ff".repeat(32)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        BufReader::new(File::open(path).unwrap())
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    fn temp_audit_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("tproxy-share-audit-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        (dir, path)
    }

    fn forward<'a>(
        worker: &'a str,
        downstream_channel_id: u32,
        upstream_channel_id: u32,
        sequence_number: u32,
        share_hash: String,
    ) -> ForwardedShare<'a> {
        ForwardedShare {
            worker: Some(worker),
            downstream_channel_id,
            upstream_channel_id,
            job_id: 42,
            sequence_number,
            share_hash,
            block_candidate: false,
            nbits: None,
        }
    }

    /// Aggregated mode uses one upstream channel and one sequence series for every miner.
    /// A batched ack must keep each worker's hash, and the same sequences on another
    /// channel must not consume them.
    #[test]
    fn aggregated_batch_ack_correlates_each_share_on_the_shared_channel() {
        let (dir, path) = temp_audit_path("agg");
        let audit = ShareAudit::open(&path, false).unwrap();
        audit.record_forwarded(forward("rig-a", 1, 42, 8, "aa".repeat(32)));
        audit.record_forwarded(forward("rig-b", 2, 42, 9, "bb".repeat(32)));
        audit.record_forwarded(forward("rig-a", 1, 42, 10, "cc".repeat(32)));

        audit.on_upstream_success(42, 9, 2);
        audit.on_upstream_success(99, 9, 2);
        audit.on_upstream_error(42, 10, "difficulty-too-low");

        let lines = read_jsonl(&path);
        let results: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "share_upstream_result")
            .collect();
        assert_eq!(results.len(), 3);

        assert_eq!(results[0]["sequence_number"], 8);
        assert_eq!(results[0]["result"], "success");
        assert_eq!(results[0]["share_hash"], "aa".repeat(32));
        assert_eq!(results[0]["worker"], "rig-a");
        assert_eq!(results[0]["downstream_channel_id"], 1);
        assert_eq!(results[0]["upstream_channel_id"], 42);

        assert_eq!(results[1]["sequence_number"], 9);
        assert_eq!(results[1]["result"], "success");
        assert_eq!(results[1]["share_hash"], "bb".repeat(32));
        assert_eq!(results[1]["worker"], "rig-b");
        assert_eq!(results[1]["downstream_channel_id"], 2);
        assert_eq!(results[1]["upstream_channel_id"], 42);

        assert_eq!(results[2]["sequence_number"], 10);
        assert_eq!(results[2]["result"], "error");
        assert_eq!(results[2]["error_code"], "difficulty-too-low");
        assert_eq!(results[2]["share_hash"], "cc".repeat(32));
        assert_eq!(results[2]["worker"], "rig-a");
        assert_eq!(results[2]["downstream_channel_id"], 1);
        assert_eq!(results[2]["upstream_channel_id"], 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-aggregated mode gives each miner its own upstream channel. Sequence 1 on
    /// channel 11 and sequence 1 on channel 22 are different shares.
    #[test]
    fn non_aggregated_ack_stays_on_the_channel_that_forwarded_it() {
        let (dir, path) = temp_audit_path("nonagg");
        let audit = ShareAudit::open(&path, false).unwrap();
        audit.record_forwarded(forward("rig-a", 11, 11, 1, "aa".repeat(32)));
        audit.record_forwarded(forward("rig-b", 22, 22, 1, "bb".repeat(32)));

        audit.on_upstream_success(11, 1, 1);
        audit.on_upstream_error(22, 1, "difficulty-too-low");

        let lines = read_jsonl(&path);
        let results: Vec<&Value> = lines
            .iter()
            .filter(|line| line["event"] == "share_upstream_result")
            .collect();
        assert_eq!(results.len(), 2);

        assert_eq!(results[0]["upstream_channel_id"], 11);
        assert_eq!(results[0]["downstream_channel_id"], 11);
        assert_eq!(results[0]["sequence_number"], 1);
        assert_eq!(results[0]["result"], "success");
        assert_eq!(results[0]["share_hash"], "aa".repeat(32));
        assert_eq!(results[0]["worker"], "rig-a");

        assert_eq!(results[1]["upstream_channel_id"], 22);
        assert_eq!(results[1]["downstream_channel_id"], 22);
        assert_eq!(results[1]["sequence_number"], 1);
        assert_eq!(results[1]["result"], "error");
        assert_eq!(results[1]["error_code"], "difficulty-too-low");
        assert_eq!(results[1]["share_hash"], "bb".repeat(32));
        assert_eq!(results[1]["worker"], "rig-b");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
